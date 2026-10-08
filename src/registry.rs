//! Minimal client for sparse registry indexes (crates.io or any `sparse+` registry).

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

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
    #[serde(default)]
    yanked: bool,
}

/// Where a package goes.
pub enum Choice {
    Registry(String),
    /// `package.publish` doesn't allow the `--registry` one.
    NotAllowed,
    /// `package.publish` lists several registries and no `--registry` picks one.
    Ambiguous(Vec<String>),
}

/// One published version as the index lists it.
#[derive(Clone)]
struct IndexEntry {
    version: Version,
    checksum: String,
    yanked: bool,
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
    config: IndexConfig,
    /// name -> every published version, yanked ones included
    versions: Mutex<HashMap<String, Vec<IndexEntry>>>,
}

enum Fetched {
    Body(Vec<u8>),
    NotFound,
    Unauthorized,
}

impl Registry {
    /// Resolves the registry and reads its `config.json`.
    fn connect(name: String, index: String, token: Option<String>) -> erris::Result<Self> {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        let url = format!("{index}config.json");
        let mut fetched = fetch(&agent, &url, None)?;
        if matches!(fetched, Fetched::Unauthorized) {
            let token = token.as_deref().ok_or_report_with(|| auth_error(&name))?;
            fetched = fetch(&agent, &url, Some(token))?;
        }
        let Fetched::Body(body) = fetched else {
            return Err(report!("{url}: not found or not authorized"));
        };
        let config = serde_json::from_slice(&body).wrap_report_with(|| report!("invalid {url}"))?;
        Ok(Self {
            name,
            index,
            token,
            agent,
            config,
            versions: Mutex::new(HashMap::new()),
        })
    }

    fn get(&self, url: &str) -> erris::Result<Option<Vec<u8>>> {
        let token = if self.config.auth_required {
            Some(self.token.as_deref().ok_or_report_with(|| auth_error(&self.name))?)
        } else {
            None
        };
        match fetch(&self.agent, url, token)? {
            Fetched::Body(body) => Ok(Some(body)),
            Fetched::NotFound => Ok(None),
            Fetched::Unauthorized => Err(auth_error(&self.name)),
        }
    }

    /// `f` over the index entries of `name`, fetched on first use.
    fn with_entries<R>(&self, name: &str, f: impl FnOnce(&[IndexEntry]) -> R) -> erris::Result<R> {
        let versions = self.versions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entries) = versions.get(name) {
            return Ok(f(entries));
        }
        // Not held over the request: other packages are looked up meanwhile.
        drop(versions);
        let url = format!("{}{}", self.index, index_path(name));
        let body = self.get(&url)?.unwrap_or_default();
        let mut entries = Vec::new();
        for line in body.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let entry: IndexLine =
                serde_json::from_slice(line).wrap_report_with(|| report!("invalid index entry in {url}"))?;
            entries.push(IndexEntry {
                version: Version::parse(&entry.vers)?,
                checksum: entry.cksum,
                yanked: entry.yanked,
            });
        }
        let result = f(&entries);
        let mut versions = self.versions.lock().unwrap_or_else(PoisonError::into_inner);
        versions.insert(name.to_owned(), entries);
        Ok(result)
    }

    /// Versions of `name` in the index (empty if never published).
    pub fn versions(&self, name: &str) -> erris::Result<Versions> {
        self.with_entries(name, |entries| Versions {
            all: entries.iter().map(|e| e.version.clone()).collect(),
            live: entries
                .iter()
                .filter(|e| !e.yanked)
                .map(|e| e.version.clone())
                .collect(),
        })
    }

    pub fn is_published(&self, name: &str, version: &Version) -> erris::Result<bool> {
        let versions = self.versions(name)?;
        Ok(versions.all.contains(version))
    }

    /// Downloads `name@version` and returns its files keyed by path inside the package.
    pub fn download(&self, name: &str, version: &Version) -> erris::Result<BTreeMap<String, Vec<u8>>> {
        let checksum = self.checksum(name, version)?;
        let (url, body) = self.download_crate(name, version, &checksum)?;
        unpack(&body, &format!("{name}-{version}/"), None).wrap_report_with(|| report!("failed to unpack {url}"))
    }

    /// The commit `name@version` was packaged from, from its `.cargo_vcs_info.json`; `None` without one (packaged
    /// outside git, or with `--allow-dirty` of an untracked package). A `.crate` never changes, so the answer is
    /// cached by its checksum.
    pub fn vcs_sha(&self, name: &str, version: &Version) -> erris::Result<Option<String>> {
        let checksum = self.checksum(name, version)?;
        let cache = (!checksum.is_empty())
            .then(|| cache_dir().map(|dir| dir.join("vcs").join(&checksum)))
            .flatten();
        if let Some(path) = &cache {
            let cached = std::fs::read_to_string(path);
            if let Ok(sha) = cached {
                return Ok(Some(sha.trim().to_owned()).filter(|sha| !sha.is_empty()));
            }
        }
        let (url, body) = self.download_crate(name, version, &checksum)?;
        // cargo packs `.cargo_vcs_info.json` first: the rest of the archive is not read.
        let files = unpack(&body, &format!("{name}-{version}/"), Some(VCS_INFO))
            .wrap_report_with(|| report!("failed to unpack {url}"))?;
        let sha = files.get(VCS_INFO).and_then(|info| vcs_info_sha(info));
        if let Some(path) = &cache {
            store(path, sha.as_deref().unwrap_or_default());
        }
        Ok(sha)
    }

    fn checksum(&self, name: &str, version: &Version) -> erris::Result<String> {
        self.with_entries(name, |entries| {
            let entry = entries.iter().find(|e| &e.version == version);
            entry.map(|e| e.checksum.clone()).unwrap_or_default()
        })
    }

    fn download_crate(&self, name: &str, version: &Version, checksum: &str) -> erris::Result<(String, Vec<u8>)> {
        let url = download_url(&self.config.dl, name, version, checksum);
        let body = self.get(&url)?.ok_or_report_with(|| report!("{url} not found"))?;
        Ok((url, body))
    }
}

/// Versions of a package in its registry.
#[derive(Default)]
pub struct Versions {
    /// Every published version, yanked ones included: none of them can be published again.
    pub all: Vec<Version>,
    /// The versions that are not yanked.
    pub live: Vec<Version>,
}

pub const VCS_INFO: &str = ".cargo_vcs_info.json";

#[derive(Deserialize)]
struct VcsInfo {
    git: VcsGit,
}

#[derive(Deserialize)]
struct VcsGit {
    sha1: String,
}

/// The commit in a `.cargo_vcs_info.json`.
pub fn vcs_info_sha(info: &[u8]) -> Option<String> {
    serde_json::from_slice::<VcsInfo>(info).ok().map(|info| info.git.sha1)
}

/// `$XDG_CACHE_HOME/publish-plz`, `%LOCALAPPDATA%\publish-plz`, else `~/.cache/publish-plz`. An empty or relative
/// variable is ignored, as the XDG spec asks: CI images set `XDG_CACHE_HOME=`, and a relative cache would land in
/// the repository being checked.
fn cache_dir() -> Option<PathBuf> {
    cache_dir_from(|var| std::env::var_os(var))
}

fn cache_dir_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute = |var: &str| env(var).map(PathBuf::from).filter(|path| path.is_absolute());
    let base = absolute("XDG_CACHE_HOME")
        .or_else(|| absolute("LOCALAPPDATA"))
        .or_else(|| absolute("HOME").map(|home| home.join(".cache")))?;
    Some(base.join("publish-plz"))
}

/// Best effort: a cache that can't be written is only slower. Written aside and renamed, so a reader never sees half
/// a file.
fn store(path: &Path, content: &str) {
    let Some(dir) = path.parent() else {
        return;
    };
    let created = std::fs::create_dir_all(dir);
    let aside = path.with_extension(format!("{}.tmp", std::process::id()));
    let written = created.and_then(|()| std::fs::write(&aside, content));
    let renamed = written.and_then(|()| std::fs::rename(&aside, path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&aside);
    }
}

fn fetch(agent: &ureq::Agent, url: &str, token: Option<&str>) -> erris::Result<Fetched> {
    let mut request = agent.get(url);
    if let Some(token) = token {
        request = request.header("Authorization", token);
    }
    let mut response = request.call().wrap_report_with(|| report!("GET {url}"))?;
    let status = response.status().as_u16();
    match status {
        404 | 410 | 451 => return Ok(Fetched::NotFound),
        401 | 403 => return Ok(Fetched::Unauthorized),
        _ => {}
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
    Ok(Fetched::Body(body))
}

fn auth_error(registry: &str) -> Report {
    report!(
        "registry `{registry}` requires authentication; set CARGO_REGISTRIES_{}_TOKEN",
        env_key(registry)
    )
}

/// The files of a `.crate`; with `only`, just that one, reading no further than it.
fn unpack(crate_file: &[u8], prefix: &str, only: Option<&str>) -> erris::Result<BTreeMap<String, Vec<u8>>> {
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
        if only.is_some_and(|only| only != path) {
            continue;
        }
        let mut content = Vec::new();
        entry.read_to_end(&mut content)?;
        files.insert(path, content);
        if only.is_some() {
            break;
        }
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

/// What is wrong with a package `package.publish` allows to go to several registries.
pub fn several_registries(list: &[String]) -> String {
    format!(
        "can be published to several registries ({}); pass --registry",
        list.join(", ")
    )
}

/// Resolves registries by name, caching index lookups across packages.
pub struct Registries {
    config: CargoConfig,
    registries: HashMap<String, Registry>,
}

impl Registries {
    /// Config files are read from the ancestors of `cwd`, as cargo run there would.
    pub fn new(cwd: &Path) -> Self {
        Self {
            config: CargoConfig::load(cwd),
            registries: HashMap::new(),
        }
    }

    /// The registry a package is published to, as `cargo publish` picks it: `--registry`,
    /// else the single entry of `package.publish`, else `registry.default`, else crates.io.
    pub fn choose(&self, explicit: Option<&str>, publish: Option<&[String]>) -> Choice {
        if let Some(name) = explicit {
            let allowed = publish.is_none_or(|list| list.iter().any(|r| r == name));
            return if allowed {
                Choice::Registry(name.to_owned())
            } else {
                Choice::NotAllowed
            };
        }
        match publish {
            Some([single]) => return Choice::Registry(single.clone()),
            Some(list) if list.len() > 1 => return Choice::Ambiguous(list.to_vec()),
            _ => {}
        }
        let default = self.config.get("CARGO_REGISTRY_DEFAULT", &["registry", "default"]);
        Choice::Registry(default.unwrap_or_else(|| CRATES_IO.to_owned()))
    }

    /// [`Registries::choose`] for commands that need one registry: several allowed ones are an error.
    /// `None` when `package.publish` doesn't allow the `--registry` one.
    pub fn name_for(
        &self,
        explicit: Option<&str>,
        package: &str,
        publish: Option<&[String]>,
    ) -> erris::Result<Option<String>> {
        match self.choose(explicit, publish) {
            Choice::Registry(name) => Ok(Some(name)),
            Choice::NotAllowed => Ok(None),
            Choice::Ambiguous(list) => Err(report!("`{package}` {}", several_registries(&list))),
        }
    }

    /// Resolves a registry once; later lookups go through [`Registries::get`].
    pub fn connect(&mut self, name: &str) -> erris::Result<&Registry> {
        match self.registries.entry(name.to_owned()) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let registry = Self::resolve(&self.config, name)?;
                Ok(entry.insert(registry))
            }
        }
    }

    pub fn get(&self, name: &str) -> erris::Result<&Registry> {
        self.registries
            .get(name)
            .ok_or_report_with(|| report!("registry `{name}` is not connected"))
    }

    fn resolve(config: &CargoConfig, name: &str) -> erris::Result<Registry> {
        if name == CRATES_IO {
            return Registry::connect(name.to_owned(), CRATES_IO_INDEX.to_owned(), None);
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
        Registry::connect(name.to_owned(), index, config.token(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_choice() {
        let registries = Registries::new(&std::env::temp_dir());
        let both = ["a".to_owned(), "b".to_owned()];
        let picked = |choice: Choice| match choice {
            Choice::Registry(name) => format!("registry {name}"),
            Choice::NotAllowed => "not allowed".to_owned(),
            Choice::Ambiguous(list) => format!("ambiguous {}", list.join(",")),
        };
        assert_eq!(picked(registries.choose(None, Some(&both))), "ambiguous a,b");
        assert_eq!(picked(registries.choose(Some("b"), Some(&both))), "registry b");
        assert_eq!(picked(registries.choose(Some("c"), Some(&both))), "not allowed");
        assert_eq!(picked(registries.choose(None, Some(&both[..1]))), "registry a");
        assert!(registries.name_for(None, "x", Some(&both)).is_err());
    }

    #[test]
    fn the_cache_is_never_relative() {
        let root = std::env::temp_dir();
        let home = root.join("home");
        let with = |xdg: &str| {
            let home = home.clone();
            cache_dir_from(move |var| match var {
                "XDG_CACHE_HOME" => Some(xdg.into()),
                "HOME" => Some(home.clone().into()),
                _ => None,
            })
        };
        assert_eq!(with(""), Some(home.join(".cache/publish-plz")));
        assert_eq!(with("cache"), Some(home.join(".cache/publish-plz")));
        let xdg = root.join("xdg");
        assert_eq!(with(&xdg.to_string_lossy()), Some(xdg.join("publish-plz")));
        assert_eq!(cache_dir_from(|_| Some("relative".into())), None);
    }

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
