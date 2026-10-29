---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0019: Inject `JAVA_TOOL_OPTIONS` on the parallel path under cgroup limits

## Context and Problem Statement

On the `--parallel-stages` path, `RUN` runs in a bare `chroot` and the JVM
cannot detect the container's CPU and memory limits. It sizes thread pools,
forks and heaps from the host's CPUs and RAM, and large Maven and Gradle
builds are OOM-killed under `--memory`. The fake `/proc` fixes this for every
JDK under cgroup v1 but not for current JDKs under cgroup v2, which detect
cgroup v2 with `statfs`. Should ranikust correct the JVM's view itself?

## Considered Options

- Do nothing and document how to set JVM flags in the Dockerfile
  (experiment 14's recommendation).
- Rely on the fake `/proc` only.
- Inject `-XX:ActiveProcessorCount` and `-XX:MaxRAM` through
  `JAVA_TOOL_OPTIONS`.
- The same through `JDK_JAVA_OPTIONS`.
- Also inject a GC choice (`-XX:+UseSerialGC`) for small limits.

## Decision Outcome

On the `--parallel-stages` path, when ranikust's cgroup has a CPU or memory
limit and the stage rootfs contains `*/bin/java`, ranikust prepends
`-XX:+IgnoreUnrecognizedVMOptions -XX:ActiveProcessorCount=<N> -XX:MaxRAM=<memory.max>`
to `JAVA_TOOL_OPTIONS` in the environment of each `RUN` process, and never
injects GC flags.

- `N` = `ceil(quota / period)` from `cpu.max` (or the v1 equivalents).
- Prepended, so a value the user already set in `JAVA_TOOL_OPTIONS` wins.
- Only the `RUN` process environment; never `ENV` or the image config.
- One log line per stage says what was injected and why.
- Not `JDK_JAVA_OPTIONS`: it overrides the user's `JAVA_TOOL_OPTIONS`, the
  `javac` launcher ignores it, and JDK 8 ignores it.
- **Never inject GC flags:** a JVM whose build selects another collector
  dies with `Multiple garbage collectors selected`.
- **1.75 GiB caveat:** below 1792 MiB a container-aware JVM picks SerialGC;
  `MaxRAM` does not change that choice, so the JVM still picks G1 and uses
  more memory. When `memory.max` < 1792 MiB and a stage contains Java,
  ranikust warns before the build that multi-JVM builds may be OOM-killed and
  suggests running without `--parallel-stages` or setting a GC explicitly.

### Consequences

- Good, because large Java builds under a memory limit go from always failing
  to passing, with the same time and peak memory as with a real `/proc`.
- Bad, because every JVM prints `Picked up JAVA_TOOL_OPTIONS: ...` on stderr
  (33 lines per `mvn` build, 17 per `gradle`), which breaks scripts that parse
  the first line of `java -version`; Maven's compiler with `fork=true` logs a
  warning; a Gradle daemon log kept in the image contains the value.
- Bad, because builds under 1.75 GiB can still fail.

## Evidence

- [Experiment 15](../research/experiments/15-java-next-concurrency.md): a
  3,040-class project under `--cpus=4 --memory=2g`: real `/proc` 3/3 passed;
  bare `chroot` and fake `/proc` 0/3 (Maven OOM-killed, Gradle out of heap);
  with the injection 16/16 passed. Both flags are needed. At 1.5 GiB Maven
  passed 4/4 but Gradle was OOM-killed 4/4; `-XX:+UseSerialGC` fixed it, and
  forcing G1 made even the real-`/proc` build fail.
- [Experiment 16](../research/experiments/16-cgroup-v1-cpuset.md): with the
  fake `/proc`, all four JDKs saw the limits under cgroup v1; under v2,
  21.0.12.1 and 17.0.20.1 still saw 8 CPUs and a 2486 MiB heap.
- [Experiment 14](../research/experiments/14-real-builds-without-proc.md):
  without limits detected, the JVM saw 32 CPUs and a 7620 MiB maximum heap
  under `--cpus=4 --memory=3g`.

## More Information

Experiments 14 and 16 advised against automatic injection because of the
stderr noise; experiment 15 measured the OOM failures and the fix, and this
ADR follows it. If only one limit is set, emitting only the matching flag is
[inferred] and to be confirmed in phase 2. Design:
[kaniko-mode](../design/kaniko-mode.md).
