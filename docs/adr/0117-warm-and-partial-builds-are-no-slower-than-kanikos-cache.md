---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0117: Warm and partial builds are no slower than kaniko's cache

## Context and Problem Statement

Phase 2's acceptance asked that no build be slower than kaniko's on W1 to W3
([phase-2](../agents/phase-2.md), the time criterion, a frozen record).
[ADR-0108](0108-rootless-overlay-has-no-performance-target-and-its-time-is-recorded.md)
gave rootless builds no target and recorded their time instead. W2 has never
been built with a cache. What performance does Phase 4 have to reach (raised
while planning Phase 4, see [the roadmap](../../README.md#roadmap))?

## Considered Options

- (a) Warm and partial W2 no slower than kaniko with `--cache=true`, in overlay
  mode as root, in kaniko mode on `/` and in kaniko mode with a directory per
  stage; rootless recorded only (ADR-0108).
- (b) Record the times, no target.
- (c) (a) for overlay mode only; kaniko mode recorded.

## Decision Outcome

Phase 4's acceptance requires a warm and a partial W2 build with `--cache` to be
no slower than kaniko with `--cache=true`, in overlay mode as root, in kaniko
mode on `/` and in kaniko mode with a directory per stage; a rootless build's
time is recorded only.

Option (a) (the maintainer, 2026-11-06: "Không chậm hơn kaniko (khuyến nghị)",
"No slower than kaniko (recommended)").

- **Target.** Each of the three modes, warm and partial, against kaniko
  `--cache=true` on the same workload. It is Phase 2's bar applied to the cache,
  which is what kaniko users compare.
- **Rootless** has no target: its time is recorded against overlay as root and
  kaniko mode, as ADR-0108 says.
- **When it is measured.** In Phase 4's acceptance run, which runs only when the
  maintainer asks.

### Consequences

- Good, because the bar is the comparison kaniko users make, and Phase 2's
  already.
- Good, because a mode that is slower than kaniko with a cache fails the
  criterion instead of passing unnoticed.
- Bad, because a baseline run of kaniko `--cache=true` with a registry is needed
  for each comparison, and a mode that is slower can hold up the phase's
  acceptance.

## Evidence

- [phase-2](../agents/phase-2.md): Phase 2's time criterion and its figures:
  every build faster than kaniko on W1 to W3 (frozen record).
- **[inferred]** No figure exists for a cached build: W2 has not been built with
  a cache, so whether (a) is reachable in each mode is not known.

## More Information

Raised while planning Phase 4. Related: ADR-0108 (a rootless build's time
recorded),
[ADR-0114](0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)
(what makes a warm build fast). The cache scenarios of the end-to-end harness
(W2 warm and partial) are Phase 4's task 13
([#136](https://github.com/tran4774/ranikust/issues/136)).
