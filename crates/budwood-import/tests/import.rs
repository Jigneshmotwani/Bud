//! End-to-end tests: scripted git repositories imported into a real server.

mod common;

use std::os::unix::ffi::OsStrExt;
use std::process::Command;

use budwood_client::Remote;
use budwood_core::api::CommitRequest;
use budwood_core::{Change, Changeset, EntryKind, FileKind, FileState, ObjectId, RepoPath};
use budwood_import::{git_commit_trailer, import, verify, Options, Summary};
use common::{path, GitRepo, TestServer};

fn run(remote: &Remote, git: &GitRepo, configure: impl FnOnce(&mut Options)) -> anyhow::Result<Summary> {
    let mut opts = Options::new(git.path());
    opts.verify = true;
    configure(&mut opts);
    import(remote, &opts)
}

/// The whole Budwood log, oldest first.
fn history(remote: &Remote) -> Vec<Changeset> {
    let mut log = remote.log(&RepoPath::root(), None, 1000).unwrap();
    log.reverse();
    log
}

/// A changeset's changes as `["+path kind", "-path"]`.
fn changes(cs: &Changeset) -> Vec<String> {
    cs.changes
        .iter()
        .map(|c| match &c.new {
            Some(state) => format!("+{} {:?}", c.path, state.kind),
            None => format!("-{}", c.path),
        })
        .collect()
}

fn kind_at(remote: &Remote, p: &str) -> EntryKind {
    remote.tree("head", &path(p)).unwrap().kind
}

#[test]
fn imports_every_kind_of_change() {
    let server = TestServer::start();
    let remote = server.repo("demo");
    let git = GitRepo::new();

    git.write("a.txt", "one\n");
    git.write("dir/b.txt", "bee\n");
    git.write("run.sh", "#!/bin/sh\n");
    git.set_executable("run.sh", true);
    git.symlink("a.txt", "link");
    git.write("x", "file x\n");
    let c1 = git.commit("Initial commit\n\nWith a body.");

    git.write("a.txt", "two\n");
    git.set_executable("dir/b.txt", true);
    git.set_executable("run.sh", false);
    git.commit("Modify, chmod");

    // File to directory, and a deleted symlink.
    git.remove("x");
    git.write("x/y", "now a dir\n");
    git.remove("link");
    git.commit("x becomes a directory");

    // Directory to file.
    git.remove("dir");
    git.write("dir", "now a file\n");
    git.commit("dir becomes a file");

    git.commit("An empty commit");

    // Unrepresentable names are skipped; the rest of the commit is imported.
    git.write(".bud/config", "reserved\n");
    git.write("nested/.bud", "reserved\n");
    let bad_name = std::ffi::OsStr::from_bytes(b"latin1-\xe9.txt");
    std::fs::write(git.path().join(bad_name), "x").unwrap();
    git.write("ok.txt", "fine\n");
    git.commit("Odd names");

    // A submodule on its own is an empty changeset; replacing a file with one deletes the file.
    git.git(&["update-index", "--add", "--cacheinfo", &format!("160000,{c1},sub")]);
    git.commit_index("Add a submodule");
    git.git(&["rm", "-q", "--cached", "ok.txt"]);
    git.git(&["update-index", "--add", "--cacheinfo", &format!("160000,{c1},ok.txt")]);
    let last = git.commit_index("Replace ok.txt with a submodule");

    let summary = run(&remote, &git, |_| {}).unwrap();
    assert_eq!(summary.history_len, 8);
    assert_eq!(summary.imported, 6, "{summary}");
    assert_eq!(summary.empty_skipped, 2);
    assert_eq!(summary.paths_skipped, 3);
    assert_eq!(summary.gitlinks_skipped, 2);
    assert_eq!(summary.head_seq, 6);
    assert_eq!(summary.remaining, 0);
    assert_eq!(summary.synced_to.as_deref(), Some(last.as_str()));
    assert_eq!(summary.verified_files, Some(4)); // a.txt, dir, run.sh, x/y

    let log = history(&remote);
    assert_eq!(log.len(), 6);
    assert_eq!(changes(&log[0]), ["+a.txt File", "+dir/b.txt File", "+link Symlink", "+run.sh Executable", "+x File"]);
    assert_eq!(changes(&log[1]), ["+a.txt File", "+dir/b.txt Executable", "+run.sh File"]);
    assert_eq!(changes(&log[2]), ["-link", "-x", "+x/y File"]);
    assert_eq!(changes(&log[3]), ["+dir File", "-dir/b.txt"]);
    assert_eq!(changes(&log[4]), ["+ok.txt File"]);
    assert_eq!(changes(&log[5]), ["-ok.txt"]);

    // Messages carry the git message, id and author date; authors are "Name <email>".
    let date = git.git(&["log", "-1", "--format=%aI", &c1]);
    assert!(date.ends_with("+02:00"), "{date}");
    assert_eq!(log[0].message, format!("Initial commit\n\nWith a body.\n\nGit-Commit: {c1}\nGit-Date: {date}"));
    assert_eq!(git_commit_trailer(&log[0].message), Some(c1.as_str()));
    assert_eq!(log[0].author, "t <t@x>");

    // File contents and kinds at head.
    assert_eq!(remote.file("head", &path("a.txt")).unwrap(), b"two\n");
    assert_eq!(remote.file("1", &path("link")).unwrap(), b"a.txt");
    assert_eq!(kind_at(&remote, "x"), EntryKind::Tree);
    assert_eq!(kind_at(&remote, "dir"), EntryKind::File);
    assert_eq!(kind_at(&remote, "run.sh"), EntryKind::File);
    let root: Vec<String> =
        remote.tree("head", &RepoPath::root()).unwrap().entries.into_iter().map(|e| e.name).collect();
    assert_eq!(root, ["a.txt", "dir", "run.sh", "x"]);
}

#[test]
fn merges_import_their_first_parent_diff() {
    let server = TestServer::start();
    let remote = server.repo("demo");
    let git = GitRepo::new();
    git.write("a", "a\n");
    git.commit("base");
    git.git(&["checkout", "-q", "-b", "feature"]);
    git.write("f1", "f1\n");
    git.commit("feature 1");
    git.write("f2", "f2\n");
    git.write("a", "changed on feature\n");
    git.commit("feature 2");
    git.git(&["checkout", "-q", "main"]);
    git.write("m", "m\n");
    git.commit("main work");
    git.git(&["merge", "-q", "--no-ff", "-m", "Merge feature", "feature"]);
    let merge = git.head();

    let summary = run(&remote, &git, |_| {}).unwrap();
    assert_eq!(summary.history_len, 3, "side-branch commits are not imported on their own");
    assert_eq!(summary.imported, 3);
    assert_eq!(summary.verified_files, Some(4));
    let log = history(&remote);
    assert_eq!(changes(&log[2]), ["+a File", "+f1 File", "+f2 File"]);
    assert!(log[2].message.starts_with("Merge feature\n\nGit-Commit: "));
    assert_eq!(git_commit_trailer(&log[2].message), Some(merge.as_str()));

    // Importing a side branch's ref follows its own first-parent chain.
    let other = server.repo("feature");
    let summary = run(&other, &git, |o| o.rev = "feature".into()).unwrap();
    assert_eq!((summary.history_len, summary.imported), (3, 3));
}

#[test]
fn resumes_where_the_last_import_stopped() {
    let server = TestServer::start();
    let remote = server.repo("demo");
    let git = GitRepo::new();
    let mut commits = Vec::new();
    for i in 0..5 {
        git.write(&format!("f{i}"), &format!("{i}\n"));
        git.write("shared", &format!("v{i}\n"));
        commits.push(git.commit(&format!("commit {i}")));
    }
    git.commit("trailing empty commit");

    let first = run(&remote, &git, |o| o.limit = Some(2)).unwrap();
    assert_eq!((first.imported, first.remaining, first.head_seq), (2, 4, 2));
    assert_eq!(first.resumed_after, None);
    assert_eq!(first.verified_files, Some(3), "verifies against the last processed commit");
    assert_eq!(first.blobs_uploaded, 4);

    let dry = run(&remote, &git, |o| o.dry_run = true).unwrap();
    assert_eq!((dry.imported, dry.empty_skipped, dry.head_seq), (3, 1, 2));
    assert_eq!(remote.head().unwrap().seq, 2, "a dry run commits nothing");

    let second = run(&remote, &git, |_| {}).unwrap();
    assert_eq!(second.resumed_after.as_deref(), Some(commits[1].as_str()));
    assert_eq!((second.imported, second.empty_skipped, second.remaining, second.head_seq), (3, 1, 0, 5));
    assert_eq!(second.verified_files, Some(6));

    // Nothing new: the trailing empty commit is looked at again, and skipped.
    let third = run(&remote, &git, |_| {}).unwrap();
    assert_eq!((third.imported, third.head_seq), (0, 5));
    assert_eq!(third.resumed_after.as_deref(), Some(commits[4].as_str()));

    git.write("f0", "changed\n");
    let newest = git.commit("one more");
    let fourth = run(&remote, &git, |_| {}).unwrap();
    assert_eq!((fourth.imported, fourth.head_seq), (1, 6));
    assert_eq!(fourth.synced_to.as_deref(), Some(newest.as_str()));
    verify(&remote, git.path(), "HEAD").unwrap();
}

#[test]
fn refuses_to_import_over_foreign_history_without_force() {
    let server = TestServer::start();
    let git = GitRepo::new();
    git.write("a", "a\n");
    git.commit("one");

    // A repository with a changeset that wasn't imported.
    let remote = server.repo("demo");
    let blob = ObjectId::for_blob(b"hi\n");
    remote.put_blob(&blob, b"hi\n".to_vec()).unwrap();
    let state = FileState { kind: FileKind::File, blob };
    let req = CommitRequest {
        base: 0,
        author: "alice".into(),
        message: "hand-made".into(),
        changes: vec![Change::set(path("hello"), state)],
    };
    remote.commit(&req).unwrap();

    let err = run(&remote, &git, |_| {}).unwrap_err();
    assert!(format!("{err:#}").contains("--force"), "{err:#}");
    let summary = run(&remote, &git, |o| {
        o.force = true;
        o.verify = false;
    })
    .unwrap();
    assert_eq!((summary.imported, summary.head_seq), (1, 2));
    // Budwood now has an extra file, so verification reports it.
    let err = verify(&remote, git.path(), "HEAD").unwrap_err();
    assert!(format!("{err:#}").contains("not in git: hello"), "{err:#}");

    // A non-import changeset after the last import is refused too.
    let req = CommitRequest {
        base: 2,
        author: "alice".into(),
        message: "more by hand".into(),
        changes: vec![Change::set(path("hello2"), state)],
    };
    remote.commit(&req).unwrap();
    git.write("b", "b\n");
    git.commit("two");
    let err = run(&remote, &git, |_| {}).unwrap_err();
    assert!(format!("{err:#}").contains("1 changeset(s) after the last import"), "{err:#}");
    let summary = run(&remote, &git, |o| {
        o.force = true;
        o.verify = false;
    })
    .unwrap();
    assert_eq!((summary.imported, summary.head_seq), (1, 4));
}

#[test]
fn rewritten_git_history_needs_force_and_still_converges() {
    let server = TestServer::start();
    let remote = server.repo("demo");
    let git = GitRepo::new();
    git.write("a", "a\n");
    let c1 = git.commit("one");
    git.write("b", "b\n");
    git.commit("two");
    run(&remote, &git, |_| {}).unwrap();

    // Rewrite: drop "two" and commit something else instead.
    git.git(&["reset", "-q", "--hard", &c1]);
    git.write("c", "c\n");
    git.commit("three");
    let err = run(&remote, &git, |_| {}).unwrap_err();
    assert!(format!("{err:#}").contains("not in the first-parent history"), "{err:#}");

    let summary = run(&remote, &git, |o| o.force = true).unwrap();
    assert_eq!((summary.imported, summary.head_seq), (1, 3));
    assert_eq!(summary.verified_files, Some(2));
    assert_eq!(changes(&history(&remote)[2]), ["-b", "+c File"]);
}

#[test]
fn command_line() {
    let server = TestServer::start();
    let git = GitRepo::new();
    git.write("hello.txt", "hello\n");
    git.commit("hello");
    git.write("hello.txt", "hello again\n");
    git.commit("again");

    let bin = env!("CARGO_BIN_EXE_budwood-import");
    let cmd = |extra: &[&str]| {
        let out = Command::new(bin)
            .arg(git.path())
            .args(["--server", &server.url, "--repo", "cli"])
            .args(extra)
            .env_remove("BUD_SERVER")
            .env_remove("BUD_REPO")
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    };

    let (ok, out) = cmd(&["--dry-run"]);
    assert!(ok, "{out}");
    assert!(out.contains("would import 2 changeset(s)"), "{out}");
    assert!(out.contains("(1 change(s)): again"), "{out}");

    let (ok, out) = cmd(&[]);
    assert!(!ok, "the repository doesn't exist yet: {out}");
    assert!(out.contains("--create-repo"), "{out}");

    let (ok, out) = cmd(&["--create-repo", "--verify", "--limit", "1", "HEAD"]);
    assert!(ok, "{out}");
    assert!(out.contains("imported 1 changeset(s)") && out.contains("1 commit(s) remaining"), "{out}");
    assert!(out.contains("verified: 1 file(s)"), "{out}");

    let (ok, out) = cmd(&["--create-repo", "--verify"]);
    assert!(ok, "{out}");
    assert!(out.contains("resumed after") && out.contains("budwood head: seq 2"), "{out}");
    let remote = Remote::new(&server.url, "cli").unwrap();
    assert_eq!(remote.file("head", &path("hello.txt")).unwrap(), b"hello again\n");
}
