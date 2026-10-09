use std::collections::BTreeSet;
use std::process::ExitCode;

use clap::Args;
use erris::prelude::*;
use erris::report;
use serde::Serialize;

use crate::Format;
use crate::parallel;
use crate::registry::Registries;
use crate::update::print_json;
use crate::workspace::{Selection, Workspace};

#[derive(Args, Debug)]
pub struct PublishArgs {
    #[command(flatten)]
    selection: Selection,
    /// Only publish packages allowed to go to this registry.
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
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    format: Format,
    /// Extra arguments passed to `cargo publish`.
    #[arg(last = true)]
    cargo_args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Outcome {
    Published,
    /// Checked by `cargo publish --dry-run`.
    DryRun,
    /// Its `cargo publish` call was skipped (dry run of a call depending on an earlier one).
    NotRun,
    AlreadyPublished,
    NotAllowed,
    /// `publish = false`.
    NotPublishable,
}

#[derive(Serialize)]
struct PackageReport {
    name: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    registry: Option<String>,
    outcome: Outcome,
}

/// One `cargo publish` call.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct Batch {
    registry: String,
    packages: Vec<String>,
    /// Some packages depend on packages published by an earlier call.
    after_earlier: bool,
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    packages: &'a [PackageReport],
    batches: &'a [Batch],
    dry_run: bool,
}

struct Pending {
    name: String,
    registry: String,
    /// Packages this one has to be published after.
    deps: Vec<String>,
}

pub fn run(args: &PublishArgs) -> erris::Result<ExitCode> {
    let ws = Workspace::load(&args.selection)?;
    let selected = ws.select(&args.selection)?;
    let mut registries = Registries::new(&std::env::current_dir()?);

    let mut reports = Vec::new();
    let mut targets = Vec::new();
    for member in &selected {
        if !member.is_publishable() {
            eprintln!(
                "warning: {}@{}: `publish = false`, skipping",
                member.name, member.version
            );
            reports.push(PackageReport {
                name: member.name.clone(),
                version: member.version.to_string(),
                registry: None,
                outcome: Outcome::NotPublishable,
            });
            continue;
        }
        let registry = registries.name_for(args.registry.as_deref(), &member.name, member.publish.as_deref())?;
        if let Some(registry) = &registry {
            registries.connect(registry)?;
        }
        targets.push((*member, registry));
    }
    let registries = &registries;
    let published = parallel::map(&targets, |(member, registry)| match registry {
        Some(registry) => registries.get(registry)?.is_published(&member.name, &member.version),
        None => Ok(false),
    });

    let mut pending = Vec::new();
    for ((member, registry), published) in targets.iter().zip(published) {
        let published = published?;
        let outcome = match registry {
            None => {
                eprintln!(
                    "warning: {} is not allowed to be published to {} by `package.publish`, skipping",
                    member.name,
                    args.registry.as_deref().unwrap_or_default()
                );
                Outcome::NotAllowed
            }
            Some(registry) if published => {
                eprintln!(
                    "warning: {}@{} is already published on {registry}, skipping",
                    member.name, member.version
                );
                Outcome::AlreadyPublished
            }
            Some(registry) => {
                pending.push(Pending {
                    name: member.name.clone(),
                    registry: registry.clone(),
                    deps: member.publish_deps().map(str::to_owned).collect(),
                });
                Outcome::NotRun
            }
        };
        reports.push(PackageReport {
            name: member.name.clone(),
            version: member.version.to_string(),
            registry: registry.clone(),
            outcome,
        });
    }

    let batches = batches(pending)?;
    if batches.is_empty() {
        eprintln!("nothing to publish");
    }
    for batch in &batches {
        if args.dry_run && batch.after_earlier {
            eprintln!(
                "dry run: skipping {} ({}): they depend on packages of an earlier call that were not uploaded",
                batch.registry,
                batch.packages.join(", ")
            );
            continue;
        }
        eprintln!("publishing to {}: {}", batch.registry, batch.packages.join(", "));
        let outcome = if args.dry_run {
            Outcome::DryRun
        } else {
            Outcome::Published
        };
        let mut cmd = crate::cargo();
        cmd.arg("publish").arg("--manifest-path").arg(&ws.root_manifest);
        for package in &batch.packages {
            cmd.arg("-p").arg(package);
        }
        // Always explicit, so cargo can't pick a different one (e.g. `registry.default`).
        cmd.arg("--registry").arg(&batch.registry);
        let flags = [
            ("--dry-run", args.dry_run),
            ("--allow-dirty", args.allow_dirty),
            ("--no-verify", args.no_verify),
        ];
        cmd.args(flags.iter().filter(|(_, on)| *on).map(|(flag, _)| flag));
        cmd.args(&args.cargo_args);
        let status = cmd.status()?;
        if !status.success() {
            return Err(report!("`cargo publish` failed for {}: {status}", batch.registry));
        }
        for report in reports.iter_mut().filter(|r| batch.packages.contains(&r.name)) {
            report.outcome = outcome;
        }
    }

    if args.format == Format::Json {
        print_json(&JsonOutput {
            packages: &reports,
            batches: &batches,
            dry_run: args.dry_run,
        })?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Splits packages into `cargo publish` calls. A call goes to one registry and cargo orders
/// the packages inside it; a package depending on a package of another registry goes to
/// a later call than that dependency.
fn batches(mut rest: Vec<Pending>) -> erris::Result<Vec<Batch>> {
    let mut batches = Vec::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    while !rest.is_empty() {
        let mut candidates: Vec<&str> = Vec::new();
        for p in &rest {
            if !candidates.contains(&p.registry.as_str()) {
                candidates.push(&p.registry);
            }
        }
        let batch = candidates.iter().find_map(|registry| {
            let blocked = blocked_by_other_registries(&rest, registry);
            let packages: Vec<String> = rest
                .iter()
                .filter(|p| p.registry == *registry && !blocked.contains(&p.name))
                .map(|p| p.name.clone())
                .collect();
            let after_earlier = rest
                .iter()
                .filter(|p| packages.contains(&p.name))
                .any(|p| p.deps.iter().any(|d| done.contains(d)));
            (!packages.is_empty()).then(|| Batch {
                registry: (*registry).to_owned(),
                packages,
                after_earlier,
            })
        });
        let Some(batch) = batch else {
            let names: Vec<&str> = rest.iter().map(|p| p.name.as_str()).collect();
            return Err(report!(
                "dependency cycle across registries between: {}",
                names.join(", ")
            ));
        };
        rest.retain(|p| !batch.packages.contains(&p.name));
        done.extend(batch.packages.iter().cloned());
        batches.push(batch);
    }
    Ok(batches)
}

/// Packages of `registry` that (transitively) wait for a package of another registry.
fn blocked_by_other_registries(rest: &[Pending], registry: &str) -> BTreeSet<String> {
    let mut blocked: BTreeSet<String> = rest
        .iter()
        .filter(|p| p.registry != registry)
        .map(|p| p.name.clone())
        .collect();
    let mut growing = true;
    while growing {
        let newly: Vec<String> = rest
            .iter()
            .filter(|p| !blocked.contains(&p.name) && p.deps.iter().any(|d| blocked.contains(d)))
            .map(|p| p.name.clone())
            .collect();
        growing = !newly.is_empty();
        blocked.extend(newly);
    }
    blocked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(name: &str, registry: &str, deps: &[&str]) -> Pending {
        Pending {
            name: name.to_owned(),
            registry: registry.to_owned(),
            deps: deps.iter().map(|d| (*d).to_owned()).collect(),
        }
    }

    fn batch(registry: &str, packages: &[&str], after_earlier: bool) -> Batch {
        Batch {
            registry: registry.to_owned(),
            packages: packages.iter().map(|p| (*p).to_owned()).collect(),
            after_earlier,
        }
    }

    #[test]
    fn one_registry_is_one_call() {
        let result = batches(vec![pending("b", "x", &["a"]), pending("a", "x", &[])]).unwrap();
        assert_eq!(result, [batch("x", &["b", "a"], false)]);
    }

    #[test]
    fn interleaves_registries_by_dependencies() {
        // base (local) <- mid (crates-io) <- top (local)
        let result = batches(vec![
            pending("top", "local", &["mid"]),
            pending("mid", "crates-io", &["base"]),
            pending("base", "local", &[]),
            pending("free", "local", &[]),
        ])
        .unwrap();
        assert_eq!(
            result,
            [
                batch("local", &["base", "free"], false),
                batch("crates-io", &["mid"], true),
                batch("local", &["top"], true),
            ]
        );
    }

    #[test]
    fn same_registry_cycles_are_left_to_cargo() {
        let result = batches(vec![pending("a", "x", &["b"]), pending("b", "x", &["a"])]).unwrap();
        assert_eq!(result, [batch("x", &["a", "b"], false)]);
    }

    #[test]
    fn cross_registry_cycle_is_an_error() {
        assert!(batches(vec![pending("a", "x", &["b"]), pending("b", "y", &["a"])]).is_err());
    }

    #[test]
    fn already_published_dependencies_are_ignored() {
        let result = batches(vec![pending("a", "x", &["published-elsewhere"])]).unwrap();
        assert_eq!(result, [batch("x", &["a"], false)]);
    }
}
