---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0115: rnk-cache holds keys and transport, and HTTPS downloads live in rnk-registry

## Context and Problem Statement

[#7](https://github.com/tran4774/ranikust/issues/7) (Q-028) lists crate edges
nobody had decided, from Phase 1 to Phase 5: `rnk-context` → `rnk-ignore` and
`rnk-sched` → `rnk-dockerfile` (declared as interim); `rnk-cache` →
`rnk-registry`, `rnk-dockerfile`, `rnk-ignore` and `rnk-layer` (Phase 4);
`rnk-context` → `rnk-layer` for tar contexts and to the TLS set-up that
`rnk-registry` owns since [ADR-0024](0024-tls-rustls-ring.md) (Phase 5); and
`ranikust` → `rnk-registry` for `push`. For each edge the choice is to allow it
or to route it through `rnk-builder`.

Phase 5's HTTPS contexts and `ADD <url>` both need TLS roots, proxies, retries
and limits, which live in `rnk-registry`
([ADR-0079](0079-resource-limits-with-a-limit-flag.md)); tar contexts need the
decoders of `rnk-layer`. The three ways to place the download were:
`rnk-context` fetches itself, through both `rnk-registry` and `rnk-layer`;
`rnk-registry` gains a plain HTTPS download that callers use; or a new crate.
Linked to this is [#6](https://github.com/tran4774/ranikust/issues/6) (Q-027):
is `ContextSource` sync or async? The trait is sync today with one
implementation (`DirSource`), and the registry client is async.

## Considered Options

The option letters for Phase 5 are those of the question put to the maintainer;
#7's text lists only (a) and (b) per edge.

Phase 4 edges, each (a) allowed or (b) routed through `rnk-builder`.

Phase 5, where the download lives:

- (a) `rnk-context` → `rnk-registry` and → `rnk-layer`: `rnk-context` fetches
  itself.
- (b) `rnk-context` → `rnk-layer` only; `rnk-registry` gains a plain HTTPS
  download, which `ranikust` calls for contexts (passing `rnk-context` a reader)
  and `rnk-builder` for `ADD`.
- (c) A new crate for HTTPS downloads, used by `rnk-registry`, `rnk-context` and
  `rnk-builder`; TLS set-up moves out of `rnk-registry`.

`ContextSource`:

- (a) Sync: the network is done by the async client and handed to `rnk-context`
  as a reader or a spooled file; `gix` is blocking anyway.
- (b) Async: every caller in `rnk-builder` and `ranikust` changes, and `gix`
  still needs `spawn_blocking`.

## Decision Outcome

`rnk-cache` may depend on `rnk-registry`, `rnk-oci` and `rnk-rootfs`,
`rnk-builder` on `rnk-cache` and `rnk-context` on `rnk-layer`; the two interim
edges stay; `rnk-registry` gains the one HTTPS download that fetched contexts
and `ADD <url>` share; and `ContextSource` stays synchronous.

For the Phase 4 edges the maintainer answered (2026-11-06: "Registry + OCI
layout an toàn (khuyến nghị)", "Registry plus a safe OCI layout (recommended)"),
which stood for the Phase 4 edges of #7 as the question put to him recommended
them (#7 itself says "Recommendation: none"); and for Phase 5
(2026-11-06: "Tải trong rnk-registry, chặt (khuyến nghị)", "Download in
rnk-registry, strict (recommended)"), which stood for the Phase 5 download
question's option (b) and option (a) of #6.

| Edge | Decision | Reason |
|---|---|---|
| `rnk-cache` → `rnk-registry` | allowed | the registry cache looks entries up and pushes layers |
| `rnk-cache` → `rnk-oci` | allowed | the cache entry's documents and the layout |
| `rnk-cache` → `rnk-rootfs` | allowed | the OCI layout cache reaches a host directory only through `rnk-rootfs` |
| `rnk-builder` → `rnk-cache` | allowed | `rnk-builder` builds the key inputs and drives lookups and pushes |
| `rnk-context` → `rnk-layer` | allowed (Phase 5) | tar contexts need the decoders |
| `rnk-context` → `rnk-ignore` | interim made final | ADR-0027's walk calls `matches_using_parent` and `may_reinclude_below`, which only work inside the walk |
| `rnk-sched` → `rnk-dockerfile` | interim made final | the scheduler walks `StageGraph::needed` and `StageNode::dependencies()` |
| `ranikust` → `rnk-registry` | stays | `push` and the registry client, and in Phase 5 the context download |
| `rnk-cache` → `rnk-dockerfile`, `rnk-ignore`, `rnk-layer` | refused | `rnk-builder` builds `KeyInputs` from parsed instructions and the context walk, so `rnk-cache` stays keys and transport |
| `rnk-context` → `rnk-registry` | refused | no network code in `rnk-context` but `gix` |

- **HTTPS downloads live in `rnk-registry`**, where TLS roots, proxies, retries
  and limits already are. `ranikust` calls the download for a context and gives
  `rnk-context` a reader; `rnk-builder` calls it for `ADD <url>`.
  `rnk-registry`'s boundary widens from "moves blobs and manifests" to include
  it. No new crate; `rnk-layer` stays pure codecs.
- **`ContextSource` stays sync.** Nothing in `rnk-context` awaits anything once
  downloads live in the registry client, and the interim already is (a).
  Downloads come in as a reader or a spooled file.

`rnk-cache` → `rnk-store` is [ADR-0021](0021-rnk-store-crate.md)'s and stays.

Not settled here: how `gix`, the one network client left in `rnk-context`, gets
the TLS roots and proxies that `rnk-registry` owns. It is decided before Phase
5's task 3 ([#142](https://github.com/tran4774/ranikust/issues/142)) is built (a
`question` issue if it needs the maintainer).

### Consequences

- Good, because `rnk-cache` stays keys and transport: it has no Dockerfile
  parser and no ignore matcher to keep in step with `rnk-builder`.
- Good, because there is one HTTPS stack, where TLS already lives, and the only
  network code in `rnk-context` is `gix`.
- Good, because `ContextSource` keeps its signature and no caller changes.
- Bad, because `rnk-registry` is no longer only blobs and manifests: its
  `# Boundaries` section and card widen.
- Bad, because `rnk-context` cannot fetch by itself: `ranikust` and
  `rnk-builder` each call the download and hand the bytes on.
- Neutral, because the two interim edges were reversible one-line dependencies
  on crates that are pure data, and are now final.

## Evidence

- **[code]** Declared today: `rnk-cache` has no internal dependency;
  `rnk-context` depends on `rnk-ignore`, `rnk-rootfs`; `rnk-sched` on
  `rnk-dockerfile`; `ranikust` on `rnk-registry`; `rnk-builder` on
  `rnk-registry` and `rnk-context` but not on `rnk-cache`
  (`crates/*/Cargo.toml`). The sync builder already calls the async registry
  client through a runtime handle (`rnk-builder`, `pull.rs`).
- **[code]** The interim edges' reasons are in #7's text (ADR-0027's walk, the
  scheduler's graph walk); both crates they point at hold only data.
- **[inferred]** `gix` is blocking, so an async `ContextSource` would still need
  `spawn_blocking` for it.

## More Information

Answers #7 (every edge, both phases) and #6. Related:
[ADR-0027](0027-context-filtering-follows-buildkit-walk.md),
[ADR-0024](0024-tls-rustls-ring.md),
[ADR-0079](0079-resource-limits-with-a-limit-flag.md),
[ADR-0035](0035-git-contexts-built-in-or-external-git.md),
[ADR-0067](0067-the-cli-crate-orchestrates-the-build.md). The bounds of a fetch
are
[ADR-0118](0118-fetches-are-bounded-by-fetch-idle-and-http-contexts-and-fetches-under-network-none-are-refused.md)'s.

The markers that named #6 and #7 point to this ADR instead. Follow-ups, when the
edges are declared: each crate's `# Boundaries` section (`rnk-cache`,
`rnk-registry`, `rnk-context`), the crate cards and
[ARCHITECTURE.md](../../ARCHITECTURE.md#intended-dependency-direction).
