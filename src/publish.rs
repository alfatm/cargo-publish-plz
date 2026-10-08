use std::collections::BTreeMap;

use clap::Args;
use erris::prelude::*;
use erris::report;

use crate::registry::Registries;
use crate::workspace::{Selection, Workspace};

#[derive(Args, Debug)]
pub struct PublishArgs {
    #[command(flatten)]
    selection: Selection,
    /// Registry to publish to.
    #[arg(long)]
    registry: Option<String>,
    /// Perform all checks without uploading.
    #[arg(long)]
    dry_run: bool,
    /// Allow dirty working directories to be packaged.
    #[arg(long)]
    allow_dirty: bool,
    /// Don't verify the contents by building them.
    #[arg(long)]
    no_verify: bool,
    /// Extra arguments passed to `cargo publish`.
    #[arg(last = true)]
    cargo_args: Vec<String>,
}

pub fn run(args: &PublishArgs) -> erris::Result<()> {
    let ws = Workspace::load(&args.selection)?;
    let mut registries = Registries::new(&ws.root);

    // registry -> packages to publish there
    let mut pending: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for member in ws.select(&args.selection)? {
        let registry_name = registries.name_for(args.registry.as_deref(), &member.name, member.publish.as_deref())?;
        let Some(registry_name) = registry_name else {
            eprintln!(
                "warning: {} is not allowed to be published to {} by `package.publish`, skipping",
                member.name,
                args.registry.as_deref().unwrap_or_default()
            );
            continue;
        };
        let registry = registries.get(&registry_name)?;
        let published = registry.is_published(&member.name, &member.version)?;
        if published {
            eprintln!(
                "warning: {}@{} is already published on {registry_name}, skipping",
                member.name, member.version
            );
            continue;
        }
        pending.entry(registry_name).or_default().push(member.name.clone());
    }

    if pending.is_empty() {
        eprintln!("nothing to publish");
        return Ok(());
    }

    for (registry, packages) in &pending {
        eprintln!("publishing to {registry}: {}", packages.join(", "));
        let mut cmd = crate::cargo();
        cmd.arg("publish").arg("--manifest-path").arg(&ws.root_manifest);
        for package in packages {
            cmd.arg("-p").arg(package);
        }
        // Always explicit, so cargo can't pick a different one (e.g. `registry.default`).
        cmd.arg("--registry").arg(registry);
        let flags = [
            ("--dry-run", args.dry_run),
            ("--allow-dirty", args.allow_dirty),
            ("--no-verify", args.no_verify),
        ];
        cmd.args(flags.iter().filter(|(_, on)| *on).map(|(flag, _)| flag));
        cmd.args(&args.cargo_args);
        let status = cmd.status()?;
        if !status.success() {
            return Err(report!("`cargo publish` failed for {registry}: {status}"));
        }
    }
    Ok(())
}
