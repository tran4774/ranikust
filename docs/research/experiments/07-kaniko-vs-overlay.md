# Experiment 07: kaniko vs overlay

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | On the full W1 build, on one machine, how do upstream kaniko, optimized kaniko, an optimized Go walk and overlay compare, base pull and push included? |
| **Answer** | Overlay was 5.5–6.7 s (33–39 %) faster than optimized kaniko: 9.95 s against 15.63 s at 32 CPUs, 11.18 s against 18.27 s at 4 CPUs. A well-written walk already recovers 3.1–4.2 s of that gap; overlay adds only 1.3–3.7 s on top of the walk. |
| **Related** | [ADR-0002](../../adr/0002-two-snapshot-modes.md), [ADR-0016](../../adr/0016-overlay-mount-options.md), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md), [ADR-0006](../../adr/0006-persistent-store.md), [overlay snapshot design](../../design/overlay-snapshot.md), [kaniko-mode design](../../design/kaniko-mode.md), experiments [06](06-overlay-vs-walk.md), [08](08-parallel-stages-lazy-export.md), [09](09-kaniko-mode-prototype.md) |
| **Original** | `docs-rust/poc/07-kaniko-vs-overlay/` (Vietnamese, not in this repository) |

## Summary

- [Experiment 06](06-overlay-vs-walk.md) and
  [survey 06](../survey/06-overlay-and-parallelism.md) §3.5 compared overlay
  only with a Go walk and **inferred** "overlay saves 7–10 s over kaniko" from
  measurements on another machine (kaniko optimization notes, docs/27, on
  OrbStack; external, not in this repository). This experiment measured all
  builders on one machine with the same W1 Dockerfile, registry and timing
  method, base pull and push included.
- Overlay was **5.5–6.7 s (33–39 %) faster than optimized kaniko**, at both 32
  and 4 CPUs. The earlier 7–10 s estimate was too high because docs/27 ran on a
  slower VM (kaniko W1 33–37 s there, 15.6–18.3 s here); the **ratio**, about
  a third of the build, matches **[measured]**.
- **Most of that gap does not need overlay.** A well-written walk (parallel,
  stat cache, no per-path resolution, tar streamed into pgzip) recovered
  3.1–4.2 s; overlay added only 1.3–3.7 s on top, consistent with
  experiment 06 (1.3–1.9 s over five `RUN`s) **[measured]**.
- Optimized kaniko was 17.5–19.9 s (2.0–2.3×) faster than upstream kaniko,
  which ran almost the same at 32 and 4 CPUs (35.6 s against 35.7 s): it is
  close to sequential **[measured]**.
- Overlay made commands slower and its export time varied widely, because the
  export sometimes re-read the layer from disk **[measured]**; the eviction
  mechanism behind it is **[inferred]**.
- The images of all four builders matched, apart from the files that differ
  between any two kaniko runs. The check found two correctness bugs that
  become design requirements: raw-mode `chmod`, and parent directory entries
  in walk-mode layers **[measured]**.

## Questions

1. What is the host wall time of each builder on W1 at 32 CPUs and at 4 CPUs?
2. Where does the time go, phase by phase, and which phases explain the gaps?
3. Do the four builders produce the same image?
4. Why does overlay's export time vary so much between runs?
5. Is exporting each layer as soon as it is done (async) better than
   exporting everything at the end of the build?

## Environment

- **Date:** 2026-10-29, 08:29–09:20 (main sets) and 09:05–09:19 (export
  experiments).
- **Machine:** AMD Ryzen 9 9950X3D (16 cores, 32 threads), 30 GB RAM, NVMe,
  ext4 `/home`, kernel 7.0.0-34, podman 5.7.0 rootless, cgroup v2.
- **Limits:** `--memory=8g` for every build; 32 CPUs (no limit) or
  `--cpus=4`.
- **Network and services:** network `nextjs-kaniko_internal`, registry
  `nk-registry`, npm through `nk-verdaccio` with a warm cache. The registry is
  on loopback.
- **Privilege:** `ovbuild` ran in rootless `podman run --privileged` (root in a
  user namespace, overlay with `userxattr`), image `golang` (`5d8d6618be1c`,
  Go 1.26.9). kaniko ran as in the benchmark harness, without `--privileged`.
- **Load:** before each run the harness waited for an idle machine (CPU busy
  < 10 % over 3 s, three times in a row, as in experiment 06). No run in the
  main sets had to wait. A container from
  [experiment 09](09-kaniko-mode-prototype.md) (`rk9-nodebench`) was running
  at the start of the session and used about 0.4 cores.
- **Page cache:** warm (verdaccio, base image) in every configuration.

## Method

### Builders

| Configuration | What it is |
|---|---|
| `k-up` | `kaniko-bench:upstream-34ba9ea` (image id `c55f95028d6e`): the `kaniko-bench:stream` image with `/kaniko/executor` replaced by a binary built from `34ba9eadf` (kaniko 1.28.6, the commit kaniko optimization notes, docs/24 use as "upstream"), `make out/executor` in `golang:1.26.9`. No flags, no metrics. The image was created for this experiment; no existing image was removed |
| `k-stream` | `localhost/kaniko-bench:stream` (`0804d72cdde7`, branch `bench-metrics` at `44aa7ddc0`) with `cfg-final-opt.env` + `FF_KANIKO_STREAM_TAR_COMPRESS=1`: exactly the `k-stream` configuration of `run-fair.sh`, 41 flags in total. Writes `metrics.json` |
| `walk` | `ovbuild -mode walk -export all` (Go). Simulates a well-built ranikust kaniko mode: one plain rootfs per stage, base extracted sequentially, a baseline scan, and after each `RUN`: `syncfs` + walk + parallel `lstat`/HighwayHash (GOMAXPROCS goroutines) with a stat cache and a 2 s racy window. `COPY` scans only the destination tree. tar + pgzip right after each command (the next command still modifies the rootfs, so export cannot be deferred). `FROM deps AS final` reuses the `deps` rootfs, like kaniko's "squash". Stages run sequentially |
| `ov` | `ovbuild -mode overlay -export final -export-when async`: one overlay mount per `RUN`/`COPY` with `userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile` on the workdir (podman volume, ext4 `/home`). Diff = walk of the upperdir. Base layers are pulled and extracted **in parallel**, one directory per layer. Only the final image's layers are tar + gzipped, each as soon as it is done, in parallel with the next command. Stages run in parallel (`assets` alongside `deps`) |
| `ov-end` | Like `ov`, but all exports wait until every stage is done (as BuildKit does) |

The three `ovbuild` configurations are one binary (`main.go`) that reads the
**actual W1 Dockerfile** (subset: `ARG`, `FROM … AS`, `ENV`, `WORKDIR`,
`LABEL`, `COPY [--from]`, `RUN`). In both modes:

- `RUN` runs under `chroot` (Go `SysProcAttr.Chroot`) with `/proc`, `/dev`,
  `/etc/hosts` and `/etc/resolv.conf` bind-mounted; the environment comes from
  the base config plus `ENV`;
- `COPY` uses `cp -a`, owner 0:0;
- tar + pgzip at `BestSpeed` (the level kaniko uses), threads = GOMAXPROCS
  (32 or 4), 1 MiB blocks; diffID and digest are computed on separate
  goroutines; blobs are written to the workdir;
- push: base blobs are cross-mounted from the `node` repository (like
  `FF_KANIKO_CROSS_REPO_MOUNT`, `kaniko@44aa7ddc0 pkg/config/featureflags.go:145`
  **[code]**), new blobs are uploaded with POST + PUT, 4 blobs in parallel,
  then the OCI config and manifest.

The walk/hash core and the tar writer come from experiment 06, which was not
modified.

### Timing

- As in `run-fair.sh`: T0 just before `podman run -d`, T1 when `podman wait`
  returns. Removing the container (kaniko) or the container plus `rm -rf` of
  the workdir (`ovbuild`) is timed separately as teardown and excluded.
- Each build pushes to a new repository
  `registry:5000/kvo-<set>-<configuration>-<round>`, so the target repository
  never holds a blob in advance. This replaces `run-fair.sh`'s registry
  restart, because other work could be using the registry at the same time.
- Every cell is the **median (min–max) of 5 rounds** after one discarded
  warm-up round. Configurations are interleaved and their order is rotated
  every round. Differences are paired by round.
- The breakdown tables use each process's own clock: kaniko's `metrics.json`
  (`meta.wall_ns`, `kaniko@44aa7ddc0 pkg/metrics/report.go:126` **[code]**)
  and `ovbuild.json`. Host wall time is about 0.2–0.4 s larger (container
  start and exit).

### Image check

`listing.sh` pulls an image and lists every path (type, mode, uid:gid, size,
link target) with the sha256 of its content; `cmp-listing.py` compares two
listings. It was run on the round-1 images of the 32-CPU set.

### Export follow-up

Three extra experiments at 32 CPUs on `ov-end` investigated the export
variance (`-export-syncfs` to `syncfs` before exporting; recording
`read_bytes` from `/proc/self/io` during export; `--memory=24g`). Sets:
`w1-exportvar-cpu32`, `w1-exportio-cpu32`, `w1-exportio-mem24g-cpu32`,
`w1-exportio-mem8g-cpu32`.

## Results

All values are **[measured]** unless labelled otherwise.

### Host wall time, W1 (s)

| W1, host wall (s) | 32 CPUs | `--cpus=4` |
|---|---|---|
| upstream kaniko 1.28.6 (`34ba9eadf`, no flags) | 35.55 (35.22–36.59) | 35.70 (35.62–36.58) |
| **optimized kaniko** (`kaniko-bench:stream`, 41 flags + `STREAM_TAR_COMPRESS`) | **15.63** (15.44–15.98) | **18.27** (17.92–18.42) |
| optimized Go walk (`ovbuild -mode walk`) | 11.29 (10.96–11.78) | 14.83 (14.40–15.70) |
| **overlay** (`ovbuild -mode overlay`, async export) | **9.95** (8.13–11.52) | **11.18** (9.96–12.94) |
| overlay, export at end of build | 9.50 (8.63–10.29) | 12.22 (10.96–14.08) |

Differences paired by round, median (min–max), s:

| | 32 CPUs | `--cpus=4` |
|---|---|---|
| overlay async − optimized kaniko | **−5.5** (−7.9 to −4.0), −36 % | **−6.7** (−8.5 to −5.3), −38 % |
| overlay end of build − optimized kaniko | −6.1 (−7.4 to −5.4), −39 % | −6.1 (−7.5 to −3.8), −33 % |
| Go walk − optimized kaniko | −4.2 (−5.0 to −3.9), −27 % | −3.1 (−3.9 to −2.5), −17 % |
| overlay async − Go walk | −1.3 (−2.8 to +0.1) | −3.7 (−5.7 to −1.5) |
| overlay end of build − Go walk | −2.0 (−2.5 to −1.3) | −2.5 (−4.5 to −0.8) |
| optimized kaniko − upstream kaniko | −19.9 (−21.0 to −19.5) | −17.5 (−18.7 to −17.2) |

### Image check

Round-1 images of the 32-CPU set (`results/image-check.txt`):

| Against `k-stream` | Paths | Metadata differs | Content differs |
|---|---|---|---|
| `k-up` (kaniko against kaniko) | 262,240 / 262,240, 1 log name differs | 17, all in `/tmp/node-compile-cache` | 1,654 |
| `walk` | same as above | 15, same as above | 1,654, same group |
| `ov` | same as above | 20, same as above | 1,654, same group |

The 1,654 files with different content are the same group in all three pairs:
`/root/.local/share` (pnpm store, 1,092), `/tmp/node-compile-cache` (553),
`/root/.npm/_cacache` (2), `.modules.yaml`, `.pnpm-workspace-state-v1.json`
and `/etc/hostname`. These files are not deterministic between runs. The
listing does not compare mtimes or hardlink structure (overlay breaks
hardlinks on copy-up; see [experiment 06](06-overlay-vs-walk.md)).

**Two bugs found by the check, both with design consequences:**

1. **Extracting the base lost the sticky, setuid and setgid bits** when the Go
   code called `os.Chmod(p, os.FileMode(h.Mode&0o7777))`: `os.FileMode` does
   not use bit `0o1000` for sticky, so `/tmp` went from 1777 to 777. Fixed with
   `unix.Chmod` and the raw bits. Experiment 06's code has the same bug, which
   does not affect its measurements. The Rust implementation must use
   `fchmodat` with the raw mode and test `/tmp` and a setuid binary.
2. **Walk-mode layers must contain a header for every parent directory.**
   Without them, `podman` (containers-storage, overlay driver) extracts each
   layer into its own directory and creates missing parents with mode 0755.
   That directory then **masks** the real mode from the lower layer: `/root`
   700 became 755 and `/tmp` 1777 became 755. The case that triggered it: the
   1,813 hardlinked pnpm store files whose mtime changes in `churn-edit`,
   while `/root/...` was not in the layer. kaniko adds parent directories, and
   an overlay upperdir contains them by construction. ranikust's kaniko mode
   must keep this behaviour and test "a layer that only changes a deep file
   below a directory with an unusual mode".

### Time breakdown, 32 CPUs (s, median)

Each process's own clock: kaniko from `metrics.json` (`meta.wall_ns`),
`ovbuild` from `ovbuild.json`.

| Phase | optimized kaniko | Go walk | overlay async | overlay end of build |
|---|---|---|---|---|
| base pull + extract | 0.88 | 1.45 ¹ | 0.37 | 0.37 |
| baseline scan (`snapshot.init`) | 0.36 | 0.06 | 0 | 0 |
| running `RUN`/`COPY` | 4.03 | 3.51 | 5.31 | 4.70 |
| diff ² | 6.29 | 3.06 | 0.76 | 0.75 |
| tar + gzip on the critical path ³ | 2.58 | 1.94 | 2.80 (0.77–3.95) | 2.06 (2.00–3.71) |
| push | 0.72 | 0.74 | 0.75 | 0.71 |
| other | 0.29 | 0.01 | −0.18 ⁴ | −0.18 ⁴ |
| **process total** | **15.23** | **11.08** | **9.60** | **9.03** |
| total minus base and push | 13.62 | 8.89 | 8.47 | 7.94 |

¹ The walk extracts the base twice (stages `assets` and `deps`) and does not
reuse it the way kaniko's `REUSE_BASE_ROOTFS` does
(`kaniko@44aa7ddc0 pkg/config/featureflags.go:167` **[code]**). This costs the
walk about 0.5 s.
² kaniko: `snapshot.command` − `snapshot.tar` = `syncfs` 0.48 + `scan_full`
6.52 (including walk 3.18 and resolve 2.48) …; walk: `syncfs` + walk/hash;
overlay: mount, umount and the upperdir walk. Metric definitions:
`kaniko@44aa7ddc0 pkg/metrics/stats.go:67`–`77` **[code]**.
³ kaniko: `snapshot.tar` (tar, with gzip running concurrently thanks to
streaming) plus the remaining compression; walk: tar + gzip after each
command; overlay: the export left after the last stage finishes.
⁴ Negative because overlay runs the `assets` stage in parallel with `deps` and
extracts the base in parallel.

At 4 CPUs (see the appendix), in the same column order: running commands
5.17 / 5.62 / 6.59 / 6.72; diff 6.90 / 4.14 / 0.76 / 0.76; tar + gzip on the
critical path 3.34 / 2.31 / 2.57 / 3.45; process total 17.85 / 14.60 /
10.81 / 11.86.

### Where the gaps come from

**Optimized kaniko → Go walk (−4.2 s at 32 CPUs)**, from the breakdown:

- diff 6.29 → 3.06 s. kaniko spends 2.48 s in `snapshot.resolve` (resolving
  symlinks and checking the ignore list for every path) and 3.18 s in a
  sequential walk. The Go walk does not resolve, and walks faster;
- tar 2.58 → 1.94 s;
- baseline scan 0.36 → 0.06 s.

This is the part **ranikust's kaniko mode** can recover without mount
privileges, provided the ignore-list semantics can be kept at low cost
**[inferred]**; the Go walk here only skips 5 fixed paths.

**Go walk → overlay (−1.3 to −3.7 s):**

- diff 3.06 → 0.76 s (−2.3 s), and tar leaves the critical path when export
  runs in parallel;
- but commands run slower: +1.2 to +1.8 s at 32 CPUs, +1.0 s at 4 CPUs. Per
  command (32 CPUs, median): `pnpm install` took 3.28 s (overlay async) and
  2.79 s (overlay end of build) against 2.01 s (walk); the 4 commands of the
  `final` stage took 1.03–1.11 s against 0.56 s (copy-up, whiteouts). With
  async export, the export of the `pnpm` layer runs at the same time as the
  `final` commands and competes for CPU and disk;
- overlay's export time varies widely (next section).

`pnpm install` itself also differs between kaniko and the walk: 2.31 s
(kaniko) and 2.01 s (walk) at 32 CPUs, but 3.57 s and 4.17 s at 4 CPUs.
**Not explained** (the same kind of open question as kaniko optimization
notes, docs/27 §3 row 6).

### Overlay export variance: re-reading from disk

The `pnpm install` layer (tar 1.37 GB, gz 333 MB) took 1.75–1.79 s to tar +
gzip in walk mode in every run. In overlay mode the same layer took sometimes
1.84–2.00 s and sometimes 3.5–6.6 s. Three follow-up experiments (32 CPUs,
`ov-end`; raw numbers in the appendix):

| Hypothesis | Experiment | Result |
|---|---|---|
| Dirty data (`volatile` does not fsync) is written back during the export | `-export-syncfs`: `syncfs` before exporting | **Rejected.** Dirty data was down to 6–23 MB and export still took 4.75–5.91 s in 5 of 6 runs |
| The export has to re-read from disk | record `read_bytes` from `/proc/self/io` during export | **Confirmed.** Slow runs read 124–466 MB from disk; fast runs read 0–18 MB. The walk always read 0 MB |
| The container's 8 GB memory limit | `--memory=24g` | **Rejected.** Still slow in 2 of 5 runs. Cgroup memory during export was only 1.7–2.4 GB (peak 4.7–5.1 GB). Slow runs had about 300–450 MB less cgroup memory, i.e. cache pages had already been evicted |

**[inferred]** Pages of the `pnpm` files are reclaimed machine-wide (shared
machine, about 5 GB free). In walk mode, freshly written files are **read
again immediately** for hashing (they are inside the 2 s racy window, so the
stat cache misses). That second read promotes the pages to the active list,
so they are not evicted. In overlay mode the export is the first read after
the write, so the pages are still on the inactive list and get reclaimed
first. Not verified: it needs root to inspect `/proc/kpageflags` or to apply
controlled memory pressure.

Consequence: overlay's lazy or async export is not always "CPU only". On a
runner short of RAM it may have to re-read the layer from disk. Exporting
**as soon as the layer is done** (async) keeps pages warmer than exporting at
the end of the build. On W1 the two did not differ meaningfully: async − end
of build = +0.3 (−1.0 to +2.0) s at 32 CPUs and −1.0 (−3.2 to +1.6) s at
4 CPUs.

### Teardown

Removal after the build (not included in wall time): kaniko 1.37–1.52 s
(removing the container), `ovbuild` 1.46–1.59 s (removing the container and
`rm -rf` of the workdir holding rootfs and uppers). About equal.

## Findings

1. Overlay is 5.5–6.7 s (33–39 %) faster than optimized kaniko on W1, at both
   32 and 4 CPUs; the earlier 7–10 s estimate came from a slower VM, but the
   ratio (about a third of the build) matches **[measured]** (host wall
   tables).
2. Most of the gap needs no overlay: an optimized walk recovers 3.1–4.2 s, and
   overlay adds 1.3–3.7 s over the walk, consistent with experiment 06
   **[measured]** (paired differences).
3. Optimized kaniko is 17.5–19.9 s (2.0–2.3×) faster than upstream kaniko;
   upstream kaniko takes 35.6 s at 32 CPUs and 35.7 s at 4, so it is close to
   sequential **[measured]** (host wall table).
4. Overlay makes the commands slower (4.7–5.3 s against 3.5 s for the walk at
   32 CPUs) and its export time varies widely; both eat into the gain from an
   almost free diff **[measured]** (breakdown; export variance).
5. The four builders produce the same image content, except the files that
   already differ between two kaniko runs **[measured]** (image check).
6. In kaniko, `snapshot.resolve` (2.48 s) and the sequential walk (3.18 s) are
   the largest costs a walk mode can avoid **[measured]** (where the gaps come
   from).
7. Slow overlay exports coincide with 124–466 MB of real disk reads; syncing
   first or raising the memory limit does not remove them **[measured]**; page
   reclaim order explains it **[inferred]** (export variance).

## Implications for ranikust

1. **Overlay mode stays, with the gain stated correctly:** on W1 it is about
   **a third** faster than optimized kaniko (5.5–6.7 s on this machine) and
   only **1.3–3.7 s** faster than a good walk mode
   ([ADR-0002](../../adr/0002-two-snapshot-modes.md)). The earlier estimate
   ("against real kaniko … 7.3 s of rescans") in the
   [survey summary](../survey/00-summary.md) §2 item 3 and
   [survey 06](../survey/06-overlay-and-parallelism.md) §3.5 is superseded by
   these measurements.
2. **Kaniko (walk) mode deserves as much investment as overlay mode.** Most
   CI pods land in it ([survey 06](../survey/06-overlay-and-parallelism.md)
   §1). A simple Go walk is already 3.1–4.2 s faster than optimized kaniko; the
   biggest opportunities are kaniko's `snapshot.resolve` (2.5 s) and its
   sequential walk (3.2 s) ([kaniko-mode design](../../design/kaniko-mode.md)).
3. **Overlay slows the commands** (+1.0 to +1.8 s on W1, in `pnpm install`
   and the copy-up commands). When a workload is mostly one large command that
   writes many files, the gain nearly disappears. It grows with the number of
   `RUN`s on a large rootfs (experiment 06, Finding 6).
4. **Export each layer as soon as it is done**, pipelined with the next
   command, rather than all at the end. W1 showed no meaningful difference,
   but the mechanism above favours async. An option to cap compression threads
   is needed while export runs alongside `RUN`
   ([overlay snapshot design](../../design/overlay-snapshot.md)).
5. **Extract base layers in parallel, one directory per layer** (possible only
   in overlay mode): 0.37 s against 0.88 s for kaniko on the 5-layer node
   base. With a persistent `layers/` store kept between builds
   ([survey 06](../survey/06-overlay-and-parallelism.md) §2,
   [ADR-0006](../../adr/0006-persistent-store.md)), this drops to 0 from the
   second build.
6. Two correctness requirements from the image check: `chmod` with the raw
   mode, and kaniko-mode layers must include parent directories
   ([kaniko-mode design](../../design/kaniko-mode.md)). Remaining content
   differences between modes are covered by
   [ADR-0012](../../adr/0012-accept-divergence-between-modes.md).

## Limitations and open questions

- **Different timing boundaries.** kaniko times itself through its metrics and
  `ovbuild` through its own clock. Only host wall time is a common yardstick;
  the breakdown tables only attribute causes.
- **`ovbuild` is not a complete builder.** It has no cache, `.dockerignore`,
  `USER`, `ADD`, `--chown`, heredocs, variable substitution outside `FROM`,
  `ONBUILD`, mountinfo-based ignore list, symlink checks at the `COPY`
  destination, `pivot_root` or private namespaces for `RUN`. kaniko does all
  of this, so part of the gap (especially `snapshot.resolve`) reflects a
  difference in scope **[inferred]**.
- **Base pull:** kaniko downloads the blobs once (`base.download_shared`,
  `kaniko@44aa7ddc0 pkg/metrics/stats.go:34` **[code]**) and extracts them into
  `/` twice, with `REUSE_BASE_ROOTFS`. The Go walk extracts twice without
  reuse. Overlay extracts once, in parallel. The registry is on loopback, so
  the network part is close to zero. **Not measured on a real network.**
- **Push:** same loopback registry and same cross-repository mount of base
  blobs, but kaniko uses go-containerregistry while `ovbuild` uses a minimal
  HTTP client (4 blobs in parallel). Push took 0.71–0.75 s in every
  configuration.
- **Container start-up differs:** kaniko runs without `--privileged` from a
  small image; `ovbuild` runs `--privileged` from the `golang` image.
- **BuildKit was not measured for W1 here.** The benchmark session
  `tranpc-fair-1` on the same machine (2026-10-28, `--cpus=4`) recorded
  rootless BuildKit W1 at 50.9–52.4 s and kaniko `k-stream` at 17.9–18.8 s;
  those numbers were not re-measured here.
- Overlay was rootless overlay in a user namespace (`userxattr`). Real-root
  overlay was not re-measured; [experiment 06a](06a-root-rerun.md) showed the
  same direction for experiment 06.
- Five rounds per cell, one shared machine, one workload; no bootstrap
  confidence intervals. Overlay's min–max ranges are wide because of the
  export variance.
- Warm page cache (verdaccio, base) in every configuration; a cold cache was
  not measured.
- Each build leaves orphaned blobs in `nk-registry` (`./run.sh clean` deletes
  the `kvo-*` repositories but does not run garbage collection).
- Open: why `pnpm install` runs at different speeds under kaniko and under the
  walk (faster under the walk at 32 CPUs, slower at 4 CPUs).

## Reproduction

The PoC sources are archived outside this repository, under
`docs-rust/poc/07-kaniko-vs-overlay/`:

- `main.go`: `ovbuild`, the W1-subset builder with `-mode walk|overlay`,
  `-export all|final` and `-export-when async|end`.
- `run.sh`: the harness (also used by experiment 08): `build`,
  `matrix <set> <rounds> <app> <cpus|0> -- <configurations>`, `clean`.
- `all.sh`: builds `bin/ovbuild` and runs the `w1-cpu32` and `w1-cpu4` sets
  (5 rounds + warm-up).
- `summarize.py`: produces `results/tables.md`; `--export-diag` adds the export
  follow-up table.
- `listing.sh`, `cmp-listing.py`: the image check.
- `results/<set>/<configuration>-<round>/` (`run.json`, `build.log`, kaniko's
  `metrics.json` or `ovbuild.json`), `results/<set>/run.log`,
  `results/all.log`.

```sh
./all.sh                                     # build bin/ovbuild; sets w1-cpu32 and w1-cpu4 (5 rounds + warm-up)
./run.sh matrix <set> <rounds> app <cpus|0> -- k-stream k-up walk ov ov-end
./run.sh matrix w1-exportio-cpu32 4 app 0 -- ov-end walk          # export follow-up
MEMORY=24g ./run.sh matrix w1-exportio-mem24g-cpu32 4 app 0 -- ov-end
./run.sh matrix w1-exportvar-cpu32 5 app 0 -- ov-end ov-end-sync
./listing.sh kvo-w1-cpu32-ov-1:bench /tmp/ov.txt && python3 -I cmp-listing.py /tmp/k.txt /tmp/ov.txt
python3 -I summarize.py results/w1-cpu32 results/w1-cpu4 > results/tables.md
python3 -I summarize.py --export-diag results/w1-export* >> results/tables.md
./run.sh clean                               # remove volumes kvo-work, kvo-gomod and the kvo-* repositories
```

Requirements: the `nextjs-kaniko` compose project running (`nk-registry`,
`nk-verdaccio`, network `nextjs-kaniko_internal`); the image
`localhost/kaniko-bench:stream`; and, for `k-up`, the image
`localhost/kaniko-bench:upstream-34ba9ea` (built with `make out/executor` from
`git archive 34ba9eadf` in `golang:1.26.9`, then
`FROM localhost/kaniko-bench:stream` + `COPY executor /kaniko/executor`). No
real root is needed; no step runs under sudo.

The `w1-cpu32` and `w1-cpu4` sets were measured with a binary built before the
export diagnostics (`-export-syncfs`, `export_disk_read_mb`, `cgroup_mem_*`)
and the `walk-lazy` branch (used only in experiment 08) were added. Those
changes do not touch the W1 code path.

## References

1. [Experiment 06: Overlay vs walk](06-overlay-vs-walk.md).
2. [Experiment 06a: Root re-run](06a-root-rerun.md).
3. [Experiment 08: Parallel stages and lazy export](08-parallel-stages-lazy-export.md)
   (same harness and binary).
4. [Survey 00: Summary](../survey/00-summary.md) §2;
   [survey 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md)
   §1, §2, §3.5.
5. Kaniko optimization notes, docs/24 (upstream baseline) and docs/27 (W1 on
   OrbStack) (external, not in this repository); benchmark harness
   `nextjs-kaniko/bench/run-fair.sh` and `cfg-final-opt.env`.
6. `kaniko@44aa7ddc0 pkg/metrics/stats.go:34` (`base.download_shared`),
   `:67`–`77` (`snapshot.init`, `snapshot.command`, `snapshot.scan_full`,
   `snapshot.resolve`, `snapshot.tar`).
7. `kaniko@44aa7ddc0 pkg/metrics/report.go:126` (`wall_ns`).
8. `kaniko@44aa7ddc0 pkg/config/featureflags.go:145` (`FF_KANIKO_CROSS_REPO_MOUNT`),
   `:167` (`FF_KANIKO_REUSE_BASE_ROOTFS`), `:168`
   (`FF_KANIKO_STREAM_TAR_COMPRESS`).

## Appendix: full tables

Generated by `summarize.py`. Host wall and paired differences are medians
(min–max) of 5 rounds; the export follow-up lists every run.

### w1-cpu32

Host wall time (s): T0 just before `podman run -d`, T1 when `podman wait`
returns. Teardown (removing the container and workdir) is excluded.

| configuration | n | host wall | self-reported by the process | teardown |
|---|---|---|---|---|
| upstream kaniko 1.28.6 | 5 | **35.55 (35.22–36.59)** | – | 1.49 (1.48–1.50) |
| optimized kaniko (`kaniko-bench:stream`) | 5 | **15.63 (15.44–15.98)** | 15.23 (15.03–15.55) | 1.37 (1.36–1.39) |
| optimized Go walk | 5 | **11.29 (10.96–11.78)** | 11.08 (10.75–11.57) | 1.50 (1.48–2.00) |
| overlay (async export) | 5 | **9.95 (8.13–11.52)** | 9.60 (7.74–11.11) | 1.57 (1.56–1.59) |
| overlay (export at end of build) | 5 | **9.50 (8.63–10.29)** | 9.03 (8.26–9.91) | 1.57 (1.56–1.62) |

Host wall difference against `k-stream`, paired by round (s), median (min–max):

| configuration | Δ wall | Δ / kaniko wall |
|---|---|---|
| upstream kaniko 1.28.6 | 19.92 (19.54–21.04) | 127 (123–135) % |
| optimized Go walk | −4.15 (−5.02 to −3.90) | −27 (−31 to −25) % |
| overlay (async export) | −5.48 (−7.85 to −4.04) | −36 (−49 to −26) % |
| overlay (export at end of build) | −6.06 (−7.35 to −5.39) | −39 (−46 to −34) % |

Other pairs, paired by round (s), median (min–max). "excl. base/push" = process
total minus base pull/extract and push:

| comparison | Δ host wall | Δ excl. base/push |
|---|---|---|
| overlay (async export) − optimized Go walk | −1.33 (−2.83 to 0.06) | −0.42 (−1.88 to 0.91) |
| overlay (export at end of build) − optimized Go walk | −1.96 (−2.53 to −1.27) | −1.15 (−1.61 to −0.17) |
| overlay (async export) − overlay (export at end of build) | 0.30 (−1.04 to 2.02) | 0.12 (−0.88 to 2.06) |
| optimized kaniko (`kaniko-bench:stream`) − upstream kaniko 1.28.6 | −19.92 (−21.04 to −19.54) | – |

Process time breakdown (s), median (min–max):

| part | optimized kaniko (`kaniko-bench:stream`) | optimized Go walk | overlay (async export) | overlay (export at end of build) |
|---|---|---|---|---|
| base pull + extract | 0.88 (0.87–0.89) | 1.45 (1.42–1.46) | 0.37 (0.36–0.37) | 0.37 (0.36–0.38) |
| baseline scan (`snapshot.init`) | 0.36 (0.35–0.36) | 0.06 (0.06–0.07) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) |
| running RUN/COPY commands | 4.03 (3.95–4.67) | 3.51 (3.41–4.44) | 5.31 (4.29–5.46) | 4.70 (4.32–5.30) |
| diff (kaniko: `snapshot.command` − `snapshot.tar` = syncfs, walk/hash, resolve; walk: syncfs + walk/hash; overlay: mount/umount + upperdir walk) | 6.29 (5.93–6.40) | 3.06 (2.90–3.37) | 0.76 (0.76–0.79) | 0.75 (0.74–0.75) |
| tar+gzip on the critical path (kaniko: `snapshot.tar` + remaining compression; walk: tar+gzip after each command; overlay: export left after the last stage) | 2.58 (2.57–2.71) | 1.94 (1.94–1.97) | 2.80 (0.77–3.95) | 2.06 (2.00–3.71) |
| push | 0.72 (0.71–0.73) | 0.74 (0.69–0.78) | 0.75 (0.71–0.75) | 0.71 (0.69–0.73) |
| other (startup, config, saving/removing stage FS, overlap) | 0.29 (0.29–0.30) | 0.01 (0.01–0.01) | −0.18 (−0.19 to −0.18) | −0.18 (−0.18 to −0.18) |
| **process total** | 15.23 (15.03–15.55) | 11.08 (10.75–11.57) | 9.60 (7.74–11.11) | 9.03 (8.26–9.91) |
| total minus base pull/extract and push | 13.62 (13.43–13.96) | 8.89 (8.55–9.38) | 8.47 (6.67–10.00) | 7.94 (7.18–8.79) |
| for reference: tar+gzip of all layers (overlay runs it in parallel with commands) | – | 1.94 (1.94–1.97) | 4.08 (2.05–5.25) | 2.06 (2.00–3.71) |

### w1-cpu4

Host wall time (s): T0 just before `podman run -d`, T1 when `podman wait`
returns. Teardown (removing the container and workdir) is excluded.

| configuration | n | host wall | self-reported by the process | teardown |
|---|---|---|---|---|
| upstream kaniko 1.28.6 | 5 | **35.70 (35.62–36.58)** | – | 1.52 (1.51–1.60) |
| optimized kaniko (`kaniko-bench:stream`) | 5 | **18.27 (17.92–18.42)** | 17.85 (17.50–17.99) | 1.38 (1.38–1.41) |
| optimized Go walk | 5 | **14.83 (14.40–15.70)** | 14.60 (14.17–15.47) | 1.46 (1.45–1.48) |
| overlay (async export) | 5 | **11.18 (9.96–12.94)** | 10.81 (9.50–12.44) | 1.58 (1.56–1.62) |
| overlay (export at end of build) | 5 | **12.22 (10.96–14.08)** | 11.86 (10.53–13.70) | 1.59 (1.56–1.61) |

Host wall difference against `k-stream`, paired by round (s), median (min–max):

| configuration | Δ wall | Δ / kaniko wall |
|---|---|---|
| upstream kaniko 1.28.6 | 17.47 (17.23–18.66) | 96 (94–104) % |
| optimized Go walk | −3.09 (−3.87 to −2.45) | −17 (−21 to −14) % |
| overlay (async export) | −6.73 (−8.46 to −5.33) | −38 (−46 to −29) % |
| overlay (export at end of build) | −6.09 (−7.46 to −3.84) | −33 (−41 to −21) % |

Other pairs, paired by round (s), median (min–max). "excl. base/push" = process
total minus base pull/extract and push:

| comparison | Δ host wall | Δ excl. base/push |
|---|---|---|
| overlay (async export) − optimized Go walk | −3.65 (−5.69 to −1.46) | −2.72 (−4.92 to −0.64) |
| overlay (export at end of build) − optimized Go walk | −2.45 (−4.50 to −0.75) | −1.53 (−3.62 to 0.11) |
| overlay (async export) − overlay (export at end of build) | −1.00 (−3.24 to 1.62) | −0.95 (−3.39 to 1.48) |
| optimized kaniko (`kaniko-bench:stream`) − upstream kaniko 1.28.6 | −17.47 (−18.66 to −17.23) | – |

Process time breakdown (s), median (min–max):

| part | optimized kaniko (`kaniko-bench:stream`) | optimized Go walk | overlay (async export) | overlay (export at end of build) |
|---|---|---|---|---|
| base pull + extract | 0.88 (0.86–0.89) | 1.44 (1.38–1.45) | 0.36 (0.34–0.37) | 0.36 (0.36–0.38) |
| baseline scan (`snapshot.init`) | 0.37 (0.36–0.38) | 0.06 (0.06–0.07) | 0.00 (0.00–0.00) | 0.00 (0.00–0.00) |
| running RUN/COPY commands | 5.17 (4.99–5.64) | 5.62 (5.53–6.64) | 6.59 (6.55–6.75) | 6.72 (6.47–6.77) |
| diff (kaniko: `snapshot.command` − `snapshot.tar` = syncfs, walk/hash, resolve; walk: syncfs + walk/hash; overlay: mount/umount + upperdir walk) | 6.90 (6.70–7.22) | 4.14 (4.08–4.44) | 0.76 (0.76–0.78) | 0.76 (0.75–0.77) |
| tar+gzip on the critical path (kaniko: `snapshot.tar` + remaining compression; walk: tar+gzip after each command; overlay: export left after the last stage) | 3.34 (3.33–3.38) | 2.31 (2.25–2.33) | 2.57 (1.25–3.97) | 3.45 (2.35–5.23) |
| push | 0.72 (0.72–0.73) | 0.76 (0.71–0.79) | 0.72 (0.70–0.78) | 0.75 (0.70–0.76) |
| other (startup, config, saving/removing stage FS, overlap) | 0.29 (0.29–0.30) | 0.01 (0.01–0.01) | −0.18 (−0.19 to −0.18) | −0.18 (−0.18 to −0.18) |
| **process total** | 17.85 (17.50–17.99) | 14.60 (14.17–15.47) | 10.81 (9.50–12.44) | 11.86 (10.53–13.70) |
| total minus base pull/extract and push | 16.28 (15.90–16.37) | 12.46 (11.94–13.31) | 9.73 (8.39–11.30) | 10.74 (9.41–12.57) |
| for reference: tar+gzip of all layers (overlay runs it in parallel with commands) | – | 2.31 (2.25–2.33) | 3.82 (2.46–5.27) | 3.45 (2.35–5.23) |

### Follow-up: export time of the `pnpm install` layer in overlay mode

| set | run | memory limit | tar+gzip of the pnpm layer (s) | actual disk reads during export (MB) | machine-wide Dirty+Writeback at start (MB) | cgroup memory during export (MB) | syncfs before export |
|---|---|---|---|---|---|---|---|
| w1-exportvar-cpu32 | ov-end-1 | 8 GB | 3.90 | – | 268 | – | no |
| w1-exportvar-cpu32 | ov-end-2 | 8 GB | 1.98 | – | 771 | – | no |
| w1-exportvar-cpu32 | ov-end-3 | 8 GB | 3.87 | – | 502 | – | no |
| w1-exportvar-cpu32 | ov-end-4 | 8 GB | 3.85 | – | 221 | – | no |
| w1-exportvar-cpu32 | ov-end-5 | 8 GB | 4.02 | – | 785 | – | no |
| w1-exportvar-cpu32 | ov-end-sync-1 | 8 GB | 5.14 | – | 10 | – | yes |
| w1-exportvar-cpu32 | ov-end-sync-2 | 8 GB | 4.85 | – | 13 | – | yes |
| w1-exportvar-cpu32 | ov-end-sync-3 | 8 GB | 1.84 | – | 17 | – | yes |
| w1-exportvar-cpu32 | ov-end-sync-4 | 8 GB | 5.15 | – | 18 | – | yes |
| w1-exportvar-cpu32 | ov-end-sync-5 | 8 GB | 4.75 | – | 23 | – | yes |
| w1-exportvar-cpu32 | ov-end-sync-warmup | 8 GB | 5.91 | – | 6 | – | yes |
| w1-exportvar-cpu32 | ov-end-warmup | 8 GB | 6.56 | – | 591 | – | no |
| w1-exportio-cpu32 | ov-end-1 | 8 GB | 1.85 | 0 | 428 | – | no |
| w1-exportio-cpu32 | ov-end-2 | 8 GB | 1.84 | 0 | 291 | – | no |
| w1-exportio-cpu32 | ov-end-3 | 8 GB | 3.50 | 204 | 663 | – | no |
| w1-exportio-cpu32 | ov-end-4 | 8 GB | 4.11 | 341 | 466 | – | no |
| w1-exportio-cpu32 | ov-end-warmup | 8 GB | 1.86 | 0 | 320 | – | no |
| w1-exportio-cpu32 | walk-1 | 8 GB | 1.75 | 0 | 0 | – | no |
| w1-exportio-cpu32 | walk-2 | 8 GB | 1.79 | 0 | 0 | – | no |
| w1-exportio-cpu32 | walk-3 | 8 GB | 1.75 | 0 | 29 | – | no |
| w1-exportio-cpu32 | walk-4 | 8 GB | 1.75 | 0 | 30 | – | no |
| w1-exportio-cpu32 | walk-warmup | 8 GB | 1.75 | 0 | 0 | – | no |
| w1-exportio-mem24g-cpu32 | ov-end-1 | 24576 MB | 2.00 | 18 | 1197 | 2223 | no |
| w1-exportio-mem24g-cpu32 | ov-end-2 | 24576 MB | 1.83 | 0 | 1298 | 2363 | no |
| w1-exportio-mem24g-cpu32 | ov-end-3 | 24576 MB | 3.97 | 271 | 1422 | 1946 | no |
| w1-exportio-mem24g-cpu32 | ov-end-4 | 24576 MB | 1.85 | 0 | 1306 | 2363 | no |
| w1-exportio-mem24g-cpu32 | ov-end-warmup | 24576 MB | 3.80 | 262 | 1420 | 1973 | no |
| w1-exportio-mem8g-cpu32 | ov-end-1 | 8192 MB | 3.78 | 297 | 252 | 1870 | no |
| w1-exportio-mem8g-cpu32 | ov-end-2 | 8192 MB | 4.63 | 432 | 598 | 1745 | no |
| w1-exportio-mem8g-cpu32 | ov-end-3 | 8192 MB | 1.96 | 8 | 240 | 2226 | no |
| w1-exportio-mem8g-cpu32 | ov-end-4 | 8192 MB | 2.79 | 124 | 249 | 2133 | no |
| w1-exportio-mem8g-cpu32 | ov-end-warmup | 8192 MB | 4.88 | 466 | 403 | 1712 | no |
