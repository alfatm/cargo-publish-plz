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
#[derive(Args, Clone, Debug)]
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
    /// `[package.metadata.publish-plz] update`
    pub update: Option<bool>,
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
    /// `[workspace.metadata.publish-plz] update`
    pub update: Option<bool>,
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
            let settings = Settings::parse(&p.metadata)
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
                ignore: settings.ignore,
                update: settings.update,
                manifest_path,
                dir,
            });
        }

        let settings = Settings::parse(&metadata.workspace_metadata)
            .wrap_report_with(|| report!("invalid [workspace.metadata.{METADATA_KEY}]"))?;
        let root = PathBuf::from(&metadata.workspace_root);
        Ok(Self {
            root_manifest: root.join("Cargo.toml"),
            root,
            members,
            ignore: settings.ignore,
            update: settings.update,
        })
    }

    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    /// Whether `update` bumps the member: its `update`, else the workspace's, else yes.
    pub fn updates(&self, member: &Member) -> bool {
        member.update.or(self.update).unwrap_or(true)
    }

    /// Packages picked by `-p` / `--workspace` / the current directory, the same way `cargo publish`
    /// would pick them, `publish = false` ones included.
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

        Ok(selected)
    }
}

/// `{ "publish-plz": { "ignore": [...], "update": true } }` from a `metadata` table.
#[derive(Debug, Default, PartialEq)]
struct Settings {
    ignore: Option<Vec<String>>,
    update: Option<bool>,
}

impl Settings {
    fn parse(metadata: &serde_json::Value) -> erris::Result<Self> {
        let Some(table) = metadata.get(METADATA_KEY) else {
            return Ok(Self::default());
        };
        let ignore = match table.get("ignore") {
            Some(ignore) => {
                Some(serde_json::from_value(ignore.clone()).wrap_report("`ignore` must be an array of globs")?)
            }
            None => None,
        };
        // `update = "false"` reads as meant, not as a type error.
        let update = match table.get("update") {
            None => None,
            Some(serde_json::Value::Bool(update)) => Some(*update),
            Some(serde_json::Value::String(update)) if update == "true" => Some(true),
            Some(serde_json::Value::String(update)) if update == "false" => Some(false),
            Some(_) => return Err(report!("`update` must be true or false")),
        };
        Ok(Self { ignore, update })
    }
}

pub fn nearest_manifest() -> erris::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .map(|dir| dir.join("Cargo.toml"))
        .find(|path| path.is_file())
        .ok_or_report_with(|| report!("could not find Cargo.toml in {} or any parent", cwd.display()))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> erris::Result<Settings> {
        Settings::parse(&serde_json::from_str(json)?)
    }

    #[test]
    fn reads_settings() -> erris::Result<()> {
        assert_eq!(parse("null")?, Settings::default());
        assert_eq!(parse(r#"{"other": {"update": false}}"#)?, Settings::default());
        let settings = parse(r#"{"publish-plz": {"ignore": ["*.md"], "update": false}}"#)?;
        assert_eq!(settings.ignore, Some(vec!["*.md".to_owned()]));
        assert_eq!(settings.update, Some(false));
        assert_eq!(parse(r#"{"publish-plz": {"update": "true"}}"#)?.update, Some(true));
        assert_eq!(parse(r#"{"publish-plz": {"update": "false"}}"#)?.update, Some(false));
        assert!(parse(r#"{"publish-plz": {"update": "no"}}"#).is_err());
        assert!(parse(r#"{"publish-plz": {"ignore": "*.md"}}"#).is_err());
        Ok(())
    }
}
