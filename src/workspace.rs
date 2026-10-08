use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cargo_metadata::{Dependency, DependencyKind, MetadataCommand};
use clap::Args;
use erris::prelude::*;
use erris::report;
use semver::Version;

/// Key of the tool's settings in `[package.metadata]` / `[workspace.metadata]`.
const METADATA_KEY: &str = "publish-plz";

/// Which packages a command operates on, mirroring cargo's own flags.
#[derive(Args, Debug)]
pub struct Selection {
    /// Path to Cargo.toml.
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    /// Package(s) to operate on.
    #[arg(short, long = "package", value_name = "SPEC")]
    pub packages: Vec<String>,
    /// Operate on all workspace members.
    #[arg(long)]
    pub workspace: bool,
}

pub struct Member {
    pub name: String,
    pub version: Version,
    pub manifest_path: PathBuf,
    pub dir: PathBuf,
    /// `None`: any registry, `Some([])`: `publish = false`.
    pub publish: Option<Vec<String>>,
    pub readme: Option<PathBuf>,
    pub license_file: Option<PathBuf>,
    /// Effective (workspace-inherited) manifest values, as `cargo metadata` resolves them.
    pub dependencies: Vec<Dependency>,
    pub features: BTreeMap<String, Vec<String>>,
    pub edition: String,
    pub rust_version: Option<Version>,
    pub license: Option<String>,
    /// `[package.metadata.publish-plz] ignore`
    pub ignore: Option<Vec<String>>,
}

impl Member {
    pub fn is_publishable(&self) -> bool {
        self.publish.as_ref().is_none_or(|registries| !registries.is_empty())
    }

    /// Workspace members this package has to be published after: normal and build
    /// dependencies, and dev-dependencies that keep a version requirement.
    pub fn publish_deps(&self) -> impl Iterator<Item = &str> {
        self.dependencies
            .iter()
            .filter(|d| d.path.is_some())
            .filter(|d| d.kind != DependencyKind::Development || d.req.to_string() != "*")
            .map(|d| d.name.as_str())
    }
}

pub struct Workspace {
    pub root: PathBuf,
    pub root_manifest: PathBuf,
    pub members: Vec<Member>,
    /// `[workspace.metadata.publish-plz] ignore`
    pub ignore: Option<Vec<String>>,
}

impl Workspace {
    pub fn load(selection: &Selection) -> erris::Result<Self> {
        let mut cmd = MetadataCommand::new();
        cmd.no_deps();
        if let Some(path) = &selection.manifest_path {
            cmd.manifest_path(path);
        }
        let metadata = cmd.exec().wrap_report("failed to run `cargo metadata`")?;

        let mut members = Vec::new();
        for p in metadata.workspace_packages() {
            let manifest_path = PathBuf::from(&p.manifest_path);
            let dir = manifest_path.parent().map(Path::to_path_buf).unwrap_or_default();
            let ignore = ignore_patterns(&p.metadata)
                .wrap_report_with(|| report!("invalid [package.metadata.{METADATA_KEY}] in `{}`", p.name))?;
            members.push(Member {
                name: p.name.to_string(),
                version: p.version.clone(),
                readme: p.readme.as_ref().map(|r| dir.join(r)),
                license_file: p.license_file.as_ref().map(|l| dir.join(l)),
                publish: p.publish.clone(),
                dependencies: p.dependencies.clone(),
                features: p.features.clone(),
                edition: p.edition.as_str().to_owned(),
                rust_version: p.rust_version.clone(),
                license: p.license.clone(),
                ignore,
                manifest_path,
                dir,
            });
        }

        let ignore = ignore_patterns(&metadata.workspace_metadata)
            .wrap_report_with(|| report!("invalid [workspace.metadata.{METADATA_KEY}]"))?;
        let root = PathBuf::from(&metadata.workspace_root);
        Ok(Self {
            root_manifest: root.join("Cargo.toml"),
            root,
            members,
            ignore,
        })
    }

    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    /// Publishable packages picked by `-p` / `--workspace` / the current directory,
    /// the same way `cargo publish` would pick them.
    pub fn select(&self, selection: &Selection) -> erris::Result<Vec<&Member>> {
        let selected: Vec<&Member> = if !selection.packages.is_empty() {
            let mut picked = Vec::new();
            for spec in &selection.packages {
                let name = spec.split_once('@').map_or(spec.as_str(), |(name, _)| name);
                let Some(member) = self.member(name) else {
                    return Err(report!("package `{spec}` is not a member of the workspace"));
                };
                picked.push(member);
            }
            picked
        } else if selection.workspace {
            self.members.iter().collect()
        } else {
            let manifest = match &selection.manifest_path {
                Some(path) => path.clone(),
                None => nearest_manifest()?,
            };
            let manifest = canonical(&manifest);
            let current = self.members.iter().find(|m| canonical(&m.manifest_path) == manifest);
            match current {
                // The workspace root is also a package: same as cargo, only that package.
                Some(member) => vec![member],
                None => self.members.iter().collect(),
            }
        };

        Ok(selected.into_iter().filter(|m| m.is_publishable()).collect())
    }
}

/// `{ "publish-plz": { "ignore": [...] } }` from a `metadata` table.
fn ignore_patterns(metadata: &serde_json::Value) -> erris::Result<Option<Vec<String>>> {
    let Some(ignore) = metadata.get(METADATA_KEY).and_then(|m| m.get("ignore")) else {
        return Ok(None);
    };
    let patterns = serde_json::from_value(ignore.clone()).wrap_report("`ignore` must be an array of globs")?;
    Ok(Some(patterns))
}

fn nearest_manifest() -> erris::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .map(|dir| dir.join("Cargo.toml"))
        .find(|path| path.is_file())
        .ok_or_report_with(|| report!("could not find Cargo.toml in {} or any parent", cwd.display()))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
