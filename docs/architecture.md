# Budwood architecture

This document describes what Budwood v0.1 actually implements: the data model, the commit protocol, derived data,
the client working copy, and caching. It ends with the design tradeoffs. Everything here is **unstable**: encodings
and layouts may change before 1.0 without a migration path.

The code lives in three crates:

- `budwood-core`: the data model and the tree algorithms, shared by everything else.
- `budwood-server`: the authoritative changeset log, object storage, derived data (blame) and the HTTP API.
- `budwood-client`: the HTTP client, the verified object cache, sparse workspaces and the `bud` CLI.

A regional read-through cache (`budwood-cache`) is in progress and isn't covered here yet.

```text
 bud / tools / agents                    budwood-server
 +-----------------------+   HTTP v1   +--------------------------------------+
 | Workspace             | ----------> | Service: resolve, log, tree, diff,   |
 |   .bud/config.json    |             |          commit (+ rebase), revert,  |
 |   .bud/state.json     |             |          blame (derived data)        |
 |   .bud/objects/       | <---------- | MetaStore (SQLite): changesets,      |
 |   (verified cache)    |   objects   |          path_history, derived memo  |
 |                       |   by hash   | ObjectStore (files): blobs, trees    |
 +-----------------------+             +--------------------------------------+
```

## 1. Data model

### 1.1 Object ids

Every stored object is named by an `ObjectId`, a 256-bit BLAKE3 hash with **domain separation**
(`crates/budwood-core/src/id.rs`):

```text
id = BLAKE3( tag || u64_le(len(data)) || data )

tag = "budwood:blob:v1\0"       for file contents
      "budwood:tree:v1\0"       for encoded trees
      "budwood:changeset:v1\0"  for changesets (see 1.5)
```

A blob, a tree and a changeset with identical bytes therefore never share an id, and a stored tree can't be passed
off as a blob. The length prefix is part of the hashed input.

On the wire and in JSON, ids are 64 lowercase hex characters (parsing accepts either case). The "short" form shown
by `bud` is the first 12 hex characters, which is for display only. The API accepts only full ids.

Examples: the blob `hello\n` has id `c966255a1935a5c8f54433b1a4165e8aaf05f6fa98fb08312abd60463987b903`, and the
empty tree has id `c21243b09637570e852524ce0d37a68e6242b0ddc6ea08d45846e0b7d714b9d2`.

### 1.2 Repository paths

A `RepoPath` (`crates/budwood-core/src/path.rs`) is a normalized, `/`-separated path relative to the repository root:

- Leading and trailing `/` are stripped on input. The empty string is the **root**. The root is valid as a prefix
  (for log, diff, tree listings and sparse sets) but never as the path of a file. It displays as `/`.
- Every component must be non-empty (so `a//b` is rejected), must not be `.` or `..`, must not contain `/` or NUL,
  and must not be `.bud`, which is reserved for workspace metadata at any depth.
- Any other UTF-8 is allowed. Paths are case-sensitive.
- Paths order by plain byte-wise string comparison, which is also the order of entries in a tree.
- `is_within(dir)` is true when the path equals `dir` or lies under `dir/`. `ab` is not within `a`.

In JSON a path is a plain string, `""` for the root.

### 1.3 File states and entry kinds

```text
FileKind  = file | executable | symlink          (the blob of a symlink holds its target)
EntryKind = file | executable | symlink | tree
FileState = { kind: FileKind, blob: ObjectId }
```

The executable bit is part of the entry kind, not a mode, so Budwood records exactly three kinds of file and
directories. Empty directories cannot be represented: applying changes prunes directories that become empty.

### 1.4 Trees and their binary encoding

A `Tree` (`crates/budwood-core/src/tree.rs`) is one directory level: a map from name to `(EntryKind, ObjectId)`. Its
canonical encoding, which is what gets hashed, stored and served, is:

```text
"BWT1"                    4 bytes, magic
count        u32 LE       number of entries
count × {
  kind       u8           1 = file, 2 = executable, 3 = symlink, 4 = tree
  name_len   u32 LE       length of name in bytes
  name       name_len     UTF-8
  id         32 bytes     raw ObjectId of the blob or subtree
}
```

Entries are sorted by name in strictly increasing byte order. The empty tree is `"BWT1" 00 00 00 00`.

Decoding **rejects anything non-canonical**, so `decode(bytes).id()` always equals the id the bytes were stored under.
It rejects bad magic, truncation, an unknown kind byte, names that aren't UTF-8, empty names, `.`, `..`, the
reserved name `.bud` (as `RepoPath` does), names containing `/` or NUL, entries that are unsorted or duplicated, and
trailing bytes.

Trees are built and stored by the server only. Clients never upload trees. They fetch them by id and verify them by
re-encoding and re-hashing.

### 1.5 Changesets

History is a **linear log** of changesets per repository (`crates/budwood-core/src/changeset.rs`):

| Field | Meaning |
| --- | --- |
| `id` | Content address of the changeset (below). |
| `seq` | Position in the log. The first changeset is `1`, and `0` denotes the empty repository. |
| `parent` | `id` of changeset `seq - 1`, or `null` for `seq` 1. |
| `base` | The `seq` the author's working copy was at. `base < seq - 1` means the server rebased the change. |
| `root_tree` | Root tree after applying `changes` to the parent's root tree. Computed by the server. |
| `author` | Free-form, non-empty string supplied by the client (not authenticated in v0.1). |
| `message` | Non-empty commit message. |
| `timestamp` | Seconds since the Unix epoch, assigned by the server. |
| `changes` | The changed **files**, relative to the parent, sorted by path. |

Each `Change` is `{ "path": RepoPath, "new": FileState | null }`, where `null` deletes the file. Changes are
validated by `validate_changes`: there must be at least one, paths must be unique, and no path may be the root. A
change always names a file. Deleting a directory means deleting each file in it, and replacing a file with a
directory (or the reverse) is a delete plus a set (see `apply_changes` in `crates/budwood-core/src/ops.rs`).

**Changeset id.** The id hashes every field except `id` itself. The fields are serialized in this fixed order as
compact JSON by `serde_json`: `seq`, `parent`, `base`, `root_tree`, `author`, `message`, `timestamp`, `changes`. The
result is hashed with the changeset tag:

```text
id = BLAKE3("budwood:changeset:v1\0" || u64_le(len(json)) || json)
json = {"seq":5,"parent":"34f9…","base":4,"root_tree":"7f2d…","author":"ci-bot",
        "message":"Add hello.txt","timestamp":1792906775,"changes":[{"path":"hello.txt",
        "new":{"kind":"file","blob":"c966…"}}]}
```

Because `root_tree` and `parent` are included, a changeset id commits to the full repository state and its whole
history. `bud` verifies the id of every changeset it receives, whether from log, show or a commit response.

Changesets are stored in the server's metadata database, not in the object store. They aren't fetchable through
`/objects/`.

### 1.6 Tree algorithms

`crates/budwood-core/src/ops.rs` implements the algorithms both sides use, over abstract `TreeSource` and `TreeSink`
traits. The server backs them with its object store, and the client with its lazily-fetching cache.

- `lookup(root, path)` resolves a path to an entry.
- `walk_files(root, prefix)` lists every file under a prefix.
- `diff_trees(from, to, prefix)` returns the file-level changes that turn `from` into `to`. It skips any subtree
  whose id is equal on both sides without reading it, so its cost is proportional to what changed, not to the size
  of the repository.
- `apply_changes(root, changes)` builds the new root tree, rewriting only the directories on changed paths. It fails
  with `PathNotFound` when deleting a file that doesn't exist. It fails with `PathConflict` when a file would be
  treated as a directory or vice versa, for example setting `a/b` while `a` is a file that isn't being deleted. It
  prunes directories left empty. The result is canonical: the same final file set always yields the same root id,
  whatever order the changes were applied in.

## 2. Commit protocol

A commit is a small number of HTTP requests. Nothing is locked on the client's behalf at any point.

### 2.1 Upload blobs

1. The client computes its local changes (see section 4) and collects the blob ids of every `set` change.
2. `POST /v1/repos/{repo}/objects/missing` with up to 1024 ids per call. The server returns the subset it doesn't
   have.
3. `PUT /v1/repos/{repo}/objects/{id}` for each missing blob. The server recomputes the blob id of the body and
   rejects a mismatch with `400`. Uploads are idempotent. Only blobs can be uploaded, because trees are always
   computed by the server. The client re-reads and re-hashes each file just before uploading it, and aborts if the
   file changed since `status`.

### 2.2 Submit the changeset

`POST /v1/repos/{repo}/commit` with:

```json
{ "base": 4, "author": "alice", "message": "Fix parser", "changes": [ { "path": "src/parse.rs", "new": { "kind": "file", "blob": "…" } } ] }
```

`base` is the revision the working copy was at. The server (`Service::commit` in
`crates/budwood-server/src/service.rs`) then works through these steps:

1. **Validate.** The repository must exist, `message` and `author` must be non-empty after trimming, and the
   changes must pass `validate_changes` (otherwise `400`).
2. **Check blobs.** Every referenced blob must be present in the object store. If any are missing, it returns `412`
   with `code: "missing_objects"` and the list of ids.
3. **Optimistic loop**, up to 32 attempts:
   1. Read the current head. If `base > head.seq`, it returns `400`.
   2. If `base < head.seq`, it collects every path touched by changesets in `(base, head.seq]` from the
      `path_history` index (section 3). A changed path **P** conflicts with a touched path **T** when:
      - `P == T` (the same file), or
      - `P` is a proper ancestor of `T` (you set file `a`, someone else changed `a/x`), or
      - `T` is a proper ancestor of `P` (someone replaced directory `a` with file `a`, and you changed `a/x`).

      Any conflict returns `409` with `code: "conflict"` and the conflicting paths in `paths`. Conflict detection is
      purely by path. Two identical edits to the same file still conflict, and nothing is merged textually.
   3. Apply the changes to **head's** root tree with `apply_changes`. New trees are written to the object store.
      `PathNotFound` or `PathConflict` returns `409 conflict` naming that path.
   4. Build the changeset with `seq = head.seq + 1`, `parent = head.id`, the request's `base`, the new `root_tree`
      and a server timestamp, then compute its id (`seal`).
   5. **Append with compare-and-swap** (`MetaStore::append` in `crates/budwood-server/src/meta.rs`). In one SQLite
      `IMMEDIATE` transaction, it checks that `MAX(seq) + 1 == changeset.seq`, inserts the changeset row, and inserts
      one `path_history` row per changed path. If another commit won the slot, it retries from step 1 against the
      new head, re-checking conflicts.
4. If all 32 attempts lose the race, it returns `503` with `code: "unavailable"`, and the client may retry.

The response is `{ "changeset": …, "rebased": bool }`. `rebased` is true when the changeset landed on a newer head
than `base`.

Rebasing at file granularity is sound because of the conflict rule. When no path in the request overlaps any path
changed since `base`, applying the request to head gives the same files as applying it to `base` and then replaying
the newer changesets.

### 2.4 Server-side revert

`POST …/changesets/{rev}/revert` (`Service::revert`) lets an automated pipeline back out a changeset without a
working copy. The server builds the inverse changes of `{rev}`: for each path `{rev}` changed, it looks the path up
in the parent's root tree and sets it to that `FileState`, or deletes it if it wasn't a file there. Those changes
then go through `Service::commit` (section 2.2) with `base = {rev}.seq`, unchanged. So a revert:

- is rebased over later changesets that touched other paths, and lands on head;
- is rejected with `409 conflict` if any later changeset touched an overlapping path, because the inverse was
  computed against `{rev}`'s state of those paths and would silently discard the later edits;
- needs no uploads, since every restored blob is already part of history.

The default message is `Revert "<summary>"`, a blank line, then `This reverts revision N (<id>).`. `bud backout REV`
is the CLI for it.

### 2.3 Update the working copy

After a successful commit, `bud` updates the workspace to the new changeset (section 4.4). Its own changes are
already on disk, so they match the incoming diff and are only re-indexed. Any files other people changed in the
rebased-over history (within the sparse set) are written. That count is the "N file(s) updated locally" message.

If a commit conflicts, `bud` reports the paths and suggests `bud sync`. After syncing, which fails if local edits
collide with the incoming ones, the user commits again with a newer `base`.

## 3. Derived data

The changeset log is the source of truth. Everything else is derived from it, and the cost of deriving it scales
with the change, not with the repository:

- **Trees.** Each `root_tree` is computed from the parent's root plus `changes`. Trees are immutable and
  content-addressed, so they are shared freely across revisions and repositories.
- **`path_history`.** This is a SQLite table with primary key `(repo, path, seq)` and a secondary index on
  `(repo, seq)`. It is written in the same transaction as the changeset it describes, with one row per changed file
  path. It answers three questions:
  - *Which changesets touched this path or anything under it?* (`GET …/log?path=p`)
  - *Which paths changed between revisions `a` and `b`?* (the commit conflict check)
  - *Which changeset last changed exactly this file, at or before revision `r`?* (blame, below)
- **Blame**, through the generic derived-data mechanism (section 3.1).

**The prefix-range trick.** Paths are stored as text, and only file paths are recorded. To find everything under
directory `p` with an index range scan instead of a `LIKE` scan, `MetaStore::log` queries:

```sql
path = 'p' OR (path >= 'p/' AND path < 'p0')
```

Every path strictly under `p` starts with `p/`. The byte `'0'` (0x30) immediately follows `'/'` (0x2F), so the
half-open range `['p/', 'p0')` contains exactly the strings that start with `p/`. It excludes siblings like `p.txt`
or `pa/b`. SQLite's default `BINARY` collation compares bytes, so this is an index range on the primary key. The
root path short-circuits to the plain log. The equality term also catches a file named exactly `p`, so the history
of a path that was once a file and later a directory is complete.

History is linear, so the commit graph is just `seq` order.

### 3.1 The derived-data mechanism

`crates/budwood-server/src/derived.rs` defines a `Derivation` trait. A derivation is a pure function from a key to
a value:

```rust
trait Derivation {
    const KIND: &'static str;          // e.g. "blame.v1"; bump the version when the format changes
    type Key; type Inputs; type Value; // Value: Serialize + Deserialize
    fn encode_key(key) -> String;
    fn inputs(svc, repo, key) -> (Inputs, Vec<Key>);   // read history; list the keys this one depends on
    fn compute(svc, repo, key, inputs, deps: Vec<Value>) -> Value;
}
```

`derive::<D>(svc, repo, key)` returns the value, computing whatever is missing:

1. Look `(repo, D::KIND, encode_key(key))` up in the `derived` table. A hit is returned as is.
2. Otherwise evaluate depth-first with an **explicit stack** rather than recursion, so a dependency chain as long
   as a file's whole history cannot overflow the stack. Each frame gathers its inputs and pushes the dependencies
   that aren't memoized yet. Once they are computed, it computes its own value and stores it.
3. Every computed value is written to the memo table (`INSERT OR IGNORE`), so it is computed at most once per server.
   Values computed during one call are handed to their dependents in memory. A chain consumes each one once, so
   memory stays bounded. Shared dependencies (a DAG) are computed once. Meeting a key that is still being computed
   is reported as a cycle.

```sql
CREATE TABLE IF NOT EXISTS derived (
    repo TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,   -- value: JSON
    PRIMARY KEY (repo, kind, key)
);
```

Values depend only on history, which is append-only, so rows are never invalidated. Changing a format means a new
`KIND`. Evaluation today is **lazy**, on the first request. Eager derivation is the same `derive` call made ahead
of time (for example by a background job after each commit). It isn't wired up, so commits stay as cheap as before.
`Service::derivations()` counts values computed rather than served from the memo, which tests use to check that
work is incremental.

### 3.2 Blame

`crates/budwood-server/src/blame.rs` implements the `blame.v1` derivation. Its key is `(path, seq)`, where `seq` is
a changeset that changed `path`. Its value is one of:

```text
absent                      seq deleted the file
binary                      the file contains NUL or isn't UTF-8
text { blob, runs }         blob id of this version, and each line's origin seq, run-length encoded as [seq, count]
```

- **Inputs:** the file's `FileState` in `seq`'s root tree. **Dependency:** `(path, prev)`, where `prev` is the
  newest changeset before `seq` that changed exactly `path`, found with one `path_history` index lookup. A deleted
  file has no dependency, so a delete cuts the chain.
- **Compute:** split both versions into lines and diff them (Myers, from the `similar` crate, bounded to 2 seconds
  per pair). Lines in `Equal` runs keep their origin from `prev`'s value. Inserted and replaced lines get `seq`. If
  `prev` was absent or binary, or there is no `prev`, every line gets `seq`. If only the kind changed (same blob),
  the runs are copied.

`GET …/blame?path=P&rev=R` resolves `R`, looks `P` up in its tree (`404` if missing, `400` for a directory), and
finds `s`, the last changeset at or before `R` that changed `P`. The blame of `P` at `R` is the value of `(P, s)`,
since nothing changed `P` between `s` and `R`. A `binary` value is a `400`. The server then reads the blob, splits
it, and attaches each origin's id, author and timestamp. Those come from SQLite's `json_extract` on the stored
changeset, so a huge changeset isn't decoded just for its author.

**Cost.** Memo entries are per version of a file, never per revision of the repository. The first blame of a file
computes one diff per version in its history (or back to its last re-add). Every later blame of any revision costs
at most one diff per new version. Unrelated commits, however many, cost nothing. Storage is one run list per
version, typically a few runs.

## 4. Client working copy

### 4.1 Layout

A workspace is a directory containing `.bud/`:

```text
.bud/
  config.json   { "server": "http://…", "repo": "demo", "sparse": ["docs", "src"], "cache_max_bytes": … }
  state.json    { "base": 7, "base_root": "<tree id>", "files": { "<path>": IndexEntry, … } }
  objects/      verified object cache: objects/ab/cdef… (blobs and trees, by id)
  objects/usage approximate cache size, so budget checks don't need a scan
  lock          OS file lock held by mutating commands; contains the holder's pid
```

- `sparse` holds the checked-out prefixes. They are normalized to be sorted, deduplicated and non-overlapping, and
  `[""]` means the whole repository.
- `state.json` records the base revision and its root tree, plus the **index**: one entry per checked-out file.
  ```json
  "src/main.rs": { "state": { "kind": "file", "blob": "983e…" }, "size": 29, "mtime_ns": 1792906746123456789 }
  ```
- JSON files are written to a temporary file and renamed into place.
- The object cache is **bounded** (`cache_max_bytes`, default 2 GiB). Recency is the file mtime, refreshed on a hit
  at most once a minute. When the size estimate in `objects/usage` exceeds the budget, the cache is scanned and the
  least recently used objects are evicted down to 80% of the budget. Eviction only runs at safe points (after each
  checkout batch of 1024 files, and at the end of an operation), never between fetching an object and using it.
  Every read falls back to the server, so evicted objects are re-fetched transparently.
- Mutating operations (`status`, `diff`, `commit`, `sync`, `revert`, `sparse add`, cache changes) hold an exclusive
  OS file lock (`File::try_lock`) on `.bud/lock`. A second process gets a `WorkspaceLocked` error naming the
  holder's pid. Because the OS drops the lock when the holder exits, a killed `bud` never leaves a stale lock.

Only paths inside the sparse set exist in the index or on disk. Files on disk outside the sparse set are ignored.

### 4.2 Lazy, verified fetching

`Objects` (`crates/budwood-client/src/objects.rs`) sits in front of the server:

- **Trees** are fetched one at a time with `GET objects/{id}` when an algorithm needs them. They are decoded and
  verified by recomputing their id, whether they came from the server or from the cache. The empty tree is never
  fetched.
- **Blobs** for a checkout or update are prefetched in batches with `POST objects/fetch` (up to 1024 ids per
  request). Each returned frame must be one that was requested and must hash to its id. A blob that wasn't
  prefetched, for example one `bud diff` needs, is fetched on its own with `GET objects/{id}` and verified the same
  way.
- Only verified bytes are written to the cache, so a misbehaving server, proxy or cache cannot corrupt a working
  copy.

Clones, syncs and sparse additions only fetch the trees and blobs under the sparse prefixes. History, logs and old
file versions are never downloaded. They are answered by the server.

### 4.3 Status, the stat cache and the racy-timestamp window

`status` walks each sparse prefix on disk. It uses `symlink_metadata`, so symlinks are never followed. It skips
`.bud/` at the root, ignored untracked files, and ignored untracked directories (an ignored directory that contains
tracked files is still walked). For each file:

- If the index entry has a **non-zero** `mtime_ns`, and the kind, size and mtime all match the disk, the recorded
  `FileState` is trusted without reading the file.
- Otherwise the file is read and hashed. A symlink's content is its target.
- The result is compared with the index: a new path is **A**dded, a different state is **M**odified, and an indexed
  path missing from disk is **D**eleted.
- When a file's content is unchanged but its stat data was stale, the entry is refreshed and `state.json` is saved.
  This is why `status` takes `&mut self`.

The kind is detected from disk. A symlink is `symlink`, any execute bit makes a file `executable`, and anything else
is `file`. Checkouts write files with mode `0644` or `0755`.

**Racy timestamps.** Filesystem timestamps have limited granularity, so a same-size rewrite within the same tick
would look unchanged. When an index entry is recorded, `mtime_ns` is stored as **0** if the file's mtime is within
`RACY_WINDOW` (2 seconds) of the current time. A zero forces a re-hash on every `status` until a later status
records the aged mtime.

### 4.4 Update and sync rules

`bud sync [--rev REV]`, and the update after a commit, move the workspace from `base` to a target revision
(`Workspace::update_to`):

1. **Incoming changes**: `diff_trees(base_root, target_root, prefix)` for each sparse prefix.
2. **Local changes**: a fresh `status`.
3. **Collision check.** An incoming change at path **P** collides when:
   - there is a local change at `P` that differs from the incoming one (an identical local change is fine, and this
     is how your own commit is absorbed), or
   - a proper ancestor of `P` has a local add or modify (a local file where an incoming directory must go), or
   - the incoming change creates a file at `P` and there are local changes strictly under `P` (local files inside a
     directory being replaced by a file).

   Any collision aborts the whole update before anything is written, and lists every colliding path. There is no
   merge. Revert or move your changes aside, then sync again.
4. **Apply.** Incoming blobs are prefetched in batches. Deletes run first, so files can replace directories and the
   reverse; each removal prunes empty parent directories. Then sets are written. A file already identical on disk is
   only re-indexed. Finally `base` and `base_root` are updated and `state.json` is saved.

Local changes that don't collide survive the update untouched and still show in `status`.

**Other operations:**

- **revert** restores the base content of changed files under the given paths, and deletes added files.
- **sparse add** extends the sparse set and checks out the new prefix's files at the base revision. It refuses to
  overwrite an existing local file with different content.
- **commit** with path arguments commits only the local changes under those paths.

## 5. Caching

Objects are immutable and named by their hash, so any copy of `id → bytes` is valid forever and can be verified by
whoever receives it. The server takes advantage of this:

- `GET /v1/repos/{repo}/objects/{id}` responds with `Content-Type: application/octet-stream` and
  `Cache-Control: public, max-age=31536000, immutable`. Any HTTP cache, CDN or proxy may store it indefinitely.
  Because clients verify every object, intermediaries don't need to be trusted for integrity.
- Mutable or revision-relative responses are not marked cacheable. `GET …/file` (which may name `head`) and the
  batched `POST …/objects/fetch` respond with `Cache-Control: no-cache`, and the JSON endpoints send no caching
  headers.
- Every error response, including framework rejections, carries `Cache-Control: no-store`. A `404` for an object
  is only true until someone uploads it, so a shared cache must never pin one.
- The server's object store is global, shared by all repositories on that server, so identical content is stored
  once.

The client's `.bud/objects/` is the first cache tier. The regional cache tier (`budwood-cache`, in progress) builds on
the same property: it can answer object reads from its own disk and verify what it fetches upstream. A per-node
daemon that shares one cache between workspaces is on the roadmap.

## 6. Server storage

```text
<data>/
  meta.sqlite3        repos, changesets (JSON bodies), path_history, derived   (WAL mode, synchronous=NORMAL)
  objects/ab/cdef…    one file per object
  objects/tmp/        staging for atomic writes
```

`FsObjectStore` writes each object to `objects/tmp/` with `fsync`, then renames it into place. Writing an existing id
is a no-op. The storage traits (`ObjectStore` in `crates/budwood-server/src/store.rs`, and `MetaStore`) are the seams
for future cloud backends. Request handlers run storage calls on Tokio's blocking pool.

## 7. Design tradeoffs

**Linear history, one sequence per repository.** A total order with small integer revisions is easy for people and
tools to reason about ("is my change in revision 41233?"). It makes `base` meaningful and makes path history a
simple index lookup. The cost is a single append point per repository. Today that append is a short SQLite
`IMMEDIATE` transaction behind one connection mutex, so write throughput is bounded by one node. Scaling writes
further means sharding or replicating the log behind the `MetaStore` seam. Branches are not modeled.

**Optimistic, path-level rebasing instead of a merge queue.** Commits never wait for a lock or a queue. They are
checked only against what changed since their base, and land immediately if disjoint. That keeps commit latency flat
as commit rates grow, and pushes testing to *after* landing (post-merge testing), as the inspiring article advocates.
The price is that any path overlap is a conflict, even an identical or textually mergeable edit, and the author must
sync and retry. A rebased changeset's exact tree was never built or tested by its author. That is inherent to landing
without a queue.

**The server computes all trees.** Clients upload only blobs and a list of file changes, so they can't upload a
malformed or malicious tree, and they never need the whole tree to commit. The cost is server CPU, plus orphaned trees
from commit attempts that lost the append race. Garbage collection is not implemented yet.

**Changesets as file lists, not snapshots.** A changeset is small and cheap to validate, index and ship. It is
proportional to the change, not the repository. Snapshots (root trees) are derived, and clients fetch only the parts
they look at.

**Hashing JSON for changeset ids.** Using `serde_json`'s compact output keeps the implementation tiny and the id easy
to recompute in other languages. It does tie the id format to that exact serialization, including field order and
string escaping. This is one reason the formats are labelled unstable. Trees use a hand-specified binary encoding
because they are the hot path for fetching and hashing.

**One object at a time for trees.** Fetching trees individually by id is simple, perfectly cacheable and
deduplicating, but walking a large, cold tree costs one round trip per directory. Batched or pipelined tree fetching
and a local daemon are planned. Blobs already use the batch endpoint.

**Always online.** Every read that isn't served by the local cache goes to the server, and commits need the server.
This is a deliberate non-goal for offline use. In exchange, clients stay small and fast regardless of repository size
or history depth.

**Shared object namespace.** Storing objects globally gives cross-repository deduplication. However, the object
endpoints only check that the named repository *exists*, not that the object belongs to it. Repository isolation for
objects is therefore nominal until authorization lands.

**No authentication in v0.1.** `author` is whatever the client says. Run servers only on trusted networks.
