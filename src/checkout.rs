//! `check --rev`: the committed state of a revision, checked out into a throwaway `git worktree`, so the working
//! tree is not touched and uncommitted work doesn't count. The worktree shares the repository's objects, refs and
//! shallow boundary, so `--rev origin/main` resolves and `fetch --unshallow` deepens the repository itself.
//!
//! The worktree is a hidden directory beside the repository, so paths leaving it (`path = "../other-repo/crate"`)
//! resolve as they do there. Submodules the revision pins are checked out as worktrees of the clones in the
//! repository. No hook runs on the way ([`git::NoHooks`]).

use std::path::{Path, PathBuf};

use erris::prelude::*;
use erris::report;

use crate::git;
use crate::interrupt;
use crate::workspace::{Selection, nearest_manifest};

/// A revision checked out into a temporary worktree; removed on drop.
pub struct Checkout {
    /// The repository the worktree belongs to, its top level as git prints it.
    repo: PathBuf,
    /// Worktrees of submodule clones inside the checkout, as (clone, worktree); nested ones come later.
    submodules: Vec<(PathBuf, PathBuf)>,
    /// The caller's selection, its manifest path moved into the checkout.
    pub selection: Selection,
    /// Every worktree above, for removal on Ctrl-C.
    _on_signal: Vec<interrupt::Registered>,
    /// The worktree. Declared last: removed after [`Drop`] has removed the worktree.
    tree: tempfile::TempDir,
}

impl Checkout {
    pub fn new(selection: &Selection, rev: &str) -> erris::Result<Self> {
        let manifest = match &selection.manifest_path {
            Some(path) => std::path::absolute(path)?,
            None => nearest_manifest()?,
        };
        let dir = manifest.parent().unwrap_or(Path::new(".")).to_path_buf();
        // Paths are taken as git prints them: canonicalized ones are `\\?\C:\...` on Windows, which git rejects.
        let repo =
            git::toplevel(&dir).ok_or_report_with(|| report!("--rev needs a git repository: {}", dir.display()))?;
        let prefix = git::run(&dir, &["rev-parse", "--show-prefix"])?;
        let file_name = manifest.file_name().unwrap_or("Cargo.toml".as_ref());
        let relative = Path::new(prefix.trim()).join(file_name);
        let commit = git::run(&repo, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
            .wrap_report_with(|| report!("--rev {rev}: no such commit"))?;

        let no_hooks = git::NoHooks::new()?;
        // Made before the worktree, so a failed one is cleaned up too.
        let tree = worktree_dir(&repo)?;
        let on_dir = interrupt::dir(tree.path());
        let mut checkout = Checkout {
            repo,
            submodules: Vec::new(),
            selection: selection.clone(),
            _on_signal: vec![on_dir],
            tree,
        };
        let top = checkout.tree.path().to_path_buf();
        checkout._on_signal.push(interrupt::worktree(&checkout.repo, &top));
        add_worktree(&checkout.repo, &top, commit.trim(), &no_hooks)?;
        let source = checkout.repo.clone();
        checkout.add_submodules(&source, &top, &no_hooks);
        checkout.selection.manifest_path = Some(top.join(relative));
        Ok(checkout)
    }

    /// The submodules `tree` pins, as worktrees of the clones checked out at `source`: no network, and exactly the
    /// commit pinned. One not checked out there, or without that commit, is left empty with a warning.
    fn add_submodules(&mut self, source: &Path, tree: &Path, no_hooks: &git::NoHooks) {
        let has_submodules = tree.join(".gitmodules").is_file();
        if !has_submodules {
            return;
        }
        let listed = git::run(tree, &["ls-files", "--stage", "-z"]);
        let Ok(listed) = listed else {
            return;
        };
        for entry in listed.split('\0') {
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            let mut fields = meta.split_whitespace();
            let (Some("160000"), Some(sha)) = (fields.next(), fields.next()) else {
                continue;
            };
            let clone = source.join(path);
            let target = tree.join(path);
            let checked_out = is_checked_out(&clone);
            if !checked_out {
                eprintln!("warning: --rev: submodule `{path}` is not checked out in the repository, left empty");
                continue;
            }
            let registered = interrupt::worktree(&clone, &target);
            let added = add_worktree(&clone, &target, sha, no_hooks);
            match added {
                Ok(()) => {
                    self._on_signal.push(registered);
                    self.submodules.push((clone.clone(), target.clone()));
                    self.add_submodules(&clone, &target, no_hooks);
                }
                Err(err) => eprintln!("warning: --rev: submodule `{path}` at {sha}: {err}; left empty"),
            }
        }
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        // git refuses to remove a worktree that still holds others.
        for (clone, worktree) in self.submodules.iter().rev() {
            git::remove_worktree(clone, worktree);
        }
        git::remove_worktree(&self.repo, self.tree.path());
    }
}

/// Beside the repository, hidden, with a random name only this user can open (`0700`). In the system temporary
/// directory when the repository's parent can't be written, or the repository is a submodule (of a project
/// repository): beside it the worktree would show as untracked in the superproject, and a cargo workspace there
/// would claim it. A repository merely inside another one's directory (`~/.git` for dotfiles) still gets it beside.
fn worktree_dir(repo: &Path) -> erris::Result<tempfile::TempDir> {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!(".{name}-publish-plz-");
    let parent = repo.parent();
    let nested = git::superproject(repo).is_some();
    let beside = parent
        .filter(|_| !nested)
        .map(|parent| tempfile::Builder::new().prefix(&prefix).tempdir_in(parent));
    match beside {
        Some(Ok(dir)) => Ok(dir),
        _ => tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir()
            .map_err(|err| report!("creating a temporary directory for --rev: {err}")),
    }
}

/// Whether `dir` is the top of a clone of its own, not an empty submodule directory of its superproject.
fn is_checked_out(dir: &Path) -> bool {
    let top = git::toplevel(dir).and_then(|top| top.canonicalize().ok());
    let dir = dir.canonicalize().ok();
    top.is_some() && top == dir
}

fn add_worktree(repo: &Path, dir: &Path, commit: &str, no_hooks: &git::NoHooks) -> erris::Result<()> {
    let dir = dir.to_string_lossy();
    let [config, hooks] = no_hooks.args();
    git::run(
        repo,
        &[&config, &hooks, "worktree", "add", "--quiet", "--detach", &dir, commit],
    )?;
    Ok(())
}
