use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cargo_metadata::DependencyKind;
use clap::Args;
use erris::prelude::*;
use erris::report;
use semver::Version;
use serde::Serialize;

use crate::Format;
use crate::checkout::Checkout;
use crate::commits::{self, Change, Level};
use crate::effective::Effective;
use crate::git;
use crate::ignore::{self, Ignore};
use crate::manifest::{Manifests, needs_req_update};
use crate::parallel;
use crate::registry::{self, Choice, Registries, Registry, Versions};
use crate::workspace::{Member, Selection, Workspace};

/// Files cargo generates into the `.crate`; the manifest is compared separately.
const GENERATED: [&str; 4] = ["Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"];

/// Options shared by `update` and `check`.
#[derive(Args, Clone, Debug)]
pub struct DetectArgs {
    #[command(flatten)]
    selection: Selection,
    /// Registry to compare against.
    #[arg(long)]
    registry: Option<String>,
    /// Count changes in all files, ignoring nothing (not even `*.md`).
    #[arg(long, conflicts_with = "ignore")]
    all: bool,
    /// Ignore changes in files matching this glob, relative to the package (repeatable).
    /// Adds to `[package.metadata.publish-plz] ignore` / `[workspace.metadata.publish-plz] ignore`,
    /// which default to `["*.md"]`.
    #[arg(long, value_name = "GLOB")]
    ignore: Vec<String>,
    /// Don't fetch git history in shallow clones.
    #[arg(long)]
    no_fetch: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    format: Format,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[command(flatten)]
    detect: DetectArgs,
    /// Bump the selected packages by this level instead of the one derived from commits.
    #[arg(long, value_enum, conflicts_with = "version")]
    bump: Option<Level>,
    /// Set the selected packages to this version.
    #[arg(long, value_name = "VERSION")]
    version: Option<Version>,
    /// Print what would change without writing anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    #[command(flatten)]
    detect: DetectArgs,
    /// Check this commit instead of the working tree: only committed work counts. It is checked out, with its
    /// submodules, into a temporary `git worktree` beside the repository, without running hooks, and removed
    /// afterwards; a shallow repository may be deepened with `git fetch --unshallow` to find the commits releases
    /// were made from.
    #[arg(long, value_name = "REV")]
    rev: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Status {
    /// The crate was never published.
    New,
    /// The crate exists, but not with the local version: it is waiting for `publish`.
    Pending,
    /// The local version is published.
    Published,
    /// `package.publish` doesn't allow the selected registry.
    NotAllowed,
    /// The local version is below the newest published one, which was released from this history (the version is
    /// set at publish time, not kept in the repository) or from a later commit of it (an old checkout). Comparing it
    /// with an old release would report changes that are not there.
    Behind,
    /// `package.publish` lists several registries and no `--registry` picks one.
    AmbiguousRegistry,
    /// Changed since the release of its version (or depends on a package that did), but the next version is already
    /// published, from a commit neither in this history nor after it: another branch, or one not fetched. Not bumped;
    /// fails `check`.
    VersionTaken,
}

/// Where a package stands against its registry's versions, [`Status::Behind`] aside.
fn registry_status(local: &Version, published: &[Version]) -> Status {
    if published.is_empty() {
        Status::New
    } else if published.contains(local) {
        Status::Published
    } else {
        Status::Pending
    }
}

#[derive(Clone)]
enum Target {
    Change(Change),
    Level(Level),
    Exact(Version),
}

impl Target {
    fn apply(&self, version: &Version) -> Version {
        match self {
            Target::Change(change) => commits::next_version(version, *change),
            Target::Level(level) => commits::bump_level(version, *level),
            Target::Exact(exact) => exact.clone(),
        }
    }

    fn label(&self) -> String {
        match self {
            Target::Change(change) => change.to_string(),
            Target::Level(level) => level.to_string(),
            Target::Exact(version) => format!("={version}"),
        }
    }
}

#[derive(Clone)]
struct Bump {
    target: Target,
    reason: String,
}

/// Result of comparing a package with its published `.crate`.
struct Detection {
    files: Vec<String>,
    /// What the manifest inherits from the workspace and changed, e.g. "dependency `serde`".
    inherited: Vec<String>,
    /// Commit the published version was packaged from.
    sha: Option<String>,
    warnings: Vec<String>,
}

/// A commit since the published version that touches the package.
#[derive(Clone, Serialize)]
struct CommitReport {
    sha: String,
    subject: String,
}

#[derive(Serialize)]
struct PackageReport {
    name: String,
    version: String,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    registry: Option<String>,
    /// For `ambiguous-registry`: the registries `package.publish` allows.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    registries: Vec<String>,
    /// Newest version in the registry that is not yanked.
    #[serde(skip_serializing_if = "Option::is_none")]
    latest: Option<String>,
    /// Commit the published local version was packaged from, from its `.cargo_vcs_info.json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    published_sha: Option<String>,
    /// Commits since `published_sha` that touch the package, newest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    commits: Vec<CommitReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    changed_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inherited_changes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bump: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct RequirementReport {
    manifest: String,
    dependency: String,
    old: String,
    new: String,
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    packages: &'a [PackageReport],
    requirements: &'a [RequirementReport],
    #[serde(skip_serializing_if = "Option::is_none")]
    dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
}

struct Plan {
    packages: Vec<PackageReport>,
    requirements: Vec<RequirementReport>,
}

impl Plan {
    fn bumped(&self) -> impl Iterator<Item = &PackageReport> {
        self.packages.iter().filter(|p| p.next_version.is_some())
    }

    fn count(&self, status: Status) -> usize {
        self.packages.iter().filter(|p| p.status == status).count()
    }

    fn print_human(&self) {
        for package in self.bumped() {
            let next = package.next_version.as_deref().unwrap_or_default();
            let reason = package.reason.as_deref().unwrap_or_default();
            println!("{}: {} -> {next} ({reason})", package.name, package.version);
        }
        for req in &self.requirements {
            println!("  {}: `{}` {} -> {}", req.manifest, req.dependency, req.old, req.new);
        }
    }
}

pub fn run(args: &UpdateArgs) -> erris::Result<ExitCode> {
    let forced = match (&args.bump, &args.version) {
        (Some(level), _) => Some(Target::Level(*level)),
        (None, Some(version)) => Some(Target::Exact(version.clone())),
        (None, None) => None,
    };
    let ws = Workspace::load(&args.detect.selection)?;
    let mut manifests = Manifests::load(&ws)?;
    let plan = plan(&args.detect, forced.as_ref(), &ws, &mut manifests, true)?;
    let bumped = plan.bumped().count();
    let taken = plan.count(Status::VersionTaken);

    match args.detect.format {
        Format::Human if bumped == 0 && taken == 0 => println!("all packages are up to date"),
        Format::Human => plan.print_human(),
        Format::Json => print_json(&JsonOutput {
            packages: &plan.packages,
            requirements: &plan.requirements,
            dry_run: Some(args.dry_run),
            ok: None,
        })?,
    }
    // The other bumps are written all the same; the taken ones need a decision.
    let code = if taken > 0 {
        eprintln!("error: {}", taken_error(taken));
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    };
    if bumped == 0 {
        return Ok(code);
    }
    if args.dry_run {
        eprintln!("dry run: nothing written");
        return Ok(code);
    }

    manifests.save()?;
    let has_lockfile = ws.root.join("Cargo.lock").exists();
    if has_lockfile {
        let status = crate::cargo()
            .args(["update", "--workspace", "--quiet", "--manifest-path"])
            .arg(&ws.root_manifest)
            .status()?;
        if !status.success() {
            return Err(report!("`cargo update --workspace` failed: {status}"));
        }
    }
    Ok(code)
}

fn taken_error(taken: usize) -> String {
    format!(
        "{taken} package(s) changed, but their next version is already published from another history; fetch it, \
         or set a version past the newest release"
    )
}

/// Like `update --dry-run`, but fails when a package changed without a version bump.
/// A package whose registry is ambiguous, or whose next version is taken, is reported and fails the check, the rest
/// is checked as usual.
pub fn check(args: &CheckArgs) -> erris::Result<ExitCode> {
    let checkout = match &args.rev {
        Some(rev) => Some(Checkout::new(&args.detect.selection, rev)?),
        None => None,
    };
    let mut detect = args.detect.clone();
    if let Some(checkout) = &checkout {
        detect.selection = checkout.selection.clone();
    }
    let ws = Workspace::load(&detect.selection)?;
    let mut manifests = Manifests::load(&ws)?;
    let plan = plan(&detect, None, &ws, &mut manifests, false)?;
    let bumped = plan.bumped().count();
    let ambiguous = plan.count(Status::AmbiguousRegistry);
    let taken = plan.count(Status::VersionTaken);
    let ok = bumped == 0 && ambiguous == 0 && taken == 0;

    match args.detect.format {
        Format::Human if ok => println!("all changed packages have their versions bumped"),
        Format::Human => {
            plan.print_human();
            if bumped > 0 {
                eprintln!(
                    "error: {bumped} package(s) changed since their last release without a version bump; \
                     run `cargo publish-plz update`"
                );
            }
            if ambiguous > 0 {
                eprintln!("error: {ambiguous} package(s) can go to several registries; pass --registry");
            }
            if taken > 0 {
                eprintln!("error: {}", taken_error(taken));
            }
        }
        Format::Json => print_json(&JsonOutput {
            packages: &plan.packages,
            requirements: &plan.requirements,
            dry_run: None,
            ok: Some(ok),
        })?,
    }
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

pub fn print_json(value: &impl Serialize) -> erris::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Detects changed packages and edits `manifests` in memory accordingly. `strict`: a selected package that can go
/// to several registries is an error rather than an `ambiguous-registry` line of the report.
fn plan(
    args: &DetectArgs,
    forced: Option<&Target>,
    ws: &Workspace,
    manifests: &mut Manifests,
    strict: bool,
) -> erris::Result<Plan> {
    let selected = ws.select(&args.selection)?;
    let selected_names: BTreeSet<&str> = selected.iter().map(|m| m.name.as_str()).collect();
    let publishable: Vec<&Member> = ws.members.iter().filter(|m| m.is_publishable()).collect();

    // Registries are connected up front so lookups can run in parallel.
    let mut registries = Registries::new(&std::env::current_dir()?);
    let mut registry_of: HashMap<&str, Option<String>> = HashMap::new();
    let mut ambiguous: HashMap<&str, Vec<String>> = HashMap::new();
    for member in &publishable {
        let choice = registries.choose(args.registry.as_deref(), member.publish.as_deref());
        let selected = selected_names.contains(member.name.as_str());
        let name = match choice {
            Choice::Registry(name) => Some(name),
            Choice::NotAllowed => None,
            Choice::Ambiguous(list) if strict && selected => {
                return Err(report!("`{}` {}", member.name, registry::several_registries(&list)));
            }
            // Selected: the report says so.
            Choice::Ambiguous(list) if selected => {
                ambiguous.insert(&member.name, list);
                None
            }
            // Not selected: it only matters for propagation; leave it alone.
            Choice::Ambiguous(_) => None,
        };
        if let Some(name) = &name {
            registries.connect(name)?;
        }
        registry_of.insert(&member.name, name);
    }
    let registry_of = &registry_of;
    let registries = &registries;
    let registry_for = |member: &Member| -> erris::Result<Option<&Registry>> {
        match registry_of.get(member.name.as_str()) {
            Some(Some(name)) => Ok(Some(registries.get(name)?)),
            _ => Ok(None),
        }
    };

    let ambiguous = &ambiguous;
    let statuses = parallel::map(&publishable, |member| -> erris::Result<(Status, Versions)> {
        if ambiguous.contains_key(member.name.as_str()) {
            return Ok((Status::AmbiguousRegistry, Versions::default()));
        }
        let Some(registry) = registry_for(member)? else {
            return Ok((Status::NotAllowed, Versions::default()));
        };
        let versions = registry.versions(&member.name)?;
        Ok((registry_status(&member.version, &versions.all), versions))
    });
    let mut status_of: HashMap<&str, Status> = HashMap::new();
    let mut latest_of: HashMap<&str, Version> = HashMap::new();
    let mut newest_of: HashMap<&str, Version> = HashMap::new();
    let mut published_of: HashMap<&str, Vec<Version>> = HashMap::new();
    for (member, lookup) in publishable.iter().zip(statuses) {
        let (status, versions) = lookup?;
        status_of.insert(&member.name, status);
        let latest = versions.live.iter().max();
        if let Some(latest) = latest {
            latest_of.insert(&member.name, latest.clone());
        }
        let newest = newest_comparable(&member.version, &versions.live);
        if let Some(newest) = newest {
            newest_of.insert(&member.name, newest.clone());
        }
        published_of.insert(&member.name, versions.all);
    }

    // A newer release made from this history means the repository doesn't keep the version; one made from a later
    // commit, that the checkout is old. Either way the version here is not what was last released. One made elsewhere
    // (a backport line, a pre-release branch) leaves the package to be compared as usual.
    // As git prints it: a canonicalized path is `\\?\C:\...` on Windows, which `git -C` rejects.
    let repo = git::toplevel(&ws.root);
    let mut history = repo.as_deref().map(|repo| History::new(repo, args.no_fetch));
    let newest_of = &newest_of;
    let newer: Vec<&Member> = publishable
        .iter()
        .copied()
        .filter(|m| {
            matches!(
                status_of.get(m.name.as_str()),
                Some(Status::Published | Status::Pending)
            )
        })
        .filter(|m| newest_of.get(m.name.as_str()).is_some_and(|newest| newest > &m.version))
        .collect();
    let newer_shas = parallel::map(&newer, |member| -> erris::Result<Option<String>> {
        let (Some(registry), Some(newest)) = (registry_for(member)?, newest_of.get(member.name.as_str())) else {
            return Ok(None);
        };
        let sha = registry.vcs_sha(&member.name, newest);
        match sha {
            // Only there for propagation: not worth failing the packages asked for.
            Err(err) if !selected_names.contains(member.name.as_str()) => {
                eprintln!(
                    "warning: {}@{newest}: {err}; taken as released from this history",
                    member.name
                );
                Ok(None)
            }
            sha => sha,
        }
    });
    let newer_shas = newer_shas.into_iter().collect::<erris::Result<Vec<_>>>()?;
    let released = match &mut history {
        Some(history) => released_here(history, &newer_shas)?,
        None => vec![true; newer.len()],
    };
    for (member, released) in newer.iter().zip(released) {
        if released {
            status_of.insert(&member.name, Status::Behind);
        }
    }

    let mut ignores: HashMap<&str, Ignore> = HashMap::new();
    for member in &selected {
        let ignore = ignore_for(args, ws, member)?;
        ignores.insert(&member.name, ignore);
    }
    let ignores = &ignores;
    let no_ignore = Ignore::nothing();
    let ignore_of = |member: &Member| ignores.get(member.name.as_str()).unwrap_or(&no_ignore);

    let mut bumps: BTreeMap<String, Bump> = BTreeMap::new();
    if let Some(target) = forced {
        let reason = match target {
            Target::Exact(version) => format!("--version {version}"),
            other => format!("--bump {}", other.label()),
        };
        for member in &selected {
            let bump = Bump {
                target: target.clone(),
                reason: reason.clone(),
            };
            bumps.insert(member.name.clone(), bump);
        }
    }

    let to_detect: Vec<&Member> = selected
        .iter()
        .copied()
        .filter(|m| !bumps.contains_key(&m.name) && status_of.get(m.name.as_str()) == Some(&Status::Published))
        .collect();
    let detections = parallel::map(&to_detect, |member| {
        let registry =
            registry_for(member)?.ok_or_report_with(|| report!("`{}`: no registry to compare against", member.name))?;
        detect(ws, registry, member, ignore_of(member))
    });

    let mut changed: Vec<(&Member, Detection)> = Vec::new();
    let mut unchanged: BTreeSet<&str> = BTreeSet::new();
    let mut sha_of: HashMap<&str, String> = HashMap::new();
    for (member, detection) in to_detect.iter().zip(detections) {
        let detection = detection?;
        for warning in &detection.warnings {
            eprintln!("warning: {}@{}: {warning}", member.name, member.version);
        }
        if let Some(sha) = &detection.sha {
            sha_of.insert(&member.name, sha.clone());
        }
        if detection.files.is_empty() && detection.inherited.is_empty() {
            unchanged.insert(&member.name);
        } else {
            changed.push((member, detection));
        }
    }

    for member in &selected {
        let id = format!("{}@{}", member.name, member.version);
        match status_of.get(member.name.as_str()) {
            _ if bumps.contains_key(&member.name) => {}
            Some(Status::New) => eprintln!("{id}: never published, nothing to bump"),
            Some(Status::Pending) => eprintln!("{id}: not published yet, nothing to bump"),
            Some(Status::NotAllowed) | None => eprintln!("{id}: not published to the selected registry, skipping"),
            Some(Status::Behind) => eprintln!(
                "{id}: below the newest published {}, released from this history or a later commit of it: the \
                 version is set at publish time or the checkout is old, not compared",
                newest_of
                    .get(member.name.as_str())
                    .map(ToString::to_string)
                    .unwrap_or_default()
            ),
            Some(Status::AmbiguousRegistry) => eprintln!(
                "{id}: {}",
                registry::several_registries(ambiguous.get(member.name.as_str()).map_or(&[], Vec::as_slice))
            ),
            // Found while planning, below.
            Some(Status::VersionTaken) => {}
            Some(Status::Published) if unchanged.contains(member.name.as_str()) => eprintln!("{id}: unchanged"),
            Some(Status::Published) => {}
        }
    }

    if let Some(history) = &mut history {
        let shas: Vec<&str> = changed.iter().filter_map(|(_, d)| d.sha.as_deref()).collect();
        history.check(&shas, |repo, sha| git::is_ancestor(repo, sha, "HEAD"))?;
    }
    let commit_changes = parallel::map(&changed, |(member, detection)| {
        let (Some(repo), Some(sha)) = (&repo, &detection.sha) else {
            return Ok(None);
        };
        commits_change(ws, repo, member, sha, ignore_of(member))
    });
    let mut commits_of: HashMap<&str, Vec<CommitReport>> = HashMap::new();
    for ((member, detection), commit_change) in changed.iter().zip(commit_changes) {
        let commit_change = commit_change?;
        let (change, found) = commit_change.unwrap_or_else(|| {
            eprintln!(
                "warning: {}@{}: source commit of the published version is unknown, assuming a fix",
                member.name, member.version
            );
            (Change::Fix, Vec::new())
        });
        let commits = found.len();
        commits_of.insert(&member.name, found);
        let mut what = Vec::new();
        match detection.files.as_slice() {
            [] => {}
            [single] => what.push(format!("`{single}` changed")),
            all => what.push(format!("{} files changed", all.len())),
        }
        if !detection.inherited.is_empty() {
            what.push(format!("inherited {} changed", detection.inherited.join(", ")));
        }
        what.push(if commits == 1 {
            "1 commit".to_owned()
        } else {
            format!("{commits} commits")
        });
        let bump = Bump {
            target: Target::Change(change),
            reason: format!("{change}: {}", what.join(", ")),
        };
        bumps.insert(member.name.clone(), bump);
    }

    // Members depending on bumped members get a patch bump, transitively. A package compared with an older
    // release of its own (the newest one was made from another history) can come out with a next version the
    // registry already has: then it is not bumped, and propagation starts over without it. Members sharing the
    // workspace version move together, so none of them takes a version one of them can't.
    // `--bump` / `--version` chose the versions: a collision is left to `publish`.
    let forced_names = if forced.is_some() {
        selected_names.clone()
    } else {
        BTreeSet::new()
    };
    let settled = settle(
        ws,
        manifests,
        &publishable,
        &mut status_of,
        bumps,
        &published_of,
        &forced_names,
    );
    let Settled {
        bumps,
        mut new_versions,
        taken,
    } = settled;
    new_versions.retain(|name, _| !taken.contains_key(name));
    for (name, why) in &taken {
        let Some(member) = ws.member(name) else {
            continue;
        };
        eprintln!(
            "{}@{}: {why}, from a commit neither in this history nor after it: fetch it, or set a version past the \
             newest release; not bumped",
            member.name, member.version
        );
    }

    // From a branch that left before the newest release: right for a backport line, a mistake for a branch that is
    // to be merged. Nothing tells the two apart, so it is said, not refused.
    let mut below: HashMap<&str, &Version> = HashMap::new();
    for (name, new) in &new_versions {
        let newest = newest_of.get(name.as_str()).filter(|newest| *newest > new);
        if let Some(newest) = newest {
            eprintln!(
                "warning: {name}: the next version {new} is below the newest published {newest}, released from a line \
                 this branch left before it: right for a backport; otherwise merge or rebase onto that release first"
            );
            below.insert(name, newest);
        }
    }

    for (name, new) in &new_versions {
        let Some(member) = ws.member(name) else {
            continue;
        };
        if new <= &member.version {
            return Err(report!(
                "`{name}`: new version {new} is not greater than {}",
                member.version
            ));
        }
        if manifests.inherits_version(&member.manifest_path) {
            manifests.set_workspace_version(new)?;
        } else {
            manifests.set_package_version(&member.manifest_path, new)?;
        }
    }
    let requirements = manifests
        .update_reqs(&new_versions)
        .into_iter()
        .map(|change| RequirementReport {
            manifest: change
                .manifest
                .strip_prefix(&ws.root)
                .unwrap_or(&change.manifest)
                .display()
                .to_string(),
            dependency: change.dependency,
            old: change.old,
            new: change.new,
        })
        .collect();

    let mut detection_of: HashMap<&str, &Detection> = HashMap::new();
    for (member, detection) in &changed {
        detection_of.insert(&member.name, detection);
    }
    let packages = publishable
        .iter()
        .filter(|m| selected_names.contains(m.name.as_str()) || new_versions.contains_key(&m.name))
        .map(|member| {
            let bump = bumps.get(&member.name);
            let next = new_versions.get(&member.name);
            PackageReport {
                name: member.name.clone(),
                version: member.version.to_string(),
                status: status_of
                    .get(member.name.as_str())
                    .copied()
                    .unwrap_or(Status::NotAllowed),
                registry: registry_of.get(member.name.as_str()).cloned().flatten(),
                registries: ambiguous.get(member.name.as_str()).cloned().unwrap_or_default(),
                latest: latest_of.get(member.name.as_str()).map(ToString::to_string),
                published_sha: sha_of.get(member.name.as_str()).cloned(),
                commits: commits_of.get(member.name.as_str()).cloned().unwrap_or_default(),
                changed_files: detection_of
                    .get(member.name.as_str())
                    .map(|d| d.files.clone())
                    .unwrap_or_default(),
                inherited_changes: detection_of
                    .get(member.name.as_str())
                    .map(|d| d.inherited.clone())
                    .unwrap_or_default(),
                next_version: next.map(ToString::to_string),
                bump: next.map(|_| bump.map_or_else(|| "workspace".to_owned(), |b| b.target.label())),
                reason: next
                    .map(|_| bump.map_or_else(|| "shares the workspace version".to_owned(), |b| b.reason.clone()))
                    .map(|reason| match below.get(member.name.as_str()) {
                        Some(newest) => format!("{reason}; below the newest published {newest}"),
                        None => reason,
                    })
                    .or_else(|| taken.get(&member.name).cloned()),
            }
        })
        .collect();

    Ok(Plan { packages, requirements })
}

struct Settled {
    bumps: BTreeMap<String, Bump>,
    new_versions: BTreeMap<String, Version>,
    /// Package -> why its next version can't be used. Still in `new_versions`.
    taken: BTreeMap<String, String>,
}

/// [`propagate`]s `own_bumps`, then takes out every package whose next version is already published (every member
/// sharing the workspace version for one of them), marks it [`Status::VersionTaken`] and starts over, until no new
/// version is taken. The versions of `forced` packages (and of their version group) are not checked.
fn settle<'a>(
    ws: &'a Workspace,
    manifests: &Manifests,
    publishable: &[&Member],
    status_of: &mut HashMap<&'a str, Status>,
    own_bumps: BTreeMap<String, Bump>,
    published_of: &HashMap<&str, Vec<Version>>,
    forced: &BTreeSet<&str>,
) -> Settled {
    let workspace_version = manifests.workspace_version();
    let version_group: BTreeSet<&str> = ws
        .members
        .iter()
        .filter(|m| manifests.inherits_version(&m.manifest_path))
        .map(|m| m.name.as_str())
        .collect();
    let mut exempt = forced.clone();
    if exempt.iter().any(|name| version_group.contains(name)) {
        exempt.extend(&version_group);
    }
    let mut bumps = BTreeMap::new();
    let mut new_versions = BTreeMap::new();
    // Package -> why its next version can't be used.
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    // Status of a taken package before.
    let mut was: HashMap<&str, Status> = HashMap::new();
    let mut planning = true;
    while planning {
        bumps = own_bumps.clone();
        bumps.retain(|name, _| !taken.contains_key(name));
        new_versions = propagate(
            ws,
            manifests,
            publishable,
            status_of,
            &mut bumps,
            workspace_version.as_ref(),
        );
        let newly = already_published(&new_versions, published_of, &exempt, &taken);
        planning = !newly.is_empty();
        for (name, version) in newly {
            if version_group.contains(name.as_str()) {
                // Never-published and pending members keep their status: they are not bumped either way.
                let moving = version_group.iter().filter(|other| {
                    new_versions.contains_key(**other) && status_of.get(**other) == Some(&Status::Published)
                });
                for other in moving {
                    let why = format!("the next workspace version {version} is already published for `{name}`");
                    taken.entry((*other).to_owned()).or_insert(why);
                }
            }
            taken.insert(name, format!("the next version {version} is already published"));
        }
        for name in taken.keys() {
            let Some(member) = ws.member(name) else {
                continue;
            };
            let before = status_of.insert(&member.name, Status::VersionTaken);
            if let Some(before) = before {
                was.entry(&member.name).or_insert(before);
            }
        }
    }
    // Taken stays only what would still be bumped: a package that collided for a dependency's bump, the dependency
    // taken since, has no reason to move any more. A version group moves when one of its members would.
    let wants = |name: &str| {
        own_bumps.contains_key(name)
            || ws
                .member(name)
                .is_some_and(|m| trigger(manifests, m, &new_versions).is_some())
    };
    let group_moves = version_group
        .iter()
        .any(|name| taken.contains_key(*name) && wants(name));
    let idle: Vec<String> = taken
        .keys()
        .filter(|name| !wants(name) && !(group_moves && version_group.contains(name.as_str())))
        .cloned()
        .collect();
    for name in &idle {
        taken.remove(name);
        let Some(member) = ws.member(name) else {
            continue;
        };
        if let Some(before) = was.get(member.name.as_str()) {
            status_of.insert(&member.name, *before);
        }
    }
    Settled {
        bumps,
        new_versions,
        taken,
    }
}

/// The newest of `live` versions that can supersede `local`: a pre-release doesn't supersede a stable line, so 1.0.x
/// is still compared after a 1.1.0-beta.1.
fn newest_comparable<'a>(local: &Version, live: &'a [Version]) -> Option<&'a Version> {
    let stable = local.pre.is_empty();
    live.iter().filter(|v| !stable || v.pre.is_empty()).max()
}

/// The new version of every package: `bumps` plus a patch bump of every published member depending on a bumped
/// one, transitively. Adds those patch bumps to `bumps`.
fn propagate(
    ws: &Workspace,
    manifests: &Manifests,
    publishable: &[&Member],
    status_of: &HashMap<&str, Status>,
    bumps: &mut BTreeMap<String, Bump>,
    workspace_version: Option<&Version>,
) -> BTreeMap<String, Version> {
    let mut new_versions = BTreeMap::new();
    let mut propagating = true;
    while propagating {
        propagating = false;
        new_versions = plan_versions(ws, manifests, bumps, workspace_version);
        for member in publishable.iter().filter(|m| !new_versions.contains_key(&m.name)) {
            let Some(dep) = trigger(manifests, member, &new_versions) else {
                continue;
            };
            if status_of.get(member.name.as_str()) != Some(&Status::Published) {
                continue;
            }
            let bump = Bump {
                target: Target::Change(Change::Fix),
                reason: format!("dependency `{dep}` updated"),
            };
            bumps.insert(member.name.clone(), bump);
            propagating = true;
        }
    }
    new_versions
}

/// A local dependency of `member` whose new version its requirement doesn't allow.
fn trigger(manifests: &Manifests, member: &Member, new_versions: &BTreeMap<String, Version>) -> Option<String> {
    let deps = manifests.local_deps(&member.manifest_path);
    let dep = deps.iter().find(|dep| {
        let (Some(new), Some(req)) = (new_versions.get(&dep.package), &dep.req) else {
            return false;
        };
        let kind = if dep.inherited {
            DependencyKind::Normal
        } else {
            dep.kind
        };
        needs_req_update(kind, req, new)
    });
    dep.map(|dep| dep.package.clone())
}

/// New versions the registry already has, of packages not `exempt` (their versions were asked for with `--bump` /
/// `--version`) nor already known to collide.
fn already_published(
    new_versions: &BTreeMap<String, Version>,
    published_of: &HashMap<&str, Vec<Version>>,
    exempt: &BTreeSet<&str>,
    taken: &BTreeMap<String, String>,
) -> Vec<(String, Version)> {
    new_versions
        .iter()
        .filter(|(name, _)| !exempt.contains(name.as_str()) && !taken.contains_key(*name))
        .filter(|(name, new)| published_of.get(name.as_str()).is_some_and(|all| all.contains(new)))
        .map(|(name, new)| (name.clone(), new.clone()))
        .collect()
}

/// `--all` ignores nothing; otherwise the package's patterns, else the workspace's,
/// else `*.md`, plus `--ignore`.
fn ignore_for(args: &DetectArgs, ws: &Workspace, member: &Member) -> erris::Result<Ignore> {
    if args.all {
        return Ok(Ignore::nothing());
    }
    let configured = member.ignore.as_ref().or(ws.ignore.as_ref());
    let mut patterns: Vec<String> = match configured {
        Some(patterns) => patterns.clone(),
        None => ignore::DEFAULT.iter().map(|p| (*p).to_owned()).collect(),
    };
    patterns.extend(args.ignore.iter().cloned());
    Ignore::new(&patterns).wrap_report_with(|| report!("invalid ignore pattern for `{}`", member.name))
}

fn plan_versions(
    ws: &Workspace,
    manifests: &Manifests,
    bumps: &BTreeMap<String, Bump>,
    workspace_version: Option<&Version>,
) -> BTreeMap<String, Version> {
    let inherits = |m: &&Member| manifests.inherits_version(&m.manifest_path);
    // Members sharing `workspace.package.version` move together, to the highest of their bumps.
    let group_version = workspace_version.and_then(|current| {
        ws.members
            .iter()
            .filter(inherits)
            .filter_map(|m| bumps.get(&m.name))
            .map(|b| b.target.apply(current))
            .max()
    });

    let mut versions = BTreeMap::new();
    for member in &ws.members {
        let version = if inherits(&member) {
            group_version.clone()
        } else {
            bumps.get(&member.name).map(|b| b.target.apply(&member.version))
        };
        if let Some(version) = version {
            versions.insert(member.name.clone(), version);
        }
    }
    versions
}

/// Compares the local package with its published `.crate`.
fn detect(ws: &Workspace, registry: &Registry, member: &Member, ignore: &Ignore) -> erris::Result<Detection> {
    let published = registry.download(&member.name, &member.version)?;
    let mut warnings = Vec::new();
    let files = changed_files(ws, member, &published, ignore)?;
    let manifest_compared = files.iter().any(|f| f == "Cargo.toml") || ignore.is_ignored("Cargo.toml");
    let inherited = if manifest_compared {
        Vec::new()
    } else {
        inherited_changes(member, &published, &mut warnings)
    };
    Ok(Detection {
        files,
        inherited,
        sha: published
            .get(registry::VCS_INFO)
            .and_then(|info| registry::vcs_info_sha(info)),
        warnings,
    })
}

/// For each commit a newer release was made from, whether it is in the history of `HEAD` or after it. Unknown (no vcs
/// info, a relation a shallow clone can't show) counts as yes: comparing with an older release is what reports
/// changes that are not there.
fn released_here(history: &mut History, shas: &[Option<String>]) -> erris::Result<Vec<bool>> {
    let known: Vec<&str> = shas.iter().flatten().map(String::as_str).collect();
    let related = history.check(&known, |repo, sha| {
        git::is_ancestor(repo, sha, "HEAD") || git::is_ancestor(repo, "HEAD", sha)
    })?;
    let mut related = related.into_iter();
    let released = shas
        .iter()
        .map(|sha| match sha {
            Some(_) => related.next().unwrap_or(true) || history.shallow,
            None => true,
        })
        .collect();
    Ok(released)
}

/// Files that differ between the local package (as `cargo package` would pack it) and the
/// published one.
fn changed_files(
    ws: &Workspace,
    member: &Member,
    published: &BTreeMap<String, Vec<u8>>,
    ignore: &Ignore,
) -> erris::Result<Vec<String>> {
    let output = crate::cargo()
        .args(["package", "--list", "--allow-dirty", "--quiet", "--manifest-path"])
        .arg(&ws.root_manifest)
        .args(["-p", &member.name])
        .output()?;
    if !output.status.success() {
        return Err(report!(
            "`cargo package --list -p {}` failed:\n{}",
            member.name,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let local: BTreeSet<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().replace('\\', "/"))
        .filter(|l| !l.is_empty())
        .collect();
    let relevant = |path: &str| !GENERATED.contains(&path) && !ignore.is_ignored(path);

    let mut changed = Vec::new();
    for path in local.iter().filter(|p| relevant(p)) {
        let source = local_source(member, path);
        let content = std::fs::read(&source).wrap_report_with(|| report!("failed to read {}", source.display()))?;
        if published.get(path) != Some(&content) {
            changed.push(path.clone());
        }
    }
    let removed = published.keys().filter(|p| relevant(p) && !local.contains(*p));
    changed.extend(removed.cloned());

    if ignore.is_ignored("Cargo.toml") {
        return Ok(changed);
    }
    let manifest = std::fs::read(&member.manifest_path)?;
    let original = published.get("Cargo.toml.orig");
    if original.is_some_and(|orig| orig != &manifest) {
        changed.push("Cargo.toml".to_owned());
    }
    Ok(changed)
}

/// The member's own manifest is the same as published; what it inherits from the workspace
/// (dependency versions, features, edition, ...) may not be.
fn inherited_changes(
    member: &Member,
    published: &BTreeMap<String, Vec<u8>>,
    warnings: &mut Vec<String>,
) -> Vec<String> {
    let Some(normalized) = published.get("Cargo.toml") else {
        return Vec::new();
    };
    match Effective::of_published(normalized) {
        Ok(published) => Effective::of_member(member).diff(&published),
        Err(err) => {
            warnings.push(format!("can't read the published Cargo.toml: {err}"));
            Vec::new()
        }
    }
}

/// `readme` / `license-file` outside the package directory are packed at the package root.
fn local_source(member: &Member, path: &str) -> PathBuf {
    let in_package = member.dir.join(path);
    let exists = in_package.exists();
    if exists {
        return in_package;
    }
    [&member.readme, &member.license_file]
        .into_iter()
        .flatten()
        .find(|p| p.file_name().is_some_and(|n| n == path))
        .cloned()
        .unwrap_or(in_package)
}

/// The repository's history as far as the plan needs it. A shallow clone (CI) can miss the commits releases were
/// made from, or have them cut off from `HEAD` by the shallow boundary: the full history is then fetched, once.
struct History<'a> {
    repo: &'a Path,
    /// Still shallow: a relation not found may be hidden.
    shallow: bool,
    no_fetch: bool,
}

impl<'a> History<'a> {
    fn new(repo: &'a Path, no_fetch: bool) -> Self {
        Self {
            repo,
            shallow: git::is_shallow(repo),
            no_fetch,
        }
    }

    /// Whether `holds` for each of `shas`. In a shallow clone, when it doesn't for some, the full history is fetched
    /// and those are asked again.
    fn check(&mut self, shas: &[&str], holds: impl Fn(&Path, &str) -> bool + Sync) -> erris::Result<Vec<bool>> {
        let repo = self.repo;
        let found = parallel::map(shas, |sha| holds(repo, sha));
        let missing = found.iter().filter(|found| !**found).count();
        if missing == 0 || !self.shallow || self.no_fetch {
            return Ok(found);
        }
        eprintln!("shallow clone: fetching the git history to find {missing} published commit(s)");
        let no_hooks = git::NoHooks::new()?;
        let status = git::command(repo)
            .args(no_hooks.args())
            .args(["fetch", "--unshallow", "--quiet"])
            .status()?;
        if !status.success() {
            eprintln!("warning: `git fetch --unshallow` failed: {status}");
        }
        self.shallow = git::is_shallow(repo);
        let again: Vec<(&str, bool)> = shas.iter().copied().zip(found).collect();
        Ok(parallel::map(&again, |(sha, found)| *found || holds(repo, sha)))
    }
}

/// Strongest conventional-commit change among commits touching the package since `since`,
/// and those commits. `None` when `since` is not in the local history.
fn commits_change(
    ws: &Workspace,
    repo: &Path,
    member: &Member,
    since: &str,
    ignore: &Ignore,
) -> erris::Result<Option<(Change, Vec<CommitReport>)>> {
    let exists = git::commit_exists(repo, since);
    if !exists {
        return Ok(None);
    }

    let package_dir = relative_to(repo, &member.dir);
    let mut cmd = git::command(repo);
    cmd.args([
        "log",
        "--format=%x1e%H%x00%B%x00",
        "--name-only",
        &format!("{since}..HEAD"),
        "--",
    ]);
    cmd.arg(&package_dir);
    // Don't attribute commits of nested members (e.g. root package of a workspace).
    for other in &ws.members {
        if other.dir != member.dir && other.dir.starts_with(&member.dir) {
            cmd.arg(format!(":(exclude){}", relative_to(repo, &other.dir).display()));
        }
    }
    let output = cmd.output()?;
    if !output.status.success() {
        return Err(report!("`git log` failed: {}", String::from_utf8_lossy(&output.stderr)));
    }

    // `git log` paths are relative to the repository, ignore patterns to the package.
    let prefix = match package_dir.to_string_lossy().replace('\\', "/").as_str() {
        "." => String::new(),
        dir => format!("{dir}/"),
    };
    let log = String::from_utf8_lossy(&output.stdout);
    let mut change = None;
    let mut found = Vec::new();
    for record in log.split('\x1e').filter(|r| !r.trim().is_empty()) {
        let mut parts = record.split('\0');
        let (hash, message, files) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let relevant = files
            .lines()
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .any(|f| !ignore.is_ignored(f.strip_prefix(&prefix).unwrap_or(f)));
        if !relevant {
            continue;
        }
        found.push(CommitReport {
            sha: hash.trim().to_owned(),
            subject: message.lines().next().unwrap_or_default().trim().to_owned(),
        });
        change = change.max(Some(commits::classify(message)));
    }
    // Files differ but no commits: uncommitted changes or a workspace-level change.
    Ok(Some((change.unwrap_or(Change::Fix), found)))
}

/// `dir` relative to `repo`, both canonicalized (only for comparing; git is given paths as it prints them).
fn relative_to(repo: &Path, dir: &Path) -> PathBuf {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    match dir.strip_prefix(&repo) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|_| Version::new(0, 0, 0))
    }

    #[test]
    fn statuses_against_the_registry() {
        let published = [v("0.54.0"), v("0.58.0"), v("0.59.0")];
        assert_eq!(registry_status(&v("0.1.0"), &[]), Status::New);
        assert_eq!(registry_status(&v("0.54.0"), &published), Status::Published);
        assert_eq!(registry_status(&v("0.58.1"), &published), Status::Pending);
    }

    fn head(dir: &Path) -> String {
        git::run(dir, &["rev-parse", "HEAD"])
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    }

    #[test]
    fn behind_only_releases_from_this_history() -> erris::Result<()> {
        let dir = scratch("history");
        sh(&dir, &["init", "-q"]);
        sh(&dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let here = head(&dir);
        sh(&dir, &["checkout", "-q", "-b", "next"]);
        sh(&dir, &["commit", "-q", "--allow-empty", "-m", "feat!: next"]);
        let elsewhere = head(&dir);
        sh(&dir, &["checkout", "-q", "-"]);
        sh(&dir, &["commit", "-q", "--allow-empty", "-m", "fix: main"]);
        sh(&dir, &["commit", "-q", "--allow-empty", "-m", "chore: release"]);
        let later = head(&dir);
        sh(&dir, &["checkout", "-q", "--detach", "HEAD~1"]);

        let shas = [
            Some(here),
            Some(later),
            Some(elsewhere),
            Some("0123456789012345678901234567890123456789".to_owned()),
            None,
        ];
        let released = released_here(&mut History::new(&dir, true), &shas)?;
        let _ = std::fs::remove_dir_all(&dir);
        // A release from an ancestor: the version is not kept in the repository. From a later commit: an old
        // checkout.
        assert_eq!(released.get(..2), Some(&[true, true][..]));
        // A pre-release or backport from another branch, or a commit that doesn't exist in a full clone.
        assert_eq!(released.get(2..4), Some(&[false, false][..]));
        // Unknown origin.
        assert_eq!(released.get(4), Some(&true));
        Ok(())
    }

    #[test]
    fn a_shallow_boundary_hides_no_release() -> erris::Result<()> {
        let source = scratch("boundary-source");
        sh(&source, &["init", "-q", "-b", "main"]);
        sh(&source, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let released = head(&source);
        sh(&source, &["branch", "release"]);
        sh(&source, &["checkout", "-q", "-b", "side"]);
        sh(&source, &["commit", "-q", "--allow-empty", "-m", "fix: side"]);
        let side = head(&source);
        sh(&source, &["checkout", "-q", "main"]);
        sh(&source, &["commit", "-q", "--allow-empty", "-m", "fix: main"]);

        // Every branch tip is there, cut off from its parents: the release commit is no ancestor of HEAD yet.
        let shallow = scratch("boundary");
        let _ = std::fs::remove_dir_all(&shallow);
        let url = format!("file://{}", source.display());
        let target = shallow.to_string_lossy().into_owned();
        git::run(
            Path::new("."),
            &["clone", "-q", "--depth", "1", "--no-single-branch", &url, &target],
        )?;
        // Fetching runs none of the repository's hooks either.
        let marker = shallow.join("hook-ran");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hook = shallow.join(".git/hooks/reference-transaction");
            std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display()))?;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        }
        let shas = [Some(released), Some(side)];
        let unknown = released_here(&mut History::new(&shallow, true), &shas)?;
        let found = released_here(&mut History::new(&shallow, false), &shas)?;
        let hook_ran = marker.exists();
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&shallow);
        assert!(!hook_ran, "a hook ran on fetch");
        // Without fetching, not finding a relation proves nothing.
        assert_eq!(unknown, [true, true]);
        // Deepened: the release is an ancestor, the side branch is not.
        assert_eq!(found, [true, false]);
        Ok(())
    }

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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("publish-plz-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(std::fs::create_dir_all(dir.join("src")).is_ok());
        dir
    }

    const MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

    #[test]
    fn commits_since_the_published_one() -> erris::Result<()> {
        let dir = scratch("commits");
        std::fs::write(dir.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(dir.join("src/lib.rs"), "")?;
        sh(&dir, &["init", "-q"]);
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "init"]);
        let published = head(&dir);
        std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}")?;
        sh(&dir, &["commit", "-qam", "feat: add a\n\nwith a body"]);
        std::fs::write(dir.join("README.md"), "docs")?;
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "docs: readme only"]);

        let ws = Workspace::load(&crate::workspace::Selection {
            manifest_path: Some(dir.join("Cargo.toml")),
            packages: vec![],
            workspace: false,
        })?;
        let member = ws.member("demo").ok_or_report("no demo")?;
        let repo = dir.canonicalize()?;
        let ignore = Ignore::new(&["*.md".to_owned()])?;
        let found = commits_change(&ws, &repo, member, &published, &ignore)?;
        let _ = std::fs::remove_dir_all(&dir);
        let (change, commits) = found.ok_or_report("no history")?;
        assert_eq!(change, Change::Feature);
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["feat: add a"]);
        assert_eq!(commits.first().map(|c| c.sha.len()), Some(40));
        Ok(())
    }

    #[test]
    fn a_revision_is_checked_out_without_the_working_tree() -> erris::Result<()> {
        let dir = scratch("rev");
        std::fs::write(dir.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(dir.join("src/lib.rs"), "")?;
        sh(&dir, &["init", "-q"]);
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "init"]);
        std::fs::write(dir.join("Cargo.toml"), MANIFEST.replace("0.1.0", "0.2.0"))?;
        let selection = crate::workspace::Selection {
            manifest_path: Some(dir.join("Cargo.toml")),
            packages: vec!["demo".into()],
            workspace: false,
        };
        let checkout = Checkout::new(&selection, "HEAD")?;
        let ws = Workspace::load(&checkout.selection)?;
        let version = ws.member("demo").map(|m| m.version.to_string());
        let clone = checkout.selection.manifest_path.clone();
        drop(checkout);
        let gone = clone.as_ref().is_some_and(|p| !p.exists());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(version.as_deref(), Some("0.1.0"));
        assert!(gone, "the checkout is removed");
        assert!(Checkout::new(&selection, "no-such-rev").is_err());
        Ok(())
    }

    #[test]
    fn a_revision_keeps_its_submodules_and_neighbours() -> erris::Result<()> {
        let parent = scratch("neighbours");
        // A crate beside the repository, and one in a repository used as a submodule.
        let crate_at = |dir: &Path, name: &str| -> erris::Result<()> {
            std::fs::create_dir_all(dir.join("src"))?;
            std::fs::write(dir.join("src/lib.rs"), "")?;
            let manifest = MANIFEST.replace("\"demo\"", &format!("\"{name}\""));
            std::fs::write(dir.join("Cargo.toml"), manifest)?;
            Ok(())
        };
        crate_at(&parent.join("beside"), "beside")?;
        let sub = parent.join("sub");
        crate_at(&sub, "sub")?;
        sh(&sub, &["init", "-q"]);
        sh(&sub, &["add", "."]);
        sh(&sub, &["commit", "-qm", "init"]);

        let repo = parent.join("repo");
        crate_at(&repo, "demo")?;
        let manifest = format!(
            "{MANIFEST}[dependencies]\nbeside = {{ path = \"../beside\" }}\nsub = {{ path = \"sub\" }}\n\
             [workspace]\nmembers = [\"sub\"]\n"
        );
        std::fs::write(repo.join("Cargo.toml"), manifest)?;
        sh(&repo, &["init", "-q"]);
        let url = sub.to_string_lossy().into_owned();
        sh(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &url,
                "sub",
            ],
        );
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-qm", "init"]);

        let selection = crate::workspace::Selection {
            manifest_path: Some(repo.join("Cargo.toml")),
            packages: vec![],
            workspace: true,
        };
        let checkout = Checkout::new(&selection, "HEAD")?;
        let ws = Workspace::load(&checkout.selection);
        let mut names: Vec<String> = ws
            .iter()
            .flat_map(|ws| ws.members.iter().map(|m| m.name.clone()))
            .collect();
        names.sort();
        drop(checkout);
        let left: Vec<String> = std::fs::read_dir(&parent)?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect();
        let worktrees = git::run(&sub, &["worktree", "list"]).unwrap_or_default();
        let module = repo.join(".git/modules/sub");
        let module_worktrees = git::run(&module, &["worktree", "list"]).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&parent);
        assert!(ws.is_ok(), "cargo metadata in the checkout: {:?}", ws.err());
        assert_eq!(names, ["demo", "sub"]);
        assert!(left.is_empty(), "left beside the repository: {left:?}");
        assert_eq!(worktrees.lines().count(), 1);
        assert_eq!(module_worktrees.lines().count(), 1, "{module_worktrees}");
        Ok(())
    }

    #[test]
    fn only_a_submodule_is_checked_out_elsewhere() -> erris::Result<()> {
        let outer = scratch("nested");
        sh(&outer, &["init", "-q"]);
        let source = scratch("nested-source");
        std::fs::write(source.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(source.join("src/lib.rs"), "")?;
        sh(&source, &["init", "-q"]);
        sh(&source, &["add", "."]);
        sh(&source, &["commit", "-qm", "init"]);
        // A clone that merely lies in another repository's directory (`~/.git` for dotfiles), and a submodule of a
        // project repository.
        let plain = outer.join("plain");
        let url = source.to_string_lossy().into_owned();
        git::run(&outer, &["clone", "-q", &url, "plain"])?;
        sh(
            &outer,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &url,
                "sub",
            ],
        );
        let placed_beside = |repo: &Path| -> erris::Result<bool> {
            let selection = crate::workspace::Selection {
                manifest_path: Some(repo.join("Cargo.toml")),
                packages: vec![],
                workspace: false,
            };
            let checkout = Checkout::new(&selection, "HEAD")?;
            let beside = checkout
                .selection
                .manifest_path
                .as_ref()
                .is_some_and(|p| p.starts_with(&outer) && p.is_file());
            drop(checkout);
            Ok(beside)
        };
        let plain_beside = placed_beside(&plain);
        let sub_beside = placed_beside(&outer.join("sub"));
        let _ = std::fs::remove_dir_all(&outer);
        let _ = std::fs::remove_dir_all(&source);
        assert!(plain_beside?, "a plain clone gets its worktree beside it");
        assert!(
            !sub_beside?,
            "a submodule's worktree would be untracked in the superproject"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_revision_is_checked_out_without_the_repository_hooks() -> erris::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("hooks");
        std::fs::write(dir.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(dir.join("src/lib.rs"), "")?;
        sh(&dir, &["init", "-q"]);
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "init"]);
        let marker = dir.join("hook-ran");
        let hook = dir.join(".git/hooks/post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display()))?;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        let selection = crate::workspace::Selection {
            manifest_path: Some(dir.join("Cargo.toml")),
            packages: vec![],
            workspace: false,
        };
        let checkout = Checkout::new(&selection, "HEAD")?;
        let checked_out = checkout.selection.manifest_path.as_ref().is_some_and(|p| p.is_file());
        drop(checkout);
        let ran_on_rev = marker.exists();
        // The hook itself works: a checkout in the repository runs it, whatever the global `core.hooksPath` (husky,
        // lefthook) says.
        let own_hooks = format!("core.hooksPath={}", dir.join(".git/hooks").display());
        sh(&dir, &["-c", &own_hooks, "checkout", "-q", "--detach", "HEAD"]);
        let ran_on_checkout = marker.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(checked_out);
        assert!(!ran_on_rev, "post-checkout ran for --rev");
        assert!(ran_on_checkout, "the test hook is not executable");
        Ok(())
    }

    #[test]
    fn a_next_version_already_published_is_not_proposed() {
        let published = vec![v("0.30.4"), v("0.31.0"), v("0.32.0"), v("0.34.0")];
        let published_of: HashMap<&str, Vec<Version>> =
            HashMap::from([("deploy", published.clone()), ("other", published)]);
        let none = BTreeSet::new();
        let taken: BTreeMap<String, String> = BTreeMap::new();
        // Compared with 0.30.4 although 0.34.0 exists: a breaking change asks for 0.31.0, which is taken.
        let new = BTreeMap::from([("deploy".to_owned(), v("0.31.0")), ("other".to_owned(), v("0.30.5"))]);
        let found = already_published(&new, &published_of, &none, &taken);
        assert_eq!(found, [("deploy".to_owned(), v("0.31.0"))]);
        // A backport line with a free next version goes on as usual.
        assert!(
            already_published(
                &BTreeMap::from([("deploy".to_owned(), v("0.30.5"))]),
                &published_of,
                &none,
                &taken
            )
            .is_empty()
        );
        // Asked for explicitly: left to `publish`.
        let forced = BTreeSet::from(["deploy"]);
        assert!(
            already_published(
                &BTreeMap::from([("deploy".to_owned(), v("0.31.0"))]),
                &published_of,
                &forced,
                &taken
            )
            .is_empty()
        );
        // Already known: not reported again, so the planning loop ends.
        let known = BTreeMap::from([("deploy".to_owned(), "taken".to_owned())]);
        assert!(
            already_published(
                &BTreeMap::from([("deploy".to_owned(), v("0.31.0"))]),
                &published_of,
                &none,
                &known
            )
            .is_empty()
        );
    }

    #[test]
    fn a_pre_release_does_not_supersede_a_stable_line() {
        let live = [v("1.0.0"), v("1.1.0-beta.1")];
        assert_eq!(newest_comparable(&v("1.0.0"), &live), Some(&v("1.0.0")));
        assert_eq!(newest_comparable(&v("1.1.0-alpha.1"), &live), Some(&v("1.1.0-beta.1")));
    }

    /// A workspace of `members`: name and the rest of its `[package]` (and anything after it).
    fn workspace(name: &str, members: &[(&str, &str)]) -> erris::Result<(PathBuf, Workspace)> {
        let dir = scratch(name);
        let names: Vec<String> = members.iter().map(|(name, _)| format!("\"{name}\"")).collect();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = [{}]\nresolver = \"2\"\n[workspace.package]\nversion = \"0.30.4\"\n",
                names.join(", ")
            ),
        )?;
        for (name, package) in members {
            std::fs::create_dir_all(dir.join(name).join("src"))?;
            std::fs::write(dir.join(name).join("src/lib.rs"), "")?;
            let manifest = format!("[package]\nname = \"{name}\"\nedition = \"2021\"\n{package}");
            std::fs::write(dir.join(name).join("Cargo.toml"), manifest)?;
        }
        let ws = Workspace::load(&crate::workspace::Selection {
            manifest_path: Some(dir.join("Cargo.toml")),
            packages: vec![],
            workspace: true,
        })?;
        Ok((dir, ws))
    }

    /// `a` and `b` share the workspace version, `c` depends on `b`.
    fn version_group() -> erris::Result<(PathBuf, Workspace)> {
        workspace(
            "group",
            &[
                ("a", "version.workspace = true\n"),
                ("b", "version.workspace = true\n"),
                (
                    "c",
                    "version = \"1.0.0\"\n[dependencies]\nb = { path = \"../b\", version = \"0.30.4\" }\n",
                ),
            ],
        )
    }

    fn breaking(name: &str) -> BTreeMap<String, Bump> {
        let bump = Bump {
            target: Target::Change(Change::Breaking),
            reason: "feat!".to_owned(),
        };
        BTreeMap::from([(name.to_owned(), bump)])
    }

    #[test]
    fn a_dependent_is_taken_only_while_it_would_move() -> erris::Result<()> {
        // `b` depends on `a`; both next versions were published elsewhere.
        let (dir, ws) = workspace(
            "dependent",
            &[
                ("a", "version = \"0.30.4\"\n"),
                (
                    "b",
                    "version = \"1.0.0\"\n[dependencies]\na = { path = \"../a\", version = \"0.30.4\" }\n",
                ),
            ],
        )?;
        let manifests = Manifests::load(&ws)?;
        let publishable: Vec<&Member> = ws.members.iter().collect();
        let published_of: HashMap<&str, Vec<Version>> = HashMap::from([
            ("a", vec![v("0.30.4"), v("0.31.0")]),
            ("b", vec![v("1.0.0"), v("1.0.1")]),
        ]);
        let mut status_of: HashMap<&str, Status> = ws
            .members
            .iter()
            .map(|m| (m.name.as_str(), Status::Published))
            .collect();
        let settled = settle(
            &ws,
            &manifests,
            &publishable,
            &mut status_of,
            breaking("a"),
            &published_of,
            &BTreeSet::new(),
        );
        let _ = std::fs::remove_dir_all(&dir);
        // `a` doesn't move, so neither would `b`: it is not reported as taken, and stays published.
        let taken: Vec<&String> = settled.taken.keys().collect();
        assert_eq!(taken, ["a"]);
        assert_eq!(status_of.get("b"), Some(&Status::Published));
        assert!(!settled.new_versions.contains_key("b"));
        Ok(())
    }

    #[test]
    fn a_version_group_does_not_take_a_published_version() -> erris::Result<()> {
        let (dir, ws) = version_group()?;
        let manifests = Manifests::load(&ws)?;
        let publishable: Vec<&Member> = ws.members.iter().collect();
        let published_of: HashMap<&str, Vec<Version>> = HashMap::from([
            ("a", vec![v("0.30.4"), v("0.31.0")]),
            ("b", vec![v("0.30.4")]),
            ("c", vec![v("1.0.0")]),
        ]);
        let breaking_b = || breaking("b");
        let all_published = || -> HashMap<&str, Status> {
            ws.members
                .iter()
                .map(|m| (m.name.as_str(), Status::Published))
                .collect()
        };

        // `b` moves the group to 0.31.0, which `a` already has: neither moves, nor does `c` because of `b`.
        let mut status_of = all_published();
        let settled = settle(
            &ws,
            &manifests,
            &publishable,
            &mut status_of,
            breaking_b(),
            &published_of,
            &BTreeSet::new(),
        );
        let mut proposed: Vec<&String> = settled
            .new_versions
            .keys()
            .filter(|n| !settled.taken.contains_key(*n))
            .collect();
        proposed.sort();
        let taken: Vec<(&str, &str)> = settled.taken.iter().map(|(n, w)| (n.as_str(), w.as_str())).collect();
        let statuses = (
            status_of.get("a").copied(),
            status_of.get("b").copied(),
            status_of.get("c").copied(),
        );

        // Asked for with --bump: the developer's choice, left to `publish`.
        let mut forced_status = all_published();
        let forced = settle(
            &ws,
            &manifests,
            &publishable,
            &mut forced_status,
            breaking_b(),
            &published_of,
            &BTreeSet::from(["b"]),
        );
        let _ = std::fs::remove_dir_all(&dir);

        assert!(proposed.is_empty(), "proposed {proposed:?}");
        assert_eq!(
            taken,
            [
                ("a", "the next version 0.31.0 is already published"),
                ("b", "the next workspace version 0.31.0 is already published for `a`")
            ]
        );
        assert_eq!(
            statuses,
            (
                Some(Status::VersionTaken),
                Some(Status::VersionTaken),
                Some(Status::Published)
            )
        );
        assert!(forced.taken.is_empty());
        assert_eq!(forced.new_versions.get("a"), Some(&v("0.31.0")));
        assert_eq!(forced.new_versions.get("c"), Some(&v("1.0.1")));
        Ok(())
    }

    #[test]
    fn a_revision_of_a_shallow_clone_can_be_deepened() -> erris::Result<()> {
        let source = scratch("shallow-source");
        std::fs::write(source.join("Cargo.toml"), MANIFEST)?;
        std::fs::write(source.join("src/lib.rs"), "")?;
        sh(&source, &["init", "-q", "-b", "main"]);
        sh(&source, &["add", "."]);
        sh(&source, &["commit", "-qm", "init"]);
        let first = head(&source);
        sh(&source, &["commit", "-q", "--allow-empty", "-m", "fix: second"]);
        sh(&source, &["checkout", "-q", "-b", "side"]);
        sh(&source, &["commit", "-q", "--allow-empty", "-m", "fix: side"]);
        sh(&source, &["checkout", "-q", "main"]);

        let shallow = scratch("shallow");
        let _ = std::fs::remove_dir_all(&shallow);
        let url = format!("file://{}", source.display());
        let target = shallow.to_string_lossy().into_owned();
        git::run(
            Path::new("."),
            &["clone", "-q", "--depth", "1", "--no-single-branch", &url, &target],
        )?;
        let selection = crate::workspace::Selection {
            manifest_path: Some(shallow.join("Cargo.toml")),
            packages: vec![],
            workspace: false,
        };
        // Remote-tracking refs are shared with the worktree.
        let checkout = Checkout::new(&selection, "origin/side")?;
        let top = checkout
            .selection
            .manifest_path
            .as_ref()
            .and_then(|p| p.parent())
            .ok_or_report("no checkout")?
            .to_path_buf();
        let missing_before = !git::commit_exists(&top, &first);
        History::new(&top, false).check(&[&first], git::commit_exists)?;
        let found_after = git::commit_exists(&top, &first);
        drop(checkout);
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&shallow);
        assert!(missing_before, "the clone is shallow");
        assert!(found_after, "fetch --unshallow reaches the real remote");
        Ok(())
    }
}
