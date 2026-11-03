---
status: accepted
date: 2026-11-03
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0083: A root `RUN`'s delay of its init's death is closed by a cgroup for the command where one is delegated, and by `--userns` in Phase 3

## Context and Problem Statement

When ranikust dies, the init of a full-sandbox `RUN`
([ADR-0080](0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md))
dies and the namespace with it. A root `RUN` can delay that, though not stop
it: as a process of the init's uid it may make the init `SCHED_IDLE` and pin
it to one CPU without `CAP_SYS_NICE`, then keep that CPU busy. Measured in the
VM (kernel 6.8): 0.9 to 1.8 ms without that, 7.6 s with 8 spinners, 30.6 s with
32, about 1 s per spinner, bounded only by the outer cgroup's `pids.max`. The
`RUN`'s processes keep running meanwhile. Running the init under a uid of its
own does not help: a root `RUN` has `CAP_SETUID` among Docker's default
capabilities and switches to that uid, after which renicing and pinning the
init were allowed again. How is a root `RUN` kept from delaying its init's
death (Q-452)?

## Considered Options

- (a) Put the command in a cgroup of its own, apart from the init's: group
  scheduling then gives the init's cgroup its share of the CPU however many
  spinners the `RUN` starts **[inferred]**, and a write to the `RUN` cgroup's
  `cgroup.kill` ends its whole tree as a second line. It needs a writable,
  delegated cgroup v2 tree, which a default Docker container or most pods do
  not give; ADR-0007 has cgroups only read today.
- (b) With `--userns` (Phase 3), run the command in a user namespace that does
  not map the init's uid, so that neither the same-owner rule nor `CAP_SETUID`
  inside it reaches the init **[inferred]**.
- (c) Keep the documented residual.

## Decision Outcome

A `RUN`'s command gets a cgroup of its own, apart from the init's and ended with
`cgroup.kill`, where a delegated cgroup v2 tree exists, and in Phase 3 `--userns`
runs the command in a user namespace that leaves the init's uid unmapped:
options (a) and (b), each in its own place (the maintainer, 2026-11-03: "Q-452:
(a) khi có cgroup delegate, (b) khi tới Phase 3").

- **(a), where a delegated cgroup exists.** The command runs in a cgroup
  separate from the init's; ranikust writes the `RUN` cgroup's `cgroup.kill` to
  end its whole tree as a second line. Where no writable, delegated cgroup v2
  tree exists (a default Docker container, most pods), nothing changes and the
  residual of (c) stays documented. How ranikust finds a delegated tree, and
  what it does when the write fails, is for the implementation to settle inside
  that rule, and to log as a question if the maintainer must choose.
- **(b), in Phase 3.** With `--userns` the command runs in a user namespace that
  does not map the init's uid. It comes with `--userns` itself, not before.
- **Until each is built,** the residual stays as threat-model section 6
  documents it. This decision does not make it a permanent acceptance: (c) is
  the state until (a) and (b) exist, not the choice.
- **Implementation: a follow-up for each.** Nothing is built by this ADR.

### Consequences

- Good, because the delay is closed where it can be: a cgroup is used only
  where one is delegated, and the user namespace comes with the option that
  already needs one.
- Good, because `cgroup.kill` ends a `RUN`'s whole tree in one write, a second
  line for ADR-0080's T8 where a cgroup exists.
- Bad, because ranikust then writes to a cgroup tree, where ADR-0007 said
  cgroups are only read; and a default Docker container or most pods give no
  delegated tree, so the residual stays where builds most often run until
  Phase 3.
- Bad, because both effects are still **[inferred]**: neither group scheduling
  nor the unmapped uid has been measured against the spinners.
- Neutral, because ADR-0080's choice of an init is unchanged: its rejected
  "cgroup per `RUN`" option is used here as an addition where it is possible,
  not as the mechanism that keeps T8.

## Evidence

- [measured] in the VM (kernel 6.8), by
  `red_team_a_run_can_slow_its_inits_death_but_not_stop_it` in `rnk-sandbox`
  (ADR-0080): the namespace's death after the cloning thread ended, 0.9 ms and
  1.8 ms without scheduling tricks; 7.6 s with the init made `SCHED_IDLE` and
  pinned to CPU 0 under 8 spinners, 30.6 s under 32; no process left in either.
- [measured] in the VM on 2026-11-03: an init under a uid of its own does not
  help, since a root `RUN` switches to that uid with `CAP_SETUID` and may
  renice and pin the init again.
- Options (a) and (b) are not measured: the effect of group scheduling and of
  an unmapped uid is **[inferred]**. The measurement of (a) belongs with its
  implementation.

## More Information

Answers Q-452. Partly supersedes
[ADR-0007](0007-run-anywhere-shared-store.md): where a delegated cgroup v2
tree exists, ranikust creates a cgroup under it, and "cgroups are only read" no
longer holds there; ADR-0007 stays `accepted` and the rest of it stands.
Refines [ADR-0080](0080-a-full-sandbox-run-runs-under-an-init-that-dies-with-ranikust.md)
(its "not built" residual) and the T8 row and section 6 of the
[threat model](../security/threat-model.md). Related: ADR-0003 (`--userns`,
Phase 3).

Follow-ups: (a) a cgroup for the command and `cgroup.kill` in `rnk-sandbox`
where a delegated cgroup v2 tree exists, with a T2 test (the VM, then rootful
Docker with a delegated cgroup) that 8 and 32 spinners no longer delay the
init's death; (b) with Phase 3's `--userns`, a user namespace that leaves the
init's uid unmapped. When each is built, update `docs/design/sandbox.md`, the
`rnk-sandbox` card and the threat model, and the book if users can see it.
