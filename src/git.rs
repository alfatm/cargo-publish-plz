//! Running `git`.

use std::path::{Path, PathBuf};
use std::process::Command;

use erris::report;

/// `git -C <dir>`.
pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd
}

/// Runs `git -C <dir> <args>` and returns its stdout; a non-zero exit is an error carrying stderr.
pub fn run(dir: &Path, args: &[&str]) -> erris::Result<String> {
    let output = command(dir).args(args).output()?;
    if !output.status.success() {
        return Err(report!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The top of the working tree `dir` is in, as git prints it (not canonicalized).
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    let top = run(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    Some(PathBuf::from(top.trim()))
}

/// The working tree `repo` is a submodule of.
pub fn superproject(repo: &Path) -> Option<PathBuf> {
    let top = run(repo, &["rev-parse", "--show-superproject-working-tree"]).ok()?;
    let top = top.trim();
    (!top.is_empty()).then(|| PathBuf::from(top))
}

pub fn commit_exists(repo: &Path, sha: &str) -> bool {
    let output = command(repo)
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .output();
    output.is_ok_and(|o| o.status.success())
}

/// Whether `ancestor` is `descendant` or one of its ancestors. A shallow boundary between them reads as no.
pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    let output = command(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output();
    output.is_ok_and(|o| o.status.success())
}

pub fn is_shallow(repo: &Path) -> bool {
    let output = run(repo, &["rev-parse", "--is-shallow-repository"]);
    output.is_ok_and(|o| o.trim() == "true")
}

/// An empty hooks directory for `core.hooksPath`, so git runs none of the repository's hooks: a read-only check has
/// no business running `post-checkout`, `reference-transaction` and the like (husky, lefthook, code generation). A
/// fresh temporary directory only this user can open (`0700`, random name, never one that existed before), so nobody
/// else on the machine can put a hook there in advance.
pub struct NoHooks {
    dir: tempfile::TempDir,
    /// For removal on Ctrl-C.
    _on_signal: crate::interrupt::Registered,
}

impl NoHooks {
    pub fn new() -> erris::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("publish-plz-hooks-")
            .tempdir()
            .map_err(|err| report!("creating an empty hooks directory: {err}"))?;
        let _on_signal = crate::interrupt::dir(dir.path());
        Ok(Self { dir, _on_signal })
    }

    /// `-c core.hooksPath=<empty>`, to go before the git subcommand.
    pub fn args(&self) -> [String; 2] {
        [
            "-c".to_owned(),
            format!("core.hooksPath={}", self.dir.path().to_string_lossy()),
        ]
    }
}

/// Removes the worktree `dir` of `repo`, or at least its directory and record.
pub fn remove_worktree(repo: &Path, dir: &Path) {
    let removed = run(repo, &["worktree", "remove", "--force", &dir.to_string_lossy()]);
    if removed.is_err() {
        // `prune` drops the record of a worktree whose directory is gone.
        let _ = std::fs::remove_dir_all(dir);
        let _ = run(repo, &["worktree", "prune"]);
    }
}
