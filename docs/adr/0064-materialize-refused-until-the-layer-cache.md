---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0064: `--materialize` is refused until the layer cache

## Context and Problem Statement

kaniko's `--materialize` makes the final filesystem match the target even
on full cache hits; without a layer cache it changes nothing. ADR-0040
accepted it as a kaniko-mode setting kept in `BuildConfig` until Phase 4
gives it a meaning, and overlay mode warned that it is ignored. The same
day ADR-0049 settled that nothing is accepted only to be ignored, and
refused the other Phase 4 cache flags (`--cache`, `--cache-repo`, ...).
The Phase 2 criterion-8 review found the two decisions in conflict: in
kaniko mode the flag was taken and silently did nothing. The question:
accept it as a no-op, or refuse it like the other cache flags.

## Considered Options

- Keep accepting it as a no-op (ADR-0040 as written).
- Refuse it with the Phase 4 reason, as `--cache` (ADR-0049).

## Decision Outcome

`--materialize` is refused in every mode with the Phase 4 reason, like
`--cache`, until the layer cache gives it a meaning (maintainer,
2026-11-01: "1: theo đề xuất", accepting the recommendation of the
criterion-8 review).

- The refusal runs before anything else, from the table of refused kaniko
  flags (`crates/ranikust/src/cli.rs`, `REFUSED`), exit 2, whatever its
  value (`--materialize=false` too).
- The flag leaves `BuildArgs` and `BuildConfig`; overlay mode's warning
  about it goes.
- Phase 4 brings it back with the cache, with a test of its meaning.

### Consequences

- Good, because a user never believes a flag took effect when it did
  nothing (ADR-0049).
- Good, because one fewer flag until it means something.
- Bad, because a kaniko command line with `--materialize` fails until
  Phase 4; the message says why and that the flag can be dropped.

## Evidence

- [code] `crates/ranikust/tests/cli.rs`
  `materialize_is_refused_before_any_work`: exit 2, "Phase 4", no state
  directory created, for `--materialize`, `=true` and `=false`.
- The Phase 2 criterion-8 review (2026-11-01), non-blocker #5.

## More Information

Refines ADR-0040 (its acceptance of `--materialize`) in favour of ADR-0049;
neither is rewritten. Related: Q-021 (the layer cache), Q-420
(`--single-snapshot`).
