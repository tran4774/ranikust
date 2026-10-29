---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0005: Own versioned cache key, no kaniko cache compatibility

## Context and Problem Statement

kaniko can cache each instruction's layer in a registry repository, keyed by
a hash it computes from the instruction and its inputs. Users migrating from
kaniko might want ranikust to reuse an existing kaniko cache. Matching
kaniko's key bit for bit would tie ranikust to kaniko's hashing rules,
snapshot behaviour and future changes. Should ranikust be cache-compatible
with kaniko?

## Considered Options

- Compute exactly kaniko's cache keys and read and write its cache.
- Define ranikust's own keys, with an explicit format version.
- Reuse BuildKit's cache format.

## Decision Outcome

ranikust computes its own layer cache keys, each carrying a format version, and
neither reads nor writes kaniko's cache.

- The key of an instruction depends only on the instruction, the build
  context it reads, and the keys of the instructions before it. It never
  depends on the snapshot mode, so a layer cached by one mode can be used by
  the other.
- The version field lets the key algorithm change later: new keys simply miss
  the old entries, which are never rewritten.
- Cached layers live in a registry repository (`--cache-repo`) or an OCI
  layout. The exact list of key inputs is fixed in phase 4; see
  [cache-and-store](../design/cache-and-store.md).

### Consequences

- Good, because the key algorithm is free to follow ranikust's own semantics
  and can evolve without breaking anyone's cache.
- Bad, because users migrating from kaniko start with a cold cache.
- Bad, because a layer cached in one mode may carry that mode's hardlink and
  copy-up differences into a build in the other mode
  ([ADR-0012](0012-accept-divergence-between-modes.md)).

## Evidence

- [Survey 05](../research/survey/05-decisions.md) Q3; risk R8 in
  [survey 04](../research/survey/04-risks-and-roadmap.md) rates the loss of
  compatibility low.
- [Survey 06](../research/survey/06-overlay-and-parallelism.md) §6: keys are
  mode-independent; the only differences between modes are unchanged files
  copied up by overlay and split hardlinks.
- No experiment measured caching: workload W2 (cold, warm and partial cache)
  was not run [inferred].

## More Information

Implemented in `rnk-cache`. Revisit only if kaniko-cache compatibility becomes
a requirement for adoption.
