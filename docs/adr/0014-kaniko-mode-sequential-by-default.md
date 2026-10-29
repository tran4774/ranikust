---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0014: kaniko mode is sequential by default; `--parallel-stages` opts in

## Context and Problem Statement

In kaniko mode the workdir is the root filesystem, `/` inside a container, so
stages run one after another. Experiment 12 showed that kaniko mode can also
give each stage its own rootfs directory and run stages in parallel without
any mount. Where mounting is impossible (Docker's default profile, ordinary
Kubernetes pods), `RUN` then executes in a bare `chroot` with no `/proc`, and
some commands return wrong results without failing. Should kaniko mode run
stages in parallel by default where it cannot mount?

## Considered Options

- Always sequential on `/`, exactly like kaniko.
- Parallel by default with a bare `chroot` per stage, sequential on request.
- Sequential by default, parallel on request.
- proot to restore `/proc` on the parallel path
  ([ADR-0018](0018-no-proot-in-1-0.md)).

## Decision Outcome

kaniko mode runs stages sequentially on `/` (or on the host workdir) by
default, like kaniko and with the environment's real `/proc` and `/dev`, and
`--parallel-stages` opts in to one rootfs directory per stage with `RUN` in a
bare `chroot`.

- With `--parallel-stages`: stages follow the DAG and `--jobs` like overlay
  mode; ranikust logs at build start that `RUN` has no `/proc`; a fake `/proc`
  is placed before each `RUN`; `JAVA_TOOL_OPTIONS` is injected under cgroup
  limits ([ADR-0019](0019-java-tool-options-on-parallel-path.md)); `/dev` nodes
  are created with `mknod` where permitted; per-stage roots are copies
  ([ADR-0017](0017-stage-rootfs-by-copy-not-hardlinks.md)).
- Where root **can** mount but overlay is unusable (workdir on NFS or FUSE,
  upper directory on the container's overlay, no overlay in the kernel),
  stages run in parallel by default with the full sandbox; nothing is lost.
- `--parallel-stages` is a placeholder name, fixed in phase 1 together with
  the flag list ([ADR-0011](0011-accept-common-kaniko-flags.md)).

**History.** On 2026-10-29 the maintainer first chose parallel-by-default for
this case, then reversed it the same day: a missing `/proc` fails silently
(`ps` prints nothing, `os.cpus()` returns 0, cgroup limits are ignored). Text
in experiments 12 and 12a that assumes parallel-by-default, or a
`--no-parallel-stages` flag, predates this decision.

### Consequences

- Good, because the default behaves exactly like kaniko, so no build breaks
  or silently changes when moving from kaniko.
- Good, because users who know their Dockerfiles get a large speed-up on
  stage-parallel workloads with one flag.
- Bad, because most CI users land in kaniko mode and do not get parallel
  stages unless they ask.
- Bad, because kaniko mode has two execution paths to maintain and test.

## Evidence

- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md): W3 at
  32 CPU, sequential 14.04 s, parallel bare `chroot` 7.99 s (−44 %), parallel
  with lazy export 5.38 s, overlay lazy 6.29 s; at 4 CPU 17.14 / 13.74 /
  10.02 / 10.64 s. W1 barely gains. Images were identical to sequential.
- [Experiment 13](../research/experiments/13-docker-rootful-vm.md): on real
  Docker 29.1 rootful, W3 21.15 → 13.21 s (−38 %), `mknod` gave 6/6 `/dev`
  nodes, image identical to the sequential build.
- [Experiment 12a](../research/experiments/12a-fake-proc.md): without `/proc`,
  `os.cpus().length` is 0; jest 29.3.1 ran 9.5× slower, terser-webpack-plugin
  5.3.10 3.5× slower; `next build` failed.
- [Experiment 14](../research/experiments/14-real-builds-without-proc.md):
  `next build` of W1 failed after 0.50 s without `/proc`; distro JDKs and
  TypeScript 7 fail even with a fake `/proc`.
- [Survey 05](../research/survey/05-decisions.md) Q12.

## More Information

Design: [kaniko-mode](../design/kaniko-mode.md) (fake `/proc`, warnings, tool
families), [scheduler](../design/scheduler.md).
