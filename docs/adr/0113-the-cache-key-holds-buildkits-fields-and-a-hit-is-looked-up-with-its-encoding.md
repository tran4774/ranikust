---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0113: The cache key holds BuildKit's fields, and a hit is looked up with its encoding

## Context and Problem Statement

[ADR-0005](0005-own-versioned-cache-key.md) fixes the shape of a layer cache
key, `key(k) = H(version, key(k-1), instruction, inputs(k))` (spelled out in
[cache-and-store](../design/cache-and-store.md#cache-keys)), which never depends
on the snapshot mode, the host, the time or the thread count. It leaves the rest
to Phase 4: the field list, `H`, the form of a cache entry and its tag
([#13](https://github.com/tran4774/ranikust/issues/13), Q-041). kaniko keys the
unexpanded instruction text, the environment and every declared `ARG` for a
`RUN` only, and `COPY` files by an md5 of mode, owner and content. BuildKit keys
the whole `RUN` operation, and `COPY` files by a SHA-256 of content and headers,
never mtime. Issue #13's note of 2026-11-01 adds that the key must tell a cache
mount that [ADR-0057](0057-direct-run-heredoc-from-memfd-and-refusals.md) drops
from one that was given.

A second question came up while planning Phase 4 (see [the
roadmap](../../README.md#roadmap)): the bytes of a cached layer depend on the
compression, its level and the file times (now, or the `--reproducible` epoch).
kaniko's key holds none of them and appends a hit as it is, so a warm build can
change an image's digest and break `--reproducible`.

A third question came from Phase 4's task 2
([#125](https://github.com/tran4774/ranikust/issues/125)), the key's code: under
`--reproducible`, a file newer than the epoch has its mtime clamped to it, but a
context file older than the epoch keeps its own mtime, and that mtime is in the
layer. A key with no mtime would let a warm build give another digest than a
cold one.

A fourth question came from Phase 4's task 5
([#128](https://github.com/tran4774/ranikust/issues/128)): in kaniko mode with a
directory per stage, a cached layer is extracted while it streams, and a blob
with a wrong digest is known to be bad only at its end, when the stage root is
already broken. That contradicts a bad entry being a miss and never a failed
build. On `/` the layer is verified before extraction, so it does not arise
there.

## Considered Options

The key's fields (the option letters are those of the question put to the
maintainer; #13's text lists only the choice of `H`):

- (a) BuildKit's fields
  ([ADR-0032](0032-observable-behaviour-follows-buildkit.md)), hashed with
  SHA-256.
- (b) kaniko's fields: the unexpanded text, the environment for `RUN` only.
- (c) (a) with only the build arguments a step uses.

(b) can reuse a wrong layer: a changed `$VAR` in a `COPY` or `WORKDIR` keeps its
key. (c) needs a use analysis of every shell command, which cannot be exact.

What a lookup uses:

- (a) The key plus the encoding (compression, level, time policy and epoch).
- (b) The key alone, re-encoding a hit to this build's encoding: one entry for
  all builds, but a decompress, a tar rewrite and a compress per hit.
- (c) The key alone, the entry used as it is (kaniko): free, but a warm build
  may differ from a cold one.

A file's mtime (the question of 2026-11-07, which this record carries only as
the option the maintainer chose):

- Under `--reproducible` only, the mtime of a file older than the epoch goes
  into the key: images do not change, and the cost is a few misses when such a
  file's mtime changes, which is rare.

A blob found bad after its extraction began (the question of 2026-11-07, raised
by task 5, #128):

- (a) Stream, and on a bad blob run the stage again: drop the stage's root and
  run the stage again from its base without that entry, so it counts as a miss.
  Download and extraction still overlap, as fast as kaniko
  ([ADR-0117](0117-warm-and-partial-builds-are-no-slower-than-kanikos-cache.md)).
  The extra cost comes only with a bad blob, and the work is in the builder's
  task, task 7 ([#130](https://github.com/tran4774/ranikust/issues/130)).
- (b) Verify first, then extract, as on `/`: the blob goes to the store and its
  digest is checked before extraction. Simple, and a bad blob is a miss, but
  download and extraction no longer overlap, so a hit on a large layer may be
  slower than kaniko.
- (c) A bad blob fails the build: simplest, but anyone who can push to the cache
  repository can then fail builds, and the promise that a bad entry is a miss
  must change.

## Decision Outcome

A cache key is made of BuildKit's fields hashed with SHA-256, a cache entry is a
one-layer OCI image found by a tag `rnk-v<version>-<hex>`, and a lookup uses the
key together with the layer's encoding, so a warm build gives the cold build's
digests; under `--reproducible` the key also holds the mtime of each copied file
older than the epoch, and a hit found bad after its extraction began makes the
stage start again from its base without that entry.

The maintainer's answer (2026-11-06: "Kiểu BuildKit + encoding (khuyến nghị)",
"BuildKit style plus encoding (recommended)") stood for option (a) of #13 with
the recommendations put with (a) (the entry, the tag, the TTL and
bad-entry-is-a-miss), and option (a) of the encoding question.

On 2026-11-07, asked what the key does with a file's mtime (the question
raised by task 2, #125), he answered "Đưa mtime đã clamp vào key (khuyến nghị)"
("Put the clamped mtime into the key (recommended)"). It stood for the option
that puts, only under `--reproducible`, the mtime of a file older than the epoch
into the key.

On the same day, asked what a blob found bad after its extraction began does
(the question raised by task 5, #128), he answered "Stream, hỏng thì làm lại
stage (khuyến nghị)" ("Stream, and if it is bad redo the stage (recommended)").
It stood for option (a) of that question.

- **Fields.** `H` is SHA-256 and the formula is ADR-0005's (see
  [cache-and-store](../design/cache-and-store.md#cache-keys)). A step's inputs
  are:
  - the instruction after expansion;
  - every declared `ARG` and the environment in effect;
  - `USER`, `WORKDIR` and `SHELL`;
  - for a `RUN`, its network and its mounts: a secret's id, target, mode and
    owner (never its value), a cache mount's id and sharing, and a cache mount
    marked as dropped where ADR-0057 drops it;
  - for `COPY` and `ADD`, each file's path relative to the source, content,
    mode, uid and gid after `--chown`, type, link target and
    `security.capability`, never mtime, except under `--reproducible`: the mtime
    of a file older than the epoch;
  - for `COPY --from=<image>`, that image's platform manifest digest.

  How the mtime of a file newer than the epoch is encoded is
  [#125](https://github.com/tran4774/ranikust/issues/125)'s choice, and it gives
  the same hits either way, since the epoch is already in the lookup. Every
  step's key holds the declared `ARG`s, the environment, `USER`, `WORKDIR`
  and `SHELL`, which makes it a superset of BuildKit's fields for a step that
  does not read them: safe, and it only costs misses. The chain root is the
  base's platform manifest digest together with the platform. The input of
  `COPY --from=<stage>` is
  [ADR-0114](0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)'s.
- **Entry.** A one-layer OCI image manifest whose config `created` is the push
  time. `--cache-ttl` (kaniko's flag, kaniko's meaning, ADR-0011) compares with
  it, and a hit does not refresh it. Annotations name the key, `KEY_VERSION` and
  the encoding.
- **Tag.** `rnk-v<version>-<hex>`, so an operator tells ranikust's entries from
  kaniko's. The `<hex>` is made from the key and the encoding together.
- **A bad entry is a miss.** An entry that fails its checks (its form, its
  annotations, `--limit`) is a miss with a warning, never a failed build, so a
  hostile repository cannot fail builds. An entry older than `--cache-ttl` is a
  plain miss with no warning, as in kaniko. In kaniko mode with a directory per
  stage, a hit streams into the stage root, and a blob found bad after
  extraction started makes the stage start again from its base without that
  entry, counted as a miss. "Found bad" covers every check of the blob: a
  decompression error, a bad tar header, a `--limit` overrun, or a diff ID or
  digest mismatch, which is known only at the blob's end. On `/`, a hit is
  verified before extraction.
- **Encoding.** A lookup carries the compression, its level, the time policy and
  the epoch. Builds with other settings share no entries, and the key itself
  stays ADR-0005's.
- **A layer is pushed under a key only if the step read what the key names.** A
  `COPY` or `ADD` whose files changed after they were hashed is not pushed under
  that key. The context is a live directory of a lower-trust owner
  ([ADR-0076](0076-the-build-context-owner-is-a-lower-trust-principal.md)).

Not decided here: the keys of `ADD <url>` and of `RUN --mount=type=bind`, which
come with those Phase 5 features, and how `<hex>` is derived from the key and
the encoding. It is decided before Phase 4's task 2
([#125](https://github.com/tran4774/ranikust/issues/125)) is built (a `question`
issue if it needs the maintainer), and the `rnk-cache` card records it.

Also not decided here, each decided before the builder's task 7
([#130](https://github.com/tran4774/ranikust/issues/130)) is built (a `question`
issue if it needs the maintainer):

- How often a stage may start again, and whether the restarted stage queries the
  repository again or reuses the lookups made at the start (the other hits still
  count). Without a bound, a repository full of bad entries could make a stage
  restart once per hit.
- How a restarted stage gets its base back when the parent's directory was
  handed over or released early
  ([ADR-0061](0061-per-stage-directories-hand-over-to-the-last-reader-and-release-early.md),
  [ADR-0062](0062-stage-directory-hand-over-decided-from-the-stage-graph.md)):
  "start again from its base" then has no base left.
- What a connection dropped in the middle of a hit does:
  [streaming](../design/streaming.md#credentials-and-failures) fails the build
  at once for a streamed extraction, which is not a bad blob.
- What a bad blob found while pushing the layers of a skipped, fully cached
  stage does
  ([ADR-0114](0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)):
  there is no stage root to restart.

### Consequences

- Good, because no wrong layer is reused: a changed `$VAR` in a `COPY` or a
  `WORKDIR` changes the key.
- Good, because a cache never changes the output: with `--reproducible`, a cold,
  a warm and a partial build give one digest, at no cost per hit.
- Good, because a bad entry costs a miss and never a failed build, so nobody who
  can push to the repository can fail a build with it.
- Good, because in kaniko mode with a directory per stage a hit still streams,
  so download and extraction overlap, as fast as kaniko.
- Bad, because more misses than kaniko on cosmetic changes: the build
  environment (the declared `ARG`s, the environment, `USER`, `WORKDIR` and
  `SHELL`) is in the key of every step, not only of a `RUN`, used or not.
- Bad, because builds with another compression, level or time policy share no
  entries, so the repository holds one entry per encoding in use.
- Bad, because under `--reproducible` a change to the mtime of a file older than
  the epoch is a miss, which is rare.
- Bad, because in kaniko mode with a directory per stage a bad blob costs the
  stage's work so far: the stage starts again, and the bound on restarts is not
  decided yet.
- Bad, because on `/` a hit is spooled and checked before extraction
  ([ADR-0081](0081-a-streamed-layer-on-slash-is-verified-once-per-build.md)), so
  the no-slower target of
  [ADR-0117](0117-warm-and-partial-builds-are-no-slower-than-kanikos-cache.md)
  has no overlap of download and extraction to rely on there.
- Neutral, because users moving from kaniko start with a cold cache, as ADR-0005
  accepted.

## Evidence

- **[external]** kaniko@44aa7ddc0 (the benchmark fork; which lines it changed
  from upstream was not checked): the key holds the unexpanded text, the
  environment and every declared `ARG` for `RUN` only, and each `COPY` file by
  its md5 of mode, owner and content (`pkg/executor/build.go:322-374`,
  `pkg/util/util.go:78-160`; its README, lines 1491-1496, says a changed `$VAR`
  can reuse a wrong layer). The TTL compares with the config's `created`, set at
  push time and not refreshed by a hit (`pkg/cache/cache.go:116-127`,
  `pkg/executor/push.go:447-451`); a wrong layer count in an entry fails the
  build (`pkg/commands/cache.go:27-56`, read once); a hit is appended as it is
  (`pkg/executor/build.go:861`).
- **[external]** buildkit@cd8ee244: the `RUN` key is the JSON of the whole exec
  operation and the platform (`solver/llbsolver/ops/exec.go:114-188`); secret
  mounts carry id, target, mode and owner, never the value
  (`frontend/dockerfile/dockerfile2llb/convert_secrets.go:14-90`); every
  declared `ARG` with a value is in a `RUN`'s environment, used or not
  (`frontend/dockerfile/dockerfile2llb/convert.go:1682-1730`); a `COPY` file
  hash holds mode, uid, gid, size, type, link name, `security.capability`,
  content SHA-256 and never mtime (`cache/contenthash/tarsum.go:38-72`); it
  holds no name either: a file is hashed with an empty name, and its name comes
  from the directory scan, which writes it into the directory's hash
  (`cache/contenthash/filehash.go:53`, `cache/contenthash/checksum.go:911`); a
  base image is keyed by manifest digest and platform
  (`source/containerimage/pull.go:62-84`).
- **[code]** `rnk-cache` holds no key yet; ADR-0057 drops a cache mount for a
  `Direct` `RUN`. Under `--reproducible` the layer writer clamps a later mtime
  to the epoch (`crates/rnk-layer/src/writer.rs:567-569`) and keeps an earlier
  one (`:54-59`), so the mtime of an older file is in the layer.
- **[inferred]** Nothing measures the miss rate of (a) against (b): W2 has never
  been built with a cache
  ([ADR-0005](0005-own-versioned-cache-key.md#evidence)). Nor does anything
  measure how often a cached blob is bad or what a restarted stage costs; the
  question puts the cost as coming only with a bad blob.

## More Information

Refines ADR-0005, which stays accepted: it fixes the field list that ADR-0005
left to Phase 4. Answers #13, the Phase 4 question on the encoding, the
question on a file's mtime (2026-11-07, raised by #125) and the question on a
blob found bad after its extraction began (2026-11-07, raised by #128). Related:
[ADR-0032](0032-observable-behaviour-follows-buildkit.md) (BuildKit breaks the
tie), [ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md),
[ADR-0079](0079-resource-limits-with-a-limit-flag.md) (every document read is
bounded).

The rule on pushing a layer is invariant 3 of
[#123](https://github.com/tran4774/ranikust/issues/123) (Phase 4), recorded here
with the key because it is a condition on pushing under one.

[cache-and-store](../design/cache-and-store.md#cache-keys) and the `rnk-cache`
card already point here. Follow-ups, with Phase 4's tasks and not before they
are built: the threat model's T13 row, the book's usage page, and the
`## Changelog` entry; and, with the restart
([#130](https://github.com/tran4774/ranikust/issues/130)),
[streaming](../design/streaming.md#cache-phase-4) (the Cache section and the
failures), [kaniko-mode](../design/kaniko-mode.md) and the `rnk-builder` card.
