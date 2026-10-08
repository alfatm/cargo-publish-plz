use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_metadata::DependencyKind;
use clap::Args;
use erris::prelude::*;
use erris::report;
use semver::Version;
use serde::Deserialize;

use crate::commits::{self, Change};
use crate::manifest::{Manifests, needs_req_update};
use crate::registry::Registries;
use crate::workspace::{Member, Selection, Workspace};

/// Files cargo generates into the `.crate`; `Cargo.toml.orig` is compared separately.
const GENERATED: [&str; 4] = ["Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"];

#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[command(flatten)]
    selection: Selection,
    /// Registry to compare against.
    #[arg(long)]
    registry: Option<String>,
    /// Count changes in all files. By default changes in `*.md` files are ignored.
    #[arg(long)]
    all: bool,
    /// Print what would change without writing anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    /// The crate was never published.
    New,
    /// The crate exists, but not with the local version: it is waiting for `publish`.
    Pending,
    /// The local version is published.
    Published,
    /// `package.publish` doesn't allow the `--registry` one.
    NotAllowed,
}

struct Bump {
    change: Change,
    reason: String,
}

#[derive(Deserialize)]
struct VcsInfo {
    git: VcsGit,
}

#[derive(Deserialize)]
struct VcsGit {
    sha1: String,
}

struct Ctx<'a> {
    args: &'a UpdateArgs,
    ws: &'a Workspace,
    repo: Option<PathBuf>,
    registries: Registries,
    statuses: HashMap<String, Status>,
}

pub fn run(args: &UpdateArgs) -> erris::Result<()> {
    let ws = Workspace::load(&args.selection)?;
    let selected = ws.select(&args.selection)?;
    let mut manifests = Manifests::load(&ws)?;
    let mut ctx = Ctx {
        args,
        ws: &ws,
        repo: git_toplevel(&ws.root),
        registries: Registries::new(&ws.root),
        statuses: HashMap::new(),
    };

    let mut bumps: BTreeMap<String, Bump> = BTreeMap::new();
    for member in selected {
        let bump = ctx.detect(member)?;
        if let Some(bump) = bump {
            bumps.insert(member.name.clone(), bump);
        }
    }

    // Members depending on bumped members get a patch bump, transitively.
    let workspace_version = manifests.workspace_version();
    let mut new_versions = BTreeMap::new();
    let mut changed = true;
    while changed {
        changed = false;
        new_versions = plan_versions(&ws, &manifests, &bumps, workspace_version.as_ref());
        for member in ws
            .members
            .iter()
            .filter(|m| m.is_publishable() && !new_versions.contains_key(&m.name))
        {
            let deps = manifests.local_deps(&member.manifest_path);
            let trigger = deps.iter().find(|dep| {
                let (Some(new), Some(req)) = (new_versions.get(&dep.package), &dep.req) else {
                    return false;
                };
                let kind = if dep.inherited {
                    DependencyKind::Normal
                } else {
                    dep.kind
                };
                needs_req_update(kind, req, new)
            });
            let Some(dep) = trigger else {
                continue;
            };
            let status = ctx.status(member)?;
            if status != Status::Published {
                continue;
            }
            let reason = format!("dependency `{}` updated", dep.package);
            bumps.insert(
                member.name.clone(),
                Bump {
                    change: Change::Fix,
                    reason,
                },
            );
            changed = true;
        }
    }

    if new_versions.is_empty() {
        println!("all packages are up to date");
        return Ok(());
    }

    for (name, new) in &new_versions {
        let Some(member) = ws.member(name) else {
            continue;
        };
        let inherits = manifests.inherits_version(&member.manifest_path);
        let reason = match bumps.get(name) {
            Some(bump) => bump.reason.as_str(),
            None => "shares the workspace version",
        };
        println!("{name}: {} -> {new} ({reason})", member.version);
        if inherits {
            manifests.set_workspace_version(new)?;
        } else {
            manifests.set_package_version(&member.manifest_path, new)?;
        }
    }
    for change in manifests.update_reqs(&new_versions) {
        let manifest = change.manifest.strip_prefix(&ws.root).unwrap_or(&change.manifest);
        println!(
            "  {}: `{}` {} -> {}",
            manifest.display(),
            change.dependency,
            change.old,
            change.new
        );
    }

    if args.dry_run {
        eprintln!("dry run: nothing written");
        return Ok(());
    }
    manifests.save()?;
    if ws.root.join("Cargo.lock").exists() {
        let status = crate::cargo()
            .args(["update", "--workspace", "--quiet", "--manifest-path"])
            .arg(&ws.root_manifest)
            .status()?;
        if !status.success() {
            return Err(report!("`cargo update --workspace` failed: {status}"));
        }
    }
    Ok(())
}

fn plan_versions(
    ws: &Workspace,
    manifests: &Manifests,
    bumps: &BTreeMap<String, Bump>,
    workspace_version: Option<&Version>,
) -> BTreeMap<String, Version> {
    let inherits = |m: &&Member| manifests.inherits_version(&m.manifest_path);
    let group_change = ws
        .members
        .iter()
        .filter(inherits)
        .filter_map(|m| bumps.get(&m.name))
        .map(|b| b.change)
        .max();
    let group_version = group_change
        .zip(workspace_version)
        .map(|(change, v)| commits::next_version(v, change));

    let mut versions = BTreeMap::new();
    for member in &ws.members {
        let version = if inherits(&member) {
            group_version.clone()
        } else {
            bumps
                .get(&member.name)
                .map(|b| commits::next_version(&member.version, b.change))
        };
        if let Some(version) = version {
            versions.insert(member.name.clone(), version);
        }
    }
    versions
}

impl Ctx<'_> {
    fn status(&mut self, member: &Member) -> erris::Result<Status> {
        if let Some(status) = self.statuses.get(&member.name) {
            return Ok(*status);
        }
        let registry_name =
            self.registries
                .name_for(self.args.registry.as_deref(), &member.name, member.publish.as_deref())?;
        let Some(registry_name) = registry_name else {
            self.statuses.insert(member.name.clone(), Status::NotAllowed);
            return Ok(Status::NotAllowed);
        };
        let registry = self.registries.get(&registry_name)?;
        let versions = registry.versions(&member.name)?;
        let status = if versions.is_empty() {
            Status::New
        } else if versions.contains(&member.version) {
            Status::Published
        } else {
            Status::Pending
        };
        self.statuses.insert(member.name.clone(), status);
        Ok(status)
    }

    /// Compares the local package with its published `.crate`.
    fn detect(&mut self, member: &Member) -> erris::Result<Option<Bump>> {
        let id = format!("{}@{}", member.name, member.version);
        match self.status(member)? {
            Status::New => {
                eprintln!("{id}: never published, nothing to bump");
                return Ok(None);
            }
            Status::Pending => {
                eprintln!("{id}: not published yet, nothing to bump");
                return Ok(None);
            }
            Status::NotAllowed => {
                eprintln!("{id}: not published to the selected registry, skipping");
                return Ok(None);
            }
            Status::Published => {}
        }

        let registry_name = self
            .registries
            .name_for(self.args.registry.as_deref(), &member.name, member.publish.as_deref())?
            .ok_or_report_with(|| report!("{id}: no registry to compare against"))?;
        let published = self
            .registries
            .get(&registry_name)?
            .download(&member.name, &member.version)?;
        let changed = changed_files(self.ws, member, &published, self.args.all)?;
        if changed.is_empty() {
            eprintln!("{id}: unchanged");
            return Ok(None);
        }

        let vcs_info = published
            .get(".cargo_vcs_info.json")
            .and_then(|j| serde_json::from_slice::<VcsInfo>(j).ok());
        let (change, commits) = match (&self.repo, vcs_info) {
            (Some(repo), Some(info)) => self.commits_change(repo, member, &info.git.sha1)?,
            _ => {
                eprintln!("warning: {id}: unknown source commit, assuming a fix");
                (Change::Fix, 0)
            }
        };
        let files = match changed.as_slice() {
            [single] => format!("`{single}` changed"),
            all => format!("{} files changed", all.len()),
        };
        let commits = if commits == 1 {
            "1 commit".to_owned()
        } else {
            format!("{commits} commits")
        };
        Ok(Some(Bump {
            change,
            reason: format!("{change}: {files}, {commits}"),
        }))
    }

    /// Strongest conventional-commit change among commits touching the package since `since`.
    fn commits_change(&self, repo: &Path, member: &Member, since: &str) -> erris::Result<(Change, usize)> {
        let exists = git(repo)
            .args(["cat-file", "-e", &format!("{since}^{{commit}}")])
            .output()?;
        if !exists.status.success() {
            eprintln!(
                "warning: {}@{}: published from commit {since} which is not in the local history (shallow clone?), assuming a fix",
                member.name, member.version
            );
            return Ok((Change::Fix, 0));
        }

        let mut cmd = git(repo);
        cmd.args([
            "log",
            "--format=%x1e%H%x00%B%x00",
            "--name-only",
            &format!("{since}..HEAD"),
            "--",
        ]);
        cmd.arg(relative_to(repo, &member.dir));
        // Don't attribute commits of nested members (e.g. root package of a workspace).
        for other in &self.ws.members {
            if other.dir != member.dir && other.dir.starts_with(&member.dir) {
                cmd.arg(format!(":(exclude){}", relative_to(repo, &other.dir).display()));
            }
        }
        let output = cmd.output()?;
        if !output.status.success() {
            return Err(report!("`git log` failed: {}", String::from_utf8_lossy(&output.stderr)));
        }

        let log = String::from_utf8_lossy(&output.stdout);
        let mut change = None;
        let mut count = 0;
        for record in log.split('\x1e').filter(|r| !r.trim().is_empty()) {
            let mut parts = record.split('\0');
            let (_hash, message, files) = (
                parts.next(),
                parts.next().unwrap_or_default(),
                parts.next().unwrap_or_default(),
            );
            let relevant = self.args.all || files.lines().any(|f| !f.trim().is_empty() && !is_markdown(f.trim()));
            if !relevant {
                continue;
            }
            count += 1;
            change = change.max(Some(commits::classify(message)));
        }
        // Files differ but no commits: uncommitted changes.
        Ok((change.unwrap_or(Change::Fix), count))
    }
}

/// Files that differ between the local package (as `cargo package` would pack it) and the published one.
fn changed_files(
    ws: &Workspace,
    member: &Member,
    published: &BTreeMap<String, Vec<u8>>,
    all: bool,
) -> erris::Result<Vec<String>> {
    let output = crate::cargo()
        .args(["package", "--list", "--allow-dirty", "--quiet", "--manifest-path"])
        .arg(&ws.root_manifest)
        .args(["-p", &member.name])
        .output()?;
    if !output.status.success() {
        return Err(report!(
            "`cargo package --list -p {}` failed:\n{}",
            member.name,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let local: BTreeSet<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().replace('\\', "/"))
        .filter(|l| !l.is_empty())
        .collect();
    let relevant = |path: &str| !GENERATED.contains(&path) && (all || !is_markdown(path));

    let mut changed = Vec::new();
    for path in local.iter().filter(|p| relevant(p)) {
        let source = local_source(member, path);
        let content = std::fs::read(&source).wrap_report_with(|| report!("failed to read {}", source.display()))?;
        if published.get(path) != Some(&content) {
            changed.push(path.clone());
        }
    }
    let removed = published.keys().filter(|p| relevant(p) && !local.contains(*p));
    changed.extend(removed.cloned());

    let manifest = std::fs::read(&member.manifest_path)?;
    let original = published.get("Cargo.toml.orig");
    if original.is_some_and(|orig| orig != &manifest) {
        changed.push("Cargo.toml".to_owned());
    }
    Ok(changed)
}

/// `readme` / `license-file` outside the package directory are packed at the package root.
fn local_source(member: &Member, path: &str) -> PathBuf {
    let in_package = member.dir.join(path);
    if in_package.exists() {
        return in_package;
    }
    [&member.readme, &member.license_file]
        .into_iter()
        .flatten()
        .find(|p| p.file_name().is_some_and(|n| n == path))
        .cloned()
        .unwrap_or(in_package)
}

fn is_markdown(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

fn git(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    cmd
}

fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Some(path.canonicalize().unwrap_or(path))
}

fn relative_to(repo: &Path, dir: &Path) -> PathBuf {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    match dir.strip_prefix(repo) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => dir,
    }
}
