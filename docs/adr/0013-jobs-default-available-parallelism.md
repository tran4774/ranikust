---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0013: `--jobs` defaults to `available_parallelism()`

## Context and Problem Statement

Independent stages can run in parallel ([scheduler](../design/scheduler.md)).
`--jobs` bounds how many `RUN` commands run at once across stages. An earlier
kaniko-optimization measurement on OrbStack found BuildKit slower in parallel
than sequentially on workload W3, which argued for a low default. What should
the default be?

## Considered Options

- `--jobs 1` (sequential), parallelism on request.
- A fixed small number such as 2.
- The number of CPUs the process may use.

## Decision Outcome

`--jobs` defaults to `std::thread::available_parallelism()`, and the scheduler
additionally limits concurrency by available memory.

- `available_parallelism()` honours both the cgroup CPU quota and the CPU
  affinity mask, takes the smaller, rounds a fractional quota down, and never
  returns less than 1.
- kaniko mode with `/` as the shared rootfs runs stages sequentially and
  ignores `--jobs`; kaniko mode with one directory per stage uses `--jobs`
  like overlay mode ([ADR-0014](0014-kaniko-mode-sequential-by-default.md)).
- Further measurements (W2, workloads with more stages than cores) feed a
  tuning guide, not a change of default.

### Consequences

- Good, because builds use the machine without tuning, and the default is
  already right inside CPU-limited containers.
- Bad, because parallel stages raise peak memory sharply; without the
  memory-aware limit, a runner near its memory limit slows down as exports
  re-read layers from disk.

## Evidence

- [Experiment 05](../research/experiments/05-rust-ecosystem.md) §7: Rust
  1.99.1, `--cpus=2` → 2, `--cpus=2.5` → 2, `--cpus=0.5` → 1, affinity 0-3 → 4,
  `--cpus=2` with affinity 0-3 → 2; `--cpu-shares` is ignored.
- [Experiment 08](../research/experiments/08-parallel-stages-lazy-export.md):
  W3 in overlay mode with lazy export, default `--jobs` 6.02 s vs `--jobs 1`
  8.44 s at 32 CPU, 10.73 vs 11.35 s at 4 CPU; BuildKit was also faster in
  parallel on this machine (9.37 vs 12.28 s), so the OrbStack result did not
  reproduce. Peak memory rose from 4.1–4.5 GB to 6.6–7.3 GB.
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md) perf
  §5.5: in kaniko mode with per-stage directories, the default was faster than
  `--jobs 2` by 1.5–2.4 s; parallel walk on W3 needs 8.2–8.3 GB including page
  cache.
- [Survey 05](../research/survey/05-decisions.md) Q11 and §2.

## More Information

W3 has only three independent stages, so "number of cores" could not be told
apart from "number of stages". The memory-aware limit is specified in
[scheduler](../design/scheduler.md) but not yet built or measured.
