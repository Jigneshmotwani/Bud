//! Blame, server-side revert and the related CLI commands, end to end against
//! an in-process server. Everything except the workspace checks goes through
//! the API only, the way a bisect or revert pipeline would use it.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use budwood_client::{ApiError, Remote};
use budwood_core::api::{codes, CommitRequest};
use budwood_core::{Change, FileKind, FileState, ObjectId};
use common::*;

fn upload(remote: &Remote, content: &str) -> FileState {
    let blob = ObjectId::for_blob(content.as_bytes());
    if !remote.missing(&[blob]).unwrap().is_empty() {
        remote.put_blob(&blob, content.as_bytes().to_vec()).unwrap();
    }
    FileState { kind: FileKind::File, blob }
}

/// Commits on head without a working copy; returns the new seq.
fn commit(remote: &Remote, author: &str, message: &str, files: &[(&str, Option<&str>)]) -> u64 {
    let changes = files
        .iter()
        .map(|(p, content)| match content {
            Some(c) => Change::set(path(p), upload(remote, c)),
            None => Change::delete(path(p)),
        })
        .collect();
    let base = remote.head().unwrap().seq;
    let req = CommitRequest { base, author: author.into(), message: message.into(), changes };
    remote.commit(&req).unwrap().changeset.seq
}

fn bud_output(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bud"))
        .args(args)
        .current_dir(cwd)
        .env_remove("BUD_SERVER")
        .env_remove("BUD_REPO")
        .env("BUD_AUTHOR", "cli-bot")
        .output()
        .unwrap()
}

fn bud(cwd: &Path, args: &[&str]) -> String {
    let out = bud_output(cwd, args);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "bud {args:?} failed:\n{stdout}{}", String::from_utf8_lossy(&out.stderr));
    stdout
}

fn api_error(err: anyhow::Error) -> ApiError {
    err.downcast::<ApiError>().expect("an API error")
}

#[test]
fn blame_and_revert_through_the_api() {
    let server = TestServer::with_repo("demo");
    let remote = server.remote("demo");

    commit(&remote, "alice", "Add parser", &[("src/parse.rs", Some("fn parse() {\n    todo!()\n}\n"))]);
    commit(&remote, "bob", "Add docs", &[("README.md", Some("# demo\n"))]);
    let bad = commit(&remote, "carol", "Break parser", &[("src/parse.rs", Some("fn parse() {\n    panic!()\n}\n"))]);
    commit(&remote, "dave", "Extend docs", &[("README.md", Some("# demo\n\nMore.\n"))]);

    let blame = remote.blame("head", &path("src/parse.rs")).unwrap();
    assert_eq!(blame.revision.seq, 4);
    let lines: Vec<_> = blame.lines.iter().map(|l| (l.line_no, l.seq, l.author.as_str(), l.text.as_str())).collect();
    assert_eq!(lines, [(1, 1, "alice", "fn parse() {"), (2, 3, "carol", "    panic!()"), (3, 1, "alice", "}")]);
    let culprit = remote.changeset(&blame.lines[1].seq.to_string()).unwrap();
    assert_eq!(culprit.id, blame.lines[1].id);
    assert_eq!(remote.blame("1", &path("src/parse.rs")).unwrap().lines[1].text, "    todo!()");

    // Back out the culprit: it lands on head, rebased over the docs change.
    let resp = remote.revert(&bad.to_string(), "revert-bot", None).unwrap();
    assert!(resp.rebased);
    let cs = resp.changeset;
    assert_eq!((cs.seq, cs.base, cs.author.as_str()), (5, 3, "revert-bot"));
    assert_eq!(cs.message, format!("Revert \"Break parser\"\n\nThis reverts revision 3 ({}).", culprit.id));
    assert_eq!(remote.file("head", &path("src/parse.rs")).unwrap(), b"fn parse() {\n    todo!()\n}\n");
    assert_eq!(remote.file("head", &path("README.md")).unwrap(), b"# demo\n\nMore.\n");
    // Blame now attributes the restored line to the revert.
    let blame = remote.blame("head", &path("src/parse.rs")).unwrap();
    assert_eq!(blame.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 5, 1]);

    // The same revert again now conflicts with the revert itself.
    let err = api_error(remote.revert(&bad.to_string(), "revert-bot", None).unwrap_err());
    assert_eq!(err.status, 409);
    assert!(err.is_conflict());
    assert_eq!(err.body.paths, vec![path("src/parse.rs")]);

    // Reverting a file's addition deletes it.
    let added = commit(&remote, "erin", "Add tmp", &[("tmp/scratch.txt", Some("x\n"))]);
    let cs = remote.revert(&added.to_string(), "revert-bot", Some("Drop scratch")).unwrap().changeset;
    assert_eq!(cs.message, "Drop scratch");
    assert_eq!(cs.changes, vec![Change::delete(path("tmp/scratch.txt"))]);
    assert!(api_error(remote.blame("head", &path("tmp/scratch.txt")).unwrap_err()).is_not_found());

    // Errors are JSON with the documented codes.
    let bin = ObjectId::for_blob(b"\0\x01");
    remote.put_blob(&bin, b"\0\x01".to_vec()).unwrap();
    let req = CommitRequest {
        base: remote.head().unwrap().seq,
        author: "f".into(),
        message: "binary".into(),
        changes: vec![Change::set(path("blob.bin"), FileState { kind: FileKind::File, blob: bin })],
    };
    remote.commit(&req).unwrap();
    let err = api_error(remote.blame("head", &path("blob.bin")).unwrap_err());
    assert_eq!((err.status, err.body.code.as_str()), (400, codes::BAD_REQUEST));
    let err = api_error(remote.blame("head", &path("src")).unwrap_err());
    assert_eq!(err.status, 400);
    let err = api_error(remote.revert("0", "x", None).unwrap_err());
    assert_eq!((err.status, err.body.code.as_str()), (404, codes::NOT_FOUND));
    assert_eq!(remote.resolve("00").unwrap().seq, 0);
}

#[test]
fn blame_backout_and_remote_diff_from_the_cli() {
    let server = TestServer::with_repo("demo");
    let remote = server.remote("demo");
    let tmp = tempfile::tempdir().unwrap();
    let top = tmp.path();
    let url = server.url.as_str();

    commit(&remote, "alice", "one", &[("lib/a.txt", Some("first\nsecond\n"))]);
    commit(&remote, "bob", "two", &[("lib/a.txt", Some("first\nchanged\n")), ("b.txt", Some("b\n"))]);
    commit(&remote, "carol", "three", &[("c.txt", Some("c\n"))]);

    // Without a workspace, paths are repository paths.
    let out = bud(top, &["--server", url, "--repo", "demo", "blame", "lib/a.txt"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[0].trim_start().starts_with("1 ") && lines[0].contains("alice") && lines[0].ends_with("1| first"));
    assert!(lines[1].trim_start().starts_with("2 ") && lines[1].contains("bob") && lines[1].ends_with("2| changed"));
    let out = bud(top, &["--server", url, "--repo", "demo", "blame", "--rev", "1", "lib/a.txt"]);
    assert!(out.ends_with("2| second\n"), "{out}");

    let out = bud(top, &["--server", url, "--repo", "demo", "remote-diff", "1"]);
    assert_eq!(out, "+ b.txt\n+ c.txt\n+ lib/a.txt\n");
    let out = bud(top, &["--server", url, "--repo", "demo", "remote-diff", "1", "2"]);
    assert_eq!(out, "+ b.txt\n+ lib/a.txt\n");
    let out = bud(top, &["--server", url, "--repo", "demo", "remote-diff", "3", "1"]);
    assert_eq!(out, "- b.txt\n- c.txt\n+ lib/a.txt\n");
    let out = bud(top, &["--server", url, "--repo", "demo", "remote-diff", "3", "1", "lib"]);
    assert_eq!(out, "+ lib/a.txt\n");

    let out = bud(top, &["--server", url, "--repo", "demo", "log", "--before", "3"]);
    let seqs: Vec<&str> = out.lines().map(|l| l.split_whitespace().next().unwrap()).collect();
    assert_eq!(seqs, ["2", "1"]);
    let out = bud(top, &["--server", url, "--repo", "demo", "log", "--before", "3", "-n", "1"]);
    assert_eq!(out.lines().count(), 1);

    // Inside a workspace, paths are relative to the current directory.
    bud(top, &["clone", url, "demo", "ws"]);
    let ws = top.join("ws");
    let lib = ws.join("lib");
    let out = bud(&lib, &["blame", "a.txt"]);
    assert!(out.contains("| changed"), "{out}");
    let out = bud(&lib, &["remote-diff", "2", "3", "."]);
    assert_eq!(out, "");

    let out = bud(&ws, &["backout", "2"]);
    assert!(out.starts_with("committed revision 4 ("), "{out}");
    assert!(out.contains("backing out revision 2; 2 file(s)"), "{out}");
    assert!(out.contains("bud sync"), "{out}");
    let cs = remote.changeset("4").unwrap();
    assert_eq!(cs.author, "cli-bot");
    assert!(cs.message.starts_with("Revert \"two\""));
    // Revision 4 touched the same files, so backing out 2 again conflicts.
    let out = bud_output(&ws, &["backout", "2", "-m", "again"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("changed after revision 2"), "{stderr}");

    bud(&ws, &["sync"]);
    assert_eq!(read(&ws, "lib/a.txt"), "first\nsecond\n");
    assert!(!exists(&ws, "b.txt"));
}
