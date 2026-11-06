---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0113: The cache key holds BuildKit's fields, and a hit is looked up with its encoding

## Context and Problem Statement

[ADR-0005](0005-own-versioned-cache-key.md) fixes the shape of a layer cache key,
`key(k) = H(version, key(k-1), instruction, inputs(k))` (spelled out in
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

A second question came up while planning Phase 4 (see
[the roadmap](../../README.md#roadmap)): the bytes of a cached layer depend on
the compression, its level and the file times (now, or the `--reproducible`
epoch). kaniko's key holds none of them and appends a hit as it is, so a warm
build can change an image's digest and break `--reproducible`.

## Considered Options

The key's fields (the option letters are those of the question put to the
maintainer; #13's text lists only the choice of `H`):

- (a) BuildKit's fields ([ADR-0032](0032-observable-behaviour-follows-buildkit.md)),
  hashed with SHA-256.
- (b) kaniko's fields: the unexpanded text, the environment for `RUN` only.
- (c) (a) with only the build arguments a step uses.

(b) can reuse a wrong layer: a changed `$VAR` in a `COPY` or `WORKDIR` keeps its
key. (c) needs a use analysis of every shell command, which cannot be exact.

What a lookup uses:

- (a) The key plus the encoding (compression, level, time policy and epoch).
- (b) The key alone, re-encoding a hit to this build's encoding: one entry for
  all builds, but a decompress, a tar rewrite and a compress per hit.
- (c) The key alone, the entry used as it is (kaniko): free, but a warm build may
  differ from a cold one.

## Decision Outcome

A cache key is made of BuildKit's fields hashed with SHA-256, a cache entry is a
one-layer OCI image found by a tag `rnk-v<version>-<hex>`, and a lookup uses the
key together with the layer's encoding, so a warm build gives the cold build's
digests.

The maintainer's answer (2026-11-06: "Kiểu BuildKit + encoding (khuyến nghị)",
"BuildKit style plus encoding (recommended)") stood for option (a) of #13 with the
recommended entry, tag, TTL and bad-entry-is-a-miss, and option (a) of the
encoding question.

- **Fields.** `H` is SHA-256 and the formula is ADR-0005's (see
  [cache-and-store](../design/cache-and-store.md#cache-keys)). A step's inputs are:
  - the instruction after expansion;
  - every declared `ARG` and the environment in effect;
  - `USER`, `WORKDIR` and `SHELL`;
  - for a `RUN`, its network and its mounts: a secret's id, target, mode and
    owner (never its value), a cache mount's id and sharing, and a cache mount
    marked as dropped where ADR-0057 drops it;
  - for `COPY` and `ADD`, each file's content, mode, uid and gid after
    `--chown`, type, link target and `security.capability`, never mtime;
  - for `COPY --from=<image>`, that image's platform manifest digest.
  The chain root is the base's platform manifest digest together with the
  platform. The input of `COPY --from=<stage>` is
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
  plain miss with no warning, as in kaniko.
- **Encoding.** A lookup carries the compression, its level, the time policy and
  the epoch. Builds with other settings share no entries, and the key itself
  stays ADR-0005's.
- **A layer is pushed under a key only if the step read what the key names.** A
  `COPY` or `ADD` whose files changed after they were hashed is not pushed under
  that key. The context is a live directory of a lower-trust owner
  ([ADR-0076](0076-the-build-context-owner-is-a-lower-trust-principal.md)).

Not decided here: the keys of `ADD <url>` and of `RUN --mount=type=bind`, which
come with those Phase 5 features, and how `<hex>` is derived from the key and the
encoding (Phase 4's task 2,
[#125](https://github.com/tran4774/ranikust/issues/125), fixes it, and the
`rnk-cache` card records it).

### Consequences

- Good, because no wrong layer is reused: a changed `$VAR` in a `COPY` or a
  `WORKDIR` changes the key.
- Good, because a cache never changes the output: with `--reproducible`, a cold,
  a warm and a partial build give one digest, at no cost per hit.
- Good, because a bad entry costs one miss, so nobody who can push to the
  repository can fail a build with it.
- Bad, because more misses than kaniko on cosmetic changes: every declared
  `ARG` is in the key of every `RUN`, used or not.
- Bad, because builds with another compression, level or time policy share no
  entries, so the repository holds one entry per encoding in use.
- Neutral, because users moving from kaniko start with a cold cache, as
  ADR-0005 accepted.

## Evidence

- **[external]** kaniko@44aa7ddc0 (the benchmark fork; which lines it changed from
  upstream was not checked): the key holds the unexpanded text, the environment
  and every declared `ARG` for `RUN` only, and each `COPY` file by its md5 of mode,
  owner and content (`pkg/executor/build.go:322-374`, `pkg/util/util.go:78-160`;
  its README, lines 1491-1496, says a changed `$VAR` can reuse a wrong layer).
  The TTL compares with the config's `created`, set at push time and not
  refreshed by a hit (`pkg/cache/cache.go:116-127`, `pkg/executor/push.go:447-451`);
  a wrong layer count in an entry fails the build
  (`pkg/commands/cache.go:27-56`, read once); a hit is appended as it is
  (`pkg/executor/build.go:861`).
- **[external]** buildkit@cd8ee244: the `RUN` key is the JSON of the whole exec
  operation and the platform (`solver/llbsolver/ops/exec.go:114-188`); secret
  mounts carry id, target, mode and owner, never the value
  (`frontend/dockerfile/dockerfile2llb/convert_secrets.go:14-90`); every declared
  `ARG` with a value is in a `RUN`'s environment, used or not
  (`frontend/dockerfile/dockerfile2llb/convert.go:1682-1730`); a `COPY` file hash
  holds mode, uid, gid, size, type, link name, `security.capability`, content
  SHA-256 and never mtime (`cache/contenthash/tarsum.go:38-72`); a base image is
  keyed by manifest digest and platform (`source/containerimage/pull.go:62-84`).
- **[code]** `rnk-cache` holds no key yet; ADR-0057 drops a cache mount for a
  `Direct` `RUN`.
- **[inferred]** Nothing measures the miss rate of (a) against (b): W2 has never
  been built with a cache ([ADR-0005](0005-own-versioned-cache-key.md#evidence)).

## More Information

Refines ADR-0005, which stays accepted: it fixes the field list that ADR-0005
left to Phase 4. Answers #13 and the Phase 4 question on the encoding. Related:
[ADR-0032](0032-observable-behaviour-follows-buildkit.md) (BuildKit breaks the
tie), [ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md),
[ADR-0079](0079-resource-limits-with-a-limit-flag.md) (every document read is
bounded).

The rule on pushing a layer is invariant 3 of
[#123](https://github.com/tran4774/ranikust/issues/123) (Phase 4), recorded here
with the key because it is a condition on pushing under one.

Follow-ups, with Phase 4's tasks and not before they are built: change
[cache-and-store](../design/cache-and-store.md#cache-keys), the `rnk-cache` card,
the threat model's T13 row and the book's usage page, and add the `## Changelog`
entry.
