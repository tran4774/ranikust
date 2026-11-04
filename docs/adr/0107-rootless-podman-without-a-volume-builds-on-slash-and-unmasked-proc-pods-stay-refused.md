---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0107: Rootless Podman without a volume builds in kaniko mode on `/` with the volume hint, and a `hostUsers: false` pod with unmasked `/proc` stays refused until measured

## Context and Problem Statement

[ADR-0054](0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md)
refuses kaniko mode on `/` where root may mount or `/proc/sys` is writable,
because a `Direct` `RUN` would reach the container's devices and `/proc/sys`.
Two cases are new with rootless overlay mode, planned for Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)), where ranikust runs in a
user namespace it did not create (raised while planning Phase 3):

- (i) Rootless Podman with default options and no volume at the state
  directory. Root may mount only in a nested user namespace, and the state
  directory is on overlayfs, so rootless overlay cannot run. Today this
  container builds in kaniko mode on `/`
  ([modes-and-detection](../design/modes-and-detection.md#where-environments-land)).
- (ii) A `hostUsers: false` pod with `procMount: Unmasked` and the default
  AppArmor profile. Rootless overlay is denied, and kaniko mode on `/` is refused
  for the writable `/proc/sys`, so the pod gets no mode, although most sysctls
  need capabilities in the initial user namespace **[inferred]**.

## Considered Options

For (i):

- (a) Kaniko mode on `/`, logged with the volume hint.
- (b) Refuse with the volume advice, as for root that may mount.

For (ii):

- (c) Keep refusing.
- (d) Skip the `/proc/sys` check outside the initial user namespace.

## Decision Outcome

Case (i) builds in kaniko mode on `/`, logged with the volume hint, and case (ii)
stays refused until a `poc-runner` measurement in a user-namespaced container
shows `/proc/sys` writes refused, after which an ADR decides on (d): options (a)
and (c) (the maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as
recommended).

- **Today's routing is kept for both:** (a) for (i), apart from the hint, (c)
  for (ii) ([modes-and-detection](../design/modes-and-detection.md#where-environments-land)).
  The decision matters when rootless overlay arrives (task 8 of #41, planned,
  not built): it must not take (i) or (ii) from this routing.
- **(i), the volume hint.** The log carries a hint to mount a volume at the
  state directory, which lets rootless overlay run. Today's route for (i) logs
  only "kaniko mode, because root may not mount here" and no volume hint; the
  hint is added with task 8 of #41
  ([#79](https://github.com/tran4774/ranikust/issues/79)).
- **(ii), until measured.** The measurement is in a user-namespaced container
  (`hostUsers: false`, `procMount: Unmasked`, the default AppArmor profile): do
  writes to `/proc/sys` fail? Until it shows they do, ADR-0054's check stands
  there; (d) needs its own ADR.
- **ADR-0054 is unchanged:** this refines how it applies in a user namespace
  ranikust did not create.

### Consequences

- Good, because no build that works today stops working: rootless Podman
  without a volume keeps its kaniko mode.
- Good, because mount power in a nested user namespace does not reach the
  container's devices **[inferred]**, so (i) is not the case ADR-0054 refuses.
- Good, because (ii) stays closed until a measurement shows that skipping the
  check is safe, not on the inference that most sysctls need capabilities in the
  initial user namespace.
- Bad, because a `hostUsers: false` pod with unmasked `/proc` and the default
  AppArmor profile gets no mode until then.
- Bad, because (i) builds in kaniko mode on `/`, which gives the `RUN` no
  isolation of its own, where a volume would give it rootless overlay.

## Evidence

- Nothing is measured for (ii) here: row 7c of
  [ADR-0100](0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md)
  is **[inferred]**. Case (i)'s route is built but no test takes it
  **[code]**: the T1 kaniko leg ([testing](../testing.md),
  `RANIKUST_CASE_MODE=kaniko`) runs a default rootless Podman container with a
  volume at the state directory and `--mode=kaniko`
  (`tests/smoke/run-cases.sh`), so it skips the probe; the test of (i) is the
  follow-up below.
- **[inferred]** Most sysctls need capabilities in the initial user namespace, so
  a user-namespaced container may not be able to write `/proc/sys`; the
  measurement that would show it is the one this ADR waits for.
- [modes-and-detection](../design/modes-and-detection.md#refusal-and-guidance):
  kaniko mode on `/` is refused where `refuse_unsafe_slash` stops it (root may
  mount, a writable `/proc/sys`), and rootless Podman's root, lacking
  `CAP_SYS_ADMIN`, takes kaniko mode on `/` today.

## More Information

Raised while planning Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Refines
[ADR-0054](0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md),
which stays `accepted` with its body unchanged; related:
[ADR-0050](0050-ranikust-picks-the-build-root-no-workdir-flag.md) (kaniko mode
on `/` only in a container).

Follow-ups, with task 8 of #41
([#79](https://github.com/tran4774/ranikust/issues/79)): the routing and the
volume hint in `ranikust` with a T1 test of rootless Podman without a volume
(kaniko on `/`) and, in the `k8s` suite of ADR-0100, rows 7b and 7c. The
`poc-runner` measurement for (ii) is not scheduled by this ADR; its result goes
into an ADR. When it is built, change
[modes-and-detection](../design/modes-and-detection.md) and the book's
[where to run it](../book/src/where-to-run.md).
