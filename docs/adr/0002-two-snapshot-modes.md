---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0002: Two snapshot modes: overlay when permitted, kaniko-style walk otherwise

## Context and Problem Statement

kaniko finds each layer's diff by walking and hashing the whole root
filesystem after every instruction. overlayfs gives the diff for free (the
upper directory), but mounting it needs `CAP_SYS_ADMIN` or a usable user
namespace. ranikust must run wherever kaniko or BuildKit runs
([ADR-0004](0004-support-every-privilege-level.md)), and most locked-down CI
pods cannot mount anything. Which snapshot mechanism should ranikust use?

## Considered Options

- Overlay only, like BuildKit and buildah. Fails in Docker's default profile
  and in ordinary Kubernetes pods.
- kaniko-style walk only. Works wherever kaniko works, but pays a full walk
  per instruction.
- Both, selected at run time from the privileges actually available.
- fanotify-based change tracking. Rejected by the maintainer in the survey.

## Decision Outcome

ranikust has two snapshot modes behind one interface, overlay mode when it can
mount overlayfs and kaniko mode (walk and hash, workdir = rootfs) otherwise,
chosen at run time by probing.

- `--snapshot-mode=auto` (default) probes; `overlay` and `kaniko` force a mode.
  The probe is specified in
  [modes-and-detection](../design/modes-and-detection.md).
- Overlay mode: one overlay mount per stage inside the workdir, one upper
  directory per layer, stages run in parallel along the DAG.
- kaniko mode: a port of kaniko's flow with a parallel walk and a stat cache;
  sequential on `/` by default ([ADR-0014](0014-kaniko-mode-sequential-by-default.md)).
- Only the `StageRootfs` and `Snapshotter` traits in `rnk-snapshot` differ.
  Parser, registry, layer encoding, cache and builder are shared.
- No fanotify.

### Consequences

- Good, because one binary covers both sides: rootless and privileged hosts
  get overlay speed, locked-down pods still build.
- Good, because a well-written walk recovers most of overlay's advantage, so
  the majority of CI users (kaniko mode) are not left far behind.
- Bad, because there are two snapshot paths (three with rootless overlay) to
  maintain and a test matrix per privilege level.
- Bad, because the modes can produce different layers
  ([ADR-0012](0012-accept-divergence-between-modes.md)).

## Evidence

- [Experiment 06](../research/experiments/06-overlay-vs-walk.md): on W1's five
  `RUN` steps, overlay diff extraction took 0.55 s against 2.52 s for an
  optimized 32-worker walk (4.5–5.5× faster), but overlay slowed the commands
  themselves by 7–16 %, so the total gain was 18–23 % (1.5–1.7 s).
- [Experiment 06a](../research/experiments/06a-root-rerun.md): same direction as
  real root, ext4 total 6.50 → 5.26 s (−19 %).
- [Experiment 07](../research/experiments/07-kaniko-vs-overlay.md): full W1
  build at 32 CPU, optimized kaniko 15.63 s, Go walk 11.29 s, overlay
  9.50–9.95 s; at 4 CPU 18.27 / 14.83 / 11.18–12.22 s.
- [Experiment 03](../research/experiments/03-privilege-matrix.md) and
  [experiment 13](../research/experiments/13-docker-rootful-vm.md): Docker's
  default profile and Kubernetes pods on Ubuntu/COS nodes land in kaniko mode.
- [Survey 06](../research/survey/06-overlay-and-parallelism.md) §1, §3.5, §5.

## More Information

Design: [overlay-snapshot](../design/overlay-snapshot.md),
[kaniko-mode](../design/kaniko-mode.md). The user documentation must not
advertise "overlay everywhere": ranikust's distinguishing feature is one binary
for both sides.

The mode flag is named `--mode`; `--snapshot-mode` keeps kaniko's meaning
([ADR-0023](0023-mode-flag.md)).
