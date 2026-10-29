# <crate-name>

> **Status:** planned | skeleton | in progress | done · **Phase:** N ·
> **Depends on:** `rnk-…` · **Used by:** `rnk-…` · **Path:** `crates/<crate-name>/`

**TL;DR:** One or two sentences: what this crate does and the one thing an
agent must not get wrong here.

## Owns

- Responsibilities, one per bullet. Must match the crate's `lib.rs` docs.

## Must not

- The `# Boundaries` of `lib.rs`, plus anything another crate owns instead
  (name that crate).

## API (planned)

Names and one-line purposes; short Rust signature sketches where they fix a
contract. Mark each item `[decided]`, `[design]` or `[open: Q-NNN]`.

```rust
pub struct Example; // one-line purpose
```

## Invariants

- Properties every change must preserve (safety, determinism, ordering).

## Refs

- **Design:** `design/<doc>.md#<section>`
- **Decisions:** `ADR-00NN` (one line on what it fixes here)
- **Evidence:** `exp-NN` (the number or finding that matters here)
- **kaniko:** `kaniko@44aa7ddc0 <path>:<line>` (what to port or compare)
- **External:** specs, upstream code, crate docs (full URLs)

## Tests

- Tier (T0 unprivileged / T1 user namespace / T2 VM) and what to cover;
  fixtures and oracles (BuildKit, kaniko) where relevant.

## Metrics

- 2–5 bullets: the spans this crate emits and the fields that matter, with a
  link to its rows in `design/metrics.md#<crate>`; where counting needs care
  (loops, other threads). "None" with the reason for a pure crate.

## Pitfalls

- Traps found by the experiments or by earlier work, one line each, with the
  reference that proves it.

## Phase 1

- What to build now, what to defer, and the order within this crate.
