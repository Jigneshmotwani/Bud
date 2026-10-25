# Budwood

Budwood is an open-source version control system for **very large monorepos with very high commit rates**, such as
repositories where thousands of engineers and automated agents land changes all day. It is built around a
**client-server asymmetry**. The server owns history and does the heavy work: it computes trees, answers log and diff
queries, and rebases commits. Clients stay online, fetch content lazily by hash, and keep only a bounded local cache.
No machine ever needs a full copy of the repository.

Budwood is **not a general replacement for git**. If you need offline work, distributed peers, or branches you manage
yourself, git is the better tool. Budwood is aimed at the opposite end: one authoritative history, always-online
clients, a source-control API that tools and agents call directly, and commits that stay cheap at any repository size.

> **Status: early, v0.1.** Expect rough edges. The on-disk formats, object encodings, changeset ids and the HTTP API
> are **unstable** and may change without a migration path. There is no authentication yet, so do not expose a
> server to an untrusted network.

## Inspiration and non-affiliation

Budwood's design is inspired by the ideas in
[**"Redesigning source control at Anthropic"**](https://www.anthropic.com/research/redesigning-source-control)
(October 2026), which describes an internal source-control system and the reasoning behind it.

Budwood is an **independent** open-source project. It is **not affiliated with, sponsored by, or endorsed by
Anthropic**, it does not use any of that system's code, and it is **not compatible** with that system. All mistakes
here are our own.

## Key ideas

| Idea | In Budwood today |
| --- | --- |
| **Always-online clients that fetch lazily** | Yes. `bud` fetches trees and blobs on demand by content hash. `log`, `show`, `cat`, `ls` and `sync` are server queries, so the client never downloads history. |
| **Client-server asymmetry** | Yes. The server stores the changeset log, computes every tree, and performs rebases. A client's `.bud/` holds config, a small index and a verified object cache. |
| **Source control as an API** | Yes. Every operation is a [versioned HTTP/JSON API](docs/api.md). You can use `bud` without a working copy (`--server/--repo`), or call the API directly. |
| **Cheap, lock-free commits** | Yes. Commits name the revision they were based on. The server rebases them over newer, non-overlapping history and appends with an optimistic compare-and-swap. There are no client-held locks and no merge queue. |
| **Changesets as a write-ahead log** | Yes. Each changeset lists only the files that changed relative to its parent, each pointing at a content-addressed blob. |
| **Derived data** | Yes. Root trees and a `path_history` index are derived from changesets and power path-filtered log and commit conflict checks. A generic memoized derivation layer computes line **blame** incrementally: each file version is derived from that file's previous version plus a diff, so cost scales with the file's history, not the repository's. |
| **Programmatic repairs** | Yes. `POST changesets/{rev}/revert` (`bud backout`) creates the inverse changeset on the server, for automated revert pipelines. |
| **Content-addressed fetching instead of packfiles** | Yes. Objects are fetched individually or in framed batches by BLAKE3 id, and the client verifies every one. Immutable object responses carry `Cache-Control: immutable`. |
| **Optional sparse working copies** | Yes. `bud clone --sparse PATH` and `bud sparse add PATH`. |
| **Horizontal scalability, no full copy anywhere** | Partly. No client holds a full copy. The server is currently a single node (SQLite plus a filesystem object store) behind pluggable storage traits. |
| **Tiered architecture** (per-node daemon, regional caches, authoritative servers) | Partly. `budwood-cache` is a verified, chainable read-through cache tier with request coalescing. Each workspace keeps a size-bounded LRU object cache. A per-node daemon is on the roadmap. |
| **Post-merge testing instead of a global merge queue** | Design stance. Budwood lands non-conflicting changes immediately instead of serializing them through a queue. Test-integration hooks are not built yet. |

Read [docs/architecture.md](docs/architecture.md) for how these fit together.

## Quick start

You need a stable Rust toolchain (1.89 or newer).

```sh
git clone <this repository> budwood && cd budwood
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

Start a server. `--data` is a global flag, so it goes before the subcommand. `--repo` creates the repository if it
doesn't exist, and can be repeated.

```sh
budwood-server --data ./data serve --repo demo
# listening on http://127.0.0.1:7171   (change with --listen, or BUDWOOD_LISTEN)
```

In another terminal, clone, edit and commit:

```sh
bud clone http://127.0.0.1:7171 demo        # creates ./demo
cd demo
mkdir -p src && echo 'fn main() {}' > src/main.rs
bud status                                   # A src/main.rs
bud diff                                     # unified diff against the base revision
bud commit -m "Add main.rs"                  # author: --author, $BUD_AUTHOR, or $USER
bud log
bud show 1
```

If someone else committed in the meantime, you don't need to pull first. When the newer changesets touched different
paths, the server rebases your commit onto them and `bud` updates your working copy afterwards:

```text
committed revision 3 (9081f1cdf035), 1 file(s); rebased over newer changes, 1 file(s) updated locally
```

If they touched the same paths (or a path inside a directory you changed), the commit is rejected. Run `bud sync` and
commit again.

### Sparse working copies

Check out only the parts of the tree you need, and add more later:

```sh
bud clone --sparse services/api --sparse libs/common http://127.0.0.1:7171 demo api-only
cd api-only
bud sparse add docs
bud sparse list
```

### Using Budwood without a working copy

The read-only commands (`log`, `show`, `cat`, `ls`) work against the server directly when you give `--server` and
`--repo`, or set `BUD_SERVER` and `BUD_REPO`. Paths are then repository paths:

```sh
bud --server http://127.0.0.1:7171 --repo demo log src
bud --server http://127.0.0.1:7171 --repo demo cat src/main.rs --rev 1
bud --server http://127.0.0.1:7171 --repo demo ls
bud --server http://127.0.0.1:7171 repos
bud --server http://127.0.0.1:7171 create-repo another
```

Or call the HTTP API directly:

```sh
curl http://127.0.0.1:7171/v1/repos/demo/head
curl 'http://127.0.0.1:7171/v1/repos/demo/log?path=src&limit=5'
```

The [API reference](docs/api.md) has a complete upload-and-commit walkthrough.

## Command reference

### `bud`

Global options: `--server URL` (`BUD_SERVER`) and `--repo NAME` (`BUD_REPO`). They affect `create-repo`, `repos`
and the read-only commands. Workspace commands use the server and repository recorded in `.bud/config.json`.

| Command | What it does |
| --- | --- |
| `bud create-repo NAME` | Create a repository on `--server`. |
| `bud repos` | List the repositories on `--server`. |
| `bud clone SERVER REPO [DIR] [--sparse PATH]...` | Create a working copy in `DIR` (default: `REPO`) at the head revision. `--sparse` limits it to the given repository paths, and can be repeated. The directory must be empty or absent. |
| `bud status` | Show local changes against the base revision (`A`, `M`, `D`) and the checked-out paths. |
| `bud diff [PATH]...` | Show local changes as a unified diff, optionally only under `PATH`s. |
| `bud commit -m MSG [--author NAME] [PATH]...` | Upload and commit all local changes, or only those under `PATH`s. The author defaults to `$BUD_AUTHOR`, then `$USER`. |
| `bud sync [--rev REV]` | Update the working copy to head (or `REV`), keeping local changes that don't collide with incoming ones. |
| `bud revert PATH...` | Discard local changes at or under `PATH`s. |
| `bud log [PATH] [-n N] [--before SEQ]` | Show history, newest first, optionally only changesets that touched `PATH` or anything under it (default `-n 20`). |
| `bud show REV` | Show one changeset: author, date, message, changed files, and whether the server rebased it. |
| `bud cat PATH [--rev REV]` | Print a file at a revision (default `head`). |
| `bud ls [PATH] [--rev REV]` | List a directory at a revision. Suffixes: `/` directory, `*` executable, `@` symlink. |
| `bud blame PATH [--rev REV]` | Show, for each line, the changeset that introduced it. Computed by the server. |
| `bud backout REV [-m MSG] [--author NAME]` | Have the server commit a changeset that undoes `REV`. Rejected if later changesets touched the same paths. |
| `bud remote-diff FROM [TO] [PATH]` | List the files that differ between two revisions (`TO` defaults to `head`), computed by the server. |
| `bud sparse add PATH` | Add `PATH` to the checked-out set and materialize its files. |
| `bud sparse list` | List the checked-out paths. |
| `bud cache usage` | Show the size of the workspace's object cache and its budget. |
| `bud cache gc [--max-bytes N]` | Evict least recently used objects down to the budget (or `N`). Evicted objects are re-fetched on demand. |
| `bud cache set-budget BYTES\|default` | Set the cache budget (default 2 GiB). |

Mutating commands take an OS file lock on `.bud/lock`, so concurrent `bud` processes in one working copy are refused
instead of corrupting it. The OS releases the lock when a process exits, so an interrupted `bud` never leaves a stale
lock.

Inside a workspace, `PATH` arguments are relative to the current directory. With `--server/--repo` they are
repository paths. `REV` is `head`, a sequence number (`0` is the empty repository), or a full 64-character changeset
id.

Untracked files matching patterns in a top-level `.budignore` are skipped by `status` and `commit`. Each line is a
glob. `*` and `?` don't match `/`. A trailing `/` matches directories only. A pattern containing a `/` is anchored
at the workspace root, and any other pattern matches a file or directory name at any depth. Tracked files are never
ignored.

### `budwood-server`

| Command | What it does |
| --- | --- |
| `budwood-server [--data DIR] serve [--listen ADDR] [--repo NAME]... [--max-body-bytes N]` | Serve the v1 HTTP API. `--data` (`BUDWOOD_DATA`, default `budwood-data`) holds `objects/` and `meta.sqlite3`. `--listen` (`BUDWOOD_LISTEN`) defaults to `127.0.0.1:7171`. `--max-body-bytes` (default 256 MiB) bounds the size of one request, and therefore of one blob. |
| `budwood-server [--data DIR] create-repo NAME` | Create a repository offline. |

Logging uses `RUST_LOG` (default `budwood_server=info`).

### `budwood-cache`

A read-through cache to run close to clients. Point `bud clone` (or another cache) at it instead of the server.

```sh
budwood-cache --upstream http://127.0.0.1:7171 --listen 127.0.0.1:7272 --cache-dir ./cache --max-cache-bytes 50G
bud clone http://127.0.0.1:7272 demo
curl http://127.0.0.1:7272/v1/cache/stats
```

Objects are verified against their ids before they are cached, concurrent misses for one object are coalesced into a
single upstream fetch, and everything that isn't an immutable object is proxied unchanged. Cache hits are not yet
authorized against the upstream; see the roadmap.

### `budwood-import`

Imports a git repository's first-parent history, one changeset per git commit. Imports are resumable: rerun the same
command to pick up new commits.

```sh
budwood-import ~/src/some-repo main --server http://127.0.0.1:7171 --repo some-repo --create-repo --verify
```

Each changeset's message keeps the git message plus `Git-Commit:` and `Git-Date:` trailers. Submodules (gitlinks)
are skipped. `--verify` checks that the imported tree matches git.

## Project layout

```text
crates/
  budwood-core/     data model shared by every component
    src/id.rs         ObjectId: domain-separated BLAKE3 content addresses
    src/path.rs       RepoPath: normalized repository paths
    src/tree.rs       Tree and its canonical binary encoding
    src/changeset.rs  Change, Changeset and changeset ids
    src/ops.rs        lookup, walk, diff and apply over content-addressed trees
    src/api.rs        JSON request/response types for HTTP API v1
  budwood-server/   authoritative server (`budwood-server` binary)
    src/http.rs       axum routes for API v1
    src/service.rs    repository operations: resolve, log, tree, diff, commit, revert
    src/meta.rs       SQLite changeset log and path_history index
    src/derived.rs    generic memoized derived-data layer
    src/blame.rs      incremental line blame built on derived.rs
    src/store.rs      content-addressed object store (filesystem, in-memory)
  budwood-client/   client library and the `bud` CLI
    src/remote.rs     blocking HTTP client for API v1
    src/objects.rs    verified, size-bounded LRU object cache with lazy and batched fetch
    src/workspace.rs  sparse working copy: status, commit, sync, revert, diff, locking
    src/ignore.rs     .budignore patterns
  budwood-cache/    regional read-through object cache tier
  budwood-import/   resumable git history importer
docs/
  architecture.md   data model, protocols and design tradeoffs
  api.md            HTTP API v1 reference
```

## Roadmap

These are not implemented yet, roughly in priority order:

- **Per-node daemon and virtual filesystem**: share one object cache between all workspaces on a machine, watch the
  filesystem instead of scanning it, and materialize files on first access instead of at checkout.
- **Authentication, authorization and per-directory permissions**, enforced by the server and respected by caches
  (today `budwood-cache` serves hits without asking upstream).
- **Eager derived data**: warm blame and other derivations in the background after each commit.
- **Pluggable cloud backends** for metadata (the changeset log) and objects, so the server tier can scale out
  horizontally.
- **Streaming large blobs** instead of buffering whole objects in memory, plus chunked uploads.
- **Batched and pipelined tree fetching**, so checkouts of large trees need fewer round trips.
- **Garbage collection** of unreferenced server objects (for example trees written by commit attempts that lost the
  append race).
- **git export**, to complement `budwood-import`.
- **Code review integration.**
- **Post-merge testing hooks** that report on landed changesets.
- **Windows support**: executable bits and symlinks are not handled today.
- Smaller items: `bud sparse remove`, short changeset ids, JSON output for scripting.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your
option.

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in Budwood, as
defined in the Apache-2.0 license, is dual licensed as above, without any additional terms or conditions.
