//! Conventional commits -> next semver version.

use semver::{BuildMetadata, Prerelease, Version};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Fix,
    Feature,
    Breaking,
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Change::Fix => "fix",
            Change::Feature => "feat",
            Change::Breaking => "breaking",
        })
    }
}

/// Classifies a full commit message. Non-conventional messages count as a fix.
pub fn classify(message: &str) -> Change {
    let breaking_footer = message
        .lines()
        .any(|l| l.starts_with("BREAKING CHANGE:") || l.starts_with("BREAKING-CHANGE:"));
    let header = message.lines().next().unwrap_or_default().trim();
    let Some((prefix, _)) = header.split_once(':') else {
        return Change::Fix;
    };
    let bang = prefix.ends_with('!');
    let prefix = prefix.trim_end_matches('!');
    let kind = match prefix.split_once('(') {
        Some((kind, scope)) if scope.ends_with(')') => kind,
        Some(_) => return Change::Fix,
        None => prefix,
    };
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Change::Fix;
    }
    if bang || breaking_footer {
        Change::Breaking
    } else if kind.eq_ignore_ascii_case("feat") {
        Change::Feature
    } else {
        Change::Fix
    }
}

/// Same rules as release-plz (`next_version`):
/// - pre-release: bump the pre-release counter (`1.0.0-rc.1` -> `1.0.0-rc.2`);
/// - `0.x.y`: breaking -> minor, everything else -> patch;
/// - `>=1.0.0`: breaking -> major, feat -> minor, everything else -> patch.
pub fn next_version(version: &Version, change: Change) -> Version {
    let mut next = version.clone();
    next.build = BuildMetadata::EMPTY;
    if !version.pre.is_empty() {
        next.pre = bump_prerelease(&version.pre);
        return next;
    }
    let change = if version.major == 0 && change == Change::Feature {
        Change::Fix
    } else {
        change
    };
    match (version.major, change) {
        (0, Change::Breaking) | (_, Change::Feature) => {
            next.minor += 1;
            next.patch = 0;
        }
        (_, Change::Breaking) => {
            next.major += 1;
            next.minor = 0;
            next.patch = 0;
        }
        (_, Change::Fix) => next.patch += 1,
    }
    next
}

/// An explicit `--bump` level, applied literally: `minor` on `0.3.2` gives `0.4.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Level {
    Patch,
    Minor,
    Major,
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Level::Patch => "patch",
            Level::Minor => "minor",
            Level::Major => "major",
        })
    }
}

/// A pre-release is released as is when it already is at that level
/// (`1.0.0-rc.1` + `patch` -> `1.0.0`), like `npm version`.
pub fn bump_level(version: &Version, level: Level) -> Version {
    let mut next = Version::new(version.major, version.minor, version.patch);
    let release_only = !version.pre.is_empty()
        && match level {
            Level::Patch => true,
            Level::Minor => version.patch == 0,
            Level::Major => version.minor == 0 && version.patch == 0,
        };
    if release_only {
        return next;
    }
    match level {
        Level::Patch => next.patch += 1,
        Level::Minor => {
            next.minor += 1;
            next.patch = 0;
        }
        Level::Major => {
            next.major += 1;
            next.minor = 0;
            next.patch = 0;
        }
    }
    next
}

fn bump_prerelease(pre: &Prerelease) -> Prerelease {
    let mut parts: Vec<String> = pre.as_str().split('.').map(str::to_owned).collect();
    let last = parts.last_mut().and_then(|p| p.parse::<u64>().ok().map(|n| (p, n)));
    match last {
        Some((part, n)) => *part = (n + 1).to_string(),
        None => parts.push("1".to_owned()),
    }
    // Incrementing a number or appending `.1` keeps the identifiers valid.
    Prerelease::new(&parts.join(".")).unwrap_or_else(|_| pre.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_commits() {
        assert_eq!(classify("feat: add x"), Change::Feature);
        assert_eq!(classify("feat(api): add x"), Change::Feature);
        assert_eq!(classify("fix!: drop x"), Change::Breaking);
        assert_eq!(classify("refactor(core)!: drop x"), Change::Breaking);
        assert_eq!(classify("feat: x\n\nBREAKING CHANGE: y"), Change::Breaking);
        assert_eq!(classify("chore: bump"), Change::Fix);
        assert_eq!(classify("Update readme"), Change::Fix);
        assert_eq!(classify("Merge branch 'a': b"), Change::Fix);
    }

    #[test]
    fn next_versions() {
        let v = |s| Version::parse(s).unwrap();
        assert_eq!(next_version(&v("0.1.3"), Change::Fix), v("0.1.4"));
        assert_eq!(next_version(&v("0.1.3"), Change::Feature), v("0.1.4"));
        assert_eq!(next_version(&v("0.1.3"), Change::Breaking), v("0.2.0"));
        assert_eq!(next_version(&v("1.2.3"), Change::Fix), v("1.2.4"));
        assert_eq!(next_version(&v("1.2.3"), Change::Feature), v("1.3.0"));
        assert_eq!(next_version(&v("1.2.3"), Change::Breaking), v("2.0.0"));
        assert_eq!(next_version(&v("1.0.0-rc.1"), Change::Breaking), v("1.0.0-rc.2"));
        assert_eq!(next_version(&v("1.0.0-alpha"), Change::Fix), v("1.0.0-alpha.1"));
    }

    #[test]
    fn explicit_levels() {
        let v = |s| Version::parse(s).unwrap();
        assert_eq!(bump_level(&v("0.3.2"), Level::Patch), v("0.3.3"));
        assert_eq!(bump_level(&v("0.3.2"), Level::Minor), v("0.4.0"));
        assert_eq!(bump_level(&v("0.3.2"), Level::Major), v("1.0.0"));
        assert_eq!(bump_level(&v("1.0.0-rc.1"), Level::Patch), v("1.0.0"));
        assert_eq!(bump_level(&v("1.0.0-rc.1"), Level::Major), v("1.0.0"));
        assert_eq!(bump_level(&v("1.2.3-rc.1"), Level::Minor), v("1.3.0"));
    }
}
