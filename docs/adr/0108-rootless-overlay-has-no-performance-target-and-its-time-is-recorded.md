---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0108: Rootless overlay has no performance target; its time per build is recorded against overlay as root and kaniko mode

## Context and Problem Statement

Phase 2's acceptance set a bound against kaniko: no build slower than kaniko's
([phase-2](../agents/phase-2.md), the time criterion). Nothing sets one for
rootless overlay mode, planned for Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Does rootless overlay
have a performance target (raised while planning Phase 3)?

## Considered Options

- (a) None: the time is recorded against overlay as root and kaniko mode, and
  measured further in Phase 6 ([#29](https://github.com/tran4774/ranikust/issues/29),
  Q-421).
- (b) A bound against overlay as root that the maintainer sets.

## Decision Outcome

Rootless overlay has no performance target in Phase 3; its time per build is
recorded on W1 to W3 against overlay as root and kaniko mode, and measured
further in Phase 6: option (a) (the maintainer, 2026-11-04: "tất cả theo khuyến
nghị", all as recommended).

- **Planned, Phase 3**, an acceptance criterion of #41: time per build of
  rootless overlay recorded on W1 to W3 against overlay as root and kaniko mode.
  Nothing is built or measured by this ADR.
- **Reason:** the layers come from the same overlay mechanism; what differs is
  the namespace set-up and an rbind of `/proc` **[inferred]**, so a bound would
  test little that the time record does not show.
- **Phase 6** measures further, with the performance conditions of #29 (Q-421).

### Consequences

- Good, because the time record still shows what the user namespace costs.
- Good, because Phase 3 does not wait for a target the maintainer has not set.
- Bad, because rootless overlay could be slower than overlay as root, or than
  kaniko mode, without any criterion failing.

## Evidence

- **[inferred]** The layers come from the same overlay mechanism as overlay as
  root; the differences are the namespace set-up and an rbind of `/proc` where a
  fresh `proc` is refused. No rootless build has been timed: the mode does not
  exist.
- **[docs]** [phase-2](../agents/phase-2.md): Phase 2's time criterion and its
  figures (frozen record).

## More Information

Raised while planning Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Related:
[#29](https://github.com/tran4774/ranikust/issues/29) (Q-421, the conditions
Phase 6 measures; rootless timings may join it).

Follow-ups, with the Phase 3 acceptance runs (task 15, only when the maintainer
asks): the ultra benchmark with the rootless legs records the time per build.
