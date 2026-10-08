use std::path::{Path, PathBuf};

use cargo_metadata::MetadataCommand;
use clap::Args;
use erris::prelude::*;
use erris::report;
use semver::Version;

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
}

impl Member {
    pub fn is_publishable(&self) -> bool {
        self.publish.as_ref().is_none_or(|registries| !registries.is_empty())
    }
}

pub struct Workspace {
    pub root: PathBuf,
    pub root_manifest: PathBuf,
    pub members: Vec<Member>,
}

impl Workspace {
    pub fn load(selection: &Selection) -> erris::Result<Self> {
        let mut cmd = MetadataCommand::new();
        cmd.no_deps();
        if let Some(path) = &selection.manifest_path {
            cmd.manifest_path(path);
        }
        let metadata = cmd.exec().wrap_report("failed to run `cargo metadata`")?;

        let members = metadata
            .workspace_packages()
            .into_iter()
            .map(|p| {
                let manifest_path = PathBuf::from(&p.manifest_path);
                let dir = manifest_path.parent().map(Path::to_path_buf).unwrap_or_default();
                Member {
                    name: p.name.to_string(),
                    version: p.version.clone(),
                    readme: p.readme.as_ref().map(|r| dir.join(r)),
                    license_file: p.license_file.as_ref().map(|l| dir.join(l)),
                    publish: p.publish.clone(),
                    manifest_path,
                    dir,
                }
            })
            .collect();

        let root = PathBuf::from(&metadata.workspace_root);
        Ok(Self {
            root_manifest: root.join("Cargo.toml"),
            root,
            members,
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
