# rootle-gitlab

[![ci](https://github.com/rootledev/rootle-gitlab/actions/workflows/ci.yml/badge.svg)](https://github.com/rootledev/rootle-gitlab/actions/workflows/ci.yml)
[![audit](https://github.com/rootledev/rootle-gitlab/actions/workflows/audit.yml/badge.svg)](https://github.com/rootledev/rootle-gitlab/actions/workflows/audit.yml)
[![crates.io](https://img.shields.io/crates/v/rootle-gitlab.svg)](https://crates.io/crates/rootle-gitlab)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

GitLab provider for [rootle](https://rootle.dev) — the first out-of-tree
provider ([plans/0009](https://github.com/rootledev/rootle/blob/main/plans/0009-gitlab-provider.md)).
It speaks rootle's stdio provider protocol (NDJSON-RPC 2.0 over
stdin/stdout, protocol v1.5 — the spec lives in
[doc/provider-protocol.md](https://github.com/rootledev/rootle/blob/main/doc/provider-protocol.md))
against GitLab's REST v4 API. One static binary, no shared code with
rootle: the wire contract is the entire interface.

## Install

rootle's provider manager does it — the released tarball is installed
checksum-verified (against its `.sha256` sidecar) and tracked for
`rootle provider update`/`upgrade`:

```sh
rootle provider install gitlab    # bare name → rootledev/rootle-gitlab releases
rootle provider use gitlab        # writes [provider] into config.toml
```

Self-hosted GitLab? Pass the flags through:

```sh
rootle provider use gitlab -- --instance https://gitlab.example.com
```

The token is read lazily from the environment (never at startup —
rootle may respawn this process many times per session):

```sh
export GITLAB_TOKEN=glpat-…   # scopes: read_api + read_repository
rootle                        # search GitLab, browse, grep, yank URLs
```

## The `q` grammar translation

rootle sends `search/code` queries with GitHub-style qualifiers
verbatim (the protocol's declared PROTOCOL SURFACE). This adapter
translates:

| Qualifier | GitLab |
|---|---|
| `repo:g/r` | project-scoped search endpoint (project resolved + cached) |
| `org:group` | group-scoped search endpoint (subgroups included) |
| `path:x` | client-side path filter (no server equivalent) |
| `extension:rs` | client-side suffix filter (no server equivalent) |
| free terms | `search=` terms |

GitLab's blob search returns real line numbers (`startline`), so hits
arrive `located: true` — no client-side locating pass. Self-managed
instances without advanced search answer 403; the error surfaces
honestly on the status line rather than silently.

## Behavior notes

- **Multi-slash repo ids**: nested groups (`group/subgroup/project`)
  flow through untouched; rootle splits on the first slash (org =
  top-level group, everything after = the repo).
- **Trees** aggregate GitLab's keyset pages up to 25 000 entries, then
  report `truncated: true` — the protocol's honesty mechanism.
- **Blobs** over 1 MiB are refused (rootle's preview cap; refusing
  here saves the transfer).
- **Cache** lives at `~/.cache/rootle/providers/rootle-gitlab/` —
  sha-keyed trees/blobs (immutable), project metadata (revalidated on
  404). Every path component is percent-encoded: branch names with `/`
  are legitimate; `..` never becomes path structure.
- **Revision awareness (protocol v1.5)**: `repo/refs` (branches +
  tags, the default flagged), `repo/tree` at any branch, tag, or
  commit sha (resolved to its sha — the tree cache stays
  content-keyed), `repo/log` (`path`/`ref`/`limit`; the limit rides
  the bounded-compute contract — default 500, cap 1000,
  `truncated: true` past either), `repo/blob_at`, and `repo/blame`
  (adjacent same-sha ranges coalesced). The handshake declares
  capabilities `refs`/`log`/`blame`. `repo/blob_at`'s sha is the git
  blob id of the bytes — the same id family the tree listings carry,
  so the sha feeds straight back into `repo/blob`.
- **Errors** map onto the protocol taxonomy: 401/403 → `auth`,
  429 → `rate_limited` (+ `retry_after_s` from `Retry-After`),
  404 → `not_found`, timeouts → `timeout`, connect failures →
  `network`.

## Development

```
docker compose run --build --rm test     # fmt + clippy -D warnings + wiremock suite
docker compose run --build --rm release  # static musl tarball → ./dist/
```

The wiremock suites (in `src/handlers/`, beside the handlers they
cover) are the offline gate: every protocol method against a scripted
GitLab API, including paginated trees, cache hits, the error taxonomy,
and the qualifier translation. On top of that, this adapter is gated
by [rootledev/forge-conformance](https://github.com/rootledev/forge-conformance)
(v1.5.0, 47 cases) in CI — the canonical provider-conformance suite
(pinned by tag; tags track protocol revisions). `examples/forge.rs`
serves the canonical fixture through a local GitLab mock — plain
directories walked from disk, and the `fixture/vcs` git repo answered
by git itself — and the full FC matrix must stay green. Live
validation against gitlab.com runs as a dispatch-only CI job with a
token secret.

## Advanced: manual setup

Skip the manager — `cargo install rootle-gitlab`, or a prebuilt static
musl tarball from [releases](https://github.com/rootledev/rootle-gitlab/releases)
(linux + macOS, x86_64 + aarch64) — and point rootle at the binary by
hand:

```toml
# ~/.config/rootle/config.toml
[provider]
kind = "stdio"
command = ["rootle-gitlab"]           # self-hosted: append "--instance", "https://gitlab.example.com"
```

## License

MIT
