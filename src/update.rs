use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use cargo_metadata::DependencyKind;
use clap::Args;
use erris::prelude::*;
use erris::report;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::Format;
use crate::commits::{self, Change, Level};
use crate::effective::Effective;
use crate::ignore::{self, Ignore};
use crate::manifest::{Manifests, needs_req_update};
use crate::parallel;
use crate::registry::{Registries, Registry};
use crate::workspace::{Member, Selection, Workspace};

/// Files cargo generates into the `.crate`; the manifest is compared separately.
const GENERATED: [&str; 4] = ["Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"];

/// Options shared by `update` and `check`.
#[derive(Args, Debug)]
pub struct DetectArgs {
    #[command(flatten)]
    selection: Selection,
    /// Registry to compare against.
    #[arg(long)]
    registry: Option<String>,
    /// Count changes in all files, ignoring nothing (not even `*.md`).
    #[arg(long, conflicts_with = "ignore")]
    all: bool,
    /// Ignore changes in files matching this glob, relative to the package (repeatable).
    /// Adds to `[package.metadata.publish-plz] ignore` / `[workspace.metadata.publish-plz] ignore`,
    /// which default to `["*.md"]`.
    #[arg(long, value_name = "GLOB")]
    ignore: Vec<String>,
    /// Don't fetch git history in shallow clones.
    #[arg(long)]
    no_fetch: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    format: Format,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[command(flatten)]
    detect: DetectArgs,
    /// Bump the selected packages by this level instead of the one derived from commits.
    #[arg(long, value_enum, conflicts_with = "version")]
    bump: Option<Level>,
    /// Set the selected packages to this version.
    #[arg(long, value_name = "VERSION")]
    version: Option<Version>,
    /// Print what would change without writing anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    #[command(flatten)]
    detect: DetectArgs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Status {
    /// The crate was never published.
    New,
    /// The crate exists, but not with the local version: it is waiting for `publish`.
    Pending,
    /// The local version is published.
    Published,
    /// `package.publish` doesn't allow the selected registry.
    NotAllowed,
}

#[derive(Clone)]
enum Target {
    Change(Change),
    Level(Level),
    Exact(Version),
}

impl Target {
    fn apply(&self, version: &Version) -> Version {
        match self {
            Target::Change(change) => commits::next_version(version, *change),
            Target::Level(level) => commits::bump_level(version, *level),
            Target::Exact(exact) => exact.clone(),
        }
    }

    fn label(&self) -> String {
        match self {
            Target::Change(change) => change.to_string(),
            Target::Level(level) => level.to_string(),
            Target::Exact(version) => format!("={version}"),
        }
    }
}

struct Bump {
    target: Target,
    reason: String,
}

/// Result of comparing a package with its published `.crate`.
struct Detection {
    files: Vec<String>,
    /// What the manifest inherits from the workspace and changed, e.g. "dependency `serde`".
    inherited: Vec<String>,
    /// Commit the published version was packaged from.
    sha: Option<String>,
    warnings: Vec<String>,
}

#[derive(Deserialize)]
struct VcsInfo {
    git: VcsGit,
}

#[derive(Deserialize)]
struct VcsGit {
    sha1: String,
}

#[derive(Serialize)]
struct PackageReport {
    name: String,
    version: String,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    registry: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    changed_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inherited_changes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bump: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct RequirementReport {
    manifest: String,
    dependency: String,
    old: String,
    new: String,
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    packages: &'a [PackageReport],
    requirements: &'a [RequirementReport],
    #[serde(skip_serializing_if = "Option::is_none")]
    dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
}

struct Plan {
    packages: Vec<PackageReport>,
    requirements: Vec<RequirementReport>,
}

impl Plan {
    fn bumped(&self) -> impl Iterator<Item = &PackageReport> {
        self.packages.iter().filter(|p| p.next_version.is_some())
    }

    fn print_human(&self) {
        for package in self.bumped() {
            let next = package.next_version.as_deref().unwrap_or_default();
            let reason = package.reason.as_deref().unwrap_or_default();
            println!("{}: {} -> {next} ({reason})", package.name, package.version);
        }
        for req in &self.requirements {
            println!("  {}: `{}` {} -> {}", req.manifest, req.dependency, req.old, req.new);
        }
    }
}

pub fn run(args: &UpdateArgs) -> erris::Result<ExitCode> {
    let forced = match (&args.bump, &args.version) {
        (Some(level), _) => Some(Target::Level(*level)),
        (None, Some(version)) => Some(Target::Exact(version.clone())),
        (None, None) => None,
    };
    let ws = Workspace::load(&args.detect.selection)?;
    let mut manifests = Manifests::load(&ws)?;
    let plan = plan(&args.detect, forced.as_ref(), &ws, &mut manifests)?;
    let bumped = plan.bumped().count();

    match args.detect.format {
        Format::Human if bumped == 0 => println!("all packages are up to date"),
        Format::Human => plan.print_human(),
        Format::Json => print_json(&JsonOutput {
            packages: &plan.packages,
            requirements: &plan.requirements,
            dry_run: Some(args.dry_run),
            ok: None,
        })?,
    }
    if bumped == 0 {
        return Ok(ExitCode::SUCCESS);
    }
    if args.dry_run {
        eprintln!("dry run: nothing written");
        return Ok(ExitCode::SUCCESS);
    }

    manifests.save()?;
    let has_lockfile = ws.root.join("Cargo.lock").exists();
    if has_lockfile {
        let status = crate::cargo()
            .args(["update", "--workspace", "--quiet", "--manifest-path"])
            .arg(&ws.root_manifest)
            .status()?;
        if !status.success() {
            return Err(report!("`cargo update --workspace` failed: {status}"));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Like `update --dry-run`, but fails when a package changed without a version bump.
pub fn check(args: &CheckArgs) -> erris::Result<ExitCode> {
    let ws = Workspace::load(&args.detect.selection)?;
    let mut manifests = Manifests::load(&ws)?;
    let plan = plan(&args.detect, None, &ws, &mut manifests)?;
    let bumped = plan.bumped().count();

    match args.detect.format {
        Format::Human if bumped == 0 => println!("all changed packages have their versions bumped"),
        Format::Human => {
            plan.print_human();
            eprintln!(
                "error: {bumped} package(s) changed since their last release without a version bump; \
                 run `cargo publish-plz update`"
            );
        }
        Format::Json => print_json(&JsonOutput {
            packages: &plan.packages,
            requirements: &plan.requirements,
            dry_run: None,
            ok: Some(bumped == 0),
        })?,
    }
    Ok(if bumped == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub fn print_json(value: &impl Serialize) -> erris::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Detects changed packages and edits `manifests` in memory accordingly.
fn plan(args: &DetectArgs, forced: Option<&Target>, ws: &Workspace, manifests: &mut Manifests) -> erris::Result<Plan> {
    let selected = ws.select(&args.selection)?;
    let selected_names: BTreeSet<&str> = selected.iter().map(|m| m.name.as_str()).collect();
    let publishable: Vec<&Member> = ws.members.iter().filter(|m| m.is_publishable()).collect();

    // Registries are connected up front so lookups can run in parallel.
    let mut registries = Registries::new(&ws.root);
    let mut registry_of: HashMap<&str, Option<String>> = HashMap::new();
    for member in &publishable {
        let name = registries.name_for(args.registry.as_deref(), &member.name, member.publish.as_deref());
        let name = match name {
            Ok(name) => name,
            Err(err) if selected_names.contains(member.name.as_str()) => return Err(err),
            // Not selected: it only matters for propagation; leave it alone.
            Err(_) => None,
        };
        if let Some(name) = &name {
            registries.connect(name)?;
        }
        registry_of.insert(&member.name, name);
    }
    let registry_of = &registry_of;
    let registries = &registries;
    let registry_for = |member: &Member| -> erris::Result<Option<&Registry>> {
        match registry_of.get(member.name.as_str()) {
            Some(Some(name)) => Ok(Some(registries.get(name)?)),
            _ => Ok(None),
        }
    };

    let statuses = parallel::map(&publishable, |member| -> erris::Result<Status> {
        let Some(registry) = registry_for(member)? else {
            return Ok(Status::NotAllowed);
        };
        let versions = registry.versions(&member.name)?;
        Ok(if versions.is_empty() {
            Status::New
        } else if versions.contains(&member.version) {
            Status::Published
        } else {
            Status::Pending
        })
    });
    let mut status_of: HashMap<&str, Status> = HashMap::new();
    for (member, status) in publishable.iter().zip(statuses) {
        let status: Status = status?;
        status_of.insert(&member.name, status);
    }

    let mut ignores: HashMap<&str, Ignore> = HashMap::new();
    for member in &selected {
        let ignore = ignore_for(args, ws, member)?;
        ignores.insert(&member.name, ignore);
    }
    let ignores = &ignores;
    let no_ignore = Ignore::nothing();
    let ignore_of = |member: &Member| ignores.get(member.name.as_str()).unwrap_or(&no_ignore);

    let mut bumps: BTreeMap<String, Bump> = BTreeMap::new();
    if let Some(target) = forced {
        let reason = match target {
            Target::Exact(version) => format!("--version {version}"),
            other => format!("--bump {}", other.label()),
        };
        for member in &selected {
            let bump = Bump {
                target: target.clone(),
                reason: reason.clone(),
            };
            bumps.insert(member.name.clone(), bump);
        }
    }

    let to_detect: Vec<&Member> = selected
        .iter()
        .copied()
        .filter(|m| !bumps.contains_key(&m.name) && status_of.get(m.name.as_str()) == Some(&Status::Published))
        .collect();
    let detections = parallel::map(&to_detect, |member| {
        let registry =
            registry_for(member)?.ok_or_report_with(|| report!("`{}`: no registry to compare against", member.name))?;
        detect(ws, registry, member, ignore_of(member))
    });

    let mut changed: Vec<(&Member, Detection)> = Vec::new();
    let mut unchanged: BTreeSet<&str> = BTreeSet::new();
    for (member, detection) in to_detect.iter().zip(detections) {
        let detection = detection?;
        for warning in &detection.warnings {
            eprintln!("warning: {}@{}: {warning}", member.name, member.version);
        }
        if detection.files.is_empty() && detection.inherited.is_empty() {
            unchanged.insert(&member.name);
        } else {
            changed.push((member, detection));
        }
    }

    for member in &selected {
        let id = format!("{}@{}", member.name, member.version);
        match status_of.get(member.name.as_str()) {
            _ if bumps.contains_key(&member.name) => {}
            Some(Status::New) => eprintln!("{id}: never published, nothing to bump"),
            Some(Status::Pending) => eprintln!("{id}: not published yet, nothing to bump"),
            Some(Status::NotAllowed) | None => eprintln!("{id}: not published to the selected registry, skipping"),
            Some(Status::Published) if unchanged.contains(member.name.as_str()) => eprintln!("{id}: unchanged"),
            Some(Status::Published) => {}
        }
    }

    let repo = git_toplevel(&ws.root);
    if let Some(repo) = &repo {
        let shas: Vec<&str> = changed.iter().filter_map(|(_, d)| d.sha.as_deref()).collect();
        ensure_history(repo, &shas, args.no_fetch)?;
    }
    let commit_changes = parallel::map(&changed, |(member, detection)| {
        let (Some(repo), Some(sha)) = (&repo, &detection.sha) else {
            return Ok(None);
        };
        commits_change(ws, repo, member, sha, ignore_of(member))
    });
    for ((member, detection), commit_change) in changed.iter().zip(commit_changes) {
        let commit_change = commit_change?;
        let (change, commits) = commit_change.unwrap_or_else(|| {
            eprintln!(
                "warning: {}@{}: source commit of the published version is unknown, assuming a fix",
                member.name, member.version
            );
            (Change::Fix, 0)
        });
        let mut what = Vec::new();
        match detection.files.as_slice() {
            [] => {}
            [single] => what.push(format!("`{single}` changed")),
            all => what.push(format!("{} files changed", all.len())),
        }
        if !detection.inherited.is_empty() {
            what.push(format!("inherited {} changed", detection.inherited.join(", ")));
        }
        what.push(if commits == 1 {
            "1 commit".to_owned()
        } else {
            format!("{commits} commits")
        });
        let bump = Bump {
            target: Target::Change(change),
            reason: format!("{change}: {}", what.join(", ")),
        };
        bumps.insert(member.name.clone(), bump);
    }

    // Members depending on bumped members get a patch bump, transitively.
    let workspace_version = manifests.workspace_version();
    let mut new_versions = BTreeMap::new();
    let mut propagating = true;
    while propagating {
        propagating = false;
        new_versions = plan_versions(ws, manifests, &bumps, workspace_version.as_ref());
        for member in publishable.iter().filter(|m| !new_versions.contains_key(&m.name)) {
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
            if status_of.get(member.name.as_str()) != Some(&Status::Published) {
                continue;
            }
            let bump = Bump {
                target: Target::Change(Change::Fix),
                reason: format!("dependency `{}` updated", dep.package),
            };
            bumps.insert(member.name.clone(), bump);
            propagating = true;
        }
    }

    for (name, new) in &new_versions {
        let Some(member) = ws.member(name) else {
            continue;
        };
        if new <= &member.version {
            return Err(report!(
                "`{name}`: new version {new} is not greater than {}",
                member.version
            ));
        }
        if manifests.inherits_version(&member.manifest_path) {
            manifests.set_workspace_version(new)?;
        } else {
            manifests.set_package_version(&member.manifest_path, new)?;
        }
    }
    let requirements = manifests
        .update_reqs(&new_versions)
        .into_iter()
        .map(|change| RequirementReport {
            manifest: change
                .manifest
                .strip_prefix(&ws.root)
                .unwrap_or(&change.manifest)
                .display()
                .to_string(),
            dependency: change.dependency,
            old: change.old,
            new: change.new,
        })
        .collect();

    let mut detection_of: HashMap<&str, &Detection> = HashMap::new();
    for (member, detection) in &changed {
        detection_of.insert(&member.name, detection);
    }
    let packages = publishable
        .iter()
        .filter(|m| selected_names.contains(m.name.as_str()) || new_versions.contains_key(&m.name))
        .map(|member| {
            let bump = bumps.get(&member.name);
            let next = new_versions.get(&member.name);
            PackageReport {
                name: member.name.clone(),
                version: member.version.to_string(),
                status: status_of
                    .get(member.name.as_str())
                    .copied()
                    .unwrap_or(Status::NotAllowed),
                registry: registry_of.get(member.name.as_str()).cloned().flatten(),
                changed_files: detection_of
                    .get(member.name.as_str())
                    .map(|d| d.files.clone())
                    .unwrap_or_default(),
                inherited_changes: detection_of
                    .get(member.name.as_str())
                    .map(|d| d.inherited.clone())
                    .unwrap_or_default(),
                next_version: next.map(ToString::to_string),
                bump: next.map(|_| bump.map_or_else(|| "workspace".to_owned(), |b| b.target.label())),
                reason: next
                    .map(|_| bump.map_or_else(|| "shares the workspace version".to_owned(), |b| b.reason.clone())),
            }
        })
        .collect();

    Ok(Plan { packages, requirements })
}

/// `--all` ignores nothing; otherwise the package's patterns, else the workspace's,
/// else `*.md`, plus `--ignore`.
fn ignore_for(args: &DetectArgs, ws: &Workspace, member: &Member) -> erris::Result<Ignore> {
    if args.all {
        return Ok(Ignore::nothing());
    }
    let configured = member.ignore.as_ref().or(ws.ignore.as_ref());
    let mut patterns: Vec<String> = match configured {
        Some(patterns) => patterns.clone(),
        None => ignore::DEFAULT.iter().map(|p| (*p).to_owned()).collect(),
    };
    patterns.extend(args.ignore.iter().cloned());
    Ignore::new(&patterns).wrap_report_with(|| report!("invalid ignore pattern for `{}`", member.name))
}

fn plan_versions(
    ws: &Workspace,
    manifests: &Manifests,
    bumps: &BTreeMap<String, Bump>,
    workspace_version: Option<&Version>,
) -> BTreeMap<String, Version> {
    let inherits = |m: &&Member| manifests.inherits_version(&m.manifest_path);
    // Members sharing `workspace.package.version` move together, to the highest of their bumps.
    let group_version = workspace_version.and_then(|current| {
        ws.members
            .iter()
            .filter(inherits)
            .filter_map(|m| bumps.get(&m.name))
            .map(|b| b.target.apply(current))
            .max()
    });

    let mut versions = BTreeMap::new();
    for member in &ws.members {
        let version = if inherits(&member) {
            group_version.clone()
        } else {
            bumps.get(&member.name).map(|b| b.target.apply(&member.version))
        };
        if let Some(version) = version {
            versions.insert(member.name.clone(), version);
        }
    }
    versions
}

/// Compares the local package with its published `.crate`.
fn detect(ws: &Workspace, registry: &Registry, member: &Member, ignore: &Ignore) -> erris::Result<Detection> {
    let published = registry.download(&member.name, &member.version)?;
    let mut warnings = Vec::new();
    let files = changed_files(ws, member, &published, ignore)?;
    let manifest_compared = files.iter().any(|f| f == "Cargo.toml") || ignore.is_ignored("Cargo.toml");
    let inherited = if manifest_compared {
        Vec::new()
    } else {
        inherited_changes(member, &published, &mut warnings)
    };
    let vcs_info = published
        .get(".cargo_vcs_info.json")
        .and_then(|j| serde_json::from_slice::<VcsInfo>(j).ok());
    Ok(Detection {
        files,
        inherited,
        sha: vcs_info.map(|info| info.git.sha1),
        warnings,
    })
}

/// Files that differ between the local package (as `cargo package` would pack it) and the
/// published one.
fn changed_files(
    ws: &Workspace,
    member: &Member,
    published: &BTreeMap<String, Vec<u8>>,
    ignore: &Ignore,
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
    let relevant = |path: &str| !GENERATED.contains(&path) && !ignore.is_ignored(path);

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

    if ignore.is_ignored("Cargo.toml") {
        return Ok(changed);
    }
    let manifest = std::fs::read(&member.manifest_path)?;
    let original = published.get("Cargo.toml.orig");
    if original.is_some_and(|orig| orig != &manifest) {
        changed.push("Cargo.toml".to_owned());
    }
    Ok(changed)
}

/// The member's own manifest is the same as published; what it inherits from the workspace
/// (dependency versions, features, edition, ...) may not be.
fn inherited_changes(
    member: &Member,
    published: &BTreeMap<String, Vec<u8>>,
    warnings: &mut Vec<String>,
) -> Vec<String> {
    let Some(normalized) = published.get("Cargo.toml") else {
        return Vec::new();
    };
    match Effective::of_published(normalized) {
        Ok(published) => Effective::of_member(member).diff(&published),
        Err(err) => {
            warnings.push(format!("can't read the published Cargo.toml: {err}"));
            Vec::new()
        }
    }
}

/// `readme` / `license-file` outside the package directory are packed at the package root.
fn local_source(member: &Member, path: &str) -> PathBuf {
    let in_package = member.dir.join(path);
    let exists = in_package.exists();
    if exists {
        return in_package;
    }
    [&member.readme, &member.license_file]
        .into_iter()
        .flatten()
        .find(|p| p.file_name().is_some_and(|n| n == path))
        .cloned()
        .unwrap_or(in_package)
}

/// In a shallow clone (CI) the commits packages were published from are often missing:
/// fetch the full history once instead of falling back to a patch bump.
fn ensure_history(repo: &Path, shas: &[&str], no_fetch: bool) -> erris::Result<()> {
    let mut missing = 0;
    for sha in shas {
        let exists = commit_exists(repo, sha);
        if !exists {
            missing += 1;
        }
    }
    if missing == 0 || no_fetch {
        return Ok(());
    }
    let output = git(repo).args(["rev-parse", "--is-shallow-repository"]).output()?;
    let shallow = String::from_utf8_lossy(&output.stdout).trim() == "true";
    if !shallow {
        return Ok(());
    }
    eprintln!("shallow clone: fetching the git history to find {missing} published commit(s)");
    let status = git(repo).args(["fetch", "--unshallow", "--quiet"]).status()?;
    if !status.success() {
        eprintln!("warning: `git fetch --unshallow` failed: {status}");
    }
    Ok(())
}

fn commit_exists(repo: &Path, sha: &str) -> bool {
    let output = git(repo)
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .output();
    output.is_ok_and(|o| o.status.success())
}

/// Strongest conventional-commit change among commits touching the package since `since`,
/// and how many there are. `None` when `since` is not in the local history.
fn commits_change(
    ws: &Workspace,
    repo: &Path,
    member: &Member,
    since: &str,
    ignore: &Ignore,
) -> erris::Result<Option<(Change, usize)>> {
    let exists = commit_exists(repo, since);
    if !exists {
        return Ok(None);
    }

    let package_dir = relative_to(repo, &member.dir);
    let mut cmd = git(repo);
    cmd.args([
        "log",
        "--format=%x1e%H%x00%B%x00",
        "--name-only",
        &format!("{since}..HEAD"),
        "--",
    ]);
    cmd.arg(&package_dir);
    // Don't attribute commits of nested members (e.g. root package of a workspace).
    for other in &ws.members {
        if other.dir != member.dir && other.dir.starts_with(&member.dir) {
            cmd.arg(format!(":(exclude){}", relative_to(repo, &other.dir).display()));
        }
    }
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(report!("`git log` failed: {}", String::from_utf8_lossy(&output.stderr)));
    }

    // `git log` paths are relative to the repository, ignore patterns to the package.
    let prefix = match package_dir.to_string_lossy().replace('\\', "/").as_str() {
        "." => String::new(),
        dir => format!("{dir}/"),
    };
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
        let relevant = files
            .lines()
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .any(|f| !ignore.is_ignored(f.strip_prefix(&prefix).unwrap_or(f)));
        if !relevant {
            continue;
        }
        count += 1;
        change = change.max(Some(commits::classify(message)));
    }
    // Files differ but no commits: uncommitted changes or a workspace-level change.
    Ok(Some((change.unwrap_or(Change::Fix), count)))
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
