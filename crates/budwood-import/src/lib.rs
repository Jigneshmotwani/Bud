//! Import a git repository's history into a Budwood repository.
//!
//! Budwood keeps one linear history per repository, so the importer follows
//! the **first-parent** chain of a git ref, oldest first, and turns each git
//! commit into one changeset holding its diff against its first parent. A merge
//! therefore becomes a single changeset with everything it brought in.
//!
//! Per commit, the importer:
//!
//! 1. reads the raw diff from a long-lived `git diff-tree --stdin` process and
//!    maps git modes to Budwood file kinds (`100644` file, `100755` executable,
//!    `120000` symlink). Submodules (`160000`) and paths Budwood can't represent
//!    (non-UTF-8 names, `.bud` components) are skipped with a warning;
//! 2. reads new blobs from a long-lived `git cat-file --batch` process, hashes
//!    them into Budwood ids, asks the server which are missing and uploads those
//!    from a small thread pool;
//! 3. commits with the git message plus `Git-Commit:` and `Git-Date:` trailers.
//!    The server assigns its own timestamp, so the git author date only survives
//!    in the trailer.
//!
//! Imports are incremental: the `Git-Commit:` trailer of the newest imported
//! changeset says where to resume. Commits whose changes are all filtered out
//! (for example empty commits) produce no changeset.

mod git;
mod upload;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use budwood_client::{ApiError, Objects, Remote};
use budwood_core::api::{CommitRequest, MAX_BATCH};
use budwood_core::{walk_files, Change, FileKind, FileState, ObjectId, RepoPath};

pub use git::{parse_commit, CommitInfo};

use git::{CatFile, Git, RawChange, MODE_GITLINK, MODE_SYMLINK};
use upload::Uploader;

/// Trailer naming the git commit a changeset was imported from.
pub const COMMIT_TRAILER: &str = "Git-Commit";
/// Trailer holding the git author date, which Budwood doesn't otherwise keep.
pub const DATE_TRAILER: &str = "Git-Date";

/// Changesets per log page when looking for the last import.
const LOG_PAGE: usize = 1000;
/// Pending blob bytes that trigger an upload before the commit is complete.
const FLUSH_BYTES: usize = 64 << 20;
/// Print progress every this many git commits.
const PROGRESS_EVERY: usize = 100;
/// Individual warnings printed before the rest are only counted.
const MAX_WARNINGS: usize = 20;

#[derive(Clone, Debug)]
pub struct Options {
    /// The local git repository (any directory inside its work tree, or a bare repository).
    pub git_dir: PathBuf,
    /// The git revision whose first-parent history is imported.
    pub rev: String,
    /// Process at most this many git commits in this run.
    pub limit: Option<usize>,
    /// Import even when the Budwood repository holds changesets that aren't
    /// imports, or the last imported commit isn't in the history of `rev`.
    pub force: bool,
    /// Print what would be imported without uploading or committing anything.
    pub dry_run: bool,
    /// After importing, check that Budwood's head matches the git tree.
    pub verify: bool,
    /// Blob upload threads.
    pub threads: usize,
}

impl Options {
    pub fn new(git_dir: impl Into<PathBuf>) -> Self {
        Self {
            git_dir: git_dir.into(),
            rev: "HEAD".to_string(),
            limit: None,
            force: false,
            dry_run: false,
            verify: false,
            threads: 8,
        }
    }
}

/// What an import run did.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub dry_run: bool,
    /// The commit `rev` resolved to.
    pub tip: String,
    /// Commits in the first-parent history of `tip`.
    pub history_len: usize,
    /// The last commit imported by an earlier run, if any.
    pub resumed_after: Option<String>,
    /// Changesets created (or, in a dry run, that would be).
    pub imported: usize,
    /// Commits with no representable changes.
    pub empty_skipped: usize,
    /// Commits left for a later run because of `limit`.
    pub remaining: usize,
    /// Changed paths skipped because Budwood can't represent their names.
    pub paths_skipped: usize,
    /// Submodule entries skipped.
    pub gitlinks_skipped: usize,
    pub blobs_uploaded: usize,
    pub bytes_uploaded: u64,
    /// Budwood's head after the run.
    pub head_seq: u64,
    /// The git commit Budwood's head now corresponds to.
    pub synced_to: Option<String>,
    /// Number of files compared by `verify`.
    pub verified_files: Option<usize>,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let short = |sha: &str| sha[..sha.len().min(12)].to_string();
        writeln!(f, "git: {} first-parent commit(s) up to {}", self.history_len, short(&self.tip))?;
        match &self.resumed_after {
            Some(sha) => writeln!(f, "resumed after git commit {}", short(sha))?,
            None => writeln!(f, "started from the first commit")?,
        }
        let verb = if self.dry_run { "would import" } else { "imported" };
        writeln!(f, "{verb} {} changeset(s); skipped {} empty commit(s)", self.imported, self.empty_skipped)?;
        if self.paths_skipped + self.gitlinks_skipped > 0 {
            writeln!(
                f,
                "skipped {} change(s) to unrepresentable paths and {} submodule entr(ies)",
                self.paths_skipped, self.gitlinks_skipped
            )?;
        }
        if !self.dry_run {
            writeln!(f, "uploaded {} blob(s), {:.1} MiB", self.blobs_uploaded, self.bytes_uploaded as f64 / 1048576.0)?;
            writeln!(f, "budwood head: seq {}", self.head_seq)?;
        }
        if self.remaining > 0 {
            writeln!(f, "{} commit(s) remaining; run again to continue", self.remaining)?;
        }
        if let (Some(n), Some(sha)) = (self.verified_files, &self.synced_to) {
            writeln!(f, "verified: {n} file(s) match git commit {}", short(sha))?;
        }
        Ok(())
    }
}

/// Imports the first-parent history of `opts.rev` into the repository behind `remote`.
pub fn import(remote: &Remote, opts: &Options) -> Result<Summary> {
    let git = Git::open(&opts.git_dir)?;
    let tip = git.resolve_commit(&opts.rev)?;
    let history = git.first_parent_history(&tip)?;
    let resume = find_resume(remote, opts.dry_run)?;
    let mut summary = Summary {
        dry_run: opts.dry_run,
        tip: tip.clone(),
        history_len: history.len(),
        head_seq: resume.head_seq,
        ..Summary::default()
    };

    // The first commit to import, and the commit its diff is taken against.
    let (start, start_parent) = match &resume.last_import {
        Some((seq, sha)) => {
            if resume.foreign > 0 && !opts.force {
                bail!(
                    "the Budwood repository has {} changeset(s) after the last import (seq {seq}, git commit {sha}) \
                     that were not imported from git; use --force to import on top of them",
                    resume.foreign
                );
            }
            summary.resumed_after = Some(sha.clone());
            match history.iter().position(|c| c == sha) {
                Some(i) => (i + 1, Some(sha.clone())),
                None if !opts.force => bail!(
                    "the last imported git commit {sha} (seq {seq}) is not in the first-parent history of {}; \
                     use --force to import anyway",
                    opts.rev
                ),
                // Rewritten history: continue after the newest common ancestor,
                // diffing the first commit against what was imported last so
                // that Budwood still ends up with git's tree.
                None if git.has_commit(sha) => {
                    let base = git.merge_base(sha, &tip)?.and_then(|mb| history.iter().position(|c| *c == mb));
                    let start = base.map_or(0, |i| (i + 1).min(history.len() - 1));
                    eprintln!(
                        "warning: last imported commit {sha} is not in the history of {}; continuing from {} \
                         with a diff against {sha}",
                        opts.rev,
                        &history[start][..12]
                    );
                    (start, Some(sha.clone()))
                }
                None => {
                    eprintln!(
                        "warning: last imported commit {sha} is unknown to this git repository; importing from \
                         the first commit on top of the existing Budwood history"
                    );
                    (0, None)
                }
            }
        }
        None if resume.head_seq > 0 && !opts.force => bail!(
            "the Budwood repository has {} changeset(s) and none were imported from git; use --force to import on top",
            resume.head_seq
        ),
        None => (0, None),
    };
    let end = opts.limit.map_or(history.len(), |n| history.len().min(start.saturating_add(n)));
    summary.remaining = history.len() - end;
    if end > 0 {
        summary.synced_to = Some(history[end - 1].clone());
    }

    let mut cat = git.cat_file()?;
    let mut blobs = Blobs::new(remote, opts.threads);
    if start < end {
        let mut warnings = Warnings::default();
        let pairs = (start..end)
            .map(|i| {
                let parent = if i == start { start_parent.clone() } else { Some(history[i - 1].clone()) };
                (history[i].clone(), parent)
            })
            .collect();
        let mut diffs = git.diff_stream(pairs)?;
        let started = Instant::now();
        for (done, expected) in history[start..end].iter().enumerate() {
            let (sha, raw) = diffs.next_commit()?.context("git diff-tree ended early")?;
            ensure!(&sha == expected, "git diff-tree returned {sha}, expected {expected}");
            let changes = filter_changes(raw, &mut summary, &mut warnings)?;
            if changes.is_empty() {
                summary.empty_skipped += 1;
            } else {
                let (kind, raw_commit) = cat.read(&sha)?;
                ensure!(kind == "commit", "git object {sha} is a {kind}, not a commit");
                let info = parse_commit(&raw_commit)?;
                if opts.dry_run {
                    let subject = info.message.lines().next().unwrap_or("");
                    println!("would import {} ({} change(s)): {subject}", &sha[..12], changes.len());
                } else {
                    let changes = changes
                        .into_iter()
                        .map(|(path, new)| {
                            let new = match new {
                                Some((kind, git_sha)) => Some(FileState { kind, blob: blobs.id(&mut cat, &git_sha)? }),
                                None => None,
                            };
                            Ok(Change { path, new })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    blobs.flush()?;
                    let req = CommitRequest {
                        base: summary.head_seq,
                        author: info.author,
                        message: compose_message(&info.message, &sha, &info.author_date),
                        changes,
                    };
                    let resp = remote.commit(&req).with_context(|| format!("committing git commit {sha}"))?;
                    summary.head_seq = resp.changeset.seq;
                }
                summary.imported += 1;
            }
            if (done + 1) % PROGRESS_EVERY == 0 {
                eprintln!(
                    "{}/{} commits processed, {} imported, head seq {}, {} blob(s) uploaded, {:.1}s",
                    done + 1,
                    end - start,
                    summary.imported,
                    summary.head_seq,
                    blobs.uploaded,
                    started.elapsed().as_secs_f64()
                );
            }
        }
        warnings.finish();
    }
    summary.blobs_uploaded = blobs.uploaded;
    summary.bytes_uploaded = blobs.bytes;

    if opts.verify {
        if opts.dry_run {
            eprintln!("note: --verify is skipped in a dry run");
        } else if let Some(commit) = &summary.synced_to {
            summary.verified_files = Some(verify_tree(remote, &git, &mut cat, commit, &mut blobs.known)?);
        }
    }
    Ok(summary)
}

/// Checks that Budwood's head holds exactly the files of git revision `rev`,
/// after the same filtering the importer applies. Returns the number of files.
pub fn verify(remote: &Remote, git_dir: impl Into<PathBuf>, rev: &str) -> Result<usize> {
    let git = Git::open(&git_dir.into())?;
    let commit = git.resolve_commit(rev)?;
    verify_tree(remote, &git, &mut git.cat_file()?, &commit, &mut HashMap::new())
}

fn verify_tree(
    remote: &Remote,
    git: &Git,
    cat: &mut CatFile,
    commit: &str,
    known: &mut HashMap<String, ObjectId>,
) -> Result<usize> {
    let mut expected = BTreeMap::new();
    for file in git.ls_tree(commit)? {
        let (Some(kind), Ok(path)) = (file_kind(file.mode)?, repo_path(&file.path)) else { continue };
        let blob = match known.get(&file.sha) {
            Some(id) => *id,
            None => {
                let id = ObjectId::for_blob(&cat.blob(&file.sha)?);
                known.insert(file.sha, id);
                id
            }
        };
        expected.insert(path, FileState { kind, blob });
    }

    let head = remote.head()?;
    let cache = tempfile::tempdir().context("creating a temporary object cache")?;
    let objects = Objects::new(remote.clone(), cache.path().to_path_buf())?;
    let actual: BTreeMap<_, _> = walk_files(&objects, head.root_tree, &RepoPath::root())?.into_iter().collect();

    let mut problems = Vec::new();
    for (path, state) in &expected {
        match actual.get(path) {
            None => problems.push(format!("missing from Budwood: {path}")),
            Some(found) if found != state => {
                problems.push(format!("differs: {path} (git {state:?}, budwood {found:?})"))
            }
            Some(_) => {}
        }
    }
    problems.extend(actual.keys().filter(|p| !expected.contains_key(*p)).map(|p| format!("not in git: {p}")));
    if !problems.is_empty() {
        let shown = problems.iter().take(MAX_WARNINGS).map(|p| format!("\n  {p}")).collect::<String>();
        bail!(
            "verification failed: Budwood seq {} differs from git commit {commit} in {} path(s):{shown}",
            head.seq,
            problems.len()
        );
    }
    Ok(expected.len())
}

/// Appends the import trailers to a git commit message.
pub fn compose_message(message: &str, sha: &str, author_date: &str) -> String {
    let body = message.trim_end();
    let trailers = format!("{COMMIT_TRAILER}: {sha}\n{DATE_TRAILER}: {author_date}");
    if body.trim().is_empty() {
        trailers
    } else {
        format!("{body}\n\n{trailers}")
    }
}

/// The git commit named by a changeset message's `Git-Commit:` trailer. Only the
/// last paragraph is searched, so a mention elsewhere in a message doesn't count.
pub fn git_commit_trailer(message: &str) -> Option<&str> {
    let message = message.trim_end();
    let last_paragraph = message.rsplit_once("\n\n").map_or(message, |(_, last)| last);
    last_paragraph
        .lines()
        .filter_map(|line| line.strip_prefix(COMMIT_TRAILER)?.strip_prefix(':'))
        .map(str::trim)
        .rfind(|sha| sha.len() >= 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Where an earlier import left off.
struct Resume {
    head_seq: u64,
    /// Seq and git commit of the newest imported changeset.
    last_import: Option<(u64, String)>,
    /// Changesets newer than the last import (or all of them) that aren't imports.
    foreign: usize,
}

fn find_resume(remote: &Remote, dry_run: bool) -> Result<Resume> {
    let head_seq = match remote.head() {
        Ok(head) => head.seq,
        Err(e) if dry_run && e.downcast_ref::<ApiError>().is_some_and(ApiError::is_not_found) => {
            return Ok(Resume { head_seq: 0, last_import: None, foreign: 0 });
        }
        Err(e) => return Err(e).context("reading the Budwood head (does the repository exist? see --create-repo)"),
    };
    let mut foreign = 0;
    let mut before = None;
    while before.map_or(head_seq > 0, |seq| seq > 1) {
        let page = remote.log(&RepoPath::root(), before, LOG_PAGE)?;
        let Some(oldest) = page.last() else { break };
        before = Some(oldest.seq);
        for cs in &page {
            if let Some(sha) = git_commit_trailer(&cs.message) {
                return Ok(Resume { head_seq, last_import: Some((cs.seq, sha.to_string())), foreign });
            }
            foreign += 1;
        }
    }
    Ok(Resume { head_seq, last_import: None, foreign })
}

/// A change still naming its new content by git blob id.
type PendingChange = (RepoPath, Option<(FileKind, String)>);

/// Maps a git mode to a Budwood file kind; `None` for absent entries and submodules.
fn file_kind(mode: u32) -> Result<Option<FileKind>> {
    Ok(match mode {
        0 | MODE_GITLINK => None,
        MODE_SYMLINK => Some(FileKind::Symlink),
        // Old repositories may contain modes like 100664; git treats any
        // executable bit as executable.
        m if m & 0o170000 == 0o100000 => Some(if m & 0o111 != 0 { FileKind::Executable } else { FileKind::File }),
        m => bail!("unsupported git mode {m:o}"),
    })
}

fn repo_path(raw: &[u8]) -> Result<RepoPath, String> {
    let s = std::str::from_utf8(raw).map_err(|_| "name is not UTF-8".to_string())?;
    RepoPath::new(s).map_err(|e| e.to_string())
}

fn filter_changes(raw: Vec<RawChange>, summary: &mut Summary, warnings: &mut Warnings) -> Result<Vec<PendingChange>> {
    let mut changes = Vec::with_capacity(raw.len());
    for change in raw {
        let path = match repo_path(&change.path) {
            Ok(path) => path,
            Err(reason) => {
                summary.paths_skipped += 1;
                warnings.warn(format!("skipping {:?}: {reason}", String::from_utf8_lossy(&change.path)));
                continue;
            }
        };
        if change.new_mode == MODE_GITLINK {
            summary.gitlinks_skipped += 1;
            warnings.warn(format!("skipping submodule {path}"));
        }
        // A file replaced by a submodule still has to be deleted.
        let was_file = file_kind(change.old_mode)?.is_some();
        let new = file_kind(change.new_mode)?.map(|kind| (kind, change.new_sha));
        if new.is_some() || was_file {
            changes.push((path, new));
        }
    }
    Ok(changes)
}

/// Rate-limited warnings on stderr.
#[derive(Default)]
struct Warnings {
    count: usize,
}

impl Warnings {
    fn warn(&mut self, message: String) {
        self.count += 1;
        if self.count <= MAX_WARNINGS {
            eprintln!("warning: {message}");
        }
    }

    fn finish(&self) {
        if self.count > MAX_WARNINGS {
            eprintln!("warning: {} more warning(s) not shown", self.count - MAX_WARNINGS);
        }
    }
}

/// Maps git blobs to Budwood ids and uploads the ones the server lacks.
struct Blobs {
    remote: Remote,
    uploader: Uploader,
    /// Git blob id to Budwood id, for blobs already hashed (and, outside
    /// verification, uploaded or found on the server) in this run.
    known: HashMap<String, ObjectId>,
    pending: Vec<(ObjectId, Vec<u8>)>,
    pending_ids: HashSet<ObjectId>,
    pending_bytes: usize,
    uploaded: usize,
    bytes: u64,
}

impl Blobs {
    fn new(remote: &Remote, threads: usize) -> Self {
        Self {
            remote: remote.clone(),
            uploader: Uploader::new(remote, threads),
            known: HashMap::new(),
            pending: Vec::new(),
            pending_ids: HashSet::new(),
            pending_bytes: 0,
            uploaded: 0,
            bytes: 0,
        }
    }

    /// The Budwood id of a git blob, queueing its contents for upload.
    fn id(&mut self, cat: &mut CatFile, git_sha: &str) -> Result<ObjectId> {
        if let Some(id) = self.known.get(git_sha) {
            return Ok(*id);
        }
        let data = cat.blob(git_sha)?;
        let id = ObjectId::for_blob(&data);
        self.known.insert(git_sha.to_string(), id);
        if self.pending_ids.insert(id) {
            self.pending_bytes += data.len();
            self.pending.push((id, data));
            if self.pending.len() >= MAX_BATCH || self.pending_bytes >= FLUSH_BYTES {
                self.flush()?;
            }
        }
        Ok(id)
    }

    /// Uploads every pending blob the server doesn't have.
    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let ids: Vec<ObjectId> = self.pending.iter().map(|(id, _)| *id).collect();
        let missing: HashSet<ObjectId> = self.remote.missing(&ids)?.into_iter().collect();
        let todo: Vec<_> = self.pending.drain(..).filter(|(id, _)| missing.contains(id)).collect();
        self.pending_ids.clear();
        self.pending_bytes = 0;
        self.uploaded += todo.len();
        self.bytes += todo.iter().map(|(_, data)| data.len() as u64).sum::<u64>();
        self.uploader.upload_all(todo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailers_round_trip() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let msg = compose_message("Fix it\n\nSigned-off-by: a <a@x>\n", sha, "2026-01-01T00:00:00+00:00");
        assert_eq!(
            msg,
            format!("Fix it\n\nSigned-off-by: a <a@x>\n\nGit-Commit: {sha}\nGit-Date: 2026-01-01T00:00:00+00:00")
        );
        assert_eq!(git_commit_trailer(&msg), Some(sha));
        assert!(compose_message("  \n", sha, "d").starts_with("Git-Commit: "));
        assert_eq!(git_commit_trailer(&compose_message("", sha, "d")), Some(sha));
        // Only the final paragraph counts.
        assert_eq!(git_commit_trailer(&format!("Git-Commit: {sha}\n\nlater text")), None);
        assert_eq!(git_commit_trailer("Fix\n\nGit-Commit: not-a-sha"), None);
    }

    #[test]
    fn modes() {
        assert_eq!(file_kind(0o100644).unwrap(), Some(FileKind::File));
        assert_eq!(file_kind(0o100664).unwrap(), Some(FileKind::File));
        assert_eq!(file_kind(0o100755).unwrap(), Some(FileKind::Executable));
        assert_eq!(file_kind(0o120000).unwrap(), Some(FileKind::Symlink));
        assert_eq!(file_kind(0o160000).unwrap(), None);
        assert_eq!(file_kind(0).unwrap(), None);
        assert!(file_kind(0o040000).is_err());
    }

    #[test]
    fn paths() {
        assert!(repo_path(b"a/b.txt").is_ok());
        assert!(repo_path(b"a/.bud/x").is_err());
        assert!(repo_path(b"\xff.txt").is_err());
    }
}
