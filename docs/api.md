# Budwood HTTP API v1

This is the complete reference for the HTTP API served by `budwood-server`. The routes are defined in
`crates/budwood-server/src/http.rs`, and the JSON types in `crates/budwood-core/src/api.rs`. The `bud` CLI uses
nothing beyond what is documented here.

> **Unstable.** v1 is the first version of the API and may still change incompatibly before Budwood 1.0. There is
> **no authentication** in v0.1.

## Conventions

- **Base URL:** `http://HOST:PORT/v1`. The server listens on `127.0.0.1:7171` by default.
- **Bodies:** JSON (`Content-Type: application/json`) unless noted. Object contents are raw bytes
  (`application/octet-stream`).
- **Object ids** are 64-character hex strings. The server emits lowercase and accepts either case. See
  [architecture.md §1.1](architecture.md#11-object-ids) for how ids are computed.
- **Paths** are repository paths: `/`-separated, with leading and trailing `/` ignored. `""` (or an omitted `path`)
  is the repository root. Components may not be empty, `.`, `..` or `.bud`, and may not contain NUL. An invalid path
  returns `400 bad_request`.
- **Revisions** (`rev`, `from`, `to`, and `{rev}` in URLs) accept:
  - `head`, or an empty or omitted value: the newest changeset.
  - `0`: the empty repository, before any changeset.
  - A decimal sequence number such as `42`. Only ASCII digits are accepted (no sign), and leading zeros are
    ignored, so `00` is `0` and `042` is `42`.
  - A full 64-character changeset id. Short ids are not accepted. (A 64-digit decimal string is too large for a
    sequence number, so it is read as an id.)

  Surrounding whitespace is ignored. A string that is none of these returns `400`. A well-formed revision that
  doesn't exist returns `404`.
- **Repository names** are 1–100 characters from ASCII letters, digits, `-`, `_` and `.`, and may not start with
  `.`. Every `/v1/repos/{repo}/…` route returns `404 not_found` if the repository doesn't exist.
- **Request size:** every request body is limited by `--max-body-bytes` (default 256 MiB), which bounds the size of
  one blob. Larger bodies are rejected with `413 payload_too_large`.
- **Batch limit:** `objects/missing` and `objects/fetch` accept at most **1024** ids per request
  (`budwood_core::api::MAX_BATCH`).

## Types

```text
Revision     { "seq": u64, "id": ObjectId | null, "root_tree": ObjectId }   id is null only for seq 0
FileState    { "kind": "file" | "executable" | "symlink", "blob": ObjectId }
Change       { "path": RepoPath, "new": FileState | null }                   null deletes the file
Changeset    { "id", "seq", "parent": ObjectId | null, "base": u64, "root_tree", "author", "message",
               "timestamp": i64 (Unix seconds, server-assigned), "changes": [Change] }
BlameLine    { "line_no": u64 (1-based), "seq": u64, "id": ObjectId, "author": string, "timestamp": i64,
               "text": string }                                        the changeset that introduced the line
EntryKind    "file" | "executable" | "symlink" | "tree"
ListEntry    { "name": string, "kind": EntryKind, "id": ObjectId }
```

`Changeset.changes` is sorted by path, and each entry names a file. Clients should verify `Changeset.id`
([architecture.md §1.5](architecture.md#15-changesets)), as `bud` does.

## Errors

Every error has a JSON body and the header `Cache-Control: no-store`, so no HTTP cache ever stores one (a `404` for
an object or revision may stop being true a moment later):

```json
{
  "code": "conflict",
  "message": "1 path(s) were changed after revision 1; sync and retry",
  "paths": ["docs/README.md"]
}
```

| Field | Present | Meaning |
| --- | --- | --- |
| `code` | always | Machine-readable code (table below). |
| `message` | always | Human-readable description. |
| `paths` | only for `conflict` | The request's paths that conflicted. |
| `missing` | only for `missing_objects` | Blob ids that must be uploaded before committing. |

| HTTP status | `code` | When |
| --- | --- | --- |
| 400 Bad Request | `bad_request` | Invalid path, revision, id, repository name, batch size, commit fields, or a blob whose content doesn't match its id. Blame of a directory or a binary file. Malformed JSON, or a query string that fails to parse (for example a missing `from` on `/diff`, or a non-numeric `limit`). |
| 404 Not Found | `not_found` | Unknown repository, revision, path, object or route. |
| 405 Method Not Allowed | `bad_request` | A known route called with the wrong method. |
| 409 Conflict | `conflict` | The commit (or revert) overlaps newer history, or can't be applied to head (deleting a missing file, or a file/directory clash). |
| 412 Precondition Failed | `missing_objects` | The commit references blobs the server doesn't have. |
| 413 Payload Too Large | `payload_too_large` | Body larger than `--max-body-bytes`. |
| 415 Unsupported Media Type | `bad_request` | A JSON route called without `Content-Type: application/json`. |
| 422 Unprocessable Entity | `bad_request` | Well-formed JSON of the wrong shape, including an invalid object id inside a JSON body. |
| 503 Service Unavailable | `unavailable` | The commit lost the append race too many times. Retry. |
| 500 Internal Server Error | `internal` | Storage or other server failure. Details are logged on the server. |

Rejections that happen before a request reaches the service (`405`, `413`, `415`, `422`, and the `400`s for
malformed JSON or query strings) keep their HTTP status but use the same JSON shape, for example:

```text
HTTP/1.1 415 Unsupported Media Type
content-type: application/json
cache-control: no-store

{"code":"bad_request","message":"Expected request with `Content-Type: application/json`"}
```

Clients should still fall back gracefully when an error body isn't JSON (for example from a proxy). `bud` reports
such errors as `HTTP <status>: <text>`.

## Routes

| Method | Path | Purpose |
| --- | --- | --- |
| GET | [`/v1/health`](#get-v1health) | Liveness check |
| GET | [`/v1/repos`](#get-v1repos) | List repositories |
| POST | [`/v1/repos`](#post-v1repos) | Create a repository |
| GET | [`/v1/repos/{repo}/head`](#get-v1reposrepohead) | Current head revision |
| GET | [`/v1/repos/{repo}/log`](#get-v1reposrepolog) | History, optionally by path |
| GET | [`/v1/repos/{repo}/changesets/{rev}`](#get-v1reposrepochangesetsrev) | One changeset |
| POST | [`/v1/repos/{repo}/changesets/{rev}/revert`](#post-v1reposrepochangesetsrevrevert) | Commit the inverse of a changeset |
| POST | [`/v1/repos/{repo}/commit`](#post-v1reposrepocommit) | Append a changeset |
| GET | [`/v1/repos/{repo}/tree`](#get-v1reposrepotree) | Look up a path or list a directory |
| GET | [`/v1/repos/{repo}/file`](#get-v1reposrepofile) | File contents at a revision |
| GET | [`/v1/repos/{repo}/diff`](#get-v1reposrepodiff) | File-level diff between revisions |
| GET | [`/v1/repos/{repo}/blame`](#get-v1reposrepoblame) | Which changeset introduced each line of a file |
| POST | [`/v1/repos/{repo}/objects/missing`](#post-v1reposrepoobjectsmissing) | Which objects the server lacks |
| POST | [`/v1/repos/{repo}/objects/fetch`](#post-v1reposrepoobjectsfetch) | Batch fetch objects |
| GET | [`/v1/repos/{repo}/objects/{id}`](#get-v1reposrepoobjectsid) | Fetch one object |
| PUT | [`/v1/repos/{repo}/objects/{id}`](#put-v1reposrepoobjectsid) | Upload one blob |

---

### `GET /v1/health`

Returns `200` with the plain-text body `ok`.

---

### `GET /v1/repos`

Lists repositories, sorted by name.

```json
{ "repos": ["demo", "other"] }
```

---

### `POST /v1/repos`

Creates a repository.

Request:

```json
{ "name": "new-repo" }
```

Response: `201 Created` if the repository was created, or `200 OK` if it already existed. Both have an empty body.
An invalid name returns `400`:

```json
{ "code": "bad_request", "message": "invalid repo name \".bad\": must not start with '.'" }
```

---

### `GET /v1/repos/{repo}/head`

Returns the newest revision as a `Revision`:

```json
{
  "seq": 4,
  "id": "34f9f313993765616962ce3269cb875dffda78983994900c753b76405d36ac2b",
  "root_tree": "c8883ddf79e239b480d012d68cd1b2fb7e9db42178dc238366447a2b42f77ead"
}
```

For an empty repository: `{ "seq": 0, "id": null, "root_tree": "<empty tree id>" }`.

---

### `GET /v1/repos/{repo}/log`

Returns changesets, newest first.

| Query | Default | Meaning |
| --- | --- | --- |
| `path` | root | Only changesets that changed this path or anything under it. `src` matches `src` and `src/…`, but not `src2/…`. |
| `before` | none | Only changesets with `seq < before`. For pagination, pass the smallest `seq` you've seen. |
| `limit` | `50` | Maximum number returned, clamped to `1..=1000`. |

`GET /v1/repos/demo/log?path=docs&limit=1`:

```json
{
  "changesets": [
    {
      "id": "34f9f313993765616962ce3269cb875dffda78983994900c753b76405d36ac2b",
      "seq": 4,
      "parent": "9081f1cdf03575ad724d8ffb56d76e849928e74b924f0d2228660b3584783f5d",
      "base": 2,
      "root_tree": "c8883ddf79e239b480d012d68cd1b2fb7e9db42178dc238366447a2b42f77ead",
      "author": "bob",
      "message": "d2",
      "timestamp": 1792906746,
      "changes": [
        {
          "path": "docs/README.md",
          "new": { "kind": "file", "blob": "e75d19d63a6aeb93e4b1176fa4b3d402fd931730abdaffa2a641bf64ae133e6e" }
        }
      ]
    }
  ]
}
```

Here `base` (2) is less than `seq - 1` (3), so the server rebased this changeset over changeset 3.

---

### `GET /v1/repos/{repo}/changesets/{rev}`

Returns one `Changeset` (same shape as a `log` entry). `{rev}` is any revision form except `0`, which returns `404`
(`"revision 0 is the empty repository"`).

```text
GET /v1/repos/demo/changesets/2
GET /v1/repos/demo/changesets/head
GET /v1/repos/demo/changesets/095d09aafa6f3c0fe8953d3d2cda4d6221044b5bbe9fbdef377a92b79e31ae15
```

---

### `POST /v1/repos/{repo}/changesets/{rev}/revert`

Commits, on head, a new changeset that undoes changeset `{rev}` ("backout"). It is built for automated revert
pipelines: no working copy or blob upload is needed. For every path that `{rev}` changed, the new changeset restores
the state that path had in `{rev}`'s parent: the parent's `FileState`, or a delete if the path wasn't a file there.
(Reverting a file's addition deletes it. Reverting a deletion restores the file.)

The changes then go through the normal [commit path](#post-v1reposrepocommit) with `base = {rev}`'s `seq`, so they
are rebased over newer changesets when those touched other paths, and rejected with `409 conflict` when any changeset
after `{rev}` touched the same paths (or a directory ancestor or descendant of one). After a conflict, revert the
later changesets first, newest first, or make the change by hand.

Request (`RevertRequest`):

```json
{ "author": "revert-bot", "message": "Revert the parser change: it broke the nightly build" }
```

| Field | Rules |
| --- | --- |
| `author` | Required. Non-empty after trimming. |
| `message` | Optional. Defaults to `Revert "<summary of {rev}>"`, a blank line, and `This reverts revision <seq> (<id>).` If given, it must be non-empty after trimming. |

`{rev}` is any revision form except `0`, which returns `404`.

Response `200`: a [`CommitResponse`](#post-v1reposrepocommit), for `POST /v1/repos/demo/changesets/2/revert` with
`{"author":"revert-bot"}`:

```json
{
  "changeset": {
    "id": "0aa3c0623715d4188e783ab94c0b43d11f3423440625e5466dfc5957a2452e02",
    "seq": 3,
    "parent": "197289598c6c51354cf8cab25e1f88a2949485907c2e26a48a8cf9c00bdf34eb",
    "base": 2,
    "root_tree": "cf311188aa99cd45418f58c520ec2e88f7061b41c927f75cff83b311cb76a9e7",
    "author": "revert-bot",
    "message": "Revert \"Say hello\"\n\nThis reverts revision 2 (197289598c6c51354cf8cab25e1f88a2949485907c2e26a48a8cf9c00bdf34eb).",
    "timestamp": 1792907944,
    "changes": [
      { "path": "main.rs", "new": { "kind": "file", "blob": "a6177c49027f1ab6cb7b1756c62e2c08772be25903bc8817fe2cb199f442eeba" } }
    ]
  },
  "rebased": false
}
```

The new changeset's `base` is the reverted revision, so `rebased` is `true` whenever other changesets landed after
it. Errors are those of `commit`. Reverting the same revision twice conflicts with the first revert:

```json
{ "code": "conflict", "message": "1 path(s) were changed after revision 2; sync and retry", "paths": ["main.rs"] }
```

---

### `POST /v1/repos/{repo}/commit`

Appends a changeset built from file-level changes made against revision `base`. Every blob referenced by a `set`
change must already be uploaded (see [`objects/missing`](#post-v1reposrepoobjectsmissing) and
[`PUT objects/{id}`](#put-v1reposrepoobjectsid)). The full algorithm is in
[architecture.md §2](architecture.md#2-commit-protocol).

Request (`CommitRequest`):

```json
{
  "base": 4,
  "author": "ci-bot",
  "message": "Add hello.txt",
  "changes": [
    { "path": "hello.txt", "new": { "kind": "file", "blob": "c966255a1935a5c8f54433b1a4165e8aaf05f6fa98fb08312abd60463987b903" } },
    { "path": "old/notes.txt", "new": null }
  ]
}
```

| Field | Rules |
| --- | --- |
| `base` | The revision the changes were made against. Must not exceed head. |
| `author` | Non-empty after trimming. Free-form and unauthenticated. |
| `message` | Non-empty after trimming. |
| `changes` | At least one. Paths must be unique, non-root, and name files. `new: null` deletes. Order doesn't matter, because the server sorts by path. |

Response `200` (`CommitResponse`):

```json
{
  "changeset": {
    "id": "5c2c4838252df9504298c7ecd1efccc663c2d82b50c6f4db99d3b8321a3f6948",
    "seq": 5,
    "parent": "34f9f313993765616962ce3269cb875dffda78983994900c753b76405d36ac2b",
    "base": 4,
    "root_tree": "7f2d15283eff1bde5a2407c50f48b167dffee92aaf4b6ce848e6fff340778e01",
    "author": "ci-bot",
    "message": "Add hello.txt",
    "timestamp": 1792906775,
    "changes": [
      { "path": "hello.txt", "new": { "kind": "file", "blob": "c966255a1935a5c8f54433b1a4165e8aaf05f6fa98fb08312abd60463987b903" } }
    ]
  },
  "rebased": false
}
```

`rebased` is `true` when newer changesets landed between `base` and this one, and the server rebased over them.

Errors:

| Status | Example |
| --- | --- |
| 400 | `{"code":"bad_request","message":"commit message is empty"}`, `"author is empty"`, `"invalid changes: no changes"`, `"invalid changes: duplicate path a"`, `"base revision 9 is newer than head 4"` |
| 409 | Overlap with history since `base`: `{"code":"conflict","message":"1 path(s) were changed after revision 1; sync and retry","paths":["docs/README.md"]}` |
| 409 | Can't apply to head: `{"code":"conflict","message":"path nope does not exist","paths":["nope"]}`, or a file/directory clash such as `"conflict at a: is a directory"` |
| 412 | `{"code":"missing_objects","message":"1 blob(s) must be uploaded before committing","missing":["0000…0001"]}` |
| 503 | `{"code":"unavailable","message":"too many concurrent commits; retry"}` |

A changed path **conflicts** with a path changed since `base` when they are equal, or when one is a directory
ancestor of the other. Conflicts are path-based: nothing is merged.

---

### `GET /v1/repos/{repo}/tree`

Resolves a path at a revision. For a directory it also lists the entries.

| Query | Default | Meaning |
| --- | --- | --- |
| `rev` | `head` | Revision. |
| `path` | root | Path to resolve. |

Response (`TreeResponse`), for `GET /v1/repos/demo/tree?path=src&rev=3`:

```json
{
  "revision": {
    "seq": 3,
    "id": "9081f1cdf03575ad724d8ffb56d76e849928e74b924f0d2228660b3584783f5d",
    "root_tree": "99fd8d7e115a6eda07b0455ec332899d5069ec742847ee45ca86253d9ddb6dd5"
  },
  "path": "src",
  "kind": "tree",
  "id": "e5dedbcf55c35c71e3d8375a83519af7243a7f94ddbc59ef4198eb22094e40b5",
  "entries": [
    { "name": "main.rs", "kind": "file", "id": "983e840d918cd0f2d19877ddaaefe849857945666725c7b9a8ea2212e35a311c" }
  ]
}
```

When `path` names a file, `kind` is its file kind, `id` is its blob id, and `entries` is `[]`. A path that doesn't
exist at that revision returns `404` (`"src/x does not exist at revision 3"`). Calling this with only `rev` is the way
to resolve any revision name to a `Revision`.

---

### `GET /v1/repos/{repo}/file`

Returns a file's raw contents at a revision. For a symlink, it returns the link target.

| Query | Default | Meaning |
| --- | --- | --- |
| `rev` | `head` | Revision. |
| `path` | required in practice | File path. The root and directories return `400` (`"src is a directory"`). |

Response: `200`, `Content-Type: application/octet-stream`, `Cache-Control: no-cache`. A missing path returns `404`.

---

### `GET /v1/repos/{repo}/diff`

Returns the file-level changes that turn revision `from` into revision `to`, computed by comparing their trees.
Unchanged subtrees are skipped, so the cost is proportional to the size of the diff.

| Query | Default | Meaning |
| --- | --- | --- |
| `from` | **required** | Starting revision. A missing `from` is rejected with `400 bad_request`. |
| `to` | `head` | Ending revision. It may be older than `from`, which gives the reverse diff. |
| `path` | root | Only changes at or under this path. |

Response (`DiffResponse`), for `GET /v1/repos/demo/diff?from=1&to=3`:

```json
{
  "from": { "seq": 1, "id": "095d09aafa6f…", "root_tree": "b50a538976a5…" },
  "to":   { "seq": 3, "id": "9081f1cdf035…", "root_tree": "99fd8d7e115a…" },
  "changes": [
    { "path": "docs/README.md", "new": { "kind": "file", "blob": "73955cda44e1bb49cfb8840f4381efa0a4e29c5956396ae55050bb5390d135f5" } },
    { "path": "src/main.rs",    "new": { "kind": "file", "blob": "983e840d918cd0f2d19877ddaaefe849857945666725c7b9a8ea2212e35a311c" } }
  ]
}
```

(Ids are abbreviated here. The API always returns full ids.) `changes` is sorted by path, and applying it to
`from`'s tree yields `to`'s tree.

---

### `GET /v1/repos/{repo}/blame`

For each line of a file at a revision, returns the changeset that introduced it.

| Query | Default | Meaning |
| --- | --- | --- |
| `rev` | `head` | Revision. |
| `path` | required | File path. The root or a directory returns `400`. |

Response (`BlameResponse`), for `GET /v1/repos/demo/blame?path=main.rs`:

```json
{
  "revision": { "seq": 2, "id": "197289598c6c…", "root_tree": "d8b8aad699c9…" },
  "path": "main.rs",
  "lines": [
    { "line_no": 1, "seq": 1, "id": "27613842c6bc…", "author": "alice", "timestamp": 1792907944, "text": "fn main() {" },
    { "line_no": 2, "seq": 2, "id": "197289598c6c…", "author": "bob", "timestamp": 1792907944, "text": "    println!(\"hello\");" },
    { "line_no": 3, "seq": 1, "id": "27613842c6bc…", "author": "alice", "timestamp": 1792907944, "text": "}" }
  ]
}
```

(Ids are abbreviated here.) There is one entry per line, in order. `seq`, `id`, `author` and `timestamp` describe
the changeset that introduced the line. Lines are split on `\n`; `text` excludes the `\n` but keeps a `\r`, and a
final line without a terminator is still a line, so an empty file has no lines.

How lines are attributed:

- A line keeps its origin across versions while a line diff (Myers) between consecutive versions of the file says it
  is unchanged. Changed and inserted lines belong to the changeset that made them. A moved block is attributed to
  the move, since a diff keeps only the longest common subsequence.
- History is per path. A file that is deleted and later re-added starts over: every line belongs to the re-add, even
  if the content is identical. Renames aren't tracked, so a renamed file's lines belong to the rename.
- A change of kind only (for example `file` to `executable`) keeps every line's origin.
- Diffing two versions of a file is bounded to 2 seconds. Past that, the diff (and so the blame) is coarser but still
  correct: more lines are attributed to the newer version.

Errors: `400` for the root, a directory, or a binary file (one containing a NUL byte or not valid UTF-8), and `404`
for a path that doesn't exist at that revision.

Blame is [derived data](architecture.md#3-derived-data) computed lazily and memoized per version of the file, so the
first blame of a long-lived file costs one diff per version of that file and every later one costs one diff per new
version.

---

### `POST /v1/repos/{repo}/objects/missing`

Asks which of the given objects the server doesn't have. Use it before uploading blobs for a commit.

Request (`IdList`, at most 1024 ids):

```json
{ "ids": ["e75d19d63a6aeb93e4b1176fa4b3d402fd931730abdaffa2a641bf64ae133e6e",
          "0000000000000000000000000000000000000000000000000000000000000001"] }
```

Response: the subset that is missing, in request order.

```json
{ "ids": ["0000000000000000000000000000000000000000000000000000000000000001"] }
```

More than 1024 ids returns `400` (`"at most 1024 ids per request"`).

---

### `POST /v1/repos/{repo}/objects/fetch`

Fetches several objects in one response.

Request (`IdList`, at most 1024 ids):

```json
{ "ids": ["e75d19d63a6aeb93e4b1176fa4b3d402fd931730abdaffa2a641bf64ae133e6e"] }
```

Response: `200`, `Content-Type: application/octet-stream`, `Cache-Control: no-cache`. The body is a concatenation of
one frame per requested id, in request order, with duplicates repeated:

```text
frame = id[32]        raw 32-byte object id
        len  u64 LE   length of the object in bytes
        bytes[len]    object contents (raw blob, or encoded tree)
```

The example above returns 54 bytes: a 32-byte id, `0e 00 00 00 00 00 00 00` (length 14), then `hi\nmore\nagain\n`.

If **any** requested object is missing, the whole request fails with `404` and returns no frames. The response
isn't verified by the server. Clients must check each frame's id against its contents, and should reject frames
they didn't ask for. The response is buffered in memory on both sides, so keep batches modest when objects are
large.

---

### `GET /v1/repos/{repo}/objects/{id}`

Returns one object's raw bytes. That is a blob's contents, or a tree in its
[binary encoding](architecture.md#14-trees-and-their-binary-encoding). The id carries no type: the caller knows
whether it asked for a tree or a blob, and verifies the bytes with the matching hash domain.

Response: `200`, `Content-Type: application/octet-stream`,
`Cache-Control: public, max-age=31536000, immutable`. Objects never change, so any HTTP cache may keep them forever.
An unknown object returns `404`, and a malformed id returns `400`.

The object store is shared by all repositories on a server. This route checks that `{repo}` exists, but not that the
object is reachable from that repository.

---

### `PUT /v1/repos/{repo}/objects/{id}`

Uploads one **blob**. The body is the raw file contents (any content type). The server recomputes
`BLAKE3("budwood:blob:v1\0" || u64_le(len) || body)` and rejects a mismatch:

```json
{ "code": "bad_request", "message": "blob hashes to c966…b903, not e75d…3e6e" }
```

Response: `204 No Content`. Uploading an object that already exists also succeeds. Trees can't be uploaded, because
the server builds them from commits.

## Walkthrough: committing with curl

Compute the blob id. Standard tools can't add the domain tag, so here is a short Python example using the
`blake3` package:

```python
import struct, sys
from blake3 import blake3

data = open(sys.argv[1], "rb").read()
h = blake3(b"budwood:blob:v1\0")
h.update(struct.pack("<Q", len(data)))
h.update(data)
print(h.hexdigest())
```

```sh
S=http://127.0.0.1:7171/v1/repos/demo
printf 'hello\n' > hello.txt
ID=$(python3 blobid.py hello.txt)          # c966255a1935a5c8f54433b1a4165e8aaf05f6fa98fb08312abd60463987b903
BASE=$(curl -s $S/head | python3 -c 'import json,sys; print(json.load(sys.stdin)["seq"])')

curl -s -X POST -H 'content-type: application/json' -d "{\"ids\":[\"$ID\"]}" $S/objects/missing
curl -s -X PUT --data-binary @hello.txt $S/objects/$ID
curl -s -X POST -H 'content-type: application/json' $S/commit -d "{
  \"base\": $BASE, \"author\": \"ci-bot\", \"message\": \"Add hello.txt\",
  \"changes\": [{\"path\": \"hello.txt\", \"new\": {\"kind\": \"file\", \"blob\": \"$ID\"}}]
}"
```

If the response is `409 conflict`, re-read the conflicting paths at head, redo the change against the new head, and
commit again with the new `base`.
