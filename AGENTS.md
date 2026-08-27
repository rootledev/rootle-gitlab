# AGENTS.md — working agreements for rootle-gitlab

A GitLab provider for rootle: one Rust binary speaking the rootle
stdio provider protocol (NDJSON-RPC 2.0) against GitLab REST v4. The
protocol spec in rootledev/rootle (doc/provider-protocol.md) is the
single source of truth for the wire — this repo never links rootle
code; when the protocol and this adapter disagree, fix one or file
against the other explicitly.

## Build & test — docker first

```
docker compose run --build --rm test      # fmt + clippy -D warnings + wiremock suite
docker compose run --build --rm release   # static musl tarball → ./dist/
```

`--build` after every change or the container runs stale code. Host
`cargo test` is fine for fast iteration; finish with the compose gate.

## Where things are

| Path | Contents |
|---|---|
| `src/main.rs` | arg parsing + the shared stdin loop (`serve_stdio`) |
| `src/lib.rs` | `respond()`/`respond_transcript()` (one line in → reply lines out; tests drive this) + `serve_stdio` |
| `src/handlers.rs` | protocol surface: dispatch, wire error taxonomy, shared helpers |
| `src/handlers/*.rs` | per-method handlers (initialize/search/tree/blob/urls/code), each with the wiremock tests that cover it |
| `src/api.rs` | REST client: lazy token, error taxonomy, page aggregation |
| `src/cache.rs` | disk cache under `~/.cache/rootle/providers/rootle-gitlab/` |
| `tests/version_flag.rs` | the `--version` bin smoke test |
| `examples/forge.rs` | forge-conformance harness: the canonical fixture behind a local GitLab REST v4 mock (plans/0015) |

## Contract rules (violations get caught in review)

- **Startup does nothing**: no network, no token read — rootle kills
  and respawns this process an unbounded number of times per session
  (initialize is cheap and idempotent by protocol obligation).
- **Credentials are lazy** (`GITLAB_TOKEN` by default, `--token-env`
  to override) and cached in-process only.
- **A cache read that cannot be satisfied is a miss, not an error** —
  re-fetch. Trees/blobs are sha-keyed and immutable; project metadata
  revalidates on 404.
- **Every path component is percent-encoded** before it becomes disk
  path structure (branch names with `/` are legitimate; `..` must not
  survive).
- **The q grammar is protocol surface**: `repo:`/`org:`/`path:`/
  `extension:` — changes belong in a protocol-doc PR first, then here.
- **Blobs cap at 1 MiB**; trees cap at 25 000 entries then
  `truncated: true`.

## Workflow

- `main` is protected: PRs only, the `test` check (docker gate)
  required. The wiremock suite must stay offline-deterministic — no
  network in CI tests; live gitlab.com validation is the dispatch-only
  job (`workflow_dispatch` with the `GITLAB_TOKEN` secret).
- The `forge` job runs the canonical conformance suite
  (rootledev/forge-conformance, plans/0015) against `examples/forge.rs`
  — protocol revisions must keep the FC case matrix green.
- Releases: tag `vX.Y.Z` matching Cargo.toml; the release workflow
  builds the 4-target matrix (linux + macOS, x86_64 + aarch64),
  verifies each tarball, publishes to crates.io and the GitHub
  release. No homebrew, no site — rootle's release owns those.
