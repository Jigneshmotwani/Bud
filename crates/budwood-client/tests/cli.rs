//! Smoke test of the `bud` binary against an in-process server.

mod common;

use std::path::Path;
use std::process::Command;

use common::*;

fn bud(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_bud"))
        .args(args)
        .current_dir(cwd)
        .env_remove("BUD_SERVER")
        .env_remove("BUD_REPO")
        .env_remove("BUD_AUTHOR")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "bud {args:?} failed:\n{stdout}{stderr}");
    stdout
}

fn bud_fails(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_bud"))
        .args(args)
        .current_dir(cwd)
        .env_remove("BUD_SERVER")
        .env_remove("BUD_REPO")
        .output()
        .unwrap();
    assert!(!out.status.success(), "bud {args:?} unexpectedly succeeded");
    String::from_utf8(out.stderr).unwrap()
}

#[test]
fn cli_smoke() {
    let server = TestServer::start();
    let url = server.url.as_str();
    let tmp = tempfile::tempdir().unwrap();
    let top = tmp.path();

    assert_eq!(bud(top, &["--server", url, "create-repo", "demo"]), "created demo\n");
    assert_eq!(bud(top, &["--server", url, "create-repo", "demo"]), "already exists: demo\n");
    assert_eq!(bud(top, &["--server", url, "repos"]), "demo\n");

    let out = bud(top, &["clone", url, "demo", "ws"]);
    assert!(out.starts_with("cloned demo at revision 0"), "{out}");
    let ws = top.join("ws");
    assert!(bud(&ws, &["status"]).contains("no local changes"));

    write(&ws, "hello.txt", "hello, world\n");
    write(&ws, "src/lib.rs", "pub fn lib() {}\n");
    let out = bud(&ws, &["status"]);
    assert!(out.starts_with("demo at revision 0 (/)\n"), "{out}");
    assert!(out.contains("A hello.txt\n") && out.contains("A src/lib.rs\n"), "{out}");
    let diff = bud(&ws, &["diff"]);
    assert!(diff.contains("+++ b/hello.txt\n") && diff.contains("+hello, world\n"), "{diff}");

    let out = bud(&ws, &["commit", "-m", "first commit", "--author", "alice"]);
    assert!(out.starts_with("committed revision 1 (") && out.ends_with("), 2 file(s)\n"), "{out}");
    assert!(bud(&ws, &["status"]).contains("no local changes"));
    assert!(bud_fails(&ws, &["commit", "-m", "empty", "--author", "alice"]).contains("nothing to commit"));

    write(&ws, "src/lib.rs", "pub fn lib() -> u32 { 2 }\n");
    // Paths are relative to the current directory inside a workspace.
    bud(&ws.join("src"), &["commit", "-m", "second\n\nbody text", "--author", "bob", "lib.rs"]);

    let log = bud(&ws, &["log"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(
        lines[0].trim_start().starts_with("2 ") && lines[0].contains("bob") && lines[0].ends_with("second"),
        "{log}"
    );
    assert!(lines[1].trim_start().starts_with("1 ") && lines[1].ends_with("first commit"), "{log}");
    assert_eq!(bud(&ws, &["log", "hello.txt"]).lines().count(), 1);
    assert_eq!(bud(&ws.join("src"), &["log"]).lines().count(), 2);
    assert_eq!(bud(&ws, &["log", "-n", "1"]).lines().count(), 1);

    assert_eq!(bud(&ws, &["cat", "hello.txt"]), "hello, world\n");
    assert_eq!(bud(&ws, &["cat", "src/lib.rs", "--rev", "1"]), "pub fn lib() {}\n");
    assert_eq!(bud(&ws.join("src"), &["cat", "lib.rs"]), "pub fn lib() -> u32 { 2 }\n");
    assert_eq!(bud(&ws, &["ls"]), "hello.txt\nsrc/\n");
    assert_eq!(bud(&ws, &["ls", "src"]), "lib.rs\n");
    assert_eq!(bud(&ws, &["ls", "hello.txt"]), "hello.txt\n");

    let show = bud(&ws, &["show", "2"]);
    assert!(show.starts_with("revision 2  "), "{show}");
    assert!(show.contains("author   bob\n"), "{show}");
    assert!(
        show.contains("    second\n    \n    body text\n") || show.contains("    second\n\n    body text\n"),
        "{show}"
    );
    assert!(show.ends_with("+ src/lib.rs\n"), "{show}");
    assert!(!show.contains("rebased"), "{show}");

    // Read-only commands work outside a workspace with --server and --repo.
    assert_eq!(bud(top, &["--server", url, "--repo", "demo", "cat", "hello.txt"]), "hello, world\n");
    assert_eq!(bud(top, &["--server", url, "--repo", "demo", "log"]).lines().count(), 2);
    assert!(bud_fails(top, &["status"]).contains("not inside a bud workspace"));

    // A second, sparse clone; sync, sparse add and revert.
    bud(top, &["clone", url, "demo", "sparse", "--sparse", "src"]);
    let sparse = top.join("sparse");
    assert_eq!(files(&sparse), ["src/lib.rs"]);
    assert_eq!(bud(&sparse, &["sparse", "list"]), "src\n");
    write(&ws, "hello.txt", "changed\n");
    bud(&ws, &["commit", "-m", "third", "--author", "alice"]);
    assert_eq!(bud(&sparse, &["sync"]), "updated 2 -> 3, 0 file(s) changed\n");
    assert_eq!(bud(&sparse, &["sparse", "add", "hello.txt"]), "checked out 1 file(s) under hello.txt\n");
    assert_eq!(read(&sparse, "hello.txt"), "changed\n");
    write(&sparse, "src/lib.rs", "scratch\n");
    assert_eq!(bud(&sparse.join("src"), &["revert", "."]), "reverted src/lib.rs\n");
    assert_eq!(read(&sparse, "src/lib.rs"), "pub fn lib() -> u32 { 2 }\n");
    assert_eq!(bud(&sparse, &["sync"]), "already at revision 3\n");

    // A conflicting commit fails with a helpful message.
    write(&ws, "hello.txt", "from ws\n");
    write(&sparse, "hello.txt", "from sparse\n");
    bud(&ws, &["commit", "-m", "ws wins", "--author", "alice"]);
    let err = bud_fails(&sparse, &["commit", "-m", "loses", "--author", "bob"]);
    assert!(err.contains("commit rejected") && err.contains("bud sync"), "{err}");
    let err = bud_fails(&sparse, &["sync"]);
    assert!(err.contains("conflict") && err.contains("hello.txt"), "{err}");
}
