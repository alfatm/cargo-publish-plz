//! Files whose changes don't make a package "changed".

use globset::{Glob, GlobSet, GlobSetBuilder};

pub const DEFAULT: [&str; 1] = ["*.md"];

pub struct Ignore {
    set: GlobSet,
}

impl Ignore {
    /// `*` matches across `/`, so `*.md` covers markdown files in any directory.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> erris::Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(Glob::new(pattern.as_ref())?);
        }
        Ok(Self { set: builder.build()? })
    }

    pub fn nothing() -> Self {
        Self { set: GlobSet::empty() }
    }

    /// `path` is relative to the package root, with `/` separators.
    pub fn is_ignored(&self, path: &str) -> bool {
        self.set.is_match(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_nested_paths() {
        let ignore = Ignore::new(&["*.md", "examples/**"]).unwrap();
        assert!(ignore.is_ignored("README.md"));
        assert!(ignore.is_ignored("docs/guide.md"));
        assert!(ignore.is_ignored("examples/a/main.rs"));
        assert!(!ignore.is_ignored("src/lib.rs"));
        assert!(!Ignore::nothing().is_ignored("README.md"));
    }
}
