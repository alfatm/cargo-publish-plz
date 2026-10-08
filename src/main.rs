mod commits;
mod manifest;
mod publish;
mod registry;
mod update;
mod workspace;

use clap::{Args, Parser, Subcommand};

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
    /// Publish crates whose current version is not in the registry yet.
    Publish(publish::PublishArgs),
}

fn main() {
    let Cargo::PublishPlz(args) = Cargo::parse();
    let result = match args.command {
        Command::Update(args) => update::run(&args),
        Command::Publish(args) => publish::run(&args),
    };
    if let Err(err) = result {
        eprintln!("error: {err:?}");
        std::process::exit(1);
    }
}

/// Runs `cargo` (the same binary that invoked us when available).
pub fn cargo() -> std::process::Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    std::process::Command::new(cargo)
}
