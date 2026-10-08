//! Format-preserving edits of workspace manifests.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use cargo_metadata::DependencyKind;
use erris::prelude::*;
use erris::report;
use semver::{Version, VersionReq};
use toml_edit::{DocumentMut, Item, TableLike, Value};

use crate::workspace::Workspace;

/// A dependency of a member on another workspace member.
pub struct LocalDep {
    pub package: String,
    pub kind: DependencyKind,
    /// Version requirement, taken from `[workspace.dependencies]` when inherited.
    pub req: Option<String>,
    pub inherited: bool,
}

pub struct ReqChange {
    pub manifest: PathBuf,
    pub dependency: String,
    pub old: String,
    pub new: String,
}

pub struct Manifests {
    root: PathBuf,
    members: HashSet<String>,
    docs: BTreeMap<PathBuf, DocumentMut>,
    dirty: HashSet<PathBuf>,
}

impl Manifests {
    pub fn load(ws: &Workspace) -> erris::Result<Self> {
        let mut docs = BTreeMap::new();
        let paths = std::iter::once(&ws.root_manifest).chain(ws.members.iter().map(|m| &m.manifest_path));
        for path in paths {
            let text =
                std::fs::read_to_string(path).wrap_report_with(|| report!("failed to read {}", path.display()))?;
            let doc = text
                .parse::<DocumentMut>()
                .wrap_report_with(|| report!("failed to parse {}", path.display()))?;
            docs.insert(path.clone(), doc);
        }
        Ok(Self {
            root: ws.root_manifest.clone(),
            members: ws.members.iter().map(|m| m.name.clone()).collect(),
            docs,
            dirty: HashSet::new(),
        })
    }

    pub fn save(&self) -> erris::Result<()> {
        for (path, doc) in self.docs.iter().filter(|(path, _)| self.dirty.contains(*path)) {
            std::fs::write(path, doc.to_string()).wrap_report_with(|| report!("failed to write {}", path.display()))?;
        }
        Ok(())
    }

    /// `version.workspace = true`
    pub fn inherits_version(&self, manifest: &Path) -> bool {
        let package = self.docs.get(manifest).and_then(|doc| doc.get("package"));
        let version = package.and_then(|p| p.get("version"));
        version.and_then(|v| v.get("workspace")).and_then(Item::as_bool) == Some(true)
    }

    pub fn workspace_version(&self) -> Option<Version> {
        let root = self.docs.get(&self.root)?;
        let version = root.get("workspace")?.get("package")?.get("version")?.as_str()?;
        Version::parse(version).ok()
    }

    pub fn set_workspace_version(&mut self, version: &Version) -> erris::Result<()> {
        let root = self.root.clone();
        self.set_version(&root, &["workspace", "package", "version"], version)
    }

    pub fn set_package_version(&mut self, manifest: &Path, version: &Version) -> erris::Result<()> {
        self.set_version(manifest, &["package", "version"], version)
    }

    fn set_version(&mut self, manifest: &Path, key: &[&str], version: &Version) -> erris::Result<()> {
        let mut item = self.docs.get_mut(manifest).map(DocumentMut::as_item_mut);
        for part in key {
            item = item.and_then(|i| i.get_mut(part));
        }
        let item = item.ok_or_report_with(|| report!("`{}` not found in {}", key.join("."), manifest.display()))?;
        set_str(item, &version.to_string());
        self.dirty.insert(manifest.to_path_buf());
        Ok(())
    }

    /// `[workspace.dependencies]` entries pointing to members: key -> (package, req).
    fn workspace_deps(&self) -> HashMap<String, (String, Option<String>)> {
        let table = self
            .docs
            .get(&self.root)
            .and_then(|root| root.get("workspace"))
            .and_then(|w| w.get("dependencies"))
            .and_then(Item::as_table_like);
        let mut deps = HashMap::new();
        for (key, item) in table.into_iter().flat_map(|t| t.iter()) {
            let Some(entry) = Entry::parse(key, item) else {
                continue;
            };
            if entry.path && self.members.contains(&entry.package) {
                deps.insert(key.to_owned(), (entry.package, entry.req));
            }
        }
        deps
    }

    pub fn local_deps(&self, manifest: &Path) -> Vec<LocalDep> {
        let workspace_deps = self.workspace_deps();
        let mut deps = Vec::new();
        let Some(doc) = self.docs.get(manifest) else {
            return deps;
        };
        for (kind, table) in dep_tables(doc) {
            for (key, item) in table.iter() {
                let Some(entry) = Entry::parse(key, item) else {
                    continue;
                };
                if entry.workspace {
                    if let Some((package, req)) = workspace_deps.get(key) {
                        deps.push(LocalDep {
                            package: package.clone(),
                            kind,
                            req: req.clone(),
                            inherited: true,
                        });
                    }
                } else if entry.path && self.members.contains(&entry.package) {
                    deps.push(LocalDep {
                        package: entry.package,
                        kind,
                        req: entry.req,
                        inherited: false,
                    });
                }
            }
        }
        deps
    }

    /// Rewrites version requirements on bumped members everywhere in the workspace.
    pub fn update_reqs(&mut self, new_versions: &BTreeMap<String, Version>) -> Vec<ReqChange> {
        let mut changes = Vec::new();
        for (path, doc) in &mut self.docs {
            for (kind, table) in dep_tables_mut(doc) {
                for (key, item) in table.iter_mut() {
                    let Some(entry) = Entry::parse(key.get(), item) else {
                        continue;
                    };
                    let (Some(new), Some(old)) = (new_versions.get(&entry.package), entry.req) else {
                        continue;
                    };
                    if !entry.path || !needs_req_update(kind, &old, new) {
                        continue;
                    }
                    let version = item.as_table_like_mut().and_then(|t| t.get_mut("version"));
                    let Some(version) = version else {
                        continue;
                    };
                    let req = rewrite_req(&old, new);
                    set_str(version, &req);
                    self.dirty.insert(path.clone());
                    changes.push(ReqChange {
                        manifest: path.clone(),
                        dependency: entry.package,
                        old,
                        new: req,
                    });
                }
            }
        }
        changes
    }
}

struct Entry {
    package: String,
    req: Option<String>,
    path: bool,
    workspace: bool,
}

impl Entry {
    /// `None` for plain `name = "1.0"` registry dependencies.
    fn parse(key: &str, item: &Item) -> Option<Self> {
        let table = item.as_table_like()?;
        let get_str = |k| table.get(k).and_then(Item::as_str).map(str::to_owned);
        Some(Self {
            package: get_str("package").unwrap_or_else(|| key.to_owned()),
            req: get_str("version"),
            path: table.contains_key("path"),
            workspace: table.get("workspace").and_then(Item::as_bool) == Some(true),
        })
    }
}

/// Does a member's own manifest need its requirement on `new` rewritten?
/// Normal and build dependencies always track the new version; dev-dependencies only
/// when the old requirement no longer matches (they don't affect downstream users).
pub fn needs_req_update(kind: DependencyKind, req: &str, new: &Version) -> bool {
    if rewrite_req(req, new) == req {
        return false;
    }
    match kind {
        DependencyKind::Development => !VersionReq::parse(req).is_ok_and(|r| r.matches(new)),
        _ => true,
    }
}

/// Keeps a leading `=`, `^` or `~` operator; complex requirements become a plain version.
fn rewrite_req(old: &str, new: &Version) -> String {
    let old = old.trim();
    let simple = !old.contains([',', '*', '<', '>']);
    let op = ["=", "^", "~"].into_iter().find(|op| old.starts_with(op)).unwrap_or("");
    if simple { format!("{op}{new}") } else { new.to_string() }
}

fn kind_of(key: &str) -> Option<DependencyKind> {
    match key {
        "dependencies" => Some(DependencyKind::Normal),
        "dev-dependencies" | "dev_dependencies" => Some(DependencyKind::Development),
        "build-dependencies" | "build_dependencies" => Some(DependencyKind::Build),
        _ => None,
    }
}

/// `[dependencies]`-like tables, including `[target.'cfg(..)'.dependencies]`.
fn dep_tables(doc: &DocumentMut) -> Vec<(DependencyKind, &dyn TableLike)> {
    let mut tables = Vec::new();
    let targets = doc.get("target").and_then(Item::as_table_like);
    let target_items = targets
        .into_iter()
        .flat_map(|t| t.iter())
        .filter_map(|(_, t)| t.as_table_like());
    let items = doc.as_table().iter().chain(target_items.flat_map(|t| t.iter()));
    for (key, item) in items {
        let kind = kind_of(key);
        let table = item.as_table_like();
        if let (Some(kind), Some(table)) = (kind, table) {
            tables.push((kind, table));
        }
    }
    tables
}

/// Like [`dep_tables`], plus `[workspace.dependencies]` (treated as normal dependencies).
fn dep_tables_mut(doc: &mut DocumentMut) -> Vec<(DependencyKind, &mut dyn TableLike)> {
    let mut tables = Vec::new();
    for (key, item) in doc.as_table_mut().iter_mut() {
        match key.get() {
            "workspace" => {
                let deps = item.get_mut("dependencies").and_then(Item::as_table_like_mut);
                tables.extend(deps.map(|t| (DependencyKind::Normal, t)));
            }
            "target" => {
                let targets = item.as_table_like_mut().into_iter().flat_map(|t| t.iter_mut());
                for (_, target) in targets {
                    let entries = target.as_table_like_mut().into_iter().flat_map(|t| t.iter_mut());
                    for (key, item) in entries {
                        push_dep_table(&mut tables, key.get(), item);
                    }
                }
            }
            key => push_dep_table(&mut tables, key, item),
        }
    }
    tables
}

fn push_dep_table<'a>(tables: &mut Vec<(DependencyKind, &'a mut dyn TableLike)>, key: &str, item: &'a mut Item) {
    let kind = kind_of(key);
    let table = item.as_table_like_mut();
    if let (Some(kind), Some(table)) = (kind, table) {
        tables.push((kind, table));
    }
}

/// Replaces a string value keeping its surrounding whitespace and comments.
fn set_str(item: &mut Item, new: &str) {
    match item.as_value_mut() {
        Some(value) => {
            let decor = value.decor().clone();
            *value = Value::from(new);
            *value.decor_mut() = decor;
        }
        None => *item = toml_edit::value(new),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_reqs() {
        let v = Version::new(0, 2, 0);
        assert_eq!(rewrite_req("0.1.3", &v), "0.2.0");
        assert_eq!(rewrite_req("=0.1.3", &v), "=0.2.0");
        assert_eq!(rewrite_req("~0.1", &v), "~0.2.0");
        assert_eq!(rewrite_req(">=0.1, <0.2", &v), "0.2.0");
    }

    #[test]
    fn dev_deps_only_when_incompatible() {
        let dev = DependencyKind::Development;
        assert!(!needs_req_update(dev, "0.1", &Version::new(0, 1, 5)));
        assert!(needs_req_update(dev, "0.1", &Version::new(0, 2, 0)));
        assert!(needs_req_update(DependencyKind::Normal, "0.1", &Version::new(0, 1, 5)));
        assert!(!needs_req_update(
            DependencyKind::Normal,
            "0.1.5",
            &Version::new(0, 1, 5)
        ));
    }
}
