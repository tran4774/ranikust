---
status: accepted
date: 2026-11-09
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0146: Root empties `--userns`'s state directory with `prune --all --userns`, as the range's first uid

## Context and Problem Statement

`ranikust prune --all` refuses root and prints commands for root instead
([ADR-0105](0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md),
[ADR-0133](0133-roots-prune-commands-delete-only-the-owners-ids-and-only-in-a-directory-a-build-would-accept.md)).
`--userns` builds use a state directory of their own, owned by the first uid
of root's `ranikust` range
([ADR-0139](0139-userns-builds-in-a-state-directory-of-its-own-and-root-reads-the-credentials-before-the-namespace.md)),
which holds files of the range's other ids. Root's commands delete only the
owner's uid and its subordinate ids, so they cannot empty it, and deleting
there as root, in a tree the range's `RUN`s can change, is what ADR-0105
avoids. How does root empty that directory
([#207](https://github.com/tran4774/ranikust/issues/207))?

## Considered Options

- (a) A root-only `prune --all --userns` that runs the store's prune as the
  range's first uid, in its user namespace.
- (b) Root's commands widened to the range's ids: root would delete as root
  in a tree another uid writes.
- (c) Root's `prune --all` emptying it as root: contradicts ADR-0105.

## Decision Outcome

Root empties `--userns`'s state directory with
`ranikust prune --all --userns --state-dir DIR`, which deletes as the first
uid and gid of the `ranikust` range inside that range's user namespace, never
as root on the host.

- Root only: from any other user it is a usage error (exit code 2).
- `--all` and `--state-dir` are required: root's default, and paths at, below
  or above it, are refused, as `--userns` builds refuse them.
- The range is read as `--userns` reads it; without a usable one the prune is
  refused.
- Before the namespace, `--userns`'s build checks run outside, read-only;
  what they refuse is refused, nothing changed.
- Root's plain `prune --all` names `prune --all --userns` instead of its own
  commands only for a marked directory those checks accept; neither command
  sends root to the other. A `--userns` build, and a `prune --all --userns`
  that could not finish, point back to it. Where the range's uid cannot
  remove an entry, root gets commands in ADR-0133's form that remove all but
  the marker, to run once every `--userns` build is stopped.
- `--metrics-file` is refused with it (exit code 2): the report would be
  written at exit as the range's uid.

Phase 5's `prune` is not covered.

### Consequences

- Good, because ranikust deletes nothing in that directory as root, with the
  store code a user's `prune --all` runs.
- Bad, because what the range's uid cannot remove (a mount below, entries of
  ids the range does not map) still needs root to run printed commands.

## Evidence

- T2 `userns` quick case, `pruned_as_the_range`
  (`tests/smoke/cases/userns/case.sh`): the refusals, the prune as the range's
  uid, the printed commands run as root [measured].
- T0 tests in `crates/ranikust/src/prune/tests.rs` (the usage error, the
  refusals before the namespace, the guidance each way, the printed commands
  in each shell) [code].

## More Information

Refines ADR-0105 for root; builds on ADR-0133 and ADR-0139. Implemented by
#207; the steps: [cache-and-store](../design/cache-and-store.md#prune) and
[sandbox](../design/sandbox.md).
