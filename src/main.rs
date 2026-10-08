mod commits;
mod effective;
mod ignore;
mod manifest;
mod parallel;
mod publish;
mod registry;
mod update;
mod workspace;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo", styles = clap::builder::styling::Styles::styled())]
enum Cargo {
    PublishPlz(PublishPlzArgs),
}

/// Bump versions of changed crates and publish unpublished ones.
#[derive(Args)]
#[command(version)]
struct PublishPlzArgs {
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

fn main() -> ExitCode {
    let Cargo::PublishPlz(args) = Cargo::parse();
    let result = match args.command {
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
