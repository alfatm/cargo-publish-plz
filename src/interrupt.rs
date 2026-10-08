//! Ctrl-C and SIGTERM (a cancelled CI job): destructors don't run on a signal, so what would be left on disk
//! (`--rev` worktrees beside the repository with their records in `.git/worktrees`, empty hooks directories) is
//! registered here and removed before exiting.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Duration;

use crate::git;

enum Leftover {
    Worktree { repo: PathBuf, dir: PathBuf },
    Dir(PathBuf),
}

static LEFT: Mutex<Vec<(u64, Leftover)>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(0);

fn left() -> MutexGuard<'static, Vec<(u64, Leftover)>> {
    LEFT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A leftover to remove on a signal; dropped once its owner has removed it.
pub struct Registered(u64);

impl Drop for Registered {
    fn drop(&mut self) {
        left().retain(|(id, _)| *id != self.0);
    }
}

/// The worktree `dir` of `repo`, registered before it is added.
pub fn worktree(repo: &Path, dir: &Path) -> Registered {
    register(Leftover::Worktree {
        repo: repo.to_path_buf(),
        dir: dir.to_path_buf(),
    })
}

pub fn dir(path: &Path) -> Registered {
    register(Leftover::Dir(path.to_path_buf()))
}

fn register(leftover: Leftover) -> Registered {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    left().push((id, leftover));
    Registered(id)
}

/// How long a clean-up may take: a second Ctrl-C doesn't reach a handler still running, so a stuck `git` (waiting on
/// a lock, say) must not keep the process alive.
const CLEAN_UP_TIMEOUT: Duration = Duration::from_secs(10);

/// On a signal, removes what is registered, newest first (nested worktrees before the ones holding them), and exits
/// with 130. The child `git` / `cargo` processes get the signal themselves.
pub fn install() {
    let installed = ctrlc::set_handler(|| {
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            clean_up();
            let _ = done.send(());
        });
        let finished = finished.recv_timeout(CLEAN_UP_TIMEOUT);
        if finished.is_err() {
            eprintln!("warning: clean-up took over {CLEAN_UP_TIMEOUT:?}, giving up; see `git worktree prune`");
        }
        std::process::exit(130);
    });
    if let Err(err) = installed {
        eprintln!("warning: can't clean up on Ctrl-C: {err}");
    }
}

fn clean_up() {
    let leftovers = std::mem::take(&mut *left());
    for (_, leftover) in leftovers.into_iter().rev() {
        match leftover {
            Leftover::Worktree { repo, dir } => git::remove_worktree(&repo, &dir),
            Leftover::Dir(path) => {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}
