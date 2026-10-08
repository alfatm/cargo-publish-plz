//! `cargo publish-plz`: bump versions of changed crates and publish unpublished ones.
//!
//! The logic lives here so that both binaries, `cargo-publish-plz` (run by cargo as
//! `cargo publish-plz`) and `publish-plz`, share it.

mod commits;
mod effective;
mod ignore;
mod manifest;
mod parallel;
mod publish;
mod registry;
mod update;
mod workspace;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

/// Subcommand name cargo passes as the first argument to `cargo-publish-plz`.
const CARGO_SUBCOMMAND: &str = "publish-plz";

/// Bump versions of changed crates and publish unpublished ones.
#[derive(Parser)]
#[command(name = "publish-plz", version, styles = clap::builder::styling::Styles::styled())]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bump versions of crates changed since their last published release.
    Update(update::UpdateArgs),
    /// Fail if a crate changed since its last published release without a version bump.
    Check(update::CheckArgs),
    /// Publish crates whose current version is not in the registry yet.
    Publish(publish::PublishArgs),
}

/// Output format of a command's result on stdout; diagnostics always go to stderr.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Format {
    #[default]
    Human,
    Json,
}

/// Entry point of both binaries.
pub fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    // `cargo publish-plz update` runs `cargo-publish-plz publish-plz update`.
    let via_cargo = args.get(1).is_some_and(|arg| arg == CARGO_SUBCOMMAND);
    if via_cargo {
        args.remove(1);
    }
    let bin_name = if via_cargo { "cargo publish-plz" } else { "publish-plz" };
    let matches = Cli::command().bin_name(bin_name).get_matches_from(args);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());

    let result = match cli.command {
        Command::Update(args) => update::run(&args),
        Command::Check(args) => update::check(&args),
        Command::Publish(args) => publish::run(&args),
    };
    result.unwrap_or_else(|err| {
        eprintln!("error: {err:?}");
        ExitCode::FAILURE
    })
}

/// Runs `cargo` (the same binary that invoked us when available).
pub fn cargo() -> std::process::Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    std::process::Command::new(cargo)
}
