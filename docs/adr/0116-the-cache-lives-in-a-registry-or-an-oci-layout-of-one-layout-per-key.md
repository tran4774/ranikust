---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0116: The cache lives in a registry or in an OCI layout of one layout per key

## Context and Problem Statement

[ADR-0005](0005-own-versioned-cache-key.md) lets cached layers live in a registry
repository (`--cache-repo`) or in an OCI layout. Three questions are open:

- [#30](https://github.com/tran4774/ranikust/issues/30) (Q-441): how can kaniko mode
  on `/` keep a cache across builds safely? There a `Direct` `RUN` is root on the
  state directory's filesystem, so
  [ADR-0077](0077-kaniko-mode-on-root-never-uses-a-persistent-store.md) keeps nothing
  between builds. A layout cache on `/` would be a local cache too: a `RUN` could
  rewrite it.
- How an OCI layout cache lays out its entries (raised while planning Phase 4, see
  [the roadmap](../../README.md#roadmap)). kaniko opens one layout per key, named
  from the cache repository plus `:<key>`, which puts entries beside the directory
  the operator named [inferred from kaniko's call sites]. Two builds may write one
  directory at once.
- What `--compressed-caching=false` means. In kaniko it pushes cached layers
  uncompressed, to save memory on large builds. ranikust streams layers and keeps
  nothing in proportion to the image
  ([ADR-0055](0055-stream-layers-keep-nothing-proportional-to-the-image.md)), and one
  encoded blob serves the image and the cache.

## Considered Options

Kaniko mode on `/` (#30):

- (a) A store out of every `RUN`'s reach.
- (b) Re-checked content, with digests keyed outside the container.
- (c) A per-build snapshot of the cache.
- (d) A registry cache only.

The layout:

- (a) kaniko's: one layout beside `<dir>` per key. No shared file, but entries live
  outside the directory named.
- (b) One layout `<dir>` with every entry in its `index.json`: blobs shared, but
  `index.json` is rewritten under a lock file.
- (c) One layout per key inside `<dir>` (`<dir>/<tag>/`), each made in a temporary
  directory and renamed into place: no lock and nothing shared, but a blob used by
  two entries is stored twice.

`--compressed-caching=false`:

- (a) Accept `true`, refuse `false` as unneeded, saying why
  ([ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md)).
- (b) Accept `false` as a second, uncompressed encoding for the cache: a second
  encode and upload per layer.
- (c) Accept both and ignore `false`, which ADR-0049 refuses a flag to do.

## Decision Outcome

The layer cache lives in a registry repository or in an OCI layout on the host that
holds one layout per key, published by `rename` alone, and kaniko mode on `/` keeps
only a registry cache: `--cache-repo=oci:<dir>` is refused there.

The maintainer, 2026-11-06, in the question window: "Registry + OCI layout an toàn
(khuyến nghị)" ("Registry plus a safe OCI layout (recommended)"). It stood for
option (d) of #30 for Phase 4 with `oci:` refused on `/`, option (c) of the layout
question and option (a) of the `--compressed-caching` question.

- **Kaniko mode on `/` (#30 (d)).** It keeps only a registry cache, and
  `--cache-repo=oci:<dir>` is refused there, before any work. The registry cache
  needs credentials no `RUN` reaches
  ([ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)). Options
  (a) to (c) of #30 stay open: the issue stays open in the Backlog for a local cache
  there.
- **The layout (c).** An entry is a layout of its own in `<dir>/<tag>/`, made in a
  temporary directory and renamed into place. The layout cache publishes only by
  `rename`: no lock file, nothing shared between entries, nothing written outside the
  directory the operator named.
- **`--compressed-caching` (a).** `true`, the default, is accepted; `false` is
  refused, saying that the memory it saves in kaniko is not spent in ranikust.

### Consequences

- Good, because kaniko mode on `/` has no local cache for a `RUN` to rewrite, so
  ADR-0077 stands.
- Good, because the layout cache publishes as the store does, by `rename`, with no
  lock to hold or recover and nothing outside the operator's directory.
- Good, because there is no second, uncompressed encoding to encode, upload and key.
- Bad, because the default `docker run` and most pods have no local cache: a build
  there is cold every time unless it has a registry cache.
- Bad, because a blob used by two entries of a layout is stored twice.
- Bad, because a kaniko command line with `--compressed-caching=false` fails; the
  message says why, and the flag can be dropped.
- Neutral, because the layout is not kaniko's (ADR-0005: no kaniko cache
  compatibility).

## Evidence

- **[external]** kaniko@44aa7ddc0: the `oci:` prefix on the cache repository
  (`pkg/executor/build.go:304-306`) is read by `LayoutCache`, one layout per entry
  (`pkg/cache/cache.go:141-184`), and written with `NoPush` forced
  (`pkg/executor/push.go:471-474`); `--compressed-caching` defaults to true
  (`cmd/executor/cmd/root.go:361`) and, false, pushes cached layers uncompressed
  (`pkg/executor/push.go:410-412`).
- **[code]** Kaniko mode on `/` keeps nothing between builds and refuses a
  persistent store (ADR-0077); credentials there are verified-only (ADR-0051).
- **[inferred]** One layout per key beside the directory is read from those kaniko
  call sites, not from a run.

## More Information

Answers #30 in part: (d) is the Phase 4 answer, and #30 stays open for (a) to (c),
which this ADR does not decide; the markers that name #30 stay. Answers two Phase 4
questions (the layout, `--compressed-caching`). Related:
[ADR-0005](0005-own-versioned-cache-key.md),
[ADR-0113](0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)
(the entry),
[ADR-0115](0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md)
(`rnk-cache` → `rnk-rootfs`).

Follow-ups, with Phase 4's tasks and not before they are built: the
[cache-and-store](../design/cache-and-store.md) and
[kaniko-mode](../design/kaniko-mode.md) design docs, the threat model, the book's
usage page and the kaniko map, and the `## Changelog` entry.
