---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0125: `--cache-copy-layers` covers `COPY` and `ADD`

## Context and Problem Statement

kaniko's `--cache-copy-layers` ("Caches copy layers") decides whether a `COPY`
layer is looked up in and pushed to the cache. Phase 4 builds the flag with
kaniko's meaning
([ADR-0011](0011-accept-common-kaniko-flags.md); the plan of
[#130](https://github.com/tran4774/ranikust/issues/130) says so), and
[ADR-0032](0032-observable-behaviour-follows-buildkit.md) lets BuildKit break
ties. kaniko's README describes the flag for `COPY` only. Whether an `ADD` is
cached under it was not decided: the keys of
[ADR-0113](0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)
already cover the files an `ADD` from the build context reads, and `ADD <url>`
and other remote sources get their keys with Phase 5.

## Considered Options

- `--cache-copy-layers` covers `COPY` and `ADD`: an `ADD` from local sources is
  cached as a `COPY` is.
- `COPY` only, as kaniko's README describes the flag: an `ADD` always runs, and
  that is not a miss, so it does not end the later hits.
- `COPY` only, and an `ADD` always runs and counts as a miss: it ends the stage's
  later hits.

## Decision Outcome

With `--cache-copy-layers`, an `ADD` from local sources is cached as a `COPY` is,
by the same key fields, lookup and push; `ADD <url>` and remote sources come with
Phase 5, whose features give them their keys.

The maintainer's answer (2026-11-07: "Cache cả COPY và ADD (khuyến nghị)", "Cache
both COPY and ADD (recommended)") stood for the first option.

- **Wider than the README.** The flag covers what kaniko's README says it covers
  and `ADD`. The image is the same either way; only the hits differ.
- **Against ADR-0011 and ADR-0032.** ADR-0011 keeps kaniko's meaning of a flag:
  this keeps the flag's name and what it does for a `COPY`. BuildKit, which breaks
  ties (ADR-0032), caches an `ADD` as it caches a `COPY`.
- **Not covered:** the keys of `ADD <url>` and of remote sources (ADR-0113 leaves
  them open).

### Consequences

- Good, because an `ADD` from the context, common in real Dockerfiles, is a hit
  like a `COPY` and ends nothing.
- Good, because the image does not change: only whether a layer is built or
  reused.
- Bad, because the flag is wider than kaniko's documentation says, so the book
  and the kaniko map must say it
  ([kaniko map](../agents/kaniko-map.md#behaviour-to-match)).
- Neutral, because `ADD <url>` and remote sources are Phase 5's.

## Evidence

- **[external]** kaniko@44aa7ddc0: the flag is "Caches copy layers"
  (`cmd/executor/cmd/root.go:369`) and the README describes it for `COPY`
  (`README.md:667-672`, `:785-787`). The pinned code goes further than that text:
  an `AddCommand` takes the same `shdCache` as a `CopyCommand`
  (`pkg/commands/commands.go:86,94`), its `ShouldCacheOutput` returns it
  (`pkg/commands/add.go:290-292`; `pkg/commands/copy.go:294-296`), and the lookup
  and the push of a cache layer gate on it (`pkg/executor/build.go:551`, `:904`).
  Commit `742e75b1e` (2025-05-30, "ADD learned to cache it's output layer") added
  it; before it, an integration Dockerfile of kaniko's own, written on
  2025-05-25, records that `ADD` is "never cached" even with the flag
  (`integration/dockerfiles/Dockerfile_test_issue_add:3-7`). Its key leaves out
  remote URLs and local tar archives (`pkg/commands/add.go:361-366`).
- **[external]** buildkit@cd8ee244: `ADD` and `COPY` are converted by the same
  `dispatchCopy` (`frontend/dockerfile/dockerfile2llb/convert.go:1076-1096`,
  `:1122`; `frontend/dockerfile/dockerfile2llb/convert_copy.go:46`), so both are
  steps the solver caches.
- **[inferred]** Nothing measures how many Dockerfiles use `ADD` for local
  files, or how many hits this adds.

## More Information

Keeps ADR-0011 (a kaniko flag keeps its name and its meaning for `COPY`) and uses
ADR-0032's tie-break; neither is rewritten. The key fields are ADR-0113's, and
`ADD <url>` is left open there.

Built by [#131](https://github.com/tran4774/ranikust/issues/131) (`--cache` end to
end with kaniko's cache flags); the choice of which steps are cached by policy is
the builder's ([#130](https://github.com/tran4774/ranikust/issues/130)). The
[kaniko map](../agents/kaniko-map.md#behaviour-to-match) lists the difference now,
as planned; the book's command-line page and the `## Changelog` entry follow when
#131 builds it.
