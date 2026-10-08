//! Minimal client for sparse registry indexes (crates.io or any `sparse+` registry).

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use erris::prelude::*;
use erris::report;
use semver::Version;
use serde::Deserialize;
use toml_edit::DocumentMut;

const CRATES_IO: &str = "crates-io";
const CRATES_IO_INDEX: &str = "https://index.crates.io/";
const MAX_CRATE_SIZE: u64 = 512 * 1024 * 1024;

#[derive(Deserialize)]
struct IndexLine {
    vers: String,
    #[serde(default)]
    cksum: String,
}

#[derive(Deserialize)]
struct IndexConfig {
    dl: String,
    #[serde(default, rename = "auth-required")]
    auth_required: bool,
}

pub struct Registry {
    pub name: String,
    index: String,
    token: Option<String>,
    agent: ureq::Agent,
    config: Option<IndexConfig>,
    versions: HashMap<String, Vec<Version>>,
    checksums: HashMap<(String, Version), String>,
}

impl Registry {
    fn new(name: String, index: String, token: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Self {
            name,
            index,
            token,
            agent,
            config: None,
            versions: HashMap::new(),
            checksums: HashMap::new(),
        }
    }

    fn get(&mut self, url: &str) -> erris::Result<Option<Vec<u8>>> {
        let auth = self.auth_header()?;
        let mut request = self.agent.get(url);
        if let Some(token) = auth {
            request = request.header("Authorization", token);
        }
        let mut response = request.call().wrap_report_with(|| report!("GET {url}"))?;
        let status = response.status().as_u16();
        if matches!(status, 404 | 410 | 451) {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(report!("GET {url}: http status {status}"));
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_CRATE_SIZE)
            .read_to_vec()
            .wrap_report_with(|| report!("GET {url}"))?;
        Ok(Some(body))
    }

    fn auth_header(&self) -> erris::Result<Option<String>> {
        let auth_required = self.config.as_ref().is_some_and(|c| c.auth_required);
        if !auth_required {
            return Ok(None);
        }
        match &self.token {
            Some(token) => Ok(Some(token.clone())),
            None => Err(report!(
                "registry `{}` requires authentication; set CARGO_REGISTRIES_{}_TOKEN",
                self.name,
                env_key(&self.name)
            )),
        }
    }

    fn config(&mut self) -> erris::Result<&IndexConfig> {
        let cached = self.config.take();
        let config = match cached {
            Some(config) => config,
            None => {
                let url = format!("{}config.json", self.index);
                let body = self.get(&url)?.ok_or_report_with(|| report!("{url} not found"))?;
                serde_json::from_slice(&body).wrap_report_with(|| report!("invalid {url}"))?
            }
        };
        Ok(self.config.insert(config))
    }

    /// All versions of `name` in the index, including yanked ones. Empty if never published.
    pub fn versions(&mut self, name: &str) -> erris::Result<&[Version]> {
        if !self.versions.contains_key(name) {
            self.config()?;
            let url = format!("{}{}", self.index, index_path(name));
            let body = self.get(&url)?.unwrap_or_default();
            let mut versions = Vec::new();
            for line in body.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                let entry: IndexLine =
                    serde_json::from_slice(line).wrap_report_with(|| report!("invalid index entry in {url}"))?;
                let version = Version::parse(&entry.vers)?;
                self.checksums.insert((name.to_owned(), version.clone()), entry.cksum);
                versions.push(version);
            }
            self.versions.insert(name.to_owned(), versions);
        }
        Ok(self.versions.get(name).map_or(&[], Vec::as_slice))
    }

    pub fn is_published(&mut self, name: &str, version: &Version) -> erris::Result<bool> {
        Ok(self.versions(name)?.contains(version))
    }

    /// Downloads `name@version` and returns its files keyed by path inside the package.
    pub fn download(&mut self, name: &str, version: &Version) -> erris::Result<BTreeMap<String, Vec<u8>>> {
        let dl = self.config()?.dl.clone();
        self.versions(name)?;
        let checksum = self
            .checksums
            .get(&(name.to_owned(), version.clone()))
            .cloned()
            .unwrap_or_default();
        let url = download_url(&dl, name, version, &checksum);
        let body = self.get(&url)?.ok_or_report_with(|| report!("{url} not found"))?;
        unpack(&body, &format!("{name}-{version}/")).wrap_report_with(|| report!("failed to unpack {url}"))
    }
}

fn unpack(crate_file: &[u8], prefix: &str) -> erris::Result<BTreeMap<String, Vec<u8>>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(crate_file));
    let mut files = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().replace('\\', "/");
        let Some(path) = path.strip_prefix(prefix).map(str::to_owned) else {
            continue;
        };
        let mut content = Vec::new();
        entry.read_to_end(&mut content)?;
        files.insert(path, content);
    }
    Ok(files)
}

fn index_path(name: &str) -> String {
    let name = name.to_lowercase();
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

fn download_url(dl: &str, name: &str, version: &Version, checksum: &str) -> String {
    let markers = ["{crate}", "{version}", "{prefix}", "{lowerprefix}", "{sha256-checksum}"];
    if !markers.iter().any(|m| dl.contains(m)) {
        return format!("{}/{name}/{version}/download", dl.trim_end_matches('/'));
    }
    let path = index_path(name);
    let prefix = path.rsplit_once('/').map_or("", |(prefix, _)| prefix);
    dl.replace("{crate}", name)
        .replace("{version}", &version.to_string())
        .replace("{prefix}", prefix)
        .replace("{lowerprefix}", prefix)
        .replace("{sha256-checksum}", checksum)
}

fn env_key(registry: &str) -> String {
    registry.to_uppercase().replace('-', "_")
}

/// Cargo configuration relevant to registries: env vars, `.cargo/config.toml` files
/// from the workspace up to the filesystem root, then `$CARGO_HOME`.
struct CargoConfig {
    files: Vec<DocumentMut>,
    credentials: Vec<DocumentMut>,
}

impl CargoConfig {
    fn load(cwd: &Path) -> Self {
        let cargo_home = cargo_home();
        let mut config_paths: Vec<PathBuf> = Vec::new();
        for dir in cwd.ancestors() {
            config_paths.push(dir.join(".cargo/config.toml"));
            config_paths.push(dir.join(".cargo/config"));
        }
        if let Some(home) = &cargo_home {
            config_paths.push(home.join("config.toml"));
            config_paths.push(home.join("config"));
        }
        let credential_paths = cargo_home
            .iter()
            .flat_map(|home| [home.join("credentials.toml"), home.join("credentials")])
            .collect::<Vec<_>>();
        Self {
            files: read_tomls(&config_paths),
            credentials: read_tomls(&credential_paths),
        }
    }

    fn get(&self, env: &str, key: &[&str]) -> Option<String> {
        let from_env = std::env::var(env).ok();
        from_env.or_else(|| self.files.iter().find_map(|doc| lookup(doc, key)))
    }

    fn token(&self, registry: &str) -> Option<String> {
        let key = ["registries", registry, "token"];
        self.get(&format!("CARGO_REGISTRIES_{}_TOKEN", env_key(registry)), &key)
            .or_else(|| self.credentials.iter().find_map(|doc| lookup(doc, &key)))
    }
}

fn cargo_home() -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    home.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
}

fn read_tomls(paths: &[PathBuf]) -> Vec<DocumentMut> {
    let mut docs = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        match text.parse::<DocumentMut>() {
            Ok(doc) => docs.push(doc),
            Err(err) => eprintln!("warning: ignoring invalid {}: {err}", path.display()),
        }
    }
    docs
}

fn lookup(doc: &DocumentMut, key: &[&str]) -> Option<String> {
    let mut item = doc.as_item();
    for part in key {
        item = item.get(part)?;
    }
    item.as_str().map(str::to_owned)
}

/// Resolves registries by name, caching index lookups across packages.
pub struct Registries {
    config: CargoConfig,
    registries: HashMap<String, Registry>,
}

impl Registries {
    pub fn new(cwd: &Path) -> Self {
        Self {
            config: CargoConfig::load(cwd),
            registries: HashMap::new(),
        }
    }

    /// The registry a package is published to, as `cargo publish` picks it: `--registry`,
    /// else the single entry of `package.publish`, else `registry.default`, else crates.io.
    /// `None` when `package.publish` doesn't allow the `--registry` one.
    pub fn name_for(
        &self,
        explicit: Option<&str>,
        package: &str,
        publish: Option<&[String]>,
    ) -> erris::Result<Option<String>> {
        if let Some(name) = explicit {
            let allowed = publish.is_none_or(|list| list.iter().any(|r| r == name));
            return Ok(allowed.then(|| name.to_owned()));
        }
        match publish {
            Some([single]) => return Ok(Some(single.clone())),
            Some(list) if list.len() > 1 => {
                return Err(report!(
                    "`{package}` can be published to several registries ({}); pass --registry",
                    list.join(", ")
                ));
            }
            _ => {}
        }
        let default = self.config.get("CARGO_REGISTRY_DEFAULT", &["registry", "default"]);
        Ok(Some(default.unwrap_or_else(|| CRATES_IO.to_owned())))
    }

    pub fn get(&mut self, name: &str) -> erris::Result<&mut Registry> {
        match self.registries.entry(name.to_owned()) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let registry = Self::resolve(&self.config, name)?;
                Ok(entry.insert(registry))
            }
        }
    }

    fn resolve(config: &CargoConfig, name: &str) -> erris::Result<Registry> {
        if name == CRATES_IO {
            return Ok(Registry::new(name.to_owned(), CRATES_IO_INDEX.to_owned(), None));
        }
        let env = format!("CARGO_REGISTRIES_{}_INDEX", env_key(name));
        let Some(index) = config.get(&env, &["registries", name, "index"]) else {
            return Err(report!(
                "registry `{name}` is not configured (registries.{name}.index or {env})"
            ));
        };
        let Some(index) = index.strip_prefix("sparse+") else {
            return Err(report!(
                "registry `{name}` uses a git index `{index}`; only sparse registries are supported"
            ));
        };
        let index = format!("{}/", index.trim_end_matches('/'));
        Ok(Registry::new(name.to_owned(), index, config.token(name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_paths() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("Serde"), "se/rd/serde");
    }

    #[test]
    fn download_urls() {
        let v = Version::new(1, 2, 3);
        assert_eq!(
            download_url("https://static.crates.io/crates", "serde", &v, ""),
            "https://static.crates.io/crates/serde/1.2.3/download"
        );
        assert_eq!(
            download_url("https://x/{prefix}/{crate}-{version}.crate", "serde", &v, ""),
            "https://x/se/rd/serde-1.2.3.crate"
        );
    }
}
