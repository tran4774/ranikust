---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0065: `--single-snapshot` is refused for good

## Context and Problem Statement

kaniko's `--single-snapshot` takes one snapshot at the end of the build
instead of one per step, so the image gets one layer. ADR-0049 offers a
kaniko flag that changes the image only if it is measured to be worth it;
the Phase 2 criterion-8 review found the flag documented as offered while
the code refused it, and the maintainer asked for a measurement first
(Q-420). The question: implement it in kaniko mode, refuse it for good, or
leave it refused until Phase 4's layer cache shows what per-step layers are
worth on rebuilds.

## Considered Options

- (a) Implement it in kaniko mode: one walk and one layer at the end.
- (b) Refuse it for good.
- (c) Leave it refused until Phase 4's cache numbers.

## Decision Outcome

(b): `--single-snapshot` is refused in every mode, for good, whatever its
value (maintainer, 2026-11-01: "1. theo đề xuất", accepting the
recommendation of Q-420).

- The refusal sits in the table of refused kaniko flags
  (`crates/ranikust/src/cli.rs`, `REFUSED`, `Refusal::Refused`), so it runs
  before the arguments are parsed, exit 2, in every mode, `=false`
  included, with a message naming this ADR and saying to drop the flag.
- The flag leaves `BuildArgs`; `BuildConfig::from_args` no longer checks
  it.
- Reopening it needs a new ADR with numbers from Phase 4's cache.

### Consequences

- Good, because every image keeps one layer per step, which Phase 4's
  layer cache reuses on a rebuild and which tells a step's files apart.
- Good, because one flag fewer (ADR-0049).
- Bad, because a build of many steps on a large tree gives up a few
  seconds: about 1 s on a 250 000-file tree over 10 steps, up to about 3 s
  on W1 [inferred from the measured walk times below].
- Bad, because a kaniko command line with `--single-snapshot` fails; the
  message says why and to drop the flag.

## Evidence

- [measured] `rnk-snapshot` `single_snapshot_poc`
  (`crates/rnk-snapshot/src/kaniko/walk/tests.rs`, kept `#[ignore]`d as the
  record): release build, 32-core host, ext4 NVMe, page cache warm,
  medians of 3; 10 steps of 200 rewrites and 200 new files each.
  - 80 000 files: per-step walks 0.69 s in all, one walk at the end
    0.08 s: 0.61 s saved.
  - 250 000 files (about W1's size): 1.39 s against 0.24 s: 1.16 s saved.
- [measured] W1 per-stage in the VM spends 3.3 s in its ten walks
  (ADR-0061's run), of a 22 s build.
- [code] `crates/ranikust/src/cli/tests.rs`
  `single_snapshot_is_refused_for_good_whatever_its_value`;
  `crates/ranikust/tests/cli.rs`
  `single_snapshot_is_refused_in_auto_mode_before_any_work` and
  `red_team_single_snapshot_and_materialize_have_no_bypass_spelling`
  (exit 2 in every spelling, nothing created).

## More Information

Answers Q-420. Related: ADR-0049 (flags offered only if worth it),
ADR-0064 (`--materialize`, the other kaniko flag the criterion-8 review
settled), Q-021 (the layer cache).
