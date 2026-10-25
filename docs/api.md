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
  - A decimal sequence number such as `42`.
  - A full 64-character changeset id. Short ids are not accepted.

  Surrounding whitespace is ignored. A string that is none of these returns `400`. A well-formed revision that
  doesn't exist returns `404`.
- **Repository names** are 1–100 characters from ASCII letters, digits, `-`, `_` and `.`, and may not start with
  `.`. Every `/v1/repos/{repo}/…` route returns `404 not_found` if the repository doesn't exist.
- **Request size:** every request body is limited by `--max-body-bytes` (default 256 MiB), which bounds the size of
  one blob. Larger bodies are rejected with `413`.
- **Batch limit:** `objects/missing` and `objects/fetch` accept at most **1024** ids per request
  (`budwood_core::api::MAX_BATCH`).

## Types

```text
Revision     { "seq": u64, "id": ObjectId | null, "root_tree": ObjectId }   id is null only for seq 0
FileState    { "kind": "file" | "executable" | "symlink", "blob": ObjectId }
Change       { "path": RepoPath, "new": FileState | null }                   null deletes the file
Changeset    { "id", "seq", "parent": ObjectId | null, "base": u64, "root_tree", "author", "message",
               "timestamp": i64 (Unix seconds, server-assigned), "changes": [Change] }
EntryKind    "file" | "executable" | "symlink" | "tree"
ListEntry    { "name": string, "kind": EntryKind, "id": ObjectId }
```

`Changeset.changes` is sorted by path, and each entry names a file. Clients should verify `Changeset.id`
([architecture.md §1.5](architecture.md#15-changesets)), as `bud` does.

## Errors

Errors produced by the service have a JSON body:

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
| 400 Bad Request | `bad_request` | Invalid path, revision, id, repository name, batch size, commit fields, or a blob whose content doesn't match its id. |
| 404 Not Found | `not_found` | Unknown repository, revision, path or object. |
| 409 Conflict | `conflict` | The commit overlaps newer history, or can't be applied to head (deleting a missing file, or a file/directory clash). |
| 412 Precondition Failed | `missing_objects` | The commit references blobs the server doesn't have. |
| 503 Service Unavailable | `unavailable` | The commit lost the append race too many times. Retry. |
| 500 Internal Server Error | `internal` | Storage or other server failure. Details are logged on the server. |

Some errors come from the HTTP framework before the request reaches the service. These have a **plain-text** body
(or none) instead of the JSON shape above:

| Status | Cause |
| --- | --- |
| 400 | Malformed JSON body, or a query string that fails to parse (for example a missing `from` on `/diff`, or a non-numeric `limit`). |
| 415 | A JSON route called without `Content-Type: application/json`. |
| 422 | Well-formed JSON of the wrong shape, including an invalid object id inside a JSON body. |
| 413 | Body larger than `--max-body-bytes`. |
| 404 | Unknown route (empty body). |

Clients should fall back gracefully when an error body isn't JSON. `bud` reports such errors as
`HTTP <status>: <text>`.

## Routes

| Method | Path | Purpose |
| --- | --- | --- |
| GET | [`/v1/health`](#get-v1health) | Liveness check |
| GET | [`/v1/repos`](#get-v1repos) | List repositories |
| POST | [`/v1/repos`](#post-v1repos) | Create a repository |
| GET | [`/v1/repos/{repo}/head`](#get-v1reposrepohead) | Current head revision |
| GET | [`/v1/repos/{repo}/log`](#get-v1reposrepolog) | History, optionally by path |
| GET | [`/v1/repos/{repo}/changesets/{rev}`](#get-v1reposrepochangesetsrev) | One changeset |
| POST | [`/v1/repos/{repo}/commit`](#post-v1reposrepocommit) | Append a changeset |
| GET | [`/v1/repos/{repo}/tree`](#get-v1reposrepotree) | Look up a path or list a directory |
| GET | [`/v1/repos/{repo}/file`](#get-v1reposrepofile) | File contents at a revision |
| GET | [`/v1/repos/{repo}/diff`](#get-v1reposrepodiff) | File-level diff between revisions |
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
| `from` | **required** | Starting revision. A missing `from` is rejected with a plain-text `400`. |
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
