# <crate-name>

> **Status:** planned | skeleton | in progress | done · **Phase:** N ·
> **Depends on:** `rnk-…` · **Used by:** `rnk-…` · **Path:** `crates/<crate-name>/`

**TL;DR:** One or two sentences: what this crate does and the one thing an
agent must not get wrong here.

A card holds what an agent needs to start work in the crate. How a subsystem
works and why lives in `docs/design/` and the ADRs: link them (doc#anchor,
one line on what is there), never restate them. Describe what is true now;
the history is in git and the ADRs.

## Owns

- Responsibilities, one per bullet. Must match the crate's `lib.rs` docs.

## Must not

- What `lib.rs`'s `# Boundaries` forbids (authoritative: link it, do not
  restate it), plus anything another crate owns instead (name that crate).

## API

Entry points and key types with their paths, one line each; short Rust
sketches only where they fix a contract. Rustdoc owns the per-function
detail. Mark undecided items `[open: #N]` (a `question` issue) and planned
ones `[design]` or `[decided]` (ADR); a planned crate says `(planned)` in
the heading.

```rust
pub struct Example; // one-line purpose
```

## Invariants

- Properties every change must preserve (safety, determinism, ordering).

## Refs

- **Design:** `design/<doc>.md#<section>`
- **Decisions:** `ADR-00NN` (one line on what it fixes here)
- **Open:** `#N` (the `question` issues and tasks of this crate)
- **Evidence:** `exp-NN` (the number or finding that matters here)
- **kaniko:** `kaniko@44aa7ddc0 <path>:<line>` (what to port or compare)
- **External:** specs, upstream code, crate docs (full URLs)

## Tests

- Tier (T0 unprivileged, T1 user namespace, T2 VM, T2c rootful container;
  [docs/testing.md](../../testing.md#tiers)), where the tests live, what they
  cover and how to run them; fixtures and oracles (BuildKit, kaniko).

## Metrics

- 2-5 bullets: the spans this crate emits and the fields that matter, with a
  link to its rows in `design/metrics.md#<crate>`; where counting needs care
  (loops, other threads). "None" with the reason for a pure crate.

## Pitfalls

- Traps found by the experiments or by earlier work, one line each, with the
  reference that proves it.
