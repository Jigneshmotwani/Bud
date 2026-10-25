# Contributing to Budwood

Thanks for your interest! Budwood is early (v0.1), so issues, design discussion, bug reports and pull requests are
all welcome. Formats and APIs are still unstable, so improving them is fair game. Please call out incompatible changes
clearly in your pull request.

## Building and testing

You need a stable Rust toolchain, 1.80 or newer (the workspace's `rust-version`).

```sh
cargo build                                    # all crates and binaries
cargo test                                     # unit tests and end-to-end tests
cargo clippy --all-targets -- -D warnings      # lints; CI treats warnings as errors
cargo fmt --all                                # format (max_width = 120, see rustfmt.toml)
```

CI runs `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` on Linux and macOS
for every push and pull request. Please run them locally before opening a PR.

Unit tests live next to the code they test. End-to-end tests (`crates/*/tests/`) start a real server on a local port
in-process and drive the client library or the `bud` binary against it. A behavior change should come with a test
that would have caught the old behavior.

To try things by hand:

```sh
cargo run -p budwood-server -- --data /tmp/bw serve --repo demo
cargo run -p budwood-client --bin bud -- clone http://127.0.0.1:7171 demo /tmp/demo
```

## Design principles

When in doubt, keep a change consistent with these:

1. **The server does the heavy lifting.** History, trees, log, diff, rebases and future derived data such as blame
   are computed server-side. A client should never need the whole repository or its history.
2. **Clients keep a bounded, verified cache.** Everything the client stores or receives is checked against its
   content address. Don't add paths that trust unverified bytes.
3. **Objects are immutable and content-addressed.** New object kinds get their own hash domain tag. Encodings must
   be canonical, so that decoding and re-encoding gives the same bytes. Immutable responses should stay cacheable.
4. **The API is the product.** Everything `bud` does goes through the documented HTTP API. If you add or change a
   route, update [docs/api.md](docs/api.md) in the same PR. If you change formats or protocols, update
   [docs/architecture.md](docs/architecture.md).
5. **Commits stay cheap and lock-free.** Avoid anything whose cost grows with repository size or history length on
   the commit path, and anything that serializes commits beyond the optimistic append.
6. **Small and boring.** Prefer simple code, few dependencies, no `unsafe`, and clear error messages that tell the
   user what to do next.

Larger features (see the roadmap in the [README](README.md)) are best discussed in an issue first.

## Pull requests

- Keep PRs focused. Separate refactors from behavior changes where practical.
- Describe what changed and why, and note any format or API incompatibility.
- No CLA or DCO sign-off is required.
- By contributing, you agree that your contributions are dual licensed under MIT OR Apache-2.0, as described in the
  [README](README.md#license).

## Naming and branding

Budwood is an independent project inspired by public writing about source control at scale. It is not affiliated
with or endorsed by Anthropic. Please don't add third-party logos or branding, or claims of compatibility with other
companies' internal systems.
