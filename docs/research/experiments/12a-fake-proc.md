# Experiment 12a: Fake /proc

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Under bare `chroot` (no `/proc`, no `/sys`), do jest, terser and other tools that size their worker pools by CPU count fail, silently pick the wrong worker count, or slow down, and how much does a static snapshot of a few `/proc` and `/sys` files fix? |
| **Answer** | All three happen: `next build` always fails in 0.25 s, `java` cannot start, and under `--cpus=4` jest runs 20 workers instead of 3 and webpack's terser is 51% slower. A 16-file static snapshot (V2s) restores the CPU and memory counts, the worker counts and `next build` (10.88 s against 10.95 s natively); it cannot fix `/dev/fd`, `/proc/self/exe` (and therefore Java) or `ps`. |
| **Related** | [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md); [kaniko mode](../../design/kaniko-mode.md); experiments [12](12-kaniko-mode-parallel.md), [14](14-real-builds-without-proc.md), [15](15-java-next-concurrency.md), [16](16-cgroup-v1-cpuset.md) |
| **Original** | `docs-rust/poc/12-kaniko-mode-parallel/proc/` (Vietnamese, not in this repository) |

> **Flag names.** The Q12 decision changed after this experiment. Kaniko mode
> is now **sequential on `/` by default**, and parallel stages are enabled only
> with `--parallel-stages`
> ([ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)). When this
> experiment ran, parallel stages were briefly the default and the original
> text named the opt-out flag `--no-parallel-stages`. That flag no longer
> exists: "use `--no-parallel-stages`" now means "do not pass
> `--parallel-stages`", which keeps one rootfs on `/` with a real `/proc`. The
> recommendation to enable the fake `/proc` by default applies to builds run
> with `--parallel-stages`.

## Summary

- [Experiment 12](12-kaniko-mode-parallel.md) found that a `RUN` under bare
  `chroot`, the mechanism behind `--parallel-stages` in kaniko mode, has no
  `/proc` and no `/sys`. This experiment measured what that breaks, and
  whether copying a static snapshot of selected `/proc` and `/sys` files into
  each stage's rootfs before every `RUN` repairs it.
- Without `/proc`, failures come in three kinds. `next build` and `java` fail
  hard. Under a CPU limit, libuv, Go and Rust silently ignore the cgroup quota
  (jest 20 workers instead of 3, webpack 51% slower). Old jest and
  terser-webpack-plugin releases see 0 CPUs even without a limit and fall
  back to serial work (9.5× and 3.5× slower). **[measured]**
- A static snapshot of 16 files (V2s) makes every CPU and memory count match
  the native run, restores the worker counts and timings of jest and webpack,
  and lets `next build` succeed in native time. Faking only
  `/proc/self/stat` makes Next run but 54% slower. **[measured]**
- No snapshot can provide per-process entries: `/dev/fd` and process
  substitution, `/proc/self/exe` (so any binary that resolves `$ORIGIN`,
  including the JDK's `java`), `ps` and `/proc/<pid>`. Values that change over
  time are frozen. **[measured]**
- Recommendation: on the `--parallel-stages` `chroot` path, write a 13-file
  fake `/proc` and `/sys` before each `RUN` and remove it afterwards, on by
  default; document what is still missing. The bash prototype cost 11–27 ms
  per `RUN`.

## Questions

1. Under bare `chroot`, do jest, terser and the other tools that choose their
   worker count from the CPU count **fail**, **pick the wrong number of
   workers without saying so**, or **slow down**?
2. Can ranikust "sync" `/proc` from the container's `/` into each rootfs?
   `/proc` is a virtual filesystem whose content depends on the reader
   (`/proc/self` points at the reading process, `/proc/<pid>` is live). What
   can be copied is only a **static snapshot of a few files**, placed in
   `rootfs/proc` and `rootfs/sys` before each `RUN` and removed afterwards.
   What does such a snapshot fix, and what does it not?

## Environment

| | |
|---|---|
| Host | Same machine as [experiment 12](12-kaniko-mode-parallel.md): Linux 7.0.0-34, 32 CPUs, 29 GB RAM, podman 5.7 rootless, cgroup v2, no sudo |
| Date and load | 2026-10-29, 19:42–20:20. Shared machine: host load 0.1–0.4 at the start, 3–9 during the measurements (each run records the load in `out/*/<cfg>/_env.txt`) |
| Privilege model | E-dock as in experiment 12: rootless podman, `--cap-drop=ALL` plus Docker's 14 default capabilities plus the moby seccomp profile, `--network=host`. The `pdock` wrapper reuses the capability flags and seccomp profile of experiment 12's compatibility part (read-only) |
| Harness image | `node:22-bookworm-slim` (Node 22.23.4, libuv 1.51.0), the same release as the `node.tar` rootfs from the [root re-run](06a-root-rerun.md) used for the `chroot` variants |
| `/dev` in the rootfs | Rootless podman cannot `mknod` (experiment 12), so `null`, `zero`, `random` and `urandom` are **regular files**. Also `shm/` (a plain directory, mode 1777) and the symlinks Docker creates: `fd -> /proc/self/fd`, `stdin|stdout|stderr -> /proc/self/fd/{0,1,2}` |
| Python | The only local image with Python, `searxng/searxng` (Void Linux, glibc, Python 3.14.7). V0 ran that image directly under E-dock; the `chroot` variants used a rootfs exported from it |
| Go and Rust | Self-built static probes: Go 1.27.3 from `golang:latest` (pulled during the experiment), rustc 1.99.1 from `rust:1-alpine` (musl) |
| npm | `nk-verdaccio` **proxies npmjs** (uplink `npmjs`, `maxage: 365d`; uncached packages such as `left-pad` and `is-number` still downloaded), so the network blocked nothing |
| Packages | jest 29.7.0, webpack 5.112.2, terser-webpack-plugin 5.6.1, terser 5.51.2, lodash 4.18.1, moment 2.31.0, three 0.170.0, next 15.5.28 + react 19.3.1 (the same releases as `nextjs-kaniko/app/pnpm-lock.yaml`). Two older releases: jest 29.3.1 (forcing `jest-config@29.3.1` through `overrides`) and terser-webpack-plugin 5.3.10 |

Configurations:

| Name | Container limits | Inside the container |
|---|---|---|
| `c32` | None (32 CPUs, 29 GB) | `cpu.max` = `max 100000`, `memory.max` = `max` |
| `c4` | `--cpus=4` | `cpu.max` = `400000 100000` |
| `c4m3` | `--cpus=4 --memory=3g`; counts and spot checks only | `cpu.max` = `400000 100000`, `memory.max` = `3221225472` |

## Method

Four variants ran inside the same container for each configuration:

| Variant | How it runs | `/proc`, `/sys` |
|---|---|---|
| **V0** control | The command runs directly in the container | Real |
| **V1** bare `chroot` (Q12 path) | `chrun` (`chroot` + `exec`) into a rootfs extracted from `node.tar` | None (empty directories) |
| **V2** fake `/proc`, system-wide part | As V1. Before each command `fake_up min` copies a static snapshot; afterwards `fake_down` deletes it and restores the mtimes | 9 system-wide files: `/proc/{stat,cpuinfo,meminfo,loadavg,uptime,version}`, `/sys/devices/system/cpu/{online,possible,present}` |
| **V2s** fake `/proc` + `self` | As V2, plus static `/proc/self/{cgroup,mountinfo,stat}` and `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max,memory.high}` | 16 files |

- **Repetition:** jest, terser, webpack and next ran 3 rounds with the
  variant order rotated each round; the tables give the median. Counts and
  spot checks ran once (they are deterministic).
- **Wall time** was measured inside the container and covers only the
  command. The cost of `fake_up` + `fake_down` was recorded separately
  (`fake_ms`).
- **strace:** `strace -f -e trace=%file,sched_getaffinity,sysinfo` for each
  tool under V0 and V1, keeping only lines that touch `/proc`, `/sys` or
  `/dev/{fd,std*,shm}`. For Python under V0, Debian's strace ran through
  `ld.so` from a bundled copy, because the Void image has no package manager.

Fixtures:

- **jest:** 20 test files. Each counts primes below 7·10⁶ (about 1.1 s of CPU)
  and logs its `pid` and `JEST_WORKER_ID`. Each run uses a fresh
  `--cacheDirectory`, so jest has no previous timings with which to choose
  `runInBand` by itself.
- **terser CLI:** `three/build/three.cjs` (1.3 MB), `-c -m`; the output
  sha256 is compared.
- **webpack:** 12 entries, each bundling all of `lodash` + `moment` (CJS, not
  tree-shakeable). `TerserPlugin({ parallel: true })` with a `minify` function
  that wraps `terserMinify` and logs `pid:threadId` on every call; the number
  of distinct pairs is the number of workers actually used. The output is
  compared by the sha256 of every `dist/*.js`.
- **next:** a minimal app-router app with 40 SSG pages
  (`generateStaticParams`); each page counts primes below 3·10⁶ at build
  time. `NODE_OPTIONS=--require pidlog.js` records every Node process
  spawned.
- **misc:** `fixtures/probes/misc.sh`, one probe per line (`/dev/fd`,
  `/proc/self/*`, `/proc/sys`, `df`, ...).

## Results

All values in this section are **[measured]** unless labelled otherwise.
Bold marks a difference from V0. `c32` = no limit, `c4` = `--cpus=4`,
`c4m3` = `--cpus=4 --memory=3g`.

### What the tools read (strace)

Traces: `out/strace/<cfg>/*.trace`; summaries:
`out/strace/summary-{c32,c4m3}.txt`.

| Path | Read by (V0) | Purpose | When missing (V1) |
|---|---|---|---|
| `/proc/stat` | libuv `os.cpus()`; glibc `get_nprocs` (fallback step) | Count CPUs | Node: `os.cpus()` = `[]`. glibc falls back further to `sched_getaffinity` and still gets 32 |
| `/proc/cpuinfo` | libuv `os.cpus()` | CPU model | — |
| `/sys/devices/system/cpu/cpu<N>/cpufreq/scaling_cur_freq` (32 files) | libuv `os.cpus()` | Speed | `speed` = 0 (not faked) |
| `/sys/devices/system/cpu/{online,possible}` | glibc `sysconf(_SC_NPROCESSORS_*)`: `getconf`, `nproc`, Python `os.cpu_count()` | Count CPUs | Falls back to `/proc/stat`, then `sched_getaffinity`: correct 32 |
| `/proc/meminfo` | libuv `os.totalmem`/`freemem`; every Node process at startup (jest, webpack and next read it dozens of times) | Memory | Falls back to `sysinfo()`: `totalmem` correct, **`freemem` = `freeram`** (page cache not counted) |
| `/proc/loadavg`, `/proc/uptime` | libuv `os.loadavg`/`uptime` | | Falls back to `sysinfo()`: correct |
| `/proc/version`, `/proc/version_signature` | next; libuv **[inferred: kernel version check]** | | Falls back to `uname`: correct |
| `/proc/self/cgroup` | libuv `availableParallelism()`, `constrainedMemory()`; Go `GOMAXPROCS`; Rust `available_parallelism` | Find the cgroup | **cgroup limits ignored, no error** |
| `/proc/self/mountinfo` | Go (finds the cgroup2 mount), next | | As above |
| `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max,memory.high}` | libuv, Go, Rust | CPU quota, memory limit | As above |
| `/proc/self/stat` | libuv `uv_resident_set_memory` (`process.memoryUsage()`); Python around `os.fork` **[inferred: counts threads to warn about forking a multi-threaded process]** | RSS, thread count | **Node throws `ENOENT`**; Python ignores it |
| `/proc/self/maps` | glibc `pthread_getattr_np` for the main thread (Node, Python) | Stack bounds | Tolerates `ENOENT` |
| `/proc/self/exe` | `process.execPath`, Go `os.Executable`, Rust `current_exe` | Binary path | Node uses `argv[0]` (`"node"`); Go and Rust return an error |
| `/proc/self/fd` | Python (closing fds when spawning); bash (`/dev/fd`) | | Python falls back to another method; bash process substitution breaks |
| `/proc/sys/vm/overcommit_memory` | glibc malloc, Node, Python | | Ignored |
| `/dev/shm/sem.*` | Python `multiprocessing` (`sem_open`) | Semaphores | Needs a `/dev/shm` directory (a plain directory is enough) |

None of the tools read `/proc/<pid>` of another process.

### CPU and memory counts

Data: `out/matrix/<cfg>/raw/counts-*.log`, `python-*.log`.

| Value | cfg | V0 | V1 | V2 | V2s |
|---|---|---|---|---|---|
| node `os.cpus().length` | all | 32 | **0** | 32 | 32 |
| node `os.availableParallelism()` | c32 | 32 | 32 | 32 | 32 |
| | c4, c4m3 | 4 | **32** | **32** | 4 |
| Go 1.27 `GOMAXPROCS` | c32 / c4 | 32 / 4 | 32 / **32** | 32 / **32** | 32 / 4 |
| Rust `available_parallelism()` | c32 / c4 | 32 / 4 | 32 / **32** | 32 / **32** | 32 / 4 |
| `nproc`, `nproc --all`, `getconf _NPROCESSORS_{ONLN,CONF}`, Go `NumCPU` | all | 32 | 32 | 32 | 32 |
| Python `os.cpu_count()`, `process_cpu_count()`, `mp.cpu_count()` | all | 32 | 32 | 32 | 32 |
| Python `Pool().map`, 64 tasks | c32 | 32 workers, 0.15 s | 32, 0.16 s | 32, 0.16 s | 32, 0.16 s |
| | c4 | 32 workers, 0.62 s | 32, 0.58 s | 32, 0.53 s | 32, 0.60 s |
| node `os.freemem()` (MiB) | c32 | 19,746 | **502** | 19,728 | 19,725 |
| node `process.constrainedMemory()` (MiB) | c32 | 2⁴⁴ (unlimited) | **0** | **0** | 2⁴⁴ |
| | c4m3 | 3072 | **0** | **0** | 3072 |
| V8 `heap_size_limit` (MiB) | c32 | 4144 | 4144 | 4144 | 4144 |
| | c4m3 | 1584 | **4144** | **4144** | 1584 |
| `process.memoryUsage()` | all | Real RSS (43–44 MiB) | **throws `ENOENT`** | **throws `ENOENT`** | **1 MiB** (that of `cat` at snapshot time) |
| `process.execPath` | | `/usr/local/bin/node` | **`node`** | **`node`** | **`node`** |
| Go `os.Executable`, Rust `current_exe` | | Correct | **Error** | **Error** | **Error** |
| `os.loadavg()`, `os.uptime()`, `os.totalmem()` | | Correct | Correct (fallback to `sysinfo`) | Value at snapshot time | Value at snapshot time |

- **`freemem` under V1** comes from `sysinfo().freeram`, which excludes the
  page cache, so it shows about 0.5–3.4 GB instead of about 19.5 GB
  (`MemAvailable`). Next uses `freemem` when `memoryBasedWorkersCount` is
  enabled (off by default) **[code]**.
- **V0 is also "wrong" in Docker's way:** `os.cpus().length`, `nproc` and
  Python all report 32 under `--cpus=4`. The goal of the fake `/proc` is to
  **match V0**, not to be more correct than V0.

### jest

Data: `out/matrix/<cfg>/raw/jest*-*.log`. Each cell is `maxWorkers` (from
`--showConfig`) / number of processes that actually ran tests (distinct pids
in the log) / median wall time of 3 rounds. With 20 test files, at most 20
processes are used.

| cfg | Release | V0 | V1 | V2 | V2s |
|---|---|---|---|---|---|
| c32 | 29.7.0 | 31 / 20 processes / 2.42 s | 31 / 20 / 2.53 s | 31 / 20 / 2.49 s | 31 / 20 / 2.40 s |
| c4 | 29.7.0 | 3 / 3 / 8.58 s | **31 / 20 / 9.46 s** | **31 / 20 / 9.60 s** | 3 / 3 / 8.60 s |
| c32 | 29.3.1 | 31 / 20 / 2.43 s | **1 / 1 / 23.07 s** (`runInBand`) | 31 / 20 / 2.41 s | 31 / 20 / 2.43 s |
| c4 | 29.3.1 | 31 / 20 / 9.28 s | **1 / 1 / 23.11 s** (`runInBand`) | 31 / 20 / 9.49 s | 31 / 20 / 9.50 s |

All 20 tests passed in every cell that printed a `Tests:` line. For 29.3.1
that line was cut off by `tail`, but every run printed
`Ran all test suites.`.

- **jest ≥ 29.4.0** (released 2023-01-24) takes the CPU count from
  `availableParallelism()` **[code]** (`jest-config/build/getMaxWorkers.js`).
  **jest ≤ 29.3.1** uses `os.cpus().length`, which is 0, so
  `maxWorkers = max(0 − 1, 1) = 1`, and `shouldRunInBand` runs everything in
  one process. jest 30 uses `availableParallelism()` **[code]**.
- jest 29.3.1 uses `os.cpus()` and so ignores the cgroup. Under `--cpus=4`
  even V0 runs 31 workers. V2 and V2s reproduce that behaviour (+2%), while V1
  is 2.5× slower.
- Release 29.3.1 also prints `Jest did not exit NaN seconds…` in **every**
  variant, including V0. That comes from forcing `jest-config@29.3.1` to run
  with the 29.7 packages and is unrelated to `/proc`.

### terser and webpack

Data: `out/matrix/<cfg>/raw/{terser,webpack,wpold}-*.log`.

| Command | cfg | V0 | V1 | V2 | V2s |
|---|---|---|---|---|---|
| terser CLI 5.51.2, `three.cjs` 1.3 MB, `-c -m` | c32 | 1.10 s | 1.11 s | 1.08 s | 1.10 s |
| | c4 | 1.09 s | 1.09 s | 1.09 s | 1.09 s |
| webpack 5.112 + terser-webpack-plugin **5.6.1**, 12 assets | c32 | 12 threads / 2.67 s | 12 / 2.69 s | 12 / 2.70 s | 12 / 2.69 s |
| | c4 | 3 threads / 5.45 s | **12 / 8.24 s** | **12 / 8.11 s** | 3 / 5.37 s |
| webpack 5.112 + terser-webpack-plugin **5.3.10** | c32 | 12 threads / 2.62 s | **1 (main thread) / 9.23 s** | 12 / 2.58 s | 12 / 2.61 s |
| | c4 | 12 threads / 8.09 s | **1 (main thread) / 9.02 s** | 12 / 8.06 s | 12 / 8.15 s |

- **Outputs are identical** in every variant and configuration: terser CLI
  `sha256 c7356062ae00…`, webpack + 5.6.1 `85d2aaecec75…`, webpack + 5.3.10
  `bb482fb941fa…`. The worker count affects only speed, not content.
- terser-webpack-plugin 5.3.10 does not read the cgroup. Under `--cpus=4`,
  V0 also opens 12 threads on 4 CPUs (8.09 s, slower than release 5.6.1 with
  3 threads at 5.45 s), so the serial V1 is only 11% slower.
- The thread count is the number of distinct `pid:threadId` pairs logged by
  the wrapping `minify` function. The plugin uses worker threads, so every
  worker has the same `pid`.
- The formula **[code]** (`terser-webpack-plugin/dist/index.js`) is
  `min(number of assets, cpus − 1)`:
  - from 5.3.14, `cpus` = `availableParallelism()`;
  - up to 5.3.13, `cpus` = `os.cpus().length` = 0, so
    `availableNumberOfCores = −1`, no worker is created and minification runs
    serially on the main thread;
  - webpack 5.97.1 depends on `terser-webpack-plugin ^5.3.10`. Release 5.3.14
    came out on 2025-03-06, so lockfiles created before that date hold a
    release ≤ 5.3.13 **[code: registry metadata; how common this is, is
    inferred]**;
  - webpack 5.112.2 switched its default minimizer to
    `minimizer-webpack-plugin` 5.13.1, which uses `availableParallelism()`
    **[code]**;
  - css-minimizer-webpack-plugin 8.0.0 uses `availableParallelism()`;
    thread-loader 4.0.4 uses `os.cpus().length − 1` **[code]**.

### next build

Next 15.5.28, 40 SSG pages. Data: `out/matrix/<cfg>/raw/next-*.log`,
`out/extra/<cfg>/nextw-*`.

| cfg | V0 | V1 | V2 | V2s |
|---|---|---|---|---|
| c32 | OK, 40 pages, 10.95 s | **FAIL 0.25 s** `ENOENT … uv_resident_set_memory` | **FAIL 0.25 s** (as V1) | OK, 40 pages, 10.88 s |
| c4 | OK, 10.85 s | **FAIL 0.26 s** | **FAIL 0.26 s** | OK, 10.49 s |

Next with **only** a fake `/proc/self/stat` (`V1stat`: the `memoryUsage()`
error is gone, but `os.cpus()` is still `[]`). Data:
`out/extra/{c32,c4}/nextw-*`, 2 rounds, columns are means:

| cfg | V0 | V2s | V1stat |
|---|---|---|---|
| c32 | 67 worker processes / 10.82 s | 67 / 10.75 s | **7 / 16.70 s** (+54%) |
| c4 | 67 / 10.62 s | 67 / 10.81 s | **7 / 16.70 s** (+57%) |

"Worker processes" is the number of
`next/dist/compiled/jest-worker/processChild.js` processes in the pid log.
With `cpus = 31`, Next opens 67; with `cpus = 1`, only 7. **[inferred]**
These are two pools sized by `cpus` (page-data collection and static page
generation) plus a few fixed workers. Patching only the `memoryUsage()` error
therefore makes Next **run, but 54% slower, without any message**. `/proc/stat`
is needed as well.

- Next always calls `traceMemoryUsage('Starting build')`, which calls
  `process.memoryUsage()` **[code]** (`next/dist/lib/memory/trace.js`,
  `build/index.js:856`). **Every** `next build` (15.x) therefore dies under
  bare `chroot`, even with `/proc/stat` present. A single static
  `/proc/self/stat` is enough to get past this step (`V1stat` above).
- Next's worker count is `experimental.cpus` =
  `max(1, os.cpus().length − 1)` **[code]** (`server/config-shared.js:160`).
  Releases 14.2.30, 15.0.0, 15.5.28, 16.0.0 and 16.4.3 (the latest) all use
  `os.cpus()` **[code, grep of the tarballs]**. Next does **not** read the
  cgroup: V0 under `--cpus=4` still computes 31 workers.

### JVM

OpenJDK 21.0.8, rootfs exported from `keycloak:26.4.1-0`. Data:
`out/extra/<cfg>/jvm-*.log`, `out/extra/jvm-debug-c32.txt`.

| Variant | `java -version` | Ergonomics (c32 and c4m3 identical) |
|---|---|---|
| V0 (the keycloak image run directly) | OK | 32 CPUs, `ParallelGCThreads` 23, `MaxHeapSize` 7.99 GB |
| V1, V2, V2s | **rc=127** `error while loading shared libraries: libjli.so: cannot open shared object file` | — |
| V1 or V2s + `LD_ORIGIN_PATH=<jdk>/bin` | OK | — |
| V1 or V2s + `LD_LIBRARY_PATH=<jdk>/lib` | OK | Same as V0 |

- **The JVM does not start under bare `chroot`, and a fake `/proc` does not
  save it.**
  - The launcher `bin/java` needs `libjli.so` and has
    `RPATH [$ORIGIN:$ORIGIN/../lib]` (measured with `readelf -d`).
    **[inferred]** glibc's `ld.so` computes `$ORIGIN` for the main program
    with `readlink("/proc/self/exe")`.
  - Evidence: adding `LD_ORIGIN_PATH` alone makes `java` run, so resolving
    `$ORIGIN` is the main cause.
  - `/proc/self/exe` is per process; a static snapshot cannot provide it.
  - **[inferred]** Every binary with an `$ORIGIN` RPATH/RUNPATH breaks the
    same way: the JDK (`java`, `javac`, and therefore maven and gradle),
    conda's python, "relocatable" builds.
  - `LD_ORIGIN_PATH` cannot be set globally, because its value depends on
    each binary.
- **Ergonomics** once the libraries load: V1 and V2s give the same numbers as
  V0. But even under V0, JVM 21.0.8 **does not detect the container limits**
  on kernel 7.0 (`--cpus=4 --memory=3g` still gives 32 CPUs and a 7.99 GB
  heap). The log says
  `controller cpuset/memory is not enabled … One or more required controllers disabled at kernel level`,
  because this kernel's `/proc/cgroups` no longer has `cpuset` and `memory`
  lines. This is a JVM and kernel matter, unrelated to the fake `/proc`. The
  JVM also reads `/proc/cgroups` (a system-wide file). **[inferred]** To make
  the JVM under V2s match V0 on other kernels, this file should be added.

### /dev/fd, /proc/self and /proc/sys

Data: `fixtures/probes/misc.sh`; `out/matrix/<cfg>/raw/misc-*.log`,
`out/extra/<cfg>/writes-*.log`.

| Check | V0 | V1 | V2 | V2s |
|---|---|---|---|---|
| `diff <(echo a) <(echo a)` | `SAME` | **rc=2** `/dev/fd/63: No such file or directory` | **As V1** | **As V1** |
| `echo hi \| cat /dev/stdin` | `hi` | **`ENOENT`** | **`ENOENT`** | **`ENOENT`** |
| `echo x > /dev/stderr` | OK | **`ENOENT`** | **`ENOENT`** | **`ENOENT`** |
| `cat <<<here` (here-string) | OK | OK | OK | OK |
| `readlink /proc/self/exe` | `/usr/bin/readlink` | **rc=1** | **rc=1** | **rc=1** |
| `grep Pid /proc/self/status` | `Pid: 464` | **`ENOENT`** | **`ENOENT`** | **`ENOENT`** |
| `cat /proc/self/cgroup` | `0::/` | **`ENOENT`** | **`ENOENT`** | `0::/` (static) |
| `ls -d /proc/[0-9]*` (what `ps` relies on) | 5 processes | **0** | **0** | **0** |
| `cat /proc/{loadavg,uptime,meminfo}` | Live | **`ENOENT`** | Snapshot value | Snapshot value |
| `cat /proc/sys/kernel/pid_max` | 4194304 | **`ENOENT`** | **`ENOENT`** | **`ENOENT`** |
| `echo 2 > /proc/sys/vm/overcommit_memory` | `Read-only file system` | **`ENOENT`** | **`ENOENT`** | **`ENOENT`** |
| `df -P /` | `overlay … 53% /` | Warning `cannot read table of mounted file systems`, still prints numbers | As V1 | As V0 (static `mountinfo`) |
| `head -c 8 /dev/urandom \| wc -c` | 8 | **0** | **0** | **0** |

Writes to `/proc` and `/sys` (`out/extra/{c32,c4m3}/writes-*.log`; both
configurations gave the same result):

| Command in `RUN` | V0 | V1 | V2 |
|---|---|---|---|
| `echo hacked > /proc/meminfo` | `Permission denied` | **rc=0**: creates a regular file `rootfs/proc/meminfo` | **rc=0**: overwrites the fake file; a later `head /proc/meminfo` in the same `RUN` reads `hacked` |
| `echo 1 > /proc/sys/net/ipv4/ip_forward` | `Read-only file system` | `ENOENT` | `ENOENT` |
| `mkdir -p /proc/sys/net/ipv4 && echo 1 > …` | `Read-only file system` | **rc=0** | **rc=0** |
| `echo 0 > /sys/devices/system/cpu/online` | `Read-only file system` | `ENOENT` | **rc=0** (writes the fake file) |
| Left in `rootfs/proc`, `rootfs/sys` after the `RUN` | — | **`proc/meminfo`, `proc/sys/net/ipv4/ip_forward`** | Nothing (`fake_down` removes everything) |

- `/dev/fd` and `/dev/stdin|stdout|stderr` are symlinks to
  `/proc/self/fd/N`, a directory **private to each process**, which a static
  snapshot cannot provide. Bash uses `/dev/fd` for `<(…)` because it is built
  with `HAVE_DEV_FD` **[inferred]**. Here-strings and heredocs use a temporary
  file or a pipe and are not affected.
- `/dev/urandom` returning 0 bytes is caused by **rootless podman**
  (`mknod` is impossible, so `/dev/urandom` is an empty regular file; see
  [experiment 12](12-kaniko-mode-parallel.md#compatibility-matrix)), not by
  `/proc`. Rootful Docker can `mknod`
  ([experiment 12](12-kaniko-mode-parallel.md#capability-probe)).

### Minimal file set

Chosen from the strace results, then checked with the matrix above. Rows 1–16
are exactly the measured V2s set; `/proc/cgroups` was not measured.

| # | File | What it fixes | Required? |
|---|---|---|---|
| 1 | `/proc/stat` | libuv `os.cpus()` = 0 → Next with 1 worker, jest ≤ 29.3 `runInBand`, terser-webpack-plugin ≤ 5.3.13 serial, thread-loader | **Yes** |
| 2 | `/proc/cpuinfo` | `os.cpus()[i].model` | Yes (goes with 1) |
| 3 | `/proc/meminfo` | `os.freemem()` (V1 is 6–40× lower, depending on when it is measured), `MemTotal` | **Yes** |
| 4 | `/proc/self/cgroup` | CPU quota and memory limit for libuv (`availableParallelism`, `constrainedMemory`, V8 heap), Go `GOMAXPROCS`, Rust `available_parallelism` | **Yes**, to honour `--cpus` and `--memory` |
| 5 | `/proc/self/mountinfo` | Go finds the cgroup2 mount; `df` | Yes (goes with 4) |
| 6–9 | `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max,memory.high}` | As 4 | Yes (go with 4) |
| 10 | `/proc/self/stat` | `process.memoryUsage()` stops throwing → `next build` runs | **Yes** (Next) |
| 11–13 | `/sys/devices/system/cpu/{online,possible,present}` | glibc `sysconf(_SC_NPROCESSORS_*)` reads the same as V0. Without them glibc falls back to `sched_getaffinity` and is still correct | No, but cheap |
| — | `/proc/cgroups` | The JVM reads it to detect cgroups (the JVM log says `Can't open /proc/cgroups`) | Not tested (see [JVM](#jvm)). Worth adding if the JVM can run |
| 14–16 | `/proc/loadavg`, `/proc/uptime`, `/proc/version` | Only for scripts that `cat` them. Node falls back to `sysinfo`/`uname` and gets **live** values, so a fake file makes the values stale | No. Can be dropped |

**Must not be faked:**

- **`/proc/self/maps`**: glibc `pthread_getattr_np` uses it to compute the
  main thread's stack bounds (Node and Python both read it). Another
  process's copy gives wrong bounds **[inferred: V8 might compute a wrong
  stack limit]**. Without the file they cope.
- **`/proc/self/{exe,fd,status,environ}`** and **`/proc/<pid>`**: per process.
  Faking them is lying; for example `exe -> node` is wrong for every other
  binary.
- **`/proc/sys`**: without it, writes fail with `ENOENT` (noisy), which is
  close to Docker's `EROFS`. Faking it would make writes succeed with no
  effect (see [Risks](#risks)).
- **`/sys/devices/system/cpu/cpu*/cpufreq/*`**: only the CPU speed
  (`os.cpus()[i].speed` = 0); 32 files that change constantly.

### What is fixed and what is not

| Problem under V1 | V2 (9 system-wide files) | V2s (16 files, adds `self` and cgroup) | Alternative |
|---|---|---|---|
| `os.cpus().length == 0` (Next, jest ≤ 29.3, terser-webpack-plugin ≤ 5.3.13, thread-loader) | **Fixed** | **Fixed** | |
| `os.freemem()` excludes page cache | **Fixed** (snapshot value) | **Fixed** | |
| CPU quota ignored: `availableParallelism`, `GOMAXPROCS`, Rust | No | **Fixed** | |
| Memory limit ignored: `constrainedMemory`, V8 heap | No | **Fixed** | |
| `process.memoryUsage()` throws → `next build` dies | No | **Fixed** (RSS wrong, fixed at 1 MiB) | |
| `/dev/fd`, `/dev/std*`, process substitution | No | No | Impossible under bare `chroot`. proot, overlay mode, or a build without `--parallel-stages` |
| `/proc/self/exe` (`execPath`, `os.Executable`, `current_exe`) | No | No | As above |
| `$ORIGIN` in RPATH → `java` does not start | No | No | As above. `LD_ORIGIN_PATH` only patches one binary at a time |
| `ps`, `/proc/<pid>`, `/proc/self/status` | No | No | As above |
| Writes to `/proc/sys` | `ENOENT` (still noisy) | `ENOENT` | Docker refuses them too (`EROFS`) |

### Cost

`fake_up` + `fake_down` as a bash script, per `RUN`: V2 11–18 ms (once
92 ms), V2s 19–27 ms. Most of it is forking `cat`. **[inferred]** Written in
Rust it would take under 1 ms.

### Risks

1. **Stale values.** The snapshot is taken when the `RUN` starts:
   - `MemAvailable`, `loadavg`, `uptime` and the CPU times in `/proc/stat`
     stay frozen for the whole `RUN`.
   - **[inferred]** Tools that compute CPU load from two reads of
     `/proc/stat` (`top`, `vmstat`, `os.cpus()[i].times`) will see 0%. Tools
     that time things with `/proc/uptime` will see 0 s elapse.
   - Taking a new snapshot for **every `RUN`** bounds the staleness to one
     `RUN`. `loadavg` and `uptime` can be dropped, because Node falls back to
     `sysinfo` by itself.
2. **Exclude from layer snapshots and restore the parent directory's
   mtime.**
   - Measured: one `fake_up` + `fake_down` cycle leaves `proc/` and `sys/`
     exactly as before (names, modes, mtimes; `out/matrix/*/_env.txt`:
     `identical after fake_down`), because the mtimes of `rootfs/proc` and
     `rootfs/sys` were recorded and restored with `touch -m -d`.
   - The **ctime** of the two directories cannot be restored. ranikust's walk
     must **always skip** `rootfs/proc` and `rootfs/sys` (and `/dev`), as
     kaniko's ignore list does, rather than relying on mtime alone. This is
     the same lesson as the `/etc` mtime bug of
     [experiment 12](12-kaniko-mode-parallel.md#bugs-found).
3. **Writes to fake files succeed but have no effect.**
   - Root inside `RUN` has `CAP_DAC_OVERRIDE`, so mode `0444` does not stop
     writes. `chattr +i` needs `CAP_LINUX_IMMUTABLE`, which is not among
     Docker's 14 capabilities.
   - The fake files cannot block writes, but `fake_down` removes everything,
     so **writes do not reach the layer**.
   - `/proc/sys` is not faked, so `echo 1 > /proc/sys/...` fails with
     `ENOENT`. But `mkdir -p /proc/sys/... && echo ...` **succeeds in every
     `chroot` variant**, V1 included, because `rootfs/proc` is a plain
     directory.
   - Measured: under V1, `echo hacked > /proc/meminfo` creates a regular file
     that **remains in the rootfs after the `RUN`**. If the walk did not
     exclude `/proc`, the file would end up in the layer. Under V2, the write
     to the fake file succeeds, later reads in the same `RUN` see garbage,
     and `fake_down` cleans up.
4. **`/proc/self` becomes a plain directory** (V2s), no longer the "magic"
   symlink to the reading process.
   - The `cgroup` and `mountinfo` contents are correct for every process of
     the `RUN`, because in kaniko mode ranikust creates no separate cgroup or
     mount namespace **[inferred]**.
   - `stat` is wrong: pid, ppid, RSS and thread count are those of `cat` at
     snapshot time. Python uses the thread count in `/proc/self/stat` to warn
     about `fork()` in a multi-threaded process **[inferred, from strings in
     `libpython3.14.so` next to `os.fork`]**, so that warning never appears.
   - Tools that decide based on RSS see 1 MiB.
   - **[inferred]** A tool that checks `-d /proc/self` to guess "procfs is
     present" and then reads `fd/` or `exe` gets `ENOENT` in a different
     place than under V1. No such case appeared in the matrix.
5. **Parallel stages.**
   - Each stage sees the **whole** container quota (for example 4 CPUs), so N
     parallel stages each run jest with 3 workers. BuildKit behaves the same
     **[inferred]**.
   - The snapshot must be written into each stage's **own** rootfs, never
     into a shared store, least of all a `cp -al` hardlink store
     ([experiment 12](12-kaniko-mode-parallel.md#rootfs-preparation)).
6. **Leftover processes.** **[inferred]** A daemon left running by a `RUN`
   can still read `/proc` after `fake_down`. The correct order is to wait for
   the whole process tree of the `RUN` to exit before removing the files.
7. **cgroup v1.** **[inferred, not tested]** On cgroup v1 Kubernetes nodes,
   libuv, Go and the JVM read `cpu.cfs_quota_us`, `cpu.cfs_period_us` and
   `memory.limit_in_bytes` under `/sys/fs/cgroup/<controller>/`. The file list
   must be derived from the real `mountinfo` and `cgroup`, not hard-coded for
   v2.

## Findings

1. Without `/proc`, `next build` (15.5.28) always fails after 0.25 s with
   `ENOENT … uv_resident_set_memory`, because Next calls
   `process.memoryUsage()` at the start of every build and that reads
   `/proc/self/stat` ([next build](#next-build)).
2. `java` (OpenJDK 21) does not start: `ld.so` cannot resolve `$ORIGIN`
   without `/proc/self/exe`, so `libjli.so` is not found; no static snapshot
   can fix this ([JVM](#jvm)).
3. Under a CPU limit, libuv, Go and Rust silently ignore the quota without
   `/proc/self/cgroup`: jest 29.7 runs 20 workers instead of 3 (9.46 against
   8.58 s, +10%), terser-webpack-plugin 5.6.1 runs 12 threads instead of 3
   (8.24 against 5.45 s, +51%), and under a 3 GB limit V8 sets a 4144 MiB
   heap limit instead of 1584 MiB **[inferred: memory-hungry builds would be
   OOM-killed instead of collecting garbage early]**
   ([CPU and memory counts](#cpu-and-memory-counts), [jest](#jest),
   [terser and webpack](#terser-and-webpack)).
4. Even without limits, `os.cpus().length == 0`. Current jest (≥ 29.4) and
   terser-webpack-plugin (≥ 5.3.14) use `os.availableParallelism()` and are
   unaffected at 32 CPUs (same workers, time and output hash); jest 29.3.1
   falls back to `runInBand` (9.5× slower, 23.07 against 2.43 s) and
   terser-webpack-plugin 5.3.10 minifies serially on the main thread (3.5×
   slower, 9.23 against 2.62 s). Next (14.2.30 to 16.4.3) and thread-loader
   4.0.4 also derive worker counts from `os.cpus()` **[code]**
   ([jest](#jest), [terser and webpack](#terser-and-webpack)).
5. Unaffected: `nproc`, `getconf _NPROCESSORS_ONLN`, Go `runtime.NumCPU` and
   Python `os.cpu_count()` (glibc and Go fall back to `sched_getaffinity` and
   still report 32), the single-threaded terser CLI (same hash), and Python
   `multiprocessing.Pool` (32 workers as in V0; Python does not read the
   cgroup, so even V0 uses 32 workers under `--cpus=4`)
   ([CPU and memory counts](#cpu-and-memory-counts)).
6. The 16-file snapshot (V2s) makes every CPU and memory count match V0 in all
   three configurations, gives jest and webpack the right worker counts in V0
   time (jest 8.60 s, webpack 5.37 s at `--cpus=4`), and lets `next build`
   run (10.88 s against 10.95 s) ([CPU and memory counts](#cpu-and-memory-counts),
   [next build](#next-build)).
7. The 9 system-wide files alone (V2) fix `os.cpus()` but not the cgroup
   quota, the V8 heap or Next; faking only `/proc/self/stat` lets Next run
   with 7 worker processes instead of 67, 54% slower, without any message.
   No smaller subset fixes both Next and the cgroup quota
   ([What is fixed](#what-is-fixed-and-what-is-not), [next build](#next-build)).
8. No snapshot fixes `/dev/fd` and `/dev/std*` (process substitution,
   `tee /dev/stderr`), `/proc/self/exe` (`process.execPath` becomes `"node"`;
   Go `os.Executable` and Rust `current_exe` fail; `java` cannot start, nor
   therefore maven or gradle **[inferred]**), `ps` and `/proc/<pid>`; values
   that change over time (`freemem`, `loadavg`, `uptime`) freeze at snapshot
   time, and the RSS from `process.memoryUsage()` is always 1 MiB, the RSS of
   `cat` at snapshot time ([/dev/fd, /proc/self and /proc/sys](#devfd-procself-and-procsys)).
9. A `fake_up` + `fake_down` cycle leaves `rootfs/proc` and `rootfs/sys`
   identical apart from ctime and keeps writes out of the layer; without the
   fake files, a write such as `echo hacked > /proc/meminfo` stays in the
   rootfs ([Risks](#risks)).
10. The bash prototype costs 11–27 ms per `RUN` (one outlier of 92 ms)
    ([Cost](#cost)).

## Implications for ranikust

The recommendations below were written for the bare-`chroot` path of Q12. Under
[ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md) that path is
used only when the user passes `--parallel-stages`; without the flag, kaniko
mode runs sequentially on `/` with a real `/proc`.

1. **Yes: the parallel `chroot` path should create a fake `/proc`, on by
   default** when `--parallel-stages` is given, with a switch to turn it off.
   - Reason: with the V2s set, the most expensive measured case, `next build`,
     goes from "always fails" to "runs correctly with all 67 workers".
     Patching only the error leaves Next 54% slower.
   - Under a CPU limit, webpack gets 35% faster (8.24 → 5.37 s) and jest 9%
     faster; with jest ≤ 29.3 the difference is 9.5×.
   - Cost: 11–27 ms per `RUN` as a script (once 92 ms).
2. **File set (13):** the 10 required files of the
   [minimal set](#minimal-file-set) (`/proc/{stat,cpuinfo,meminfo}`,
   `/proc/self/{cgroup,mountinfo,stat}`,
   `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max,memory.high}`), plus
   `/sys/devices/system/cpu/{online,possible,present}` (cheap, and match V0).
   **Drop** `loadavg`, `uptime` and `version`: they only make values stale,
   since Node gets correct live values without them. No smaller subset fixes
   both Next and the cgroup quota.
   - On cgroup v1: copy the corresponding files of the `cpu` and `memory`
     controllers, located through `mountinfo` **[inferred]**. The final file
     sets, including cgroup v1, were settled in
     [experiment 16](16-cgroup-v1-cpuset.md).
3. **How** **[inferred]**:
   - Right before the `exec` of each `RUN`, ranikust reads these files from
     its own `/proc`. ranikust is in the same container, cgroup and mount
     namespace as the `RUN`. Read each file to the end, because procfs
     reports `st_size = 0`.
   - Write them into the stage's `rootfs/proc` and `rootfs/sys`, mode `0444`.
   - After **the whole process tree** of the `RUN` has exited, delete
     **everything** under `rootfs/proc` and `rootfs/sys` (not only what
     ranikust created, because a `RUN` may `mkdir -p /proc/...`), then
     restore the mtime and atime of the two directories with `utimensat`.
   - The walk always skips `/proc`, `/sys` and `/dev`.
4. **Do not fake** `/proc/self/{maps,exe,fd,status}`, `/proc/<pid>`,
   `/proc/sys` or `cpufreq` ([Minimal file set](#minimal-file-set)).
5. **Document what is still missing**: `/dev/fd`, `/dev/std*`, `<(…)`,
   `/proc/self/exe` (including **the JDK and every binary that uses
   `$ORIGIN`**), `ps`, live values. Dockerfiles that need them must be built
   without `--parallel-stages` (one rootfs on `/` with a real `/proc`); the
   original text said "with `--no-parallel-stages`".
   - **[inferred]** `RUN` commands could be scanned for `<(`, `/dev/std`,
     `/dev/fd`, `/proc/self/exe`, ` ps `, `java`/`mvn`/`gradle` to warn early,
     but detection can never be reliable (scripts call scripts).
   - **[inferred]** A more reliable approach: when a `RUN` exits non-zero on
     the parallel `chroot` path, print a hint to rebuild without
     `--parallel-stages`, but do not retry automatically (a retry is unsafe
     because the `RUN` may have written partial results).
6. **Do not use environment variables instead of a fake `/proc`** (for
   example `GOMAXPROCS`, `NODE_OPTIONS=--max-old-space-size`) **[inferred]**.
   They leak into the `RUN` environment, change the behaviour of child tools,
   and cover only a few runtimes. (The later, narrower exception for Java
   memory limits is
   [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md).)

## Limitations and open questions

- **Real rootful Docker and real Kubernetes pods** were not tested, in
  particular cgroup v1 (risk 7), LXCFS (where the container's `/proc/meminfo`
  and `/proc/cpuinfo` are already replaced) and `--cpuset-cpus`. With a
  cpuset, glibc under V1 falls back to `sched_getaffinity` and may report a
  **more** accurate number than V0 **[inferred]**. Follow-up:
  [experiment 16](16-cgroup-v1-cpuset.md).
- **A Rust implementation** of `fake_up`/`fake_down` and its cost: only the
  bash version was measured.
- Several parallel stages at once, each with its own fake `/proc`: the matrix
  ran the variants sequentially. Follow-up:
  [experiment 15](15-java-next-concurrency.md).
- Long `RUN` steps (whether a stale `MemAvailable` really hurts).
- maven and gradle: only `java -version` was checked, and it already failed.
  JVM ergonomics under cgroups, on a kernel where the JVM detects them.
  Follow-up: [experiment 14](14-real-builds-without-proc.md).
- Other binaries that use `$ORIGIN` (conda, .NET, relocatable builds)
  **[inferred: they break like `java`]**.
- `cargo build`, `go build`, esbuild, vite, turbopack
  (`next build --turbopack`), sharp, pip build backends.
- `next build` of the real app (`nextjs-kaniko/app`, about 1.2 GB of
  `node_modules`); this experiment used a minimal 40-page SSG app. Follow-up:
  [experiment 14](14-real-builds-without-proc.md).
- Python on Debian or Alpine (musl); only Void's Python 3.14 (glibc) was
  tried.
- A real `/dev` (rootful Docker has `mknod`); here `/dev/null` and
  `/dev/urandom` were regular files.

## Reproduction

PoC sources are archived outside this repository, under
`docs-rust/poc/12-kaniko-mode-parallel/proc/`. Paths below are relative to
that directory. No real root is needed.

| File | Content |
|---|---|
| `scripts/lib.sh` | `pdock`: the E-dock wrapper, reusing experiment 12's `compat/lib.sh` (read-only) |
| `scripts/prep.sh [node old go rust py]` | Installs `node_modules` through verdaccio, builds the static Go and Rust probes, exports the Python rootfs into `stage/` (git-ignored) |
| `scripts/fakeproc.sh` | `fake_up <rootfs> min\|self` and `fake_down <rootfs>`; the file lists are `FAKE_MIN` and `FAKE_CG` |
| `scripts/inner-common.sh` | Builds the rootfs and `/dev`; `runv` runs a command as V0, V1, V2 or V2s |
| `scripts/strace.sh`, `inner-strace.sh`, `strace-summary.sh` | The strace survey; output in `out/strace/` |
| `scripts/matrix.sh <cfg> <reps> <tests…>`, `inner-matrix.sh` | The command × variant matrix; output in `out/matrix/<cfg>/{results.tsv,raw/,_env.txt}` |
| `scripts/extra.sh`, `inner-extra.sh` | JVM, writes to `/proc`, Next worker counts; output in `out/extra/<cfg>/`. The JVM debugging (`LD_ORIGIN_PATH`, `LD_LIBRARY_PATH`) is `scripts/jvm-debug.sh` → `out/extra/jvm-debug-c32.txt` |
| `out/00-prep.log`, `01-strace.log`, `02-matrix.log`, `03-old-extra.log` | Logs of each run, with host load |
| `scripts/report.py` | `out/summary.txt`: medians, worker counts, OK/FAIL |
| `fixtures/` | Node, Go, Rust and Python probes, `misc.sh`, the jest, webpack and next projects |

```sh
bash scripts/prep.sh
bash scripts/strace.sh && bash scripts/strace-summary.sh
bash scripts/matrix.sh c32 3 counts misc pyprep jest terser webpack next jestold wpold
bash scripts/matrix.sh c4  3 counts misc pyprep jest terser webpack next jestold wpold
bash scripts/matrix.sh c4m3 1 counts misc
bash scripts/extra.sh && bash scripts/jvm-debug.sh
python3 scripts/report.py
```

Containers are named `rk12p-*` and run with `--rm`. Apart from `stage/`
(about 1.3 GB: `node_modules`, the Python and JVM rootfs, the probes) no state
remains. `prep.sh` uses the image `golang:latest`; it was pulled during the
experiment (Go 1.27.3, 913 MB) and deleted afterwards, and `prep.sh` pulls it
again when needed.

## References

1. [Experiment 12: Parallel stages in kaniko mode](12-kaniko-mode-parallel.md):
   E-dock, the compatibility matrix, `/dev` under rootless podman, rootfs
   preparation, the `/etc` mtime bug.
2. [Experiment 06a: Root re-run](06a-root-rerun.md): the `node.tar` rootfs.
3. [Experiment 14: Real builds without /proc](14-real-builds-without-proc.md).
4. [Experiment 15: Java, Next.js and concurrency](15-java-next-concurrency.md).
5. [Experiment 16: cgroup v1 and cpuset](16-cgroup-v1-cpuset.md).
6. [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md): kaniko
   mode sequential by default, `--parallel-stages` opt-in.
7. [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md):
   `JAVA_TOOL_OPTIONS` on the parallel path.
8. [Survey 05: Decisions](../survey/05-decisions.md), Q12.
9. Package sources read **[code]**: `jest-config/build/getMaxWorkers.js`;
   `terser-webpack-plugin/dist/index.js`; `next/dist/lib/memory/trace.js`,
   `build/index.js:856`, `server/config-shared.js:160` (next 15.5.28); grep of
   the next 14.2.30, 15.0.0, 15.5.28, 16.0.0 and 16.4.3 tarballs; registry
   metadata for the terser-webpack-plugin 5.3.14 release date.
