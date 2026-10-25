use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Error;

/// Name of the per-workspace metadata directory. It may not appear as a path
/// component anywhere in a repository.
pub const METADATA_DIR: &str = ".bud";

/// A normalized, `/`-separated path relative to the repository root.
///
/// The empty path is the repository root; it is valid as a prefix (for sparse
/// checkouts, diffs and listings) but never as the path of a file.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoPath(String);

impl RepoPath {
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Parses a path, accepting and stripping leading/trailing slashes.
    pub fn new(path: impl Into<String>) -> Result<Self, Error> {
        let path = path.into();
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(Self::root());
        }
        for component in trimmed.split('/') {
            validate_component(component).map_err(|reason| Error::InvalidPath { path: path.clone(), reason })?;
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// The final component, or `""` for the root.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }

    pub fn parent(&self) -> Option<RepoPath> {
        if self.is_root() {
            return None;
        }
        Some(match self.0.rfind('/') {
            Some(i) => Self(self.0[..i].to_string()),
            None => Self::root(),
        })
    }

    pub fn join(&self, name: &str) -> Result<RepoPath, Error> {
        validate_component(name).map_err(|reason| Error::InvalidPath { path: name.to_string(), reason })?;
        Ok(if self.is_root() { Self(name.to_string()) } else { Self(format!("{}/{}", self.0, name)) })
    }

    /// Proper, non-root ancestors, nearest last: `a/b/c` yields `a`, `a/b`.
    pub fn ancestors(&self) -> impl Iterator<Item = &str> {
        self.0.match_indices('/').map(|(i, _)| &self.0[..i])
    }

    /// True if `self` is `dir` or lies underneath it.
    pub fn is_within(&self, dir: &RepoPath) -> bool {
        dir.is_root()
            || self.0 == dir.0
            || (self.0.starts_with(&dir.0) && self.0.as_bytes().get(dir.0.len()) == Some(&b'/'))
    }
}

fn validate_component(name: &str) -> Result<(), &'static str> {
    match name {
        "" => Err("empty path component"),
        "." | ".." => Err("relative path component"),
        METADATA_DIR => Err("reserved name"),
        _ if name.contains('/') => Err("component contains '/'"),
        _ if name.contains('\0') => Err("component contains NUL"),
        _ => Ok(()),
    }
}

impl TryFrom<String> for RepoPath {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Error> {
        Self::new(value)
    }
}

// Eq, Ord and Hash are derived from the inner string, so this is consistent.
impl std::borrow::Borrow<str> for RepoPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<RepoPath> for String {
    fn from(path: RepoPath) -> String {
        path.0
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            f.write_str("/")
        } else {
            f.write_str(&self.0)
        }
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn normalizes_slashes() {
        assert_eq!(p("/a/b/"), p("a/b"));
        assert!(p("/").is_root());
    }

    #[test]
    fn rejects_bad_components() {
        for bad in ["a//b", "a/../b", "./a", ".bud/x", "a/.bud", "a\0b"] {
            assert!(RepoPath::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn structure() {
        let path = p("a/b/c");
        assert_eq!(path.file_name(), "c");
        assert_eq!(path.parent(), Some(p("a/b")));
        assert_eq!(p("a").parent(), Some(RepoPath::root()));
        assert_eq!(path.ancestors().collect::<Vec<_>>(), ["a", "a/b"]);
        assert_eq!(RepoPath::root().join("x").unwrap(), p("x"));
    }

    #[test]
    fn within() {
        assert!(p("a/b").is_within(&p("a")));
        assert!(p("a").is_within(&p("a")));
        assert!(p("a").is_within(&RepoPath::root()));
        assert!(!p("ab").is_within(&p("a")));
        assert!(!p("a").is_within(&p("a/b")));
    }
}
