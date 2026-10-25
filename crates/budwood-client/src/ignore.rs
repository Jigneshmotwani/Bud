//! `.budignore`: patterns for untracked files that `status` and `commit` skip.
//!
//! One pattern per line; blank lines and lines starting with `#` are skipped.
//! `*` matches any run of characters except `/`, and `?` matches one.
//! A trailing `/` restricts a pattern to directories. A pattern containing
//! `/` is matched against the whole path from the workspace root; otherwise it
//! is matched against each file or directory name. Tracked files are never
//! ignored.

use std::path::Path;

use anyhow::{Context, Result};
use budwood_core::RepoPath;

pub const IGNORE_FILE: &str = ".budignore";

#[derive(Debug, Default)]
pub struct Ignore {
    patterns: Vec<Pattern>,
}

#[derive(Debug)]
struct Pattern {
    glob: String,
    anchored: bool,
    dir_only: bool,
}

impl Ignore {
    pub fn load(root: &Path) -> Result<Self> {
        match std::fs::read_to_string(root.join(IGNORE_FILE)) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).context("reading .budignore"),
        }
    }

    pub fn parse(text: &str) -> Self {
        let patterns = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let dir_only = line.ends_with('/');
                let glob = line.trim_matches('/').to_string();
                let anchored = line.trim_end_matches('/').contains('/');
                Pattern { glob, anchored, dir_only }
            })
            .filter(|p| !p.glob.is_empty())
            .collect();
        Self { patterns }
    }

    pub fn is_ignored(&self, path: &RepoPath, is_dir: bool) -> bool {
        self.patterns.iter().any(|p| {
            (is_dir || !p.dir_only)
                && if p.anchored {
                    glob_match(&p.glob, path.as_str())
                } else {
                    glob_match(&p.glob, path.file_name())
                }
        })
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    // Iterative matcher with single-star backtracking.
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' && t[ti] != '/' || p[pi] == t[ti] && p[pi] != '*') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star.filter(|&(_, st)| t[st] != '/') {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.o", "main.o"));
        assert!(!glob_match("*.o", "main.oo"));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/x/lib.rs"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn patterns() {
        let ignore = Ignore::parse("# comment\ntarget/\n*.log\n/build/out\n");
        assert!(ignore.is_ignored(&path("target"), true));
        assert!(ignore.is_ignored(&path("sub/target"), true));
        assert!(!ignore.is_ignored(&path("target"), false));
        assert!(ignore.is_ignored(&path("x/y.log"), false));
        assert!(ignore.is_ignored(&path("build/out"), false));
        assert!(!ignore.is_ignored(&path("other/build/out"), false));
    }
}
