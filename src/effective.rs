//! The parts of a manifest that reach users of a crate, compared after workspace inheritance.
//!
//! A member's own `Cargo.toml` may be unchanged while what it inherits from the workspace
//! (`serde.workspace = true`, `edition.workspace = true`, ...) changed. `cargo metadata` gives
//! the inherited values, the published `.crate` has them in its normalized `Cargo.toml`.

use std::collections::{BTreeMap, BTreeSet};

use cargo_metadata::DependencyKind;
use cargo_metadata::cargo_platform::Platform;
use erris::prelude::*;
use semver::VersionReq;
use toml_edit::{DocumentMut, Item, TableLike};

use crate::workspace::Member;

/// A normal or build dependency; dev-dependencies don't affect users.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Dep {
    name: String,
    kind: &'static str,
    target: Option<String>,
    package: String,
    req: String,
    features: BTreeSet<String>,
    optional: bool,
    default_features: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Effective {
    deps: BTreeSet<Dep>,
    features: BTreeMap<String, BTreeSet<String>>,
    edition: String,
    rust_version: Option<String>,
    license: Option<String>,
}

impl Effective {
    pub fn of_member(member: &Member) -> Self {
        let deps = member
            .dependencies
            .iter()
            .filter_map(|d| {
                let kind = match d.kind {
                    DependencyKind::Normal => "normal",
                    DependencyKind::Build => "build",
                    _ => return None,
                };
                Some(Dep {
                    name: d.rename.clone().unwrap_or_else(|| d.name.clone()),
                    kind,
                    target: d.target.as_ref().map(ToString::to_string),
                    package: d.name.clone(),
                    req: d.req.to_string(),
                    features: d.features.iter().cloned().collect(),
                    optional: d.optional,
                    default_features: d.uses_default_features,
                })
            })
            .collect();
        let features = member
            .features
            .iter()
            .map(|(name, values)| (name.clone(), values.iter().cloned().collect()))
            .collect();
        Self {
            deps,
            features: explicit_features(features),
            edition: member.edition.clone(),
            rust_version: member.rust_version.as_ref().map(ToString::to_string),
            license: member.license.clone(),
        }
    }

    /// From the normalized `Cargo.toml` of a published `.crate`.
    pub fn of_published(cargo_toml: &[u8]) -> erris::Result<Self> {
        let doc = std::str::from_utf8(cargo_toml)?.parse::<DocumentMut>()?;
        let package = doc.get("package");
        let package_str = |key: &str| {
            package
                .and_then(|p| p.get(key))
                .and_then(Item::as_str)
                .map(str::to_owned)
        };

        let mut deps = BTreeSet::new();
        collect_deps(&mut deps, doc.as_table(), None);
        let targets = doc.get("target").and_then(Item::as_table_like);
        for (target, table) in targets.into_iter().flat_map(|t| t.iter()) {
            if let Some(table) = table.as_table_like() {
                collect_deps(&mut deps, table, Some(normalize_target(target)));
            }
        }

        let features_table = doc.get("features").and_then(Item::as_table_like);
        let features = features_table
            .into_iter()
            .flat_map(|t| t.iter())
            .map(|(name, values)| (name.to_owned(), strings(values)))
            .collect();

        Ok(Self {
            deps,
            features: explicit_features(features),
            edition: package_str("edition").unwrap_or_else(|| "2015".to_owned()),
            rust_version: package_str("rust-version").map(|v| normalize_rust_version(&v)),
            license: package_str("license"),
        })
    }

    /// What differs, e.g. `dependency `serde``, `features`, `edition`.
    pub fn diff(&self, published: &Self) -> Vec<String> {
        let mut changes: Vec<String> = self
            .deps
            .symmetric_difference(&published.deps)
            .map(|d| format!("dependency `{}`", d.name))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let fields = [
            ("features", self.features != published.features),
            ("edition", self.edition != published.edition),
            ("rust-version", self.rust_version != published.rust_version),
            ("license", self.license != published.license),
        ];
        changes.extend(
            fields
                .iter()
                .filter(|(_, differs)| *differs)
                .map(|(name, _)| (*name).to_owned()),
        );
        changes
    }
}

fn collect_deps(deps: &mut BTreeSet<Dep>, table: &dyn TableLike, target: Option<String>) {
    let kinds = [
        ("dependencies", "normal"),
        ("build-dependencies", "build"),
        ("build_dependencies", "build"),
    ];
    for (key, kind) in kinds {
        let Some(entries) = table.get(key).and_then(Item::as_table_like) else {
            continue;
        };
        for (name, entry) in entries.iter() {
            deps.insert(parse_dep(name, entry, kind, target.clone()));
        }
    }
}

fn parse_dep(name: &str, entry: &Item, kind: &'static str, target: Option<String>) -> Dep {
    let table = entry.as_table_like();
    let get = |key: &str| table.and_then(|t| t.get(key));
    let flag = |keys: &[&str], default: bool| {
        keys.iter()
            .find_map(|k| get(k).and_then(Item::as_bool))
            .unwrap_or(default)
    };
    let req = entry
        .as_str()
        .or_else(|| get("version").and_then(Item::as_str))
        .unwrap_or("*");
    Dep {
        name: name.to_owned(),
        kind,
        target,
        package: get("package").and_then(Item::as_str).unwrap_or(name).to_owned(),
        req: VersionReq::parse(req).map_or_else(|_| req.to_owned(), |r| r.to_string()),
        features: get("features").map(strings).unwrap_or_default(),
        optional: flag(&["optional"], false),
        default_features: flag(&["default-features", "default_features"], true),
    }
}

fn strings(item: &Item) -> BTreeSet<String> {
    let array = item.as_array().into_iter().flat_map(|a| a.iter());
    array.filter_map(|v| v.as_str()).map(str::to_owned).collect()
}

/// Cargo may or may not spell out the implicit `name = ["dep:name"]` feature of an optional
/// dependency; drop it on both sides.
fn explicit_features(features: BTreeMap<String, BTreeSet<String>>) -> BTreeMap<String, BTreeSet<String>> {
    features
        .into_iter()
        .filter(|(name, values)| {
            let implicit = format!("dep:{name}");
            !(values.len() == 1 && values.contains(&implicit))
        })
        .collect()
}

fn normalize_target(target: &str) -> String {
    target
        .parse::<Platform>()
        .map_or_else(|_| target.to_owned(), |p| p.to_string())
}

/// `1.85` -> `1.85.0`, the form `cargo metadata` reports.
fn normalize_rust_version(version: &str) -> String {
    let mut parts: Vec<&str> = version.split('.').collect();
    parts.resize(3, "0");
    parts.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLISHED: &str = r#"
[package]
edition = "2021"
rust-version = "1.85"
license = "MIT"

[features]
default = ["std"]
std = []
fast = ["dep:fast"]

[dependencies.serde]
version = "1.0.200"
features = ["derive"]

[dependencies.fast]
version = "0.1"
optional = true

[dev-dependencies.insta]
version = "1"

[target."cfg(unix)".dependencies.libc]
version = "0.2"
default-features = false
"#;

    #[test]
    fn parses_published_manifest() {
        let effective = Effective::of_published(PUBLISHED.as_bytes()).unwrap();
        assert_eq!(effective.edition, "2021");
        assert_eq!(effective.rust_version.as_deref(), Some("1.85.0"));
        assert_eq!(effective.license.as_deref(), Some("MIT"));
        assert_eq!(effective.deps.len(), 3, "dev-dependencies are skipped");
        assert!(!effective.features.contains_key("fast"), "implicit feature dropped");

        let libc = effective.deps.iter().find(|d| d.name == "libc").unwrap();
        assert_eq!(libc.target.as_deref(), Some("cfg(unix)"));
        assert!(!libc.default_features);
        let serde = effective.deps.iter().find(|d| d.name == "serde").unwrap();
        assert_eq!(serde.req, "^1.0.200");
    }

    #[test]
    fn reports_differences() {
        let old = Effective::of_published(PUBLISHED.as_bytes()).unwrap();
        let new = PUBLISHED.replace("1.0.200", "1.0.210").replace("2021", "2024");
        let new = Effective::of_published(new.as_bytes()).unwrap();
        assert_eq!(new.diff(&old), ["dependency `serde`", "edition"]);
        assert!(old.diff(&old).is_empty());
    }
}
