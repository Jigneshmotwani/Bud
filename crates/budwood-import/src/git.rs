//! Thin wrappers around the `git` command line.
//!
//! Everything goes through plumbing commands with `-z` output, so paths are raw
//! bytes and never quoted. Two long-lived processes do the bulk of the work: one
//! `git cat-file --batch` for object contents, and one `git diff-tree --stdin`
//! that streams the changes of every imported commit.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{bail, ensure, Context, Result};

/// Git's mode for submodule entries ("gitlinks").
pub const MODE_GITLINK: u32 = 0o160000;
pub const MODE_SYMLINK: u32 = 0o120000;

/// A local git repository.
pub struct Git {
    dir: PathBuf,
}

impl Git {
    pub fn open(dir: &Path) -> Result<Self> {
        let git = Self { dir: dir.to_path_buf() };
        git.output(["rev-parse", "--git-dir"]).with_context(|| format!("{} is not a git repository", dir.display()))?;
        Ok(git)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir);
        cmd
    }

    /// Runs a git command to completion and returns its stdout.
    fn output<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(&self, args: I) -> Result<Vec<u8>> {
        let mut cmd = self.command();
        cmd.args(args);
        let out = cmd.stdin(Stdio::null()).output().context("running git (is it installed?)")?;
        if !out.status.success() {
            bail!("{:?} failed: {}", cmd, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    }

    /// Resolves a revision to a full commit id.
    pub fn resolve_commit(&self, rev: &str) -> Result<String> {
        let out = self.output(["rev-parse", "--verify", "--end-of-options", &format!("{rev}^{{commit}}")])?;
        Ok(String::from_utf8(out)?.trim().to_string())
    }

    /// True if `sha` names a commit in this repository.
    pub fn has_commit(&self, sha: &str) -> bool {
        self.output(["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
    }

    /// The best common ancestor of two commits, if they share history.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let out = self.command().args(["merge-base", a, b]).stdin(Stdio::null()).output()?;
        Ok(match out.status.code() {
            Some(0) => Some(String::from_utf8(out.stdout)?.trim().to_string()),
            Some(1) => None,
            _ => bail!("git merge-base {a} {b} failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
        })
    }

    /// The first-parent history of `commit`, oldest first.
    pub fn first_parent_history(&self, commit: &str) -> Result<Vec<String>> {
        let out = self.output(["rev-list", "--first-parent", "--reverse", commit, "--"])?;
        Ok(String::from_utf8(out)?.lines().map(str::to_string).collect())
    }

    /// Every file in the tree of `commit`, recursively.
    pub fn ls_tree(&self, commit: &str) -> Result<Vec<TreeFile>> {
        let out = self.output(["ls-tree", "-r", "-z", "--full-tree", commit])?;
        let mut files = Vec::new();
        for record in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            // "<mode> SP <type> SP <object> TAB <path>"
            let tab = record.iter().position(|&b| b == b'\t').context("malformed ls-tree output")?;
            let meta = std::str::from_utf8(&record[..tab])?;
            let mut fields = meta.split(' ');
            let (Some(mode), Some(_kind), Some(sha)) = (fields.next(), fields.next(), fields.next()) else {
                bail!("malformed ls-tree output {meta:?}");
            };
            files.push(TreeFile { mode: parse_mode(mode)?, sha: sha.to_string(), path: record[tab + 1..].to_vec() });
        }
        Ok(files)
    }

    pub fn cat_file(&self) -> Result<CatFile> {
        let mut child = self
            .command()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting git cat-file")?;
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(CatFile { child, stdin: Some(stdin), stdout })
    }

    /// Streams the raw changes of each `(commit, parent)` pair, in order. A
    /// pair without a parent is diffed against the empty tree.
    pub fn diff_stream(&self, pairs: Vec<(String, Option<String>)>) -> Result<DiffStream> {
        let mut child = self
            .command()
            .args(["diff-tree", "--stdin", "-r", "-z", "--raw", "--no-renames", "--root", "--always"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting git diff-tree")?;
        let mut stdin = BufWriter::new(child.stdin.take().unwrap());
        // Feed every pair from a separate thread so neither pipe can deadlock;
        // git applies back-pressure. "<commit> <parent>" diffs against that
        // parent only, which is the first-parent diff for merges.
        std::thread::spawn(move || {
            for (commit, parent) in pairs {
                let line = match parent {
                    Some(parent) => format!("{commit} {parent}\n"),
                    None => format!("{commit}\n"),
                };
                if stdin.write_all(line.as_bytes()).is_err() {
                    return; // The reader stopped early and killed git.
                }
            }
            let _ = stdin.flush();
        });
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(DiffStream { child, stdout, next_header: None, done: false })
    }
}

/// One entry of `git ls-tree -r`.
pub struct TreeFile {
    pub mode: u32,
    pub sha: String,
    pub path: Vec<u8>,
}

/// One line of `git diff-tree --raw`.
#[derive(Debug)]
pub struct RawChange {
    pub old_mode: u32,
    pub new_mode: u32,
    pub new_sha: String,
    pub path: Vec<u8>,
}

fn parse_mode(mode: &str) -> Result<u32> {
    u32::from_str_radix(mode, 8).with_context(|| format!("bad git mode {mode:?}"))
}

/// A `git cat-file --batch` process.
pub struct CatFile {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    /// Reads an object, returning its type and contents.
    pub fn read(&mut self, sha: &str) -> Result<(String, Vec<u8>)> {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{sha}")?;
        stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        // "<sha> <type> <size>\n", or "<sha> missing\n".
        let fields: Vec<&str> = header.split_whitespace().collect();
        let [_, kind, size] = fields[..] else {
            bail!("git cat-file: cannot read object {sha}: {}", header.trim());
        };
        let size: usize = size.parse().with_context(|| format!("git cat-file: bad header {header:?}"))?;
        let mut data = vec![0; size];
        self.stdout.read_exact(&mut data)?;
        let mut newline = [0u8];
        self.stdout.read_exact(&mut newline)?;
        Ok((kind.to_string(), data))
    }

    pub fn blob(&mut self, sha: &str) -> Result<Vec<u8>> {
        let (kind, data) = self.read(sha)?;
        ensure!(kind == "blob", "git object {sha} is a {kind}, not a blob");
        Ok(data)
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        self.stdin.take(); // EOF makes git exit.
        let _ = self.child.wait();
    }
}

/// The output of `git diff-tree --stdin --always -z`: for each input line, the
/// commit id followed by zero or more raw change records.
pub struct DiffStream {
    child: Child,
    stdout: BufReader<ChildStdout>,
    next_header: Option<String>,
    done: bool,
}

impl DiffStream {
    fn token(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = Vec::new();
        if self.stdout.read_until(0, &mut buf)? == 0 {
            return Ok(None);
        }
        ensure!(buf.pop() == Some(0), "truncated git diff-tree output");
        Ok(Some(buf))
    }

    /// The next commit's id and changes, or `None` at the end.
    pub fn next_commit(&mut self) -> Result<Option<(String, Vec<RawChange>)>> {
        let commit = match self.next_header.take() {
            Some(commit) => commit,
            None => match self.token()? {
                Some(token) => String::from_utf8(token)?,
                None => return self.finish().map(|()| None),
            },
        };
        ensure!(!commit.starts_with(':'), "unexpected git diff-tree output {commit:?}");
        let mut changes = Vec::new();
        while let Some(token) = self.token()? {
            let Some(meta) = token.strip_prefix(b":") else {
                self.next_header = Some(String::from_utf8(token)?);
                break;
            };
            // ":<old mode> <new mode> <old sha> <new sha> <status>", then the path.
            let meta = std::str::from_utf8(meta)?;
            let fields: Vec<&str> = meta.split(' ').collect();
            let [old_mode, new_mode, _, new_sha, _] = fields[..] else {
                bail!("malformed git diff-tree record {meta:?}");
            };
            let path = self.token()?.context("truncated git diff-tree output")?;
            changes.push(RawChange {
                old_mode: parse_mode(old_mode)?,
                new_mode: parse_mode(new_mode)?,
                new_sha: new_sha.to_string(),
                path,
            });
        }
        Ok(Some((commit, changes)))
    }

    fn finish(&mut self) -> Result<()> {
        self.done = true;
        let status = self.child.wait()?;
        ensure!(status.success(), "git diff-tree failed: {status}");
        Ok(())
    }
}

impl Drop for DiffStream {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The parts of a commit object the importer needs.
#[derive(Debug, PartialEq, Eq)]
pub struct CommitInfo {
    /// `Name <email>`.
    pub author: String,
    /// The author date in strict ISO 8601, like `git log --format=%aI`.
    pub author_date: String,
    pub message: String,
}

/// Parses a raw commit object as printed by `git cat-file commit`.
pub fn parse_commit(raw: &[u8]) -> Result<CommitInfo> {
    let split = raw.windows(2).position(|w| w == b"\n\n");
    let (headers, message) = match split {
        Some(i) => (&raw[..i], &raw[i + 2..]),
        None => (raw, &[][..]),
    };
    let mut author = None;
    let mut encoding = None;
    for line in headers.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"author ") {
            author = Some(String::from_utf8_lossy(rest).into_owned());
        } else if let Some(rest) = line.strip_prefix(b"encoding ") {
            encoding = Some(String::from_utf8_lossy(rest).to_ascii_lowercase());
        }
    }
    let author = author.context("commit has no author")?;
    // "Name <email> <unix seconds> <+hhmm>"
    let mut parts = author.rsplitn(3, ' ');
    let (Some(tz), Some(secs), Some(ident)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("malformed author line {author:?}");
    };
    let secs: i64 = secs.parse().with_context(|| format!("malformed author date in {author:?}"))?;
    let ident = ident.trim();
    let message = match encoding.as_deref() {
        Some("iso-8859-1" | "latin1" | "latin-1") => message.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(message).into_owned(),
    };
    Ok(CommitInfo {
        author: if ident.is_empty() { "unknown".to_string() } else { ident.to_string() },
        author_date: iso_date(secs, tz)?,
        message,
    })
}

/// Formats a git timestamp and `+hhmm` zone like `%aI`: `2026-10-25T14:03:00+02:00`.
fn iso_date(secs: i64, tz: &str) -> Result<String> {
    let bad = || format!("malformed time zone {tz:?}");
    let (sign, digits) = match tz.as_bytes().first() {
        Some(b'+') => (1, &tz[1..]),
        Some(b'-') => (-1, &tz[1..]),
        _ => bail!(bad()),
    };
    ensure!(digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_digit()), bad());
    let (hh, mm): (i64, i64) = (digits[..2].parse()?, digits[2..].parse()?);
    let local = secs + sign * (hh * 3600 + mm * 60);
    let (days, rem) = (local.div_euclid(86400), local.rem_euclid(86400));
    let (y, m, d) = civil_from_days(days);
    Ok(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{}{hh:02}:{mm:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        if sign < 0 { '-' } else { '+' }
    ))
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso_date(0, "+0000").unwrap(), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso_date(1_700_000_000, "+0530").unwrap(), "2023-11-15T03:43:20+05:30");
        assert_eq!(iso_date(951_825_600, "-0800").unwrap(), "2000-02-29T04:00:00-08:00");
        assert!(iso_date(0, "0000").is_err());
    }

    #[test]
    fn parses_commits() {
        let raw = b"tree 1234\nparent 5678\nauthor A U Thor <a@x> 1700000000 +0000\n\
                    committer C <c@x> 1700000001 +0100\ngpgsig -----BEGIN-----\n x\n -----END-----\n\nSubject\n\nBody\n";
        let info = parse_commit(raw).unwrap();
        assert_eq!(info.author, "A U Thor <a@x>");
        assert_eq!(info.author_date, "2023-11-14T22:13:20+00:00");
        assert_eq!(info.message, "Subject\n\nBody\n");
    }
}
