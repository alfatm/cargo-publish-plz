//! `publish --ignore-cargo-home`: runs `cargo publish` without the config of `$CARGO_HOME`.
//!
//! Cargo can't be told to drop a setting (`--config` only adds or overrides), so it runs with a temporary
//! `CARGO_HOME` that has no `config.toml`. That is not enough on its own: cargo also reads
//! `<ancestor>/.cargo/config.toml` of its current directory, and `~/.cargo` is one of them for a workspace under
//! `~`. So cargo runs from the temporary directory, and the config files of the real current directory, but
//! `$CARGO_HOME`'s, are passed with `--config`.

use std::path::{Path, PathBuf};
use std::process::Command;

use erris::prelude::*;
use erris::report;
use tempfile::TempDir;

use crate::registry;

/// Copied into the temporary home: tokens for `cargo publish`.
const CREDENTIALS: [&str; 2] = ["credentials.toml", "credentials"];
/// Linked into the temporary home, so dependencies aren't downloaded again.
const CACHES: [&str; 2] = ["registry", "git"];

pub struct IsolatedHome {
    /// The current directory of cargo; `home` inside it is its `CARGO_HOME`.
    dir: TempDir,
    /// Config files of the real current directory, outermost first: later `--config` ones win.
    configs: Vec<PathBuf>,
    /// The toolchain rustup picks in the real current directory, unless `RUSTUP_TOOLCHAIN` already says.
    toolchain: Option<String>,
}

impl IsolatedHome {
    pub fn new(cwd: &Path) -> erris::Result<Self> {
        let real_home =
            registry::cargo_home().ok_or_report_with(|| report!("can't find the cargo home: set CARGO_HOME"))?;
        let dir = tempfile::Builder::new().prefix("publish-plz-home-").tempdir()?;
        // Cargo run in `dir` would read the real home as `<ancestor>/.cargo`.
        let home_is_ancestor = real_home.parent().is_some_and(|parent| dir.path().starts_with(parent));
        if home_is_ancestor {
            return Err(report!(
                "the temporary directory {} is under {}, so cargo would still read {}; point TMPDIR elsewhere",
                dir.path().display(),
                real_home.parent().map(Path::display).unwrap_or(real_home.display()),
                real_home.display()
            ));
        }
        let home = dir.path().join("home");
        std::fs::create_dir(&home)?;
        for name in CREDENTIALS {
            let source = real_home.join(name);
            if source.is_file() {
                std::fs::copy(&source, home.join(name))
                    .wrap_report_with(|| report!("can't copy {}", source.display()))?;
            }
        }
        for name in CACHES {
            let source = real_home.join(name);
            if !source.is_dir() {
                continue;
            }
            let linked = link_dir(&source, &home.join(name));
            if let Err(err) = linked {
                eprintln!(
                    "warning: can't link {} into the temporary cargo home, dependencies will be downloaded again: {err}",
                    source.display()
                );
            }
        }

        let mut configs = registry::config_files(cwd, true);
        configs.reverse();
        let toolchain = match std::env::var_os("RUSTUP_TOOLCHAIN") {
            Some(_) => None,
            None => active_toolchain(cwd),
        };
        Ok(Self {
            dir,
            configs,
            toolchain,
        })
    }

    /// `cargo` set up to run in the temporary home, followed by the `--config` files.
    pub fn cargo(&self) -> Command {
        let mut cmd = crate::cargo();
        cmd.current_dir(self.dir.path())
            .env("CARGO_HOME", self.dir.path().join("home"));
        if let Some(toolchain) = &self.toolchain {
            cmd.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        for config in &self.configs {
            cmd.arg("--config").arg(config);
        }
        cmd
    }
}

/// The toolchain `rustup` picks in `cwd` (a `rust-toolchain.toml` there would no longer apply), if rustup is used.
fn active_toolchain(cwd: &Path) -> Option<String> {
    let output = Command::new("rustup")
        .args(["show", "active-toolchain"])
        .current_dir(cwd)
        .output();
    let output = output.ok().filter(|output| output.status.success())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.split_whitespace().next().map(str::to_owned)
}

#[cfg(unix)]
fn link_dir(source: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(windows)]
fn link_dir(source: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(source, link)
}

#[cfg(not(any(unix, windows)))]
fn link_dir(_source: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("symlinks are not supported"))
}
