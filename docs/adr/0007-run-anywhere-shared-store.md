---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0007: Run anywhere; concurrent builds share one store

## Context and Problem Statement

ranikust runs on developer machines, long-lived CI runners, disposable VMs and
pods. On a shared machine several builds may run at the same time. BuildKit
solves this with a daemon that owns the store; kaniko avoids it by running one
build per container. ranikust is a single daemonless binary. How do concurrent
builds coordinate, and what may ranikust assume about the host (init system,
cgroup version, distribution)?

## Considered Options

- One build per store; concurrent builds need separate stores.
- A daemon that owns the store and serializes access, like BuildKit.
- Daemonless processes that share one store and coordinate through locks in
  the filesystem.

## Decision Outcome

ranikust is daemonless and runs anywhere: concurrent builds on one machine
share one store, each in its own `builds/<id>` area and coordinated by
per-digest locks, and ranikust depends on no init system, cgroup version or
distribution.

- Each build gets a private `builds/<build-id>/` directory for its context,
  upper directories and stage roots; shared content goes through the store
  and its locks ([ADR-0006](0006-persistent-store.md)).
- In kaniko mode the workdir is the build's root filesystem, so it is locked
  exclusively (`flock`): concurrent kaniko-mode builds need separate workdirs.
- No systemd, no cgroup creation or delegation. cgroups are only read, when
  present: to size the scheduler's memory limit and to build the fake `/proc`
  on the parallel kaniko path. Both cgroup v1 and v2 work.

### Consequences

- Good, because the same binary works in a pod, on a runner and on a laptop,
  with no service to install.
- Good, because concurrent builds reuse each other's downloads and
  extractions.
- Bad, because correctness depends on a lock protocol in the filesystem that
  must survive crashes; races with extraction and deletion (TOCTOU) are not
  yet tested.
- Bad, because ranikust cannot limit a `RUN`'s resources itself; that is left
  to the environment.

## Evidence

- [Experiment 13](../research/experiments/13-docker-rootful-vm.md): the
  mode-selection probe gives identical results under cgroup v1 and v2 on
  Docker 29.1 rootful.
- [Experiment 16](../research/experiments/16-cgroup-v1-cpuset.md): the
  parallel kaniko path works under both cgroup versions; W3 sequential vs
  parallel was 21.39 → 13.89 s (−35 %) on v1 and 21.29 → 14.35 s (−33 %) on
  v2.
- [Experiment 05](../research/experiments/05-rust-ecosystem.md):
  `available_parallelism()` honours cgroup v2 CPU quota and CPU affinity
  without any cgroup management by the process.
- [Survey 05](../research/survey/05-decisions.md) Q5;
  [survey 02](../research/survey/02-kaniko-mode-requirements.md) §4
  (workdir lock).

## More Information

Layout and lock rules: [cache-and-store](../design/cache-and-store.md).
Multi-build store concurrency and crash recovery are open test items for
phases 1 and 6.
