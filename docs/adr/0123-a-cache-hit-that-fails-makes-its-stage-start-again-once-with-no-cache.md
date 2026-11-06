---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0123: A cache hit that fails makes its stage start again at most once with no cache

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
[ADR-0081](0081-a-streamed-layer-on-slash-is-verified-once-per-build.md) spools
the compressed blob, and the same pass decompresses it once to check its diff ID,
before `/` changes (`crates/rnk-snapshot/src/kaniko/root.rs:1043-1055`); the
extraction decompresses the spool a second time (`:1062-1065`,
[streaming](../design/streaming.md#credentials-and-failures)). So for a streamed
hit a decompression error or a wrong diff ID is found before `/` changes. The
pass does not read the tar headers, so a hit whose digest and diff ID match can
still fail after its first entry reached `/`: a bad tar header, a header over
`--limit`, an entry the base-layer rules refuse, a full disk, or, for a blob
already in the store, a diff ID that is checked only at the end of the extraction
(`root.rs:1014-1015`). ADR-0113 says nothing about what the builder does next on
`/`.

All five meet invariant 6 of
[#123](https://github.com/tran4774/ranikust/issues/123): a cache lookup or push
that fails is a warning, and the exit status and the image do not change.

## Considered Options

The question window gave the options below; its labels are in Decision Outcome.
Three points were asked twice, the second time to make the answer exact; the
options of both are listed.

How often a stage starts again, and what the restart uses:

- Once, with no cache at all: every step of the restart runs, and its layers are
  pushed under their keys.
- Reuse the start's lookups minus the bad entry (BuildKit-like): the other hits
  still count, but a repository full of bad entries makes a stage restart once
  per hit.
- Keep the other hits and fail the build on a second bad entry: whoever can push
  to the cache can fail builds.

The base of a restarted stage:

- Always rebuild it from the layers of its chain, whether or not the parent's
  directory is still there (the second question; the first asked it for a stage
  whose parent's directory is gone).
- Use the parent's directory when it is still there, and rebuild only when it is
  gone: which path runs depends on scheduling, and a copied base may differ in
  mtimes under `--reproducible` [inferred].
- Check every hit fully before extracting it, for a stage whose parent's
  directory is gone: download and extraction no longer overlap, against
  [ADR-0117](0117-warm-and-partial-builds-are-no-slower-than-kanikos-cache.md).
- Keep a copy of the base when a child has hits: disk and copy time on every warm
  build.

A connection dropped in the middle of a hit:

- Resume with a Range request when the connection drops after the first byte, a
  few attempts without progress (about 3, as BuildKit), then count the entry as a
  miss (the second question; the first asked for a retry within the registry
  client's existing rules, then a miss).
- No resume: a drop after the first byte is a miss at once.
- Retry, then fail the build (BuildKit).
- Fail at once (kaniko).

A bad blob found while pushing a skipped, fully cached stage:

- Build that stage with no cache.
- Fail the build (kaniko).
- Verify every blob before deciding to skip: most of skipping's benefit is lost.

A hit that fails after its first entry reached `/`, the options of #167 (asked
twice):

- (a) Start the stage again from its base, as with a directory per stage.
- (b) Decompress every spooled hit once more to check it before extraction. The
  first question put its cost as a second decompression on every build, against
  ADR-0117; that was wrong, since the spool pass already decompresses the whole
  blob. The second question put it again, as checking the tar headers and
  `--limit` in that same pass, at no extra decompression.
- (c) Fail the build: ruled out by ADR-0113.

## Decision Outcome

A stage whose cache hit turns out bad starts again at most once, with no cache at
all, so that a bad entry costs a miss and a restart and not a failed build, apart
from the cases left open below.

The maintainer answered on 2026-11-07, in the question window, each time "as
recommended":

1. "Một lần, chạy lại không cache (khuyến nghị)" ("Once, run again with no cache
   (recommended)");
2. "Dựng lại base từ chuỗi layer (khuyến nghị)" ("Rebuild the base from the chain
   of layers (recommended)"), then, asked again for a stage whose parent's
   directory is still there, "Restart luôn dựng base từ chuỗi layer (khuyến
   nghị)" ("A restart always rebuilds the base from the chain of layers
   (recommended)");
3. "Thử lại có giới hạn, rồi coi là miss (khuyến nghị)" ("Retry with a limit, then
   count it as a miss (recommended)"), then, asked again for how, "Tải tiếp bằng
   Range, giới hạn (khuyến nghị)" ("Resume with Range, with a limit
   (recommended)");
4. "Build stage đó, không cache (khuyến nghị)" ("Build that stage, with no cache
   (recommended)");
5. on #167, "Bắt đầu lại stage từ base (khuyến nghị)" ("Start the stage again
   from its base (recommended)"), which is option (a), and the restart uses no
   cache, at most once per stage, as for a directory per stage. The question
   gave option (b) a wrong cost, so he was asked again with the cost corrected
   and answered "Kiểm tra header trong spool, lỗi còn lại thì restart (khuyến
   nghị)" ("Check the headers in the spool, restart on the remaining failures
   (recommended)"). Both answers are recorded; the second is the one in force.

- **Restart (1).** A stage starts again at most once after a bad entry. The
  restarted stage uses no cache at all: it looks nothing up and every step runs,
  and each layer it builds is pushed under its key
  ([ADR-0113](0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)'s
  rule on pushing under a key stands). "Bad" is everything ADR-0113's "found bad"
  covers: a decompression error, a bad tar header, a `--limit` overrun, a diff ID
  or digest mismatch. A hit found bad before its first entry reached the stage
  root, such as a request the source refuses, is a plain miss and starts nothing
  again.
- **Base (2).** A restarted stage always rebuilds its base by extracting the
  layers of its chain (the base image's layers, the layers built, the hits) by
  digest: from the store when it holds them, else from the registry that does.
  It does so whether or not the parent's directory
  (ADR-0061, ADR-0062) is still there, so the base depends only on the cache
  state, never on `--jobs` or scheduling
  ([rules](../agents/rules.md#determinism)). This costs time only when an entry
  is bad; hits still stream.
- **Dropped connection (3).** A connection dropped after the first byte of a hit
  being streamed is re-requested from the offset with a Range request, a few
  attempts without progress (about 3, as BuildKit). If it still fails, the entry
  is a miss and the stage starts again as in 1. On `/`, a drop during the spool
  is a plain miss before `/` changes. This is new work in `rnk-registry`, with
  #130. A cache failure does not change the exit status (invariant 6 of #123;
  the cases below are open). For a cache hit only, this changes what
  [streaming](../design/streaming.md#credentials-and-failures) says (a streamed
  extraction fails the build at once); a base layer whose connection drops still
  fails the build.
- **Skipped stage (4).** A bad blob found while pushing the layers of a skipped,
  fully cached stage (ADR-0114) makes the stage stop being skipped: it is built
  with no cache (its base as in 2), and its layers are pushed under their keys. A
  blob mounted across repositories
  ([ADR-0055](0055-stream-layers-keep-nothing-proportional-to-the-image.md)) is
  not read and is trusted by its digest, as the registry holds it: the cache
  repository is trusted as the destination is (threat model T25).
- **On `/` (5).** The spool pass also checks the tar headers and `--limit`, so a
  hit that fails there is a miss before `/` changes: `/` is as it was and no stage
  starts again. Only a failure found while writing makes the stage start again
  from its base: an entry refused under the base-layer rules, a full disk, a blob
  already in the store, whose diff ID is checked at the end of the extraction
  (`root.rs:1014-1015`). The new base empties `/`, as between stages, and the
  restart follows 1. No hit is decompressed one more time. The check of the
  headers is new work in `rnk-snapshot`, not built yet.

Not decided here:

- the names of the spans and fields the restart adds, and the delays between
  retries, which are the registry client's;
- what a restarted stage does when a layer of its chain is held by neither the
  store nor a registry [open: #171], decided before #130 is built;
- four cases of the restart [open: #172], decided before #130 and #132 are built:
  a `COPY --from=<stage>` of a stage that was skipped, which a restart that looks
  nothing up cannot read; a bad blob of a skipped ancestor in the target's `FROM`
  chain, which "its base as in 2" would extract again; a chain layer that passed
  its check once and fails when read again; and the restart on `/` of a stage
  built `FROM <stage>`, whose parent's root `/` cannot give back.

### Consequences

- Good, because a bad entry, a dropped connection and a bad blob in a skipped
  stage cost a miss or a restart and do not change the image; they change the exit
  status only in the cases #171 and #172 leave open.
- Good, because the work is bounded: a stage starts again at most once.
- Good, because hits still stream, and no hit is decompressed one more time on
  `/`: the spool pass that checks the diff ID also checks the headers.
- Good, because the base of a restart depends only on the cache state, never on
  scheduling or `--jobs`.
- Bad, because a bad entry costs the stage's work so far, and the restart runs
  every step.
- Bad, because rebuilding a base re-extracts its chain by digest, also where the
  parent's directory is still there.
- Bad, because ADR-0114 decides which stages are skipped before the build, so the
  builder needs a way to stop skipping one late
  ([#130](https://github.com/tran4774/ranikust/issues/130),
  [#132](https://github.com/tran4774/ranikust/issues/132)).
- Bad, because it is new work beyond the builder: a Range resume in
  `rnk-registry` and a header check in the spool pass of `rnk-snapshot`.
- Neutral, because a restart pushes every layer it builds, not only those behind
  the bad entry.

## Evidence

- **[external]** kaniko@44aa7ddc0: a bad or dropped cached blob fails the build,
  and never runs the step again (`pkg/executor/build.go:827-837`, wrapped at
  `:2060-2063`). The extraction of a cached layer has no retry of the body:
  `--image-fs-extract-retry` wraps only the unpack of the base image
  (`pkg/executor/build.go:640-645`), and the files already written stay in the
  live root (`pkg/util/fs_util.go:471-473`).
- **[external]** buildkit@cd8ee244: a pull is retried with Range requests and a
  backoff of about four attempts
  (`util/resolver/retryhandler/retry.go:20,51-77`); a read that drops is
  re-requested from its offset, and gives up after 3 attempts with no progress
  (`vendor/github.com/containerd/containerd/v2/core/remotes/docker/httpreadseeker.go:29,86-103`).
  A vertex runs again only when loading its cache record fails
  (`solver/edge.go:621-627`, test `solver/scheduler_test.go:3525-3546`). A layer
  is extracted into a throwaway snapshot that is removed on failure
  (`cache/refs.go:1345-1366,1420-1462`).
- **[inferred]** buildkit@cd8ee244: a failed lazy pull fails the build. The lines
  read memoise the error of a content-key computation and fail the edge with it
  (`solver/jobs.go:1131-1134,1185`, `solver/edge.go:700-706`); the pull itself
  (`cache/refs.go:1229-1314`) was not traced.
- **[code]** The registry client's retry rule today: a transfer into a sink that
  took bytes it cannot give back is not retried, and a streamed extraction's
  sink can restart only before its first byte
  (`crates/rnk-registry/src/io.rs:26-30,75-77`,
  `crates/rnk-registry/src/client.rs:610-612`). Resuming a body with a Range
  request is not built.
- **[code]** The spool pass of `/`: it decompresses once to compute the diff ID
  and compares it before `/` changes (`crates/rnk-snapshot/src/kaniko/root.rs:1043-1055`);
  the extraction decompresses the spool again (`:1062-1065`); a blob of the store
  is read as it is and its diff ID is checked at the end of the extraction
  (`:1014-1015`). It reads no tar header.
- **[inferred]** Nothing measures how often a cached blob is bad, how often a
  connection drops, or what a restart or a base rebuild costs; the decision puts
  the cost only where an entry is bad. That a copied base differs in mtimes under
  `--reproducible` is not shown: ADR-0061 says the handed-over and the copied
  path give the same entries.

## More Information

Settles the four points ADR-0113 leaves "decided before the builder's task 7
(#130)" and answers
[#167](https://github.com/tran4774/ranikust/issues/167), which was asked twice.
ADR-0113 stays accepted and is not rewritten; this record refines its "A bad
entry is a miss" and ADR-0114's skipping of fully cached stages, which a bad blob
can undo.

Built by [#130](https://github.com/tran4774/ranikust/issues/130) (the restart,
the base rebuild, retry then miss, the Range resume),
[#132](https://github.com/tran4774/ranikust/issues/132) (a stage that stops being
skipped) and a header check in the spool pass of `rnk-snapshot`; nothing is built
yet.

[cache-and-store](../design/cache-and-store.md#cache-keys),
[kaniko-mode](../design/kaniko-mode.md#a-cache-hit),
[streaming](../design/streaming.md#credentials-and-failures), the threat model's
T26 row and the cards of `rnk-snapshot`, `rnk-builder`, `rnk-cache`, `rnk-sched`
and `rnk-registry` point here as decided and planned. Follow-ups, when #130 and
#132 are built: the same documents say it as built. No user-visible change yet,
so no `## Changelog` entry.
