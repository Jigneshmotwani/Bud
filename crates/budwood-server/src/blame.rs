//! Line blame, as derived data.
//!
//! The blame of file `P` as of changeset `s` (where `s` changed `P`) is
//! derived from the blame of `P` as of the previous changeset that changed it,
//! found through `path_history`, plus a line diff between the two versions.
//! Values are memoized per `(path, seq)`, so the first blame of a file walks
//! its history once, and every later one costs a single diff per new version.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use budwood_core::api::{BlameLine, BlameResponse};
use budwood_core::{lookup, EntryKind, FileState, ObjectId, RepoPath};
use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffOp};

use crate::derived::{derive, Derivation};
use crate::service::{Service, ServiceError, ServiceResult};

/// Upper bound on the time spent diffing two versions of one file. Past it,
/// the diff is coarser (more lines are attributed to the newer version) but
/// still correct.
const DIFF_DEADLINE: Duration = Duration::from_secs(2);

/// Memoized blame of one version of a file: who introduced each line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum FileBlame {
    /// The changeset deleted the file.
    Absent,
    /// The file isn't text (contains NUL or isn't UTF-8).
    Binary,
    /// Line origins, run-length encoded as `[seq, count]` pairs in line order.
    Text { blob: ObjectId, runs: Vec<(u64, u32)> },
}

/// Blame of a file as of a changeset that changed it.
pub struct Blame;

#[derive(Clone, Debug)]
pub struct BlameKey {
    pub path: RepoPath,
    /// A changeset that changed `path`.
    pub seq: u64,
}

impl Derivation for Blame {
    const KIND: &'static str = "blame.v1";
    type Key = BlameKey;
    /// The file's state as of `seq`, or `None` if `seq` deleted it.
    type Inputs = Option<FileState>;
    type Value = FileBlame;

    fn encode_key(key: &BlameKey) -> String {
        // seq first: paths may contain any character except NUL.
        format!("{}:{}", key.seq, key.path.as_str())
    }

    fn inputs(svc: &Service, repo: &str, key: &BlameKey) -> ServiceResult<(Option<FileState>, Vec<BlameKey>)> {
        let state = svc.file_state_at(repo, key.seq, &key.path)?;
        let deps = match state {
            // A deleted file's blame doesn't depend on its earlier history.
            None => vec![],
            Some(_) => match key.seq.checked_sub(1) {
                Some(before) => svc
                    .meta
                    .last_touch(repo, &key.path, before)?
                    .map(|seq| BlameKey { path: key.path.clone(), seq })
                    .into_iter()
                    .collect(),
                None => vec![],
            },
        };
        Ok((state, deps))
    }

    fn compute(
        svc: &Service,
        _repo: &str,
        key: &BlameKey,
        state: Option<FileState>,
        deps: Vec<FileBlame>,
    ) -> ServiceResult<FileBlame> {
        let Some(state) = state else { return Ok(FileBlame::Absent) };
        let data = svc.load(&state.blob)?;
        let Some(new_text) = text(&data) else { return Ok(FileBlame::Binary) };
        let new_lines = split_lines(new_text);

        let origins = match deps.into_iter().next() {
            // Only the kind changed: every line keeps its origin.
            Some(FileBlame::Text { blob, runs }) if blob == state.blob => return Ok(FileBlame::Text { blob, runs }),
            Some(FileBlame::Text { blob, runs }) => {
                let old_data = svc.load(&blob)?;
                let old_text = text(&old_data).ok_or_else(|| {
                    ServiceError::Internal(anyhow::anyhow!("blob {blob} memoized as text is not text"))
                })?;
                let old_lines = split_lines(old_text);
                let old_origins = expand(&runs);
                if old_origins.len() != old_lines.len() {
                    return Err(ServiceError::Internal(anyhow::anyhow!("memoized blame of blob {blob} is corrupt")));
                }
                carry_over(&old_lines, &old_origins, &new_lines, key.seq)
            }
            // A new file, a re-added one, or one that was binary before.
            Some(FileBlame::Absent | FileBlame::Binary) | None => vec![key.seq; new_lines.len()],
        };
        Ok(FileBlame::Text { blob: state.blob, runs: compress(&origins) })
    }
}

/// The origin of each new line: carried over from the old version where the
/// diff says the line is unchanged, `seq` otherwise.
fn carry_over(old: &[&str], old_origins: &[u64], new: &[&str], seq: u64) -> Vec<u64> {
    let ops = similar::capture_diff_slices_deadline(Algorithm::Myers, old, new, Some(Instant::now() + DIFF_DEADLINE));
    let mut out = Vec::with_capacity(new.len());
    for op in ops {
        match op {
            DiffOp::Equal { old_index, len, .. } => out.extend_from_slice(&old_origins[old_index..old_index + len]),
            DiffOp::Delete { .. } => {}
            DiffOp::Insert { new_len, .. } | DiffOp::Replace { new_len, .. } => {
                out.extend(std::iter::repeat(seq).take(new_len))
            }
        }
    }
    debug_assert_eq!(out.len(), new.len());
    out
}

/// Text content, or `None` for binary data (NUL bytes or invalid UTF-8).
fn text(data: &[u8]) -> Option<&str> {
    if data.contains(&0) {
        return None;
    }
    std::str::from_utf8(data).ok()
}

/// Lines without their `\n` terminators. A final line without a terminator
/// counts as a line, so `a` and `a\n` both have one line.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n').unwrap_or(text).split('\n').collect()
}

fn compress(origins: &[u64]) -> Vec<(u64, u32)> {
    let mut runs: Vec<(u64, u32)> = Vec::new();
    for &seq in origins {
        match runs.last_mut() {
            Some((last, count)) if *last == seq && *count < u32::MAX => *count += 1,
            _ => runs.push((seq, 1)),
        }
    }
    runs
}

fn expand(runs: &[(u64, u32)]) -> Vec<u64> {
    runs.iter().flat_map(|&(seq, count)| std::iter::repeat(seq).take(count as usize)).collect()
}

impl Service {
    /// The state of file `path` in changeset `seq`'s tree, or `None` if it
    /// isn't a file there.
    fn file_state_at(&self, repo: &str, seq: u64, path: &RepoPath) -> ServiceResult<Option<FileState>> {
        let root = self
            .meta
            .root_tree_at(repo, seq)?
            .ok_or_else(|| ServiceError::Internal(anyhow::anyhow!("changeset {seq} of {repo:?} is missing")))?;
        let entry = lookup(&self.trees(), root, path).map_err(|e| ServiceError::Internal(e.into()))?;
        Ok(entry.and_then(|e| e.as_file()))
    }

    /// For each line of `path` at `rev`, the changeset that introduced it.
    pub fn blame(&self, repo: &str, rev: &str, path: &RepoPath) -> ServiceResult<BlameResponse> {
        let revision = self.resolve(repo, rev)?;
        if path.is_root() {
            return Err(ServiceError::BadRequest("blame needs a file path".into()));
        }
        let entry = lookup(&self.trees(), revision.root_tree, path)
            .map_err(|e| ServiceError::Internal(e.into()))?
            .ok_or_else(|| ServiceError::NotFound(format!("{path} does not exist at revision {}", revision.seq)))?;
        if entry.kind == EntryKind::Tree {
            return Err(ServiceError::BadRequest(format!("{path} is a directory")));
        }
        let seq = self.meta.last_touch(repo, path, revision.seq)?.ok_or_else(|| {
            ServiceError::Internal(anyhow::anyhow!("{path} exists at {} but has no history", revision.seq))
        })?;
        let runs = match derive::<Blame>(self, repo, &BlameKey { path: path.clone(), seq })? {
            FileBlame::Text { blob, runs } if blob == entry.id => runs,
            FileBlame::Binary => return Err(ServiceError::BadRequest(format!("{path} is a binary file"))),
            other => {
                return Err(ServiceError::Internal(anyhow::anyhow!(
                    "blame of {path} at {seq} doesn't match revision {}: {other:?}",
                    revision.seq
                )))
            }
        };

        let data = self.load(&entry.id)?;
        let text = text(&data).ok_or_else(|| ServiceError::BadRequest(format!("{path} is a binary file")))?;
        let mut summaries = HashMap::new();
        for &(seq, _) in &runs {
            if let std::collections::hash_map::Entry::Vacant(slot) = summaries.entry(seq) {
                let summary = self
                    .meta
                    .summary(repo, seq)?
                    .ok_or_else(|| ServiceError::Internal(anyhow::anyhow!("blame names missing changeset {seq}")))?;
                slot.insert(summary);
            }
        }
        let lines = split_lines(text)
            .into_iter()
            .zip(expand(&runs))
            .enumerate()
            .map(|(i, (text, seq))| {
                let summary = &summaries[&seq];
                BlameLine {
                    line_no: i as u64 + 1,
                    seq,
                    id: summary.id,
                    author: summary.author.clone(),
                    timestamp: summary.timestamp,
                    text: text.to_string(),
                }
            })
            .collect();
        Ok(BlameResponse { revision, path: path.clone(), lines })
    }
}

#[cfg(test)]
mod tests {
    use budwood_core::api::CommitRequest;
    use budwood_core::{Change, FileKind};

    use super::*;
    use crate::meta::MetaStore;
    use crate::store::MemObjectStore;

    fn service() -> Service {
        let svc =
            Service::new(std::sync::Arc::new(MemObjectStore::default()), MetaStore::open_in_memory().unwrap()).unwrap();
        svc.create_repo("r").unwrap();
        svc
    }

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn upload(svc: &Service, content: &[u8], kind: FileKind) -> FileState {
        let blob = ObjectId::for_blob(content);
        svc.put_blob("r", &blob, content).unwrap();
        FileState { kind, blob }
    }

    /// Commits on head as `author`; returns the new seq.
    fn commit(svc: &Service, author: &str, changes: Vec<(&str, Option<&str>)>) -> u64 {
        let base = svc.resolve("r", "head").unwrap().seq;
        let changes = changes
            .into_iter()
            .map(|(p, content)| match content {
                Some(c) => Change::set(path(p), upload(svc, c.as_bytes(), FileKind::File)),
                None => Change::delete(path(p)),
            })
            .collect();
        let req = CommitRequest { base, author: author.into(), message: format!("by {author}"), changes };
        svc.commit("r", req).unwrap().changeset.seq
    }

    /// `["seq:text", ...]`
    fn blame(svc: &Service, rev: &str, p: &str) -> Vec<String> {
        let resp = svc.blame("r", rev, &path(p)).unwrap();
        for (i, line) in resp.lines.iter().enumerate() {
            assert_eq!(line.line_no, i as u64 + 1);
        }
        resp.lines.iter().map(|l| format!("{}:{}", l.seq, l.text)).collect()
    }

    #[test]
    fn helpers() {
        assert_eq!(split_lines(""), Vec::<&str>::new());
        assert_eq!(split_lines("a"), vec!["a"]);
        assert_eq!(split_lines("a\n"), vec!["a"]);
        assert_eq!(split_lines("a\n\nb\r\n"), vec!["a", "", "b\r"]);
        assert_eq!(split_lines("\n"), vec![""]);
        assert_eq!(compress(&[1, 1, 2, 1]), vec![(1, 2), (2, 1), (1, 1)]);
        assert_eq!(expand(&compress(&[3, 3, 3, 5])), vec![3, 3, 3, 5]);
        assert!(text(b"a\0b").is_none() && text(&[0xff, b'a']).is_none() && text(b"ok").is_some());
    }

    #[test]
    fn multi_revision_history() {
        let svc = service();
        let r1 = commit(&svc, "alice", vec![("f.txt", Some("one\ntwo\nthree\n"))]);
        let r2 = commit(&svc, "bob", vec![("other", Some("x"))]);
        let r3 = commit(&svc, "bob", vec![("f.txt", Some("zero\none\ntwo\nTHREE\n"))]);
        let r4 = commit(&svc, "carol", vec![("f.txt", Some("zero\ntwo\nTHREE\nfour"))]);
        assert_eq!((r1, r2, r3, r4), (1, 2, 3, 4));

        assert_eq!(blame(&svc, "1", "f.txt"), ["1:one", "1:two", "1:three"]);
        // Revision 2 didn't touch the file: same blame as revision 1.
        assert_eq!(blame(&svc, "2", "f.txt"), ["1:one", "1:two", "1:three"]);
        assert_eq!(blame(&svc, "3", "f.txt"), ["3:zero", "1:one", "1:two", "3:THREE"]);
        assert_eq!(blame(&svc, "head", "f.txt"), ["3:zero", "1:two", "3:THREE", "4:four"]);

        let resp = svc.blame("r", "4", &path("f.txt")).unwrap();
        let cs1 = svc.changeset("r", "1").unwrap();
        assert_eq!(resp.lines[1].id, cs1.id);
        assert_eq!(resp.lines[1].author, "alice");
        assert_eq!(resp.lines[1].timestamp, cs1.timestamp);
        assert_eq!(resp.lines[3].author, "carol");
        assert_eq!(resp.revision.seq, 4);
    }

    #[test]
    fn deletes_and_re_adds_restart_history() {
        let svc = service();
        commit(&svc, "a", vec![("dir/f", Some("keep\nlines\n"))]);
        commit(&svc, "b", vec![("dir/f", Some("keep\nlines\nmore\n"))]);
        commit(&svc, "c", vec![("dir/f", None), ("dir/g", Some("g"))]);
        assert!(matches!(svc.blame("r", "3", &path("dir/f")), Err(ServiceError::NotFound(_))));
        // Re-adding identical content still attributes every line to the re-add.
        commit(&svc, "d", vec![("dir/f", Some("keep\nlines\nmore\n"))]);
        assert_eq!(blame(&svc, "4", "dir/f"), ["4:keep", "4:lines", "4:more"]);
        commit(&svc, "e", vec![("dir/f", Some("keep\nlines\nmore\nend\n"))]);
        assert_eq!(blame(&svc, "5", "dir/f"), ["4:keep", "4:lines", "4:more", "5:end"]);
        // Older revisions are still answered.
        assert_eq!(blame(&svc, "2", "dir/f"), ["1:keep", "1:lines", "2:more"]);

        // The file becomes a directory and then a file again.
        commit(&svc, "f", vec![("dir/f", None)]);
        commit(&svc, "g", vec![("dir/f/inner", Some("x\n"))]);
        assert!(matches!(svc.blame("r", "7", &path("dir/f")), Err(ServiceError::BadRequest(_))));
        commit(&svc, "h", vec![("dir/f/inner", None), ("dir/f", Some("keep\n"))]);
        assert_eq!(blame(&svc, "8", "dir/f"), ["8:keep"]);
    }

    #[test]
    fn moved_lines() {
        let svc = service();
        commit(&svc, "a", vec![("f", Some("a\nb\nc\nd\n"))]);
        commit(&svc, "b", vec![("f", Some("c\nd\na\nb\n"))]);
        // A diff keeps the longest common subsequence; the moved block is new.
        let lines = blame(&svc, "2", "f");
        assert_eq!(lines.len(), 4);
        assert_eq!(lines.iter().filter(|l| l.starts_with("1:")).count(), 2, "{lines:?}");
        assert_eq!(lines.iter().filter(|l| l.starts_with("2:")).count(), 2, "{lines:?}");
        // Swapping a single line keeps the rest.
        commit(&svc, "c", vec![("f", Some("c\nd\nb\na\n"))]);
        let lines = blame(&svc, "3", "f");
        assert_eq!(lines[..2], blame(&svc, "2", "f")[..2]);
        assert_eq!(lines.iter().filter(|l| l.starts_with("3:")).count(), 1, "{lines:?}");
    }

    #[test]
    fn memoized_and_incremental() {
        let svc = service();
        for i in 1..=20 {
            let content: String = (1..=i).map(|n| format!("line {n}\n")).collect();
            commit(&svc, "a", vec![("f", Some(&content)), (&format!("noise{i}"), Some("n"))]);
        }
        assert_eq!(svc.derivations(), 0, "blame is lazy");
        let lines = blame(&svc, "head", "f");
        assert_eq!(lines.len(), 20);
        assert_eq!(lines[6], "7:line 7");
        // One value per version of the file, nothing for unrelated paths.
        assert_eq!(svc.derivations(), 20);
        assert_eq!(svc.meta.derived_count("r", Blame::KIND).unwrap(), 20);

        // Memo hits: repeated and older queries compute nothing.
        assert_eq!(blame(&svc, "head", "f"), lines);
        assert_eq!(blame(&svc, "10", "f").len(), 10);
        assert_eq!(svc.derivations(), 20);

        // A new version costs one derivation, independent of history length.
        commit(&svc, "b", vec![("f", Some("line 1\nnew\n"))]);
        assert_eq!(blame(&svc, "head", "f"), ["1:line 1", "21:new"]);
        assert_eq!(svc.derivations(), 21);
    }

    #[test]
    fn kind_changes_keep_origins() {
        let svc = service();
        commit(&svc, "a", vec![("run.sh", Some("#!/bin/sh\necho hi\n"))]);
        let base = svc.resolve("r", "head").unwrap().seq;
        let state = upload(&svc, b"#!/bin/sh\necho hi\n", FileKind::Executable);
        let req = CommitRequest {
            base,
            author: "b".into(),
            message: "chmod".into(),
            changes: vec![Change::set(path("run.sh"), state)],
        };
        svc.commit("r", req).unwrap();
        assert_eq!(blame(&svc, "2", "run.sh"), ["1:#!/bin/sh", "1:echo hi"]);
    }

    #[test]
    fn binary_and_bad_requests() {
        let svc = service();
        let bin = upload(&svc, b"\x00\x01binary", FileKind::File);
        let latin1 = upload(&svc, b"caf\xe9\n", FileKind::File);
        let req = CommitRequest {
            base: 0,
            author: "a".into(),
            message: "m".into(),
            changes: vec![Change::set(path("bin"), bin), Change::set(path("latin1"), latin1)],
        };
        svc.commit("r", req).unwrap();
        for p in ["bin", "latin1"] {
            match svc.blame("r", "head", &path(p)) {
                Err(ServiceError::BadRequest(m)) => assert!(m.contains("binary"), "{m}"),
                other => panic!("{other:?}"),
            }
        }
        // A binary file that becomes text starts a fresh history.
        commit(&svc, "b", vec![("bin", Some("now text\n"))]);
        assert_eq!(blame(&svc, "head", "bin"), ["2:now text"]);

        commit(&svc, "c", vec![("empty", Some(""))]);
        assert!(svc.blame("r", "head", &path("empty")).unwrap().lines.is_empty());
        assert!(matches!(svc.blame("r", "head", &RepoPath::root()), Err(ServiceError::BadRequest(_))));
        assert!(matches!(svc.blame("r", "head", &path("nope")), Err(ServiceError::NotFound(_))));
        assert!(matches!(svc.blame("r", "0", &path("bin")), Err(ServiceError::NotFound(_))));
        assert!(matches!(svc.blame("nope", "head", &path("bin")), Err(ServiceError::NotFound(_))));
    }

    #[test]
    fn long_file_history_is_iterative() {
        let svc = service();
        for i in 0..1500 {
            commit(&svc, "a", vec![("f", Some(&format!("v{i}\nstable\n")))]);
        }
        assert_eq!(blame(&svc, "head", "f"), ["1500:v1499", "1:stable"]);
    }
}
