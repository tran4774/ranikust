# Experiment 08: Parallel stages and lazy export

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | On W3 (three independent `pnpm install` stages), how much do parallel stages (`--jobs`) and lazy export save in overlay mode, compared with BuildKit and kaniko? |
| **Answer** | Overlay with the default `--jobs` and lazy export built W3 in 6.02 s at 32 CPUs and 10.73 s at 4 CPUs, 70 % and 51 % below optimized kaniko. With lazy export, parallel never lost to sequential; lazy export saved 1.3–2.3 s (6.6 s under memory pressure), and skipping intermediate snapshots in walk mode saved 4.7–5.3 s. |
| **Related** | [ADR-0013](../../adr/0013-jobs-default-available-parallelism.md), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [scheduler design](../../design/scheduler.md), [overlay snapshot design](../../design/overlay-snapshot.md), [kaniko-mode design](../../design/kaniko-mode.md), experiments [06](06-overlay-vs-walk.md), [07](07-kaniko-vs-overlay.md), [12](12-kaniko-mode-parallel.md) |
| **Original** | `docs-rust/poc/08-w3-parallel-lazy/` (Vietnamese, not in this repository) |

## Summary

- W3 (`nextjs-kaniko/app-par`) was built with ten configurations: optimized
  kaniko, BuildKit parallel and sequential, a Go walk with and without
  snapshots of intermediate stages, and overlay with `--jobs` 1, 2 and default,
  each with lazy export or exporting every layer. 100 builds, none failed.
- **Parallel was not slower than sequential on this machine.** In every lazy
  configuration it was faster or equal: −2.5 s (−29 %) at 32 CPUs, −0.5 s
  (−5 %) at 4 CPUs. **BuildKit was also faster in parallel** (−2.9 s and
  −1.35 s), the opposite of the earlier W3 benchmark on OrbStack (kaniko
  optimization notes, docs/30; external, not in this repository)
  **[measured]**.
- **Lazy export** saved 1.3–1.7 s with `--jobs 1` (13 % of the build), 2.3 s
  in parallel at 4 CPUs (18 %) and 6.6 s in parallel at 32 CPUs (52 %, inflated
  by disk re-reads). In walk mode, **skipping snapshots of intermediate
  stages** saved more: 4.7–5.3 s (30–33 %) **[measured]**.
- **`--jobs` default = number of cores is reasonable on W3 with lazy export.**
  Memory, not CPU, is the real constraint: running the three installs at once
  raised peak cgroup memory from 4.1–4.5 GB (`--jobs 1`) to 6.6–7.3 GB
  (median; maximum 7.7 GB against an 8 GB limit) **[measured]**.
- Order of benefit on W3: skipping snapshot and export of intermediate stages,
  then overlay diffing, then parallelism. At 4 CPUs parallelism adds almost
  nothing **[measured]**.

## Questions

1. Is running stages in parallel slower than running them sequentially, as
   BuildKit was in the earlier W3 benchmark (docs/30)?
2. How much does lazy export save in overlay mode, and how much does skipping
   intermediate snapshots save in walk mode?
3. Is a `--jobs` default equal to the number of cores reasonable?

## Environment

- **Date:** 2026-10-29, 09:23–10:04; same machine and same session as
  [experiment 07](07-kaniko-vs-overlay.md).
- **Machine, timing, network, registry, limits:** as in experiment 07
  (`--memory=8g`; 32 CPUs or `--cpus=4`; `nk-registry` on loopback;
  `nk-verdaccio` with a warm cache).
- **Load:** the idle gate of experiment 07 ran before every build; no build
  had to wait.
- **BuildKit:** `docker.io/moby/buildkit@sha256:f8a833b2…` (v0.33.1-rootless).
- **Overlay:** rootless overlay in a user namespace (`podman --privileged`),
  as in experiment 07.

## Method

**Workload.** W3 = `nextjs-kaniko/app-par`: three independent stages `a`, `b`
and `c`. Each `COPY`s a lockfile and then runs
`RUN npm install -g pnpm && pnpm install` on one third of W1's dependencies.
The `final` stage (base `node`) `COPY --from`s one `summary.txt` file from each
stage, then runs `RUN cat`. The final image is the base plus 4 very small
layers; the three `pnpm install` layers (459 MB gz in total) are not part of
it.

**Configurations:**

- **Overlay** (`ov-jN[-all]`): each stage has its own lowerdir chain and its
  own mount point. Stages run as goroutines that wait for their parent stage
  and for the source stages of their `COPY --from`. `-jobs N` is a semaphore
  around **each `RUN`/`COPY`** (mount → command → umount), as in
  [survey 06](../survey/06-overlay-and-parallelism.md) §4.2. N = 0 means
  GOMAXPROCS (32 or 4); with 3 independent stages both mean "all three at
  once". The base (5 layers) is pulled and extracted **once**, in parallel,
  and shared by all 4 stages. `COPY --from` reads from a read-only overlay
  mount of the source stage's final rootfs.
  - `lazy` (`-export final`): only the final image's layers are tar + gzipped
    (the 4 small layers of `final`).
  - `export all layers` (`-export all`): all 10 layers are tar + gzipped
    (including the three `pnpm` layers, 459 MB gz), each as soon as it is done,
    on a single export queue that runs in parallel with the commands. This is
    kaniko's "always snapshot", but kept off the commands' critical path.
- **Walk** (`walk`, `walk-lazy`): as in experiment 07. Stages run
  sequentially in Dockerfile order, and each stage extracts the base into its
  own rootfs. `walk-lazy` (`-export final`): stages that are not part of the
  final image get **no baseline scan and no snapshot after commands**; they
  only run their commands. `COPY --from` reads directly from the source
  stage's rootfs.
- **kaniko** `k-stream`: as in experiment 07 (`kaniko-bench:stream`, 41 flags
  + `STREAM_TAR_COMPRESS`), sequential.
- **BuildKit** `bk` / `bk-serial`: exactly the `run-fair.sh` command
  (`buildctl-daemonless.sh`, `--oci-worker-no-process-sandbox`,
  `seccomp=unconfined`, `apparmor=unconfined`, an empty state volume, DNS =
  the `nextjs-kaniko_internal` gateway (10.89.2.1), sub-ids
  `user:1001:64535`, output pushed with `compression=zstd`). `bk-serial` adds
  `[worker.oci] max-parallelism = 1`. BuildKit has no internal metrics; the
  per-`RUN` times come from the `--progress plain` log (0.1 s resolution). No
  configuration used a cache.

**Harness and binary** are shared with experiment 07 (`run.sh` calls
`../07-kaniko-vs-overlay/run.sh`; binary `../07-kaniko-vs-overlay/bin/ovbuild`).

**Rounds.** Every cell is the **median (min–max) of 4 rounds** after one
discarded warm-up round; the 10 configurations were interleaved and their order
rotated every round. Differences are paired by round.

**Correctness** (quick check only, in a trial run before the measured sets):
`/out/all.txt` in the final images of BuildKit, overlay and `walk-lazy`
reported the same package counts (21/19/20) and `du -sk` sizes a few KB apart.
kaniko's image was not read in that check (the file-extraction loop did not
match kaniko's layer order). W3 images were not listed and compared in full as
in [experiment 07](07-kaniko-vs-overlay.md).

## Results

All values are **[measured]** unless labelled otherwise.

### Host wall time, W3 (s)

| W3, host wall (s) | 32 CPUs | `--cpus=4` |
|---|---|---|
| optimized kaniko (`k-stream`) | 19.91 (19.83–20.07) | 22.08 (22.03–22.14) |
| BuildKit parallel | 9.37 (9.00–9.64) | 13.98 (13.82–14.03) |
| BuildKit `max-parallelism=1` | 12.28 (12.13–12.44) | 15.37 (15.19–15.52) |
| Go walk (snapshot every stage) | 14.32 (14.22–14.44) | 17.38 (17.30–17.39) |
| Go walk, no snapshot of stages used only through `COPY --from` | 9.58 (9.49–10.03) | 12.09 (11.95–12.13) |
| overlay `--jobs 1`, lazy | 8.44 (8.26–8.62) | 11.35 (11.21–11.52) |
| overlay `--jobs 2`, lazy | 6.52 (6.26–6.89) | 10.63 (10.55–10.76) |
| **overlay default `--jobs` (= cores), lazy** | **6.02** (5.55–6.17) | **10.73** (10.52–10.92) |
| overlay `--jobs 1`, export all layers | 9.65 (9.62–9.94) | 13.06 (12.76–13.20) |
| overlay default `--jobs`, export all layers | 12.69 (7.78–13.84) | 12.99 (12.90–13.24) |

Differences paired by round (s), median (min–max):

| | 32 CPUs | `--cpus=4` |
|---|---|---|
| BuildKit: parallel − sequential | **−2.90** (−3.45 to −2.51) | **−1.35** (−1.70 to −1.24) |
| overlay lazy: default − `--jobs 1` | **−2.52** (−2.71 to −2.27) | **−0.53** (−0.99 to −0.45) |
| overlay lazy: `--jobs 2` − `--jobs 1` | −1.86 (−2.36 to −1.49) | −0.76 (−0.87 to −0.45) |
| overlay lazy: default − `--jobs 2` | −0.56 (−1.21 to −0.11) | +0.09 (−0.16 to +0.31) |
| overlay, export all layers: default − `--jobs 1` | +2.96 (−1.85 to +4.06) | +0.01 (−0.15 to +0.18) |
| lazy − export all layers, `--jobs 1` | **−1.28** (−1.37 to −1.17) | **−1.69** (−1.88 to −1.39) |
| lazy − export all layers, default `--jobs` | **−6.60** (−7.82 to −2.23) | **−2.28** (−2.64 to −2.02) |
| Go walk: no intermediate snapshots − snapshot every stage | **−4.70** (−4.91 to −4.31) | **−5.27** (−5.43 to −5.22) |
| overlay default lazy − BuildKit parallel | −3.37 (−3.92 to −2.98) | −3.24 (−3.51 to −2.90) |
| overlay `--jobs 1` lazy − BuildKit sequential | −3.85 (−3.97 to −3.71) | −4.00 (−4.15 to −3.87) |
| overlay default lazy − optimized kaniko | −13.97 (−14.28 to −13.76), −70 % | −11.35 (−11.54 to −11.18), −51 % |

### The three `pnpm install` commands in parallel

| (s, median) | Total of the 3 commands | Longest command | Span from first start to last finish |
|---|---|---|---|
| 32 CPUs, overlay `--jobs 1` | 7.04 | 2.73 | 7.48 |
| 32 CPUs, overlay `--jobs 2` | 8.71 | 3.66 | 5.60 |
| 32 CPUs, overlay default | 12.72 | 4.72 | 5.08 |
| 32 CPUs, BuildKit sequential / parallel ¹ | 8.15 / 14.10 | 3.15 / 5.20 | – |
| 4 CPUs, overlay `--jobs 1` | 9.97 | 3.71 | 10.44 |
| 4 CPUs, overlay `--jobs 2` | 15.15 | 5.88 | 9.70 |
| 4 CPUs, overlay default | 27.13 | 9.49 | 9.87 |
| 4 CPUs, BuildKit sequential / parallel ¹ | 11.10 / 28.20 | 4.15 / 9.80 | – |

¹ BuildKit: step time from the log, including runc start-up and the snapshot
commit.

- Run together, **each command slows down** 1.7–1.8× (32 CPUs) and 2.6–2.7×
  (4 CPUs), because `pnpm install` already uses several cores. At 4 CPUs the
  total time of the three commands merely shifts: the a–c span drops from 10.4
  to 9.9 s. At 32 CPUs there are idle cores, so the span drops from 7.5 to
  5.1 s.
- BuildKit shows the same shape. In parallel, the sum of the three step times
  (14.1 s at 32 CPUs) exceeds the wall time of the whole build (9.4 s), so the
  steps certainly overlapped.

### Lazy export and skipped snapshots

The snapshot (+ export) part of stages `a`, `b` and `c`, which is exactly what
lazy export removes, median:

| | 32 CPUs | 4 CPUs |
|---|---|---|
| kaniko: `snapshot.command` of the 3 `RUN`s (walk, hash, resolve, streamed tar+gzip) | 7.86 s | 9.38 s |
| Go walk: `syncfs` + walk/hash + tar+gzip | 4.51 s | 4.93 s |
| overlay: mount/umount + upperdir walk | 0.66–0.69 s | 0.64–0.79 s |
| overlay, export all layers: tar+gzip of all 10 layers (in parallel with commands) | 3.00 s (`--jobs 1`); 7.73 s (default) | 4.59 s; 3.22 s |

- Lazy overlay never turns the diffs of `a`, `b` and `c` into tars. What
  remains (0.7 s) is mount/umount and the upperdir walk. ranikust could also
  skip the upperdir walk for stages whose layer is not needed **[inferred]**.
- kaniko spends 7.9–9.4 s on this part. That is the upper bound of what kaniko
  would save by implementing the docs/30 §5.4 hypothesis. The Go walk
  simulation saved 4.7–5.3 s of real wall time.

### Parallel + export all layers: memory pressure

At 32 CPUs, default `--jobs` with every layer exported varied widely
(7.8–13.8 s). The cause is the one found in
[experiment 07](07-kaniko-vs-overlay.md): the export had to re-read layers from
disk.

| 32 CPUs, per run | Peak cgroup memory | Export of the 3 `pnpm` layers: time / actual disk reads |
|---|---|---|
| default `--jobs`, runs 1, 2, 3 and warm-up | 6.4–6.8 GB | 1.8–3.5 s / 183–538 MB per layer |
| default `--jobs`, run 4 | 7.6 GB | 0.66–1.10 s / 0 MB |
| `--jobs 1`, all 5 runs | 4.6–5.1 GB | 0.65–1.42 s / 0 MB |

Running three `pnpm install`s at once pushes cgroup memory close to the 8 GB
limit. Pages of freshly written files are evicted, and the export has to read
them back. At 4 CPUs the same configuration peaked at 7.5–7.6 GB **without**
any re-read, so the effect depends on when reclaim happens and is not
deterministic. **[inferred]** This is one more reason for **lazy export by
default**: it saves not only the CPU time of tar + gzip, but also re-reading
hundreds of MB when memory is tight.

### Comparison with the earlier W3 benchmark (another machine)

| W3 without cache, wall (s) | `bk` parallel | `bk-serial` | `k-stream` | parallel − sequential |
|---|---|---|---|---|
| docs/30, OrbStack arm64, `--cpus=4`, 5 rounds | 34.6 | 29.2 | 43.1 | **+3.5** |
| docs/30, `--cpus=8`, 3 rounds | 31.1 | 26.4 | 40.9 | **+6.1** |
| This experiment, Ryzen 9950X3D native, `--cpus=4`, 4 rounds | 13.98 | 15.37 | 22.08 | **−1.35** |
| This experiment, 32 CPUs, 4 rounds | 9.37 | 12.28 | 19.91 | **−2.90** |

On this machine every builder was 2–3.5× faster, and the sign of "parallel −
sequential" flipped. docs/30 §5.3 suspected contention for CPU, disk and
verdaccio. Here the CPU was shared too (see the `pnpm install` table) and
parallelism still won. **[inferred]** The loss in docs/30 most likely came
from I/O or networking in the OrbStack VM. Not verified.

## Findings

1. Parallel stages are not slower than sequential ones on native Linux: with
   lazy export, overlay's default `--jobs` was 2.5 s (29 %) faster than
   `--jobs 1` at 32 CPUs and 0.5 s (5 %) faster at 4 CPUs, and BuildKit was
   2.9 s and 1.35 s faster in parallel **[measured]** (paired differences).
2. The docs/30 result "parallel is slower" depends on the environment, not on
   W3 **[measured]** for this machine (comparison table); its cause on OrbStack
   is **[inferred]**.
3. The only case where parallelism hurt was parallel + export of every layer
   at 32 CPUs (+3.0 s, with a min–max range that includes 0), caused by memory
   pressure and disk re-reads **[measured]** (memory pressure table).
4. Lazy export saved 1.3–1.7 s with `--jobs 1`, 2.3 s in parallel at 4 CPUs
   and 6.6 s in parallel at 32 CPUs, and was never slower **[measured]**
   (paired differences).
5. In walk mode, skipping snapshots of stages used only through `COPY --from`
   saved 4.7–5.3 s (30–33 %), more than lazy export saves in overlay mode,
   because the walk's snapshot includes walk + hash and not only tar + gzip
   **[measured]**. This supports the docs/30 §5.4 hypothesis, though by a Go
   simulation, not real kaniko.
6. Running commands concurrently multiplies peak memory: 4.1–4.5 GB with
   `--jobs 1` against 6.6–7.3 GB in parallel on W3 **[measured]**.
7. A `--jobs` default equal to the core count was never slower than `--jobs 1`
   or `--jobs 2` beyond noise on W3; with only 3 independent stages, "cores"
   cannot be told apart from "unlimited" or "number of stages" here
   **[measured]**.

## Implications for ranikust

1. **Lazy export is the default in overlay mode**
   ([survey 06](../survey/06-overlay-and-parallelism.md) §3.2,
   [overlay snapshot design](../../design/overlay-snapshot.md)): it saves
   1.3–2.3 s on W3 without memory pressure and up to 6.6 s with it, and was
   never slower.
2. **Kaniko mode should be lazy too:** no snapshot (no baseline scan, no
   walk/hash, no tar) for stages used only through `COPY --from`/`FROM` when
   their layer is not needed (no `--cache`, no `--target`, no push). The
   simulation saved 4.7–5.3 s (30–33 %) on W3. The exact conditions remain
   those of docs/30 §5.4: detecting deleted files if the stage later needs a
   layer, and the interaction with `--cache`
   ([kaniko-mode design](../../design/kaniko-mode.md)). The walk simulation
   ran stages sequentially, which is also kaniko mode's default
   ([ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)).
3. **Keep the `--jobs` default at the number of cores**
   ([ADR-0013](../../adr/0013-jobs-default-available-parallelism.md)). On W3,
   lazy parallel was always faster than or equal to sequential at both 4 and
   32 CPUs. User guidance should talk about **memory** more than CPU: the
   number of concurrent `RUN`s multiplies peak RAM (4.1–4.5 → 6.6–7.3 GB on
   W3). **[inferred]** A soft limit based on the cgroup's `memory.max`, or
   lowering `--jobs` automatically when `memory.current` nears the limit, may
   be needed; not designed or measured
   ([scheduler design](../../design/scheduler.md)).
4. **Do not cite docs/30 for "parallel is slower".** On native Linux both
   BuildKit and overlay gain from parallelism. How much depends on whether the
   commands already use every core (32 CPUs: −29 %; 4 CPUs: −5 %).
5. **Order of benefit on W3** (32 CPUs, against optimized kaniko at 19.9 s):
   skipping snapshot and export of intermediate stages (lazy Go walk 9.6 s),
   then overlay diffing (overlay `--jobs 1` lazy 8.4 s), then parallelism
   (overlay default 6.0 s). At 4 CPUs parallelism adds almost nothing
   (11.4 → 10.7 s). This matches
   [survey 06](../survey/06-overlay-and-parallelism.md) §4.3: "the biggest
   gains come from skipping the snapshot walk and unused exports, not from
   parallelism".

## Limitations and open questions

- One workload, three stages on the same base, one warm verdaccio cache, a
  loopback registry. **No real network**, where parallelism might gain more
  by overlapping base pulls.
- W3 has only 3 independent stages, so the `--jobs` default was not tested
  with more stages than cores.
- `--jobs` limits `RUN`/`COPY`, not export. The export queue is a single
  goroutine (each layer is compressed with GOMAXPROCS threads).
- Overlay was rootless overlay in a user namespace (`podman --privileged`);
  real-root overlay was not re-measured.
- `ovbuild` is not a complete builder (see
  [experiment 07](07-kaniko-vs-overlay.md), Limitations). BuildKit and kaniko
  do more work. BuildKit's time also includes starting `buildkitd` inside the
  container and zstd-compressing the (very small) final image.
- The Go walk extracts the base 4 times (once per stage, 2.4–2.8 s) instead of
  reusing it as kaniko does with `REUSE_BASE_ROOTFS`/`SHARED_BASE_CACHE`
  (1.1 s; `kaniko@44aa7ddc0 pkg/config/featureflags.go:167`, `:189`
  **[code]**). This costs the walk about 1.3–1.7 s in both `walk` and
  `walk-lazy`.
- `pnpm install` ran faster inside kaniko at 4 CPUs (7.6 s for the three
  commands) than inside the `ovbuild` container (walk 9.1 s, overlay
  `--jobs 1` 10.0 s). **Not explained**, as in experiment 07. If it were
  explained away, overlay and walk would lead kaniko by even more at 4 CPUs.
- Four rounds per cell; no bootstrap confidence intervals. Shared machine; the
  harness waited for an idle machine before each run.
- The memory pressure effect depends on the machine-wide page cache state and
  cannot be reproduced exactly.

## Reproduction

The PoC sources are archived outside this repository, under
`docs-rust/poc/08-w3-parallel-lazy/`:

- `run.sh`: wraps experiment 07's harness;
  `./run.sh <set> <rounds> <cpus|0> <configurations>`.
- `all.sh`: runs `w3-cpu32` and `w3-cpu4`, 4 rounds + warm-up, 10
  configurations.
- `summarize.py`: reuses experiment 07's `summarize.py` and adds the W3
  tables.
- `results/<set>/<configuration>-<round>/` (`run.json`, `build.log`,
  `ovbuild.json` or `metrics.json`, plus `buildkitd.toml` for BuildKit),
  `results/<set>/run.log`, `results/all.log`.

```sh
../07-kaniko-vs-overlay/run.sh build          # if bin/ovbuild does not exist yet
./all.sh                                      # w3-cpu32 and w3-cpu4, 4 rounds + warm-up, 10 configurations
./run.sh <set> <rounds> <cpus|0> k-stream bk bk-serial walk walk-lazy ov-j1 ov-j2 ov-j0 ov-j1-all ov-j0-all
python3 -I summarize.py results/w3-cpu32 results/w3-cpu4 > results/tables.md
../07-kaniko-vs-overlay/run.sh clean          # remove the kvo-* volumes and the kvo-* repositories in the registry
```

Requirements: as for [experiment 07](07-kaniko-vs-overlay.md), plus the
BuildKit image
`docker.io/moby/buildkit@sha256:f8a833b2de9d68e27f0815e4a737abdfaf8a2e4c615650557df11025101557b4`
(already present on the machine). No real root is needed.

## References

1. [Experiment 06: Overlay vs walk](06-overlay-vs-walk.md) (lazy export cost).
2. [Experiment 07: kaniko vs overlay](07-kaniko-vs-overlay.md) (harness,
   binary, export re-reads).
3. [Experiment 12: Parallel stages in kaniko mode](12-kaniko-mode-parallel.md).
4. [Survey 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md)
   §3.2, §4.2, §4.3.
5. Kaniko optimization notes, docs/30 §5.3–§5.4 (W3 benchmark on OrbStack,
   lazy snapshot hypothesis) (external, not in this repository); benchmark
   harness `nextjs-kaniko/bench/run-fair.sh`.
6. `kaniko@44aa7ddc0 pkg/config/featureflags.go:167`
   (`FF_KANIKO_REUSE_BASE_ROOTFS`), `:189` (`FF_KANIKO_SHARED_BASE_CACHE`).

## Appendix: full tables

Generated by `summarize.py`. Medians (min–max) of 4 rounds.

### w3-cpu32

Host wall time (s): T0 just before `podman run -d`, T1 when `podman wait`
returns. Teardown (removing the container and workdir) is excluded.

| configuration | n | host wall | self-reported by the process | teardown |
|---|---|---|---|---|
| optimized kaniko (`kaniko-bench:stream`) | 4 | **19.91 (19.83–20.07)** | 19.70 (19.59–19.86) | 0.14 (0.14–0.15) |
| BuildKit parallel | 4 | **9.37 (9.00–9.64)** | – | 1.75 (1.71–1.79) |
| BuildKit max-parallelism=1 | 4 | **12.28 (12.13–12.44)** | – | 1.77 (1.74–1.79) |
| optimized Go walk | 4 | **14.32 (14.22–14.44)** | 14.12 (14.01–14.25) | 1.93 (1.91–2.00) |
| Go walk, no intermediate-stage snapshots | 4 | **9.58 (9.49–10.03)** | 9.39 (9.32–9.83) | 1.93 (1.90–1.94) |
| overlay jobs=1, lazy | 4 | **8.44 (8.26–8.62)** | 8.05 (7.92–8.19) | 1.80 (1.79–1.80) |
| overlay jobs=2, lazy | 4 | **6.52 (6.26–6.89)** | 6.15 (5.85–6.53) | 1.84 (1.79–1.84) |
| overlay jobs default, lazy | 4 | **6.02 (5.55–6.17)** | 5.66 (5.32–5.81) | 1.79 (1.77–1.82) |
| overlay jobs=1, export all layers | 4 | **9.65 (9.62–9.94)** | 9.28 (9.15–9.45) | 1.82 (1.80–1.85) |
| overlay jobs default, export all layers | 4 | **12.69 (7.78–13.84)** | 12.32 (7.29–13.59) | 1.84 (1.81–1.92) |

Host wall difference against `k-stream`, paired by round (s), median (min–max):

| configuration | Δ wall | Δ / kaniko wall |
|---|---|---|
| BuildKit parallel | −10.51 (−11.08 to −10.25) | −53 (−55 to −52) % |
| BuildKit max-parallelism=1 | −7.66 (−7.74 to −7.52) | −38 (−39 to −38) % |
| optimized Go walk | −5.62 (−5.64 to −5.56) | −28 (−28 to −28) % |
| Go walk, no intermediate-stage snapshots | −10.32 (−10.54 to −9.87) | −52 (−52 to −50) % |
| overlay jobs=1, lazy | −11.47 (−11.57 to −11.45) | −58 (−58 to −57) % |
| overlay jobs=2, lazy | −13.36 (−13.82 to −13.00) | −67 (−69 to −65) % |
| overlay jobs default, lazy | −13.97 (−14.28 to −13.76) | −70 (−72 to −69) % |
| overlay jobs=1, export all layers | −10.21 (−10.32 to −10.13) | −51 (−52 to −50) % |
| overlay jobs default, export all layers | −7.27 (−12.05 to −6.15) | −36 (−61 to −31) % |

Process time breakdown (s), median (min–max):

| part | optimized kaniko (`kaniko-bench:stream`) | optimized Go walk | Go walk, no intermediate-stage snapshots | overlay jobs=1, lazy | overlay jobs=2, lazy | overlay jobs default, lazy | overlay jobs=1, export all layers | overlay jobs default, export all layers |
|---|---|---|---|---|---|---|---|---|
| base pull + extract | 1.13 (1.11–1.15) | 2.76 (2.73–2.79) | 2.63 (2.60–3.06) | 0.38 (0.37–0.38) | 0.37 (0.37–0.38) | 0.37 (0.35–0.37) | 0.37 (0.37–0.38) | 0.37 (0.37–0.38) |
| baseline scan (`snapshot.init`) | 1.00 (0.99–1.01) | 0.12 (0.12–0.13) | 0.03 (0.03–0.03) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) |
| running RUN/COPY commands | 6.94 (6.92–7.03) | 6.46 (6.41–6.59) | 6.46 (6.43–6.56) | 7.05 (6.86–7.14) | 8.72 (8.35–9.83) | 12.74 (11.86–13.55) | 7.15 (6.97–7.25) | 12.46 (12.23–13.60) |
| diff (kaniko: `snapshot.command` − `snapshot.tar` = syncfs, walk/hash, resolve; walk: syncfs + walk/hash; overlay: mount/umount + upperdir walk) | 5.17 (5.11–5.21) | 1.98 (1.97–1.99) | 0.09 (0.09–0.10) | 0.92 (0.90–0.92) | 0.93 (0.93–0.95) | 0.90 (0.89–0.91) | 0.88 (0.86–0.90) | 0.94 (0.93–1.04) |
| tar+gzip on the critical path (kaniko: `snapshot.tar` + remaining compression; walk: tar+gzip after each command; overlay: export left after the last stage) | 2.93 (2.92–2.95) | 2.62 (2.60–2.63) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 1.14 (1.01–1.14) | 6.64 (1.69–7.81) |
| push | 0.19 (0.19–0.20) | 0.16 (0.15–0.16) | 0.15 (0.15–0.15) | 0.19 (0.17–0.20) | 0.17 (0.16–0.23) | 0.19 (0.15–0.23) | 0.18 (0.17–0.19) | 0.18 (0.16–0.21) |
| other (startup, config, saving/removing stage FS, overlap) | 2.33 (2.30–2.35) | 0.01 (0.01–0.01) | 0.01 (0.00–0.01) | −0.44 (−0.52 to −0.38) | −4.31 (−4.84 to −3.45) | −8.55 (−9.16 to −8.02) | −0.37 (−0.43 to −0.36) | −8.38 (−9.06 to −8.26) |
| **process total** | 19.70 (19.59–19.86) | 14.12 (14.01–14.25) | 9.39 (9.32–9.83) | 8.05 (7.92–8.19) | 6.15 (5.85–6.53) | 5.66 (5.32–5.81) | 9.28 (9.15–9.45) | 12.32 (7.29–13.59) |
| total minus base pull/extract and push | 18.38 (18.29–18.52) | 11.19 (11.13–11.30) | 6.59 (6.55–6.68) | 7.48 (7.37–7.61) | 5.61 (5.30–5.92) | 5.10 (4.74–5.28) | 8.72 (8.60–8.91) | 11.76 (6.74–13.03) |
| for reference: tar+gzip of all layers (overlay runs it in parallel with commands) | – | 2.62 (2.60–2.63) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 3.00 (2.98–3.06) | 7.73 (2.79–8.77) |

#### w3-cpu32: pairwise comparisons

Host wall difference, paired by round (s), median (min–max):

| comparison | Δ wall | Δ % |
|---|---|---|
| BuildKit: parallel − sequential | −2.90 (−3.45 to −2.51) | −24 (−28 to −21) % |
| overlay lazy: jobs default − jobs=1 | −2.52 (−2.71 to −2.27) | −29 (−33 to −27) % |
| overlay lazy: jobs=2 − jobs=1 | −1.86 (−2.36 to −1.49) | −22 (−27 to −18) % |
| overlay lazy: jobs default − jobs=2 | −0.56 (−1.21 to −0.11) | −8 (−18 to −2) % |
| overlay export all layers: jobs default − jobs=1 | 2.96 (−1.85 to 4.06) | 30 (−19 to 42) % |
| lazy − export all layers, jobs=1 | −1.28 (−1.37 to −1.17) | −13 (−14 to −12) % |
| lazy − export all layers, jobs default | −6.60 (−7.82 to −2.23) | −52 (−57 to −29) % |
| Go walk: no intermediate-stage snapshots − snapshot every stage | −4.70 (−4.91 to −4.31) | −33 (−34 to −30) % |
| overlay jobs=1 lazy − BuildKit sequential | −3.85 (−3.97 to −3.71) | −31 (−32 to −31) % |
| overlay jobs default lazy − BuildKit parallel | −3.37 (−3.92 to −2.98) | −36 (−41 to −33) % |
| overlay jobs default lazy − optimized kaniko | −13.97 (−14.28 to −13.76) | −70 (−72 to −69) % |
| Go walk lazy − optimized kaniko | −10.32 (−10.54 to −9.87) | −52 (−52 to −50) % |

#### w3-cpu32: the three `pnpm install` commands (stages a, b, c) and export

- Total and longest command: `RUN` run time (BuildKit: step time from the log,
  including the snapshot commit).
- a–c span: from the start of the first `RUN` to the end of the last (overlay).
- snapshot a–c: the diff (+ export) of stages a, b and c; this is what lazy
  export or skipping snapshots saves.

| configuration | 3 commands total | longest command | a–c span | snapshot+export a–c | layers tar+gzipped | tar+gzip total | gz MB |
|---|---|---|---|---|---|---|---|
| optimized kaniko (`kaniko-bench:stream`) | 6.94 (6.92–7.03) | 2.74 (2.70–2.76) | – | 7.86 (7.80–7.90) | – | – | – |
| BuildKit parallel | 14.10 (13.30–14.90) | 5.20 (4.80–5.40) | – | – | – | – | – |
| BuildKit max-parallelism=1 | 8.15 (8.10–8.20) | 3.15 (3.10–3.20) | – | – | – | – | – |
| optimized Go walk | 6.45 (6.40–6.58) | 2.43 (2.40–2.46) | 12.40 (12.30–12.52) | 4.51 (4.51–4.53) | 10 (10–10) | 2.62 (2.60–2.63) | 459 (459–459) |
| Go walk, no intermediate-stage snapshots | 6.46 (6.42–6.55) | 2.43 (2.34–2.47) | 7.60 (7.57–7.70) | 0.00 (0.00–0.00) | 4 (4–4) | 0.00 (0.00–0.00) | 0 (0–0) |
| overlay jobs=1, lazy | 7.04 (6.85–7.13) | 2.73 (2.67–2.82) | 7.48 (7.36–7.61) | 0.67 (0.66–0.67) | 4 (4–4) | 0.00 (0.00–0.00) | 0 (0–0) |
| overlay jobs=2, lazy | 8.71 (8.34–9.82) | 3.66 (2.94–4.21) | 5.60 (5.29–5.92) | 0.68 (0.68–0.69) | 4 (4–4) | 0.00 (0.00–0.00) | 0 (0–0) |
| overlay jobs default, lazy | 12.72 (11.84–13.53) | 4.72 (4.35–4.90) | 5.08 (4.72–5.26) | 0.66 (0.65–0.66) | 4 (4–4) | 0.00 (0.00–0.00) | 0 (0–0) |
| overlay jobs=1, export all layers | 7.14 (6.96–7.24) | 2.77 (2.72–2.84) | 7.63 (7.45–7.76) | 0.64 (0.64–0.66) | 10 (10–10) | 3.00 (2.98–3.06) | 459 (459–459) |
| overlay jobs default, export all layers | 12.45 (12.22–13.59) | 4.64 (4.51–5.08) | 5.06 (4.91–5.46) | 0.69 (0.68–0.73) | 10 (10–10) | 7.73 (2.79–8.77) | 459 (459–459) |

### w3-cpu4

Host wall time (s): T0 just before `podman run -d`, T1 when `podman wait`
returns. Teardown (removing the container and workdir) is excluded.

| configuration | n | host wall | self-reported by the process | teardown |
|---|---|---|---|---|
| optimized kaniko (`kaniko-bench:stream`) | 4 | **22.08 (22.03–22.14)** | 21.85 (21.82–21.93) | 0.14 (0.13–0.14) |
| BuildKit parallel | 4 | **13.98 (13.82–14.03)** | – | 1.80 (1.79–1.81) |
| BuildKit max-parallelism=1 | 4 | **15.37 (15.19–15.52)** | – | 1.74 (1.72–1.75) |
| optimized Go walk | 4 | **17.38 (17.30–17.39)** | 17.18 (17.10–17.19) | 1.91 (1.90–1.92) |
| Go walk, no intermediate-stage snapshots | 4 | **12.09 (11.95–12.13)** | 11.91 (11.76–11.95) | 1.88 (1.86–1.90) |
| overlay jobs=1, lazy | 4 | **11.35 (11.21–11.52)** | 10.99 (10.75–11.09) | 1.77 (1.76–1.78) |
| overlay jobs=2, lazy | 4 | **10.63 (10.55–10.76)** | 10.27 (10.16–10.39) | 1.80 (1.78–1.81) |
| overlay jobs default, lazy | 4 | **10.73 (10.52–10.92)** | 10.41 (10.22–10.62) | 1.86 (1.83–1.91) |
| overlay jobs=1, export all layers | 4 | **13.06 (12.76–13.20)** | 12.75 (12.49–12.82) | 1.80 (1.79–1.80) |
| overlay jobs default, export all layers | 4 | **12.99 (12.90–13.24)** | 12.54 (12.47–12.79) | 1.85 (1.83–1.85) |

Host wall difference against `k-stream`, paired by round (s), median (min–max):

| configuration | Δ wall | Δ / kaniko wall |
|---|---|---|
| BuildKit parallel | −8.11 (−8.28 to −8.04) | −37 (−37 to −36) % |
| BuildKit max-parallelism=1 | −6.74 (−6.84 to −6.58) | −30 (−31 to −30) % |
| optimized Go walk | −4.74 (−4.76 to −4.65) | −21 (−22 to −21) % |
| Go walk, no intermediate-stage snapshots | −10.02 (−10.08 to −9.96) | −45 (−46 to −45) % |
| overlay jobs=1, lazy | −10.77 (−10.82 to −10.55) | −49 (−49 to −48) % |
| overlay jobs=2, lazy | −11.45 (−11.59 to −11.27) | −52 (−52 to −51) % |
| overlay jobs default, lazy | −11.35 (−11.54 to −11.18) | −51 (−52 to −51) % |
| overlay jobs=1, export all layers | −8.99 (−9.34 to −8.94) | −41 (−42 to −40) % |
| overlay jobs default, export all layers | −9.12 (−9.17 to −8.79) | −41 (−42 to −40) % |

Process time breakdown (s), median (min–max):

| part | optimized kaniko (`kaniko-bench:stream`) | optimized Go walk | Go walk, no intermediate-stage snapshots | overlay jobs=1, lazy | overlay jobs=2, lazy | overlay jobs default, lazy | overlay jobs=1, export all layers | overlay jobs default, export all layers |
|---|---|---|---|---|---|---|---|---|
| base pull + extract | 1.11 (1.11–1.13) | 2.76 (2.74–2.77) | 2.39 (2.37–2.49) | 0.37 (0.36–0.37) | 0.37 (0.37–0.38) | 0.37 (0.37–0.37) | 0.37 (0.37–0.38) | 0.37 (0.37–0.37) |
| baseline scan (`snapshot.init`) | 1.03 (1.01–1.06) | 0.13 (0.13–0.13) | 0.03 (0.03–0.03) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) |
| running RUN/COPY commands | 7.62 (7.58–7.67) | 9.11 (8.98–9.15) | 9.21 (9.08–9.26) | 9.98 (9.83–10.07) | 15.16 (14.97–15.36) | 27.14 (26.62–27.78) | 10.61 (10.46–10.94) | 26.96 (26.76–27.41) |
| diff (kaniko: `snapshot.command` − `snapshot.tar` = syncfs, walk/hash, resolve; walk: syncfs + walk/hash; overlay: mount/umount + upperdir walk) | 5.67 (5.64–5.68) | 2.22 (2.20–2.23) | 0.09 (0.08–0.09) | 0.90 (0.89–0.92) | 0.91 (0.91–0.92) | 0.92 (0.91–0.93) | 0.88 (0.87–0.89) | 1.06 (1.04–1.10) |
| tar+gzip on the critical path (kaniko: `snapshot.tar` + remaining compression; walk: tar+gzip after each command; overlay: export left after the last stage) | 3.96 (3.95–3.99) | 2.80 (2.79–2.81) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 1.17 (0.62–1.17) | 2.14 (2.07–2.25) |
| push | 0.19 (0.19–0.20) | 0.16 (0.15–0.19) | 0.15 (0.15–0.16) | 0.17 (0.15–0.18) | 0.18 (0.16–0.20) | 0.16 (0.15–0.19) | 0.16 (0.15–0.16) | 0.16 (0.16–0.17) |
| other (startup, config, saving/removing stage FS, overlap) | 2.26 (2.26–2.29) | 0.01 (0.01–0.01) | 0.00 (0.00–0.01) | −0.44 (−0.51 to −0.38) | −6.36 (−6.41 to −6.29) | −18.16 (−18.66 to −17.88) | −0.37 (−0.49 to −0.37) | −18.15 (−18.43 to −17.99) |
| **process total** | 21.85 (21.82–21.93) | 17.18 (17.10–17.19) | 11.91 (11.76–11.95) | 10.99 (10.75–11.09) | 10.27 (10.16–10.39) | 10.41 (10.22–10.62) | 12.75 (12.49–12.82) | 12.54 (12.47–12.79) |
| total minus base pull/extract and push | 20.55 (20.51–20.61) | 14.26 (14.15–14.30) | 9.34 (9.21–9.40) | 10.45 (10.23–10.55) | 9.70 (9.60–9.87) | 9.88 (9.67–10.09) | 12.22 (11.95–12.29) | 12.01 (11.94–12.26) |
| for reference: tar+gzip of all layers (overlay runs it in parallel with commands) | – | 2.80 (2.79–2.81) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) | 0.00 (0.00–0.01) | 0.00 (0.00–0.00) | 4.59 (4.40–6.12) | 3.22 (3.18–3.28) |

#### w3-cpu4: pairwise comparisons

Host wall difference, paired by round (s), median (min–max):

| comparison | Δ wall | Δ % |
|---|---|---|
| BuildKit: parallel − sequential | −1.35 (−1.70 to −1.24) | −9 (−11 to −8) % |
| overlay lazy: jobs default − jobs=1 | −0.53 (−0.99 to −0.45) | −5 (−9 to −4) % |
| overlay lazy: jobs=2 − jobs=1 | −0.76 (−0.87 to −0.45) | −7 (−8 to −4) % |
| overlay lazy: jobs default − jobs=2 | 0.09 (−0.16 to 0.31) | 1 (−1 to 3) % |
| overlay export all layers: jobs default − jobs=1 | 0.01 (−0.15 to 0.18) | 0 (−1 to 1) % |
| lazy − export all layers, jobs=1 | −1.69 (−1.88 to −1.39) | −13 (−14 to −11) % |
| lazy − export all layers, jobs default | −2.28 (−2.64 to −2.02) | −18 (−20 to −16) % |
| Go walk: no intermediate-stage snapshots − snapshot every stage | −5.27 (−5.43 to −5.22) | −30 (−31 to −30) % |
| overlay jobs=1 lazy − BuildKit sequential | −4.00 (−4.15 to −3.87) | −26 (−27 to −25) % |
| overlay jobs default lazy − BuildKit parallel | −3.24 (−3.51 to −2.90) | −23 (−25 to −21) % |
| overlay jobs default lazy − optimized kaniko | −11.35 (−11.54 to −11.18) | −51 (−52 to −51) % |
| Go walk lazy − optimized kaniko | −10.02 (−10.08 to −9.96) | −45 (−46 to −45) % |

#### w3-cpu4: the three `pnpm install` commands (stages a, b, c) and export

- Total and longest command: `RUN` run time (BuildKit: step time from the log,
  including the snapshot commit).
- a–c span: from the start of the first `RUN` to the end of the last (overlay).
- snapshot a–c: the diff (+ export) of stages a, b and c; this is what lazy
  export or skipping snapshots saves.

| configuration | 3 commands total | longest command | a–c span | snapshot+export a–c | layers tar+gzipped | tar+gzip total | gz MB |
|---|---|---|---|---|---|---|---|
| optimized kaniko (`kaniko-bench:stream`) | 7.62 (7.58–7.67) | 2.87 (2.85–2.89) | – | 9.38 (9.38–9.40) | – | – | – |
| BuildKit parallel | 28.20 (27.80–28.20) | 9.80 (9.60–9.80) | – | – | – | – | – |
| BuildKit max-parallelism=1 | 11.10 (11.10–11.20) | 4.15 (4.10–4.20) | – | – | – | – | – |
| optimized Go walk | 9.10 (8.97–9.14) | 3.32 (3.29–3.36) | 15.45 (15.33–15.49) | 4.93 (4.92–4.94) | 10 (10–10) | 2.80 (2.79–2.81) | 459 (459–459) |
| Go walk, no intermediate-stage snapshots | 9.20 (9.08–9.26) | 3.36 (3.27–3.41) | 10.33 (10.21–10.39) | 0.00 (0.00–0.00) | 4 (4–4) | 0.00 (0.00–0.00) | 0 (0–0) |
| overlay jobs=1, lazy | 9.97 (9.82–10.07) | 3.71 (3.65–3.74) | 10.44 (10.23–10.54) | 0.66 (0.65–0.67) | 4 (4–4) | 0.00 (0.00–0.00) | 0 (0–0) |
| overlay jobs=2, lazy | 15.15 (14.96–15.35) | 5.88 (5.81–5.92) | 9.70 (9.60–9.86) | 0.66 (0.66–0.67) | 4 (4–4) | 0.00 (0.00–0.01) | 0 (0–0) |
| overlay jobs default, lazy | 27.13 (26.62–27.77) | 9.49 (9.29–9.72) | 9.87 (9.66–10.08) | 0.68 (0.65–0.68) | 4 (4–4) | 0.00 (0.00–0.00) | 0 (0–0) |
| overlay jobs=1, export all layers | 10.60 (10.46–10.93) | 4.04 (3.77–4.31) | 11.11 (10.97–11.33) | 0.64 (0.63–0.64) | 10 (10–10) | 4.59 (4.40–6.12) | 459 (459–459) |
| overlay jobs default, export all layers | 26.95 (26.75–27.40) | 9.49 (9.39–9.63) | 9.90 (9.79–10.01) | 0.79 (0.75–0.80) | 10 (10–10) | 3.22 (3.18–3.28) | 460 (459–460) |
