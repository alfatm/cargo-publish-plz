//! Releases of `publish = false` packages, which reach their users through git: a version is released by the commit
//! that set it, and the package changed when its files differ from that commit.

use std::collections::BTreeSet;
use std::path::Path;

use erris::report;
use semver::Version;
use toml_edit::{DocumentMut, Item};

use crate::git;
use crate::ignore::Ignore;
use crate::workspace::{Member, Workspace};

/// Where the package's version stands against `HEAD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// `HEAD` has no such package: it is not committed yet.
    New,
    /// `HEAD` has another version: the bump is not committed yet.
    Pending,
    /// `HEAD` has this version.
    Committed,
}

/// The commit that set the version of a [`State::Committed`] package.
#[derive(Debug, PartialEq, Eq)]
pub enum Release {
    Commit(String),
    /// The history ends at a shallow boundary before the version was set.
    Cut,
}

/// The package's files for git: its directory, without the directories of nested members (the members of a
/// workspace whose root is also a package).
pub struct Pathspec {
    pub args: Vec<String>,
    /// Of the paths git prints, relative to the repository.
    prefix: String,
}

impl Pathspec {
    pub fn of(ws: &Workspace, repo: &Path, member: &Member) -> Self {
        let package_dir = git::relative_to(repo, &member.dir);
        let mut args = vec![package_dir.to_string_lossy().into_owned()];
        for other in &ws.members {
            if other.dir != member.dir && other.dir.starts_with(&member.dir) {
                args.push(format!(":(exclude){}", git::relative_to(repo, &other.dir).display()));
            }
        }
        let prefix = match package_dir.to_string_lossy().replace('\\', "/").as_str() {
            "." => String::new(),
            dir => format!("{dir}/"),
        };
        Self { args, prefix }
    }

    /// A path git printed, relative to the package (as ignore patterns are).
    pub fn in_package<'a>(&self, path: &'a str) -> &'a str {
        path.strip_prefix(&self.prefix).unwrap_or(path)
    }
}

/// Manifests holding the package's version, relative to the repository as git takes them in `<rev>:<path>`.
struct Manifests {
    package: String,
    workspace: String,
}

impl Manifests {
    fn of(repo: &Path, ws: &Workspace, member: &Member) -> Self {
        let path = |p: &Path| git::relative_to(repo, p).to_string_lossy().replace('\\', "/");
        Self {
            package: path(&member.manifest_path),
            workspace: path(&ws.root_manifest),
        }
    }

    /// The version of the package `name` at `rev`. `None` when the manifest is not there, is another package's, or
    /// can't be read.
    fn version_at(&self, repo: &Path, rev: &str, name: &str) -> Option<Version> {
        let manifest = toml_at(repo, rev, &self.package)?;
        let package = manifest.get("package")?;
        let package_name = package.get("name")?.as_str()?;
        if package_name != name {
            return None;
        }
        let Some(version) = package.get("version") else {
            // Cargo's default for a package that is not published.
            return Some(Version::new(0, 0, 0));
        };
        if let Some(version) = version.as_str() {
            return Version::parse(version).ok();
        }
        if version.get("workspace").and_then(Item::as_bool) != Some(true) {
            return None;
        }
        let root = toml_at(repo, rev, &self.workspace)?;
        let version = root.get("workspace")?.get("package")?.get("version")?.as_str()?;
        Version::parse(version).ok()
    }
}

fn toml_at(repo: &Path, rev: &str, path: &str) -> Option<DocumentMut> {
    let output = git::command(repo)
        .args(["cat-file", "blob", &format!("{rev}:{path}")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).parse().ok()
}

pub fn state(repo: &Path, ws: &Workspace, member: &Member) -> State {
    let head = Manifests::of(repo, ws, member).version_at(repo, "HEAD", &member.name);
    match head {
        None => State::New,
        Some(head) if head != member.version => State::Pending,
        Some(_) => State::Committed,
    }
}

/// The commit that set the version a [`State::Committed`] package has: the newest commit touching its manifest (and
/// the workspace one, when it `inherits` the version from there) whose first parent has another version, or no such
/// package. `shallow`: a commit without parents may be a shallow boundary rather than the first one.
pub fn release(repo: &Path, ws: &Workspace, member: &Member, inherits: bool, shallow: bool) -> erris::Result<Release> {
    let manifests = Manifests::of(repo, ws, member);
    let mut args = vec!["log", "--format=%H %P", "HEAD", "--", manifests.package.as_str()];
    if inherits {
        args.push(manifests.workspace.as_str());
    }
    let log = git::run(repo, &args)?;
    for line in log.lines() {
        let mut shas = line.split_whitespace();
        let Some(commit) = shas.next() else {
            continue;
        };
        let Some(parent) = shas.next() else {
            return Ok(if shallow {
                Release::Cut
            } else {
                Release::Commit(commit.to_owned())
            });
        };
        let before = manifests.version_at(repo, parent, &member.name);
        if before.as_ref() != Some(&member.version) {
            return Ok(Release::Commit(commit.to_owned()));
        }
    }
    if shallow {
        return Ok(Release::Cut);
    }
    Err(report!(
        "{}@{}: no commit of `{}` sets this version",
        member.name,
        member.version,
        manifests.package
    ))
}

/// Files of the package that differ from `release`: changed in later commits, staged, unstaged or untracked. Relative
/// to the package, those `ignore` matches left out.
pub fn changed_files(
    repo: &Path,
    ws: &Workspace,
    member: &Member,
    release: &str,
    ignore: &Ignore,
) -> erris::Result<Vec<String>> {
    let pathspec = Pathspec::of(ws, repo, member);
    let mut diff = vec!["diff", "--name-only", "--no-renames", "-z", release, "--"];
    let mut untracked = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
    diff.extend(pathspec.args.iter().map(String::as_str));
    untracked.extend(pathspec.args.iter().map(String::as_str));
    let mut files = BTreeSet::new();
    for args in [diff, untracked] {
        let output = git::run(repo, &args)?;
        let paths = output.split('\0').filter(|p| !p.is_empty());
        let relevant = paths.map(|p| pathspec.in_package(p)).filter(|p| !ignore.is_ignored(p));
        files.extend(relevant.map(str::to_owned));
    }
    Ok(files.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use erris::prelude::*;

    use super::*;
    use crate::workspace::Selection;

    fn sh(dir: &Path, args: &[&str]) {
        let status = git::command(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status();
        assert!(status.is_ok_and(|s| s.success()), "git {args:?}");
    }

    fn head(dir: &Path) -> String {
        git::run(dir, &["rev-parse", "HEAD"])
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    }

    /// A workspace version `0.1.0`, `app` (version `0.1.0`) and `tool` (the workspace version).
    fn repo(name: &str) -> erris::Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!("publish-plz-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (member, version) in [("app", "version = \"0.1.0\""), ("tool", "version.workspace = true")] {
            std::fs::create_dir_all(dir.join(member).join("src"))?;
            std::fs::write(dir.join(member).join("src/lib.rs"), "")?;
            let manifest = format!("[package]\nname = \"{member}\"\n{version}\nedition = \"2021\"\npublish = false\n");
            std::fs::write(dir.join(member).join("Cargo.toml"), manifest)?;
        }
        write_root(&dir, "0.1.0")?;
        sh(&dir, &["init", "-q"]);
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "init"]);
        Ok(dir)
    }

    fn write_root(dir: &Path, version: &str) -> erris::Result<()> {
        let root = format!(
            "[workspace]\nmembers = [\"app\", \"tool\"]\nresolver = \"2\"\n[workspace.package]\nversion = \"{version}\"\n"
        );
        std::fs::write(dir.join("Cargo.toml"), root)?;
        Ok(())
    }

    fn load(dir: &Path) -> erris::Result<Workspace> {
        Workspace::load(&Selection {
            manifest_path: Some(dir.join("Cargo.toml")),
            packages: vec![],
            workspace: true,
        })
    }

    fn found(dir: &Path, name: &str, inherits: bool) -> erris::Result<Release> {
        let ws = load(dir)?;
        let member = ws.member(name).ok_or_report("no member")?;
        release(dir, &ws, member, inherits, false)
    }

    #[test]
    fn the_release_is_the_commit_that_set_the_version() -> erris::Result<()> {
        let dir = repo("release")?;
        let init = head(&dir);
        std::fs::write(dir.join("app/src/lib.rs"), "pub fn a() {}")?;
        sh(&dir, &["commit", "-qam", "feat: a"]);
        let set = std::fs::read_to_string(dir.join("app/Cargo.toml"))?.replace("0.1.0", "0.2.0");
        std::fs::write(dir.join("app/Cargo.toml"), set)?;
        write_root(&dir, "0.2.0")?;
        sh(&dir, &["commit", "-qam", "chore: release"]);
        let bump = head(&dir);
        // Other changes of the manifests keep the release where it was.
        let manifest = std::fs::read_to_string(dir.join("app/Cargo.toml"))?;
        std::fs::write(dir.join("app/Cargo.toml"), format!("{manifest}description = \"app\"\n"))?;
        std::fs::write(dir.join("README.md"), "")?;
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qam", "fix: describe"]);

        let app = found(&dir, "app", false);
        let tool = found(&dir, "tool", true);
        // Not taking the version from the workspace: changes of the workspace manifest don't count.
        let tool_own = found(&dir, "tool", false);
        let ws = load(&dir)?;
        let member = ws.member("app").ok_or_report("no app")?;
        let files = changed_files(&dir, &ws, member, &bump, &Ignore::nothing())?;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(app?, Release::Commit(bump.clone()));
        assert_eq!(tool?, Release::Commit(bump));
        assert_eq!(tool_own?, Release::Commit(init));
        assert_eq!(files, ["Cargo.toml"]);
        Ok(())
    }

    #[test]
    fn the_working_tree_against_head() -> erris::Result<()> {
        let dir = repo("state")?;
        let release = head(&dir);
        std::fs::write(dir.join("app/src/new.rs"), "")?;
        std::fs::write(dir.join("app/notes.md"), "")?;
        std::fs::write(dir.join("tool/src/lib.rs"), "pub fn t() {}")?;
        write_root(&dir, "0.1.1")?;
        let ws = load(&dir)?;
        let app = ws.member("app").ok_or_report("no app")?;
        let tool = ws.member("tool").ok_or_report("no tool")?;
        let states = (state(&dir, &ws, app), state(&dir, &ws, tool));
        let ignore = Ignore::new(&["*.md".to_owned()])?;
        let files = changed_files(&dir, &ws, app, &release, &ignore)?;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(states, (State::Committed, State::Pending));
        assert_eq!(files, ["src/new.rs"]);
        Ok(())
    }

    #[test]
    fn a_package_not_in_head_is_new() -> erris::Result<()> {
        let dir = repo("new")?;
        let manifest = std::fs::read_to_string(dir.join("app/Cargo.toml"))?.replace("\"app\"", "\"renamed\"");
        std::fs::write(dir.join("app/Cargo.toml"), manifest)?;
        let ws = load(&dir)?;
        let renamed = ws.member("renamed").ok_or_report("no renamed")?;
        let found = state(&dir, &ws, renamed);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, State::New);
        Ok(())
    }

    #[test]
    fn a_shallow_clone_can_cut_the_history_off() -> erris::Result<()> {
        let source = repo("shallow-source")?;
        std::fs::write(source.join("app/src/lib.rs"), "pub fn a() {}")?;
        sh(&source, &["commit", "-qam", "fix: a"]);
        let clone = std::env::temp_dir().join(format!("publish-plz-git-shallow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&clone);
        let url = format!("file://{}", source.display());
        git::run(
            Path::new("."),
            &["clone", "-q", "--depth", "1", &url, &clone.to_string_lossy()],
        )?;
        let ws = load(&clone)?;
        let member = ws.member("app").ok_or_report("no app")?;
        let cut = release(&clone, &ws, member, false, true);
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&clone);
        assert_eq!(cut?, Release::Cut);
        Ok(())
    }
}
