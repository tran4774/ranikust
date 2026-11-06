---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0123: A cache hit that fails makes its stage start again at most once with no cache, and never changes the exit status

## Context and Problem Statement

[ADR-0113](0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)
makes a bad cache entry a miss with a warning, never a failed build; with a
directory per stage, a blob found bad after its extraction began makes the stage
start again from its base without that entry. It lists four points "decided
before the builder's task 7
([#130](https://github.com/tran4774/ranikust/issues/130))":

- how often a stage may start again, and whether the restart looks the cache up
  again or reuses the lookups made at the start;
- how a restarted stage gets its base back when the parent's directory was
  handed over or released early
  ([ADR-0061](0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md),
  [ADR-0062](0062-stage-directory-hand-over-decided-from-the-stage-graph.md));
- what a connection dropped in the middle of a hit does:
  [streaming](../design/streaming.md#credentials-and-failures) fails the build
  at once for a streamed extraction;
- what a bad blob found while pushing the layers of a skipped, fully cached
  stage does
  ([ADR-0114](0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)):
  there is no stage root to restart.

A fifth came with
[#167](https://github.com/tran4774/ranikust/issues/167), from the snapshotter's
application of a hit in kaniko mode
([#128](https://github.com/tran4774/ranikust/issues/128)). On `/`,
[ADR-0081](0081-a-streamed-layer-on-slash-is-verified-once-per-build.md) checks
the spooled compressed blob before `/` changes. A hit whose digest matches can
still fail after its first entry reached `/`: a decompression error, a bad tar
header, a header over `--limit`, or a diff ID that does not match, known only at
the layer's end. ADR-0113 says nothing about what the builder does next on `/`.

All five meet invariant 6 of
[#123](https://github.com/tran4774/ranikust/issues/123): a cache lookup or push
that fails is a warning, and the exit status and the image do not change.

## Considered Options

The question window gave the options below; its labels are in Decision Outcome.

How often a stage starts again, and what the restart uses:

- Once, with no cache at all: every step of the restart runs, and its layers are
  pushed under their keys, replacing the bad entries.
- Reuse the start's lookups minus the bad entry (BuildKit-like): the other hits
  still count, but a repository full of bad entries makes a stage restart once
  per hit.
- Keep the other hits and fail the build on a second bad entry: whoever can push
  to the cache can fail builds.

The base of a restarted stage whose parent's directory is gone:

- Rebuild it from the layers of its chain, by digest.
- Check every hit fully before extracting it, for such stages: download and
  extraction no longer overlap, against
  [ADR-0117](0117-warm-and-partial-builds-are-no-slower-than-kanikos-cache.md).
- Keep a copy of the base when a child has hits: disk and copy time on every warm
  build.

A connection dropped in the middle of a hit:

- Retry with a limit, then count the entry as a miss.
- Retry, then fail the build (BuildKit).
- Fail at once (kaniko).

A bad blob found while pushing a skipped, fully cached stage:

- Build that stage with no cache.
- Fail the build (kaniko).
- Verify every blob before deciding to skip: most of skipping's benefit is lost.

A hit that fails after its first entry reached `/` (the options of #167):

- (a) Start the stage again from its base, as with a directory per stage.
- (b) Check everything before the first entry reaches `/`: decompress every
  spooled hit once more, checking its diff ID and its tar headers against
  `--limit`. A second decompression on every build, against ADR-0117.
- (c) Fail the build: ruled out by ADR-0113.

## Decision Outcome

A stage whose cache hit turns out bad starts again at most once, with no cache at
all, so that nothing in the cache can fail a build, change its exit status or
change its image.

The maintainer answered on 2026-11-07, in the question window, each time "as
recommended":

1. "Một lần, chạy lại không cache (khuyến nghị)" ("Once, run again with no cache
   (recommended)");
2. "Dựng lại base từ chuỗi layer (khuyến nghị)" ("Rebuild the base from the chain
   of layers (recommended)");
3. "Thử lại có giới hạn, rồi coi là miss (khuyến nghị)" ("Retry with a limit, then
   count it as a miss (recommended)");
4. "Build stage đó, không cache (khuyến nghị)" ("Build that stage, with no cache
   (recommended)");
5. on #167, "Bắt đầu lại stage từ base (khuyến nghị)" ("Start the stage again
   from its base (recommended)"), which is option (a); his comment on #167 adds
   that the restart uses no cache, at most once per stage, as for a directory per
   stage.

- **Restart (1).** A stage starts again at most once after a bad entry. The
  restarted stage uses no cache at all: it looks nothing up and every step runs,
  and each layer it builds is pushed under its key, replacing the bad entries
  ([ADR-0113](0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)'s
  rule on pushing under a key stands). "Bad" is everything ADR-0113's "found bad"
  covers: a decompression error, a bad tar header, a `--limit` overrun, a diff ID
  or digest mismatch.
- **Base (2).** With a directory per stage, when the parent's directory was
  handed over or released early (ADR-0061, ADR-0062), the restarted stage
  rebuilds its base by extracting the layers of its chain (the base image's
  layers, the layers built, the hits) by digest: from the store when it holds
  them, else from the registry that does. This costs time only when an entry is
  bad; hits still stream.
- **Dropped connection (3).** A connection dropped in the middle of streaming a
  hit is retried within the registry client's existing retry rules, resuming
  where it can. If it still fails, the entry is a miss and the stage starts again
  as in 1. A cache failure never changes the exit status (invariant 6 of #123).
  For a cache hit only, this changes what
  [streaming](../design/streaming.md#credentials-and-failures) says (a streamed
  extraction fails the build at once); a base layer whose connection drops still
  fails the build.
- **Skipped stage (4).** A bad blob found while pushing the layers of a skipped,
  fully cached stage (ADR-0114) makes the stage stop being skipped: it is built
  with no cache (its base as in 2), its layers replace the bad entries, and its
  manifest is pushed last. A blob mounted across repositories is not read and is
  trusted by its digest, as the registry holds it: the cache repository is
  trusted as the destination is (threat model T25).
- **On `/` (5).** A hit whose digest matched (ADR-0081 checks the spooled blob
  before `/` changes) but that fails after its first entry reached `/` makes the
  stage start again from its base. The new base empties `/`, as between stages,
  and the restart follows 1.

Not decided here: the number of retries and their delays, which are the registry
client's; the names of the spans and fields the restart adds; and what a
restarted stage does when a layer of its chain is held by neither the store nor
a registry [open: #171], decided before #130 is built.

### Consequences

- Good, because no cache content can fail a build or change an image: a bad
  entry, a dropped connection and a bad blob in a skipped stage all end in a build
  with no cache.
- Good, because the work is bounded: a stage runs at most twice, whatever the
  cache holds.
- Good, because hits still stream and no hit is decompressed twice on `/`: the
  cost comes only when an entry is bad (ADR-0117).
- Bad, because a bad entry costs the stage's work so far, and the restart runs
  every step.
- Bad, because rebuilding a base re-extracts its chain by digest.
- Bad, because ADR-0114 decides which stages are skipped before the build, so the
  builder needs a way to stop skipping one late
  ([#130](https://github.com/tran4774/ranikust/issues/130),
  [#132](https://github.com/tran4774/ranikust/issues/132)).
- Neutral, because a restart's pushes replace the bad entries.

## Evidence

- **[external]** kaniko@44aa7ddc0: a bad or dropped cached blob fails the build,
  and never runs the step again (`pkg/executor/build.go:827-837`, wrapped at
  `:2060-2063`). The extraction of a cached layer has no retry of the body:
  `--image-fs-extract-retry` wraps only the unpack of the base image
  (`pkg/executor/build.go:640-645`), and the files already written stay in the
  live root (`pkg/util/fs_util.go:471-473`).
- **[external]** buildkit@cd8ee244: a pull is retried with Range requests and a
  backoff of about four attempts
  (`util/resolver/retryhandler/retry.go:20,51-77`;
  `vendor/github.com/containerd/containerd/v2/core/remotes/docker/httpreadseeker.go:29,86-103`).
  A vertex runs again only when loading its cache record fails
  (`solver/edge.go:621-627`, test `solver/scheduler_test.go:3525-3546`). A failed
  lazy pull fails the build (`solver/jobs.go:1131-1134,1185`,
  `solver/edge.go:700-706`). A layer is extracted into a throwaway snapshot that
  is removed on failure (`cache/refs.go:1345-1366,1420-1462`).
- **[code]** The registry client's retry rule today: a transfer into a sink that
  took bytes it cannot give back is not retried, and a streamed extraction's
  sink can restart only before its first byte
  (`crates/rnk-registry/src/io.rs:26-30,75-77`,
  `crates/rnk-registry/src/client.rs:610-612`). Today a connection dropped after
  the first byte of a streamed hit is therefore not retried; resuming a body is
  not built.
- **[inferred]** Nothing measures how often a cached blob is bad, how often a
  connection drops, or what a restart or a base rebuild costs; the decision puts
  the cost only where an entry is bad.

## More Information

Settles the four points ADR-0113 leaves "decided before the builder's task 7
(#130)" and answers
[#167](https://github.com/tran4774/ranikust/issues/167). ADR-0113 stays accepted
and is not rewritten; this record refines its "A bad entry is a miss" and
ADR-0114's skipping of fully cached stages, which a bad blob can undo.

Built by [#130](https://github.com/tran4774/ranikust/issues/130) (the restart,
the base rebuild, retry then miss) and
[#132](https://github.com/tran4774/ranikust/issues/132) (a stage that stops being
skipped); nothing is built yet.

[cache-and-store](../design/cache-and-store.md#cache-keys),
[kaniko-mode](../design/kaniko-mode.md#a-cache-hit),
[streaming](../design/streaming.md#credentials-and-failures), the threat model's
T26 row and the cards of `rnk-snapshot`, `rnk-builder`, `rnk-cache` and
`rnk-registry` point here as decided and planned. Follow-ups, when #130 and #132
are built: the same documents say it as built. No user-visible change yet, so no
`## Changelog` entry.
