---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0118: Fetches are bounded by fetch-idle, and http contexts and fetches under --network=none are refused

## Context and Problem Statement

[ADR-0079](0079-resource-limits-with-a-limit-flag.md) bounds what a registry
makes ranikust hold or wait for. Phase 5 adds waits it does not cover:
`ADD <url>`, HTTPS and git contexts, an external `git` and a credential helper.
Neither kaniko nor BuildKit bounds any of them. Three questions came up while
planning Phase 5 (see [the roadmap](../../README.md#roadmap)):

- What bounds a fetch?
- Are `http://` contexts accepted?
  [ADR-0008](0008-out-of-scope-bake-warmer-cloud-contexts.md) lists local, tar,
  git and HTTPS contexts, and the CLI's refusal says "HTTP contexts come in
  Phase 5". kaniko accepts no `http://` context; BuildKit does. A plain-HTTP
  context can be swapped in transit and nothing checks it.
- May `ADD <url>` and `ADD <git>` fetch under `--network=none`? They make
  ranikust itself fetch a URL the Dockerfile names. With the default host
  network a `RUN` reaches the same
  ([ADR-0010](0010-run-network-host-or-none.md)); with `--network=none` the
  operator expects the Dockerfile to reach nothing, yet a fetch of the metadata
  service or an internal host would land its answer in the image. BuildKit
  fetches `ADD` sources whatever the `RUN` network, and #25's interim leaves
  ranikust's own registry requests unrestricted.

## Considered Options

What bounds a fetch:

- (a) A new `fetch-idle` (default `2m`) for the silence of any download, clone
  or helper, no size bound (a download is like a layer), at most 10 redirects as
  Go's client.
- (b) (a) plus a `fetch-size` with a default: under ADR-0079's 1 GiB ceiling it
  breaks large legitimate downloads unless the ceiling changes.
- (c) Reuse `registry-idle` for every fetch, no new name.

`http://` contexts:

- (a) Refuse them for good, naming `https://`.
- (b) Accept them, as BuildKit, with a warning.
- (c) Accept them for loopback hosts only, as Q-065 does for registries.

`ADD <url>` and `ADD <git>` under `--network=none`:

- (a) Fetch anyway, as BuildKit, documented like #25.
- (b) Refuse them, naming the flag.
- (c) Fetch, refusing loopback, link-local and private addresses (checked on the
  connected address), as #25's option (a) would.

## Decision Outcome

A new `--limit` name, `fetch-idle`, with the default `2m`, bounds the silence of
any download, clone or helper with no size bound and at most 10 redirects;
`http://` contexts are refused for good, naming `https://`; and `ADD <url>` and
`ADD <git>` are refused under `--network=none`, naming the flag.

The maintainer's answer (2026-11-06: "Tải trong rnk-registry, chặt (khuyến
nghị)", "Download in rnk-registry, strict (recommended)") stood for option (a)
of the bound, option (a) of `http://` contexts and option (b) of
`--network=none`, among other answers
([ADR-0115](0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md)
records the others).

- **`fetch-idle`.** A duration in ADR-0079's table, changed by
  `--limit fetch-idle=<duration>`. It bounds silence, not the whole transfer, as
  `registry-idle` does. The error names what it bounds and how to raise it. A
  download has no size bound, as a layer has none. A fetch follows at most 10
  redirects.
- **`http://` contexts.** Refused for good, with an error that names `https://`.
  Nothing kaniko users had is lost, and an unauthenticated context would decide
  the whole image. The refusal covers the `http://` context scheme only, not the
  `http` protocol that [ADR-0035](0035-git-contexts-built-in-or-external-git.md)
  lets a git context use with `GIT_PULL_METHOD=http`.
- **`--network=none`.** `ADD <url>` and `ADD <git>` are refused under it, naming
  the flag. The operator's intent holds with no new mechanism: such a build
  drops the flag or moves the file into the context.

Not covered: address filtering of ranikust's own requests, which stays #25's,
and the scheme of an `ADD <url>` and the `http` protocol of a git context (this
ADR decides `http://` for contexts only).

### Consequences

- Good, because a stall fails cleanly and the error names what it bounds, while
  nothing legitimate is capped.
- Good, because `--network=none` keeps its meaning for the build as a whole,
  with no address-filtering code to maintain.
- Good, because no unauthenticated context can decide an image.
- Bad, because a download has no size bound: a server that keeps sending runs
  until the disk is full, since `fetch-idle` measures silence only
  **[inferred]**. Revisit with a `fetch-size` if Phase 5's red team shows a
  bound is needed.
- Bad, because a build that uses `ADD <url>` or `ADD <git>` cannot run under
  `--network=none` until it drops the flag or moves the file into the context.
- Neutral, because with the default host network a Dockerfile-named fetch can
  still reach the metadata service or an internal host, as a `RUN` can (#25
  stays open).

## Evidence

- **[external]** kaniko@44aa7ddc0: `ADD <url>` is `http.Get` on the default
  client, with no limit and no auth (`pkg/util/fs_util.go:782-819`); an
  `https://` context is a gzip archive downloaded with no limit, and there is no
  `http://` context (`pkg/buildcontext/https.go:38-76`,
  `buildcontext.go:47-72`).
- **[external]** buildkit@cd8ee244: `RUN --network` and entitlements apply to
  exec operations only; `ADD` sources are fetched by the daemon whatever the
  network mode (`solver/llbsolver/network.go:16-32`,
  `solver/llbsolver/ops/source.go:80-120`); an http(s) context is an archive or
  a single Dockerfile by its first bytes, with no limit
  (`frontend/dockerui/context.go:62-176`, `:293-353`); its `ADD` fetch has no
  redirect policy, timeout or size limit of its own, so Go's defaults apply
  **[inferred]**.
- **[code]** ADR-0079's table and its `--limit` parsing are in
  `crates/ranikust/src/limits.rs`; the CLI refuses fetched contexts, `http://`
  included, before `rnk-context` sees them
  (`crates/ranikust/src/config.rs:695-716`).

## More Information

Answers three Phase 5 questions (the bound of a fetch, `http://` contexts,
fetches under `--network=none`). Extends ADR-0079's table with one name;
ADR-0079 is not rewritten. Related:
[ADR-0010](0010-run-network-host-or-none.md),
[ADR-0008](0008-out-of-scope-bake-warmer-cloud-contexts.md), #25 (address
filtering), #28 (a pull deadline).

Follow-ups, with Phase 5's tasks and not before they are built: the `--limit`
table in the book and the `ranikust` card, the threat model's rows for
Dockerfile-named fetches, the kaniko map, and the `## Changelog` entry. The
code's message for an `http://` context (`crates/ranikust/src/config.rs`) says
"come in Phase 5"; it must refuse it for good and name `https://`.
