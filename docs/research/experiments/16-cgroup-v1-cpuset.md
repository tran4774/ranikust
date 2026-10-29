# Experiment 16: cgroup v1 and cpuset

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | On real rootful Docker under cgroup v1 and v2, does the fake `/proc` keep JVMs, jest, webpack and `next build` identical to V0, does W3 with parallel stages stay correct under v1, and does cpuset need extra fake files? |
| **Answer** | With a per-version set of 14 files (9 common files, a `/proc/cgroups` snapshot and 4 cgroup files located through `mountinfo`), V2s matches V0 for Node, Go and all four JDKs under v1. Under v2, JDKs that detect cgroups with `statfs` (21.0.12.1, 17.0.20.1) still see 8 CPUs and 2486 MiB. jest, webpack and `next build` match V0 under both versions; W3 under v1 gives an identical image 35% faster; cpuset needs no fake files because CPU affinity passes through `chroot`. |
| **Related** | [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md); [kaniko mode](../../design/kaniko-mode.md); experiments [12](12-kaniko-mode-parallel.md), [12a](12a-fake-proc.md), [13](13-docker-rootful-vm.md), [14](14-real-builds-without-proc.md), [15](15-java-next-concurrency.md) |
| **Original** | `docs-rust/poc/16-vm-cgroupv1-cpuset/` (Vietnamese, not in this repository) |

## Summary

- On the opt-in parallel path (`--parallel-stages`), `RUN` runs under a bare
  `chroot` without `/proc` and `/sys`, and ranikust can copy in a static fake
  `/proc` ([experiment 12a](12a-fake-proc.md),
  [experiment 14](14-real-builds-without-proc.md)).
  [Experiment 13](13-docker-rootful-vm.md) found that cgroup v1 needs a
  different file list. This experiment closed the remaining gaps on real
  rootful Docker 29.1.3 in a VM, under cgroup v2 and v1.
- **JVM:** under v1, V2s matches V0 for all four JDKs (4 CPUs, 768 MiB
  maximum heap), provided the v1 file set and a `/proc/cgroups` snapshot are
  present; this holds even for JDKs that detect cgroups with `statfs`. Under
  v2, only JDKs that detect cgroups through `/proc/cgroups` (21.0.8, 17.0.12)
  are fixed; 21.0.12.1 and 17.0.20.1 still see 8 CPUs and 2486 MiB. The
  `statfs` change has been backported to the 17 line.
- **jest, webpack+terser and `next build` of W1:** V2s equals V0 in worker
  counts, exit codes and output under both cgroup versions. Under v1, V0
  itself ignores the CPU quota because libuv builds a wrong v1 path.
- **W3 with parallel stages under v1:** identical image to `-jobs 1` and 35%
  faster (v2 in the same session: 33%).
- **cpuset:** V2s equals V0 for every count. CPU affinity passes through
  `chroot`, Docker does not virtualise `/proc/stat` or `/proc/cpuinfo`, and
  no ergonomics path reads cpuset files, so none are needed in the fake set.
- **Final fake `/proc` set:** 14 files per cgroup version: 9 common files,
  `/proc/cgroups`, and 4 minimal cgroup files located at run time from
  `/proc/self/{cgroup,mountinfo}`. Each smaller set breaks something.

## Questions

1. **JVM** under cgroup v1 and v2 with `--cpus=4 --memory=3g`:
   - measure `availableProcessors`, `MaxHeapSize` and `-Xlog:os+container`;
   - on recent JDKs (detecting cgroups through `statfs`) and older ones
     (through `/proc/cgroups`);
   - with Maven and Gradle builds.
2. **jest, webpack+terser and `next build` (W1)** under v1 and v2: worker
   counts, exit codes, output equality.
3. **W3 with parallel stages** (`ovbuild -sandbox nocap`, `-jobs 1` against
   the default) under cgroup v1: is the image identical, and how long does it
   take?
4. **cpuset** (`--cpuset-cpus=0-3`, with and without `--cpus=2`):
   - do the fake `/proc/stat` and `/proc/cpuinfo` contradict the affinity
     mask?
   - must cpuset files be added to the fake set?

## Environment

- **VM:** run on 2026-10-29, 21:46–22:30 (host time). Ubuntu 22.04 under
  QEMU/KVM, kernel 5.15.0-198, 8 vCPUs, 10 GB, **rootful Docker 29.1.3**.
  The VM booted first with **cgroup v2** (the default), then with **cgroup
  v1** (`systemd.unified_cgroup_hierarchy=0`),
  and finally with v2 again for an additional test (the minimal-set test).
- **Host left untouched:**
  - QEMU ran as the unprivileged user (`/dev/kvm` through an ACL). SSH was
    forwarded only on `127.0.0.1:2216`, with a dedicated key in
    `vm/k16_key`.
  - The base image of [experiment 13](13-docker-rootful-vm.md) was used
    **only as a read-only backing file**; its sha256 still matched after the
    experiment. The qcow2 overlay and seed belong to this experiment.
  - Every image was pulled **inside the VM**. The host registry and
    verdaccio were reached through `10.0.2.2` plus socat.
- **Load:** the host was shared with other concurrent work. Host load ranged
  from 0.8 to 14.4 (`out/hostload.txt`), so **timings are comparable only
  within the VM and within one table**.
- **Security:** every `docker run` used the **default** security settings
  (seccomp, AppArmor `docker-default`, 14 capabilities).

**[measured]** (`out/vm/env-v{1,2}.txt`):

| | cgroup v2 | cgroup v1 |
|---|---|---|
| Kernel | 5.15.0-198-generic | same |
| Docker | 29.1.3 (server Go 1.24.4), overlayfs | same |
| Cgroup driver / security options | `systemd` / apparmor, seccomp, **cgroupns** | `cgroupfs` / apparmor, seccomp (cgroupns **host**) |
| `/sys/fs/cgroup` in the container | `cgroup2fs` | `tmpfs` plus one `cgroup` mount per controller |
| `/proc/self/cgroup` in the container | `0::/` | 14 lines `N:<ctl>:/docker/<id>` |
| Controller mount root (column 4 of `mountinfo`) | `/` | `/docker/<id>` (equal to the path in `/proc/self/cgroup`, so the files sit directly in `/sys/fs/cgroup/<ctl>/`) |
| `/proc/cgroups` | all 14 controllers (including `cpuset` and `memory`), **hierarchy 0** | 14 controllers, hierarchy 2–13 |
| VM | 8 vCPUs, `MemTotal` 9940 MiB, 40 GB overlay | same |

Images and versions, all pulled inside the VM (`out/01-prep.log`):

| Role | Image | Versions |
|---|---|---|
| Node (harness and rootfs) | the host's `registry:5001/node:22-bookworm-slim` | Node 22.23.3, libuv 1.51.0 |
| Recent JDKs | `eclipse-temurin:21-jdk`, `eclipse-temurin:17-jdk` | **21.0.12.1** (2026-08-18), **17.0.20.1** |
| Older JDKs | `eclipse-temurin:21.0.8_9-jdk`, `eclipse-temurin:17.0.12_7-jdk` | **21.0.8**, **17.0.12** |
| Java builds | `maven:3-eclipse-temurin-21`, `gradle:8-jdk21` | Maven 3.10.0, Gradle 8.14.6, both Temurin 21.0.12.1 |
| Go probe | `golang:alpine` | Go 1.27.3 (`GOMAXPROCS` from the cgroup), static binary `gocpu` |
| Fixtures | reused from experiments 12a and 14 | jest 29.7.0 (20 tests, each about 1.1 s of CPU); webpack 5.112.2 + terser-webpack-plugin 5.6.1 (12 assets); **W1 = `nextjs-kaniko/app`** (Next 15.5.28, React 19.3.1, `pnpm install --frozen-lockfile` through verdaccio, `node_modules` 1.4 GB); the mvn/gradle projects of experiment 14, with the Gradle `AppTest` printing an extra `JPROBE` line |

## Method

**Variants** (same file tree, same `ENV`):

| Variant | How it runs |
|---|---|
| **V0** | The command runs directly in the container, with the real `/proc` |
| **V1** | `chrun` (`chroot` + `exec`) into `/r`, a rootfs exported from **the same image**. `/proc` and `/sys` are empty; `/dev` is built with `mknod` (6/6) |
| **V2s** | V1 plus a **cgroup-version-aware** fake set (`scripts/invm/fakeproc16.sh`, parts `common+cg+pcg`, 17 files): 9 common files; a `/proc/cgroups` snapshot; 7 cgroup files for the running version. File locations are derived from `/proc/self/cgroup` and `/proc/self/mountinfo`, not hard-coded |
| V2s-nocg | V2s **without** `/proc/cgroups` |
| V2s-v2set | v1 only: 9 common files + `/proc/cgroups` + the 4 fixed v2 paths of experiment 12a. This is the **wrong** set under v1; the v2 files do not exist, so they are not copied |
| V2s-cpuset | V2s plus the cpuset files (v2: `cpuset.{cpus,mems}{,.effective}`; v1: `cpuset/cpuset.{cpus,mems,effective_cpus,effective_mems}`) |

**Limits:** `c4m3` = `--cpus=4 --memory=3g`; `cs4` = `--cpuset-cpus=0-3`;
`cs4c2` = `--cpuset-cpus=0-3 --cpus=2`.

**Harness.**

- Each run is a default `docker run` plus the limit configuration (`hrun` in
  `scripts/invm/lib.sh`). The harness image is the same image as the rootfs
  `/r` (exported with `docker export`, `/dev` built with `mknod`).
  - V0 runs directly.
  - V1 and V2* run `chrun /r`.
  - `fake_up` and `fake_down` run **in the harness**, outside the chroot.
    They copy from the harness's own `/proc`, which shares the cgroup and the
    mount namespace with the `RUN`.
- **Shared mounts.** `/work` (jest, wp), `/app` (W1), `/jc` (Maven/Gradle
  caches) and `/probes` are bind-mounted **twice**, at `/x` and at `/r/x`, so
  V0 and the chroot use exactly the same file tree.
- **strace** (`t0-disc.sh`): the VM's strace 5.16, run through its own
  `ld.so` (the `bundle` directory), `-f -e trace=%file`, at V0 and at V2s,
  with one trace per tool.
- **Rounds:**
  - counts and JVM probes: 1 round (deterministic);
  - jest, webpack, next, mvn and gradle under `c4m3`: 3 rounds with the
    variant order rotated, medians (`scripts/report.py` →
    `out/summary.txt`);
  - jest under cpuset: 2 rounds;
  - W3: 3 rounds.
- **W3:** `ovbuild -mode walk -export all -sandbox nocap` in a default
  `docker run` with `--memory=8g`, workdir a Docker volume on ext4, results
  pushed to the host `nk-registry` as `k16-w3-<cgv>-j{1,d}-r<n>`. Images
  were compared **inside the VM** (no image was pulled to the host) with
  experiment 12's `perf/listing.sh` plus `cmp-listing.py`.
- **Minimal set** (`t5-minimal.sh`): V0 and V2s re-run under `c4m3` with a
  reduced cgroup part, checking the 4 JDKs, node and Go.

## Results

### Which files each tool reads [measured]

strace, `out/vm/disc-v{1,2}/<cfg>/summary.txt`. Only cgroup- and CPU-related
files that each tool opens **at V0** under `c4m3` are listed.

| Tool | cgroup v2 | cgroup v1 |
|---|---|---|
| libuv (node 22.23.3) | `/proc/self/cgroup`, `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max,memory.high}` | `/proc/self/cgroup`; then `memory/docker/<id>/memory.{soft_limit,limit}_in_bytes` (ENOENT), falling back to `memory/memory.{soft_limit,limit}_in_bytes`. **CPU: `cpuacct:/docker/<id>/cpu.cfs_quota_us`** (wrong path, ENOENT) |
| Go 1.27.3 | `/proc/self/{cgroup,mountinfo}`, `cpu.max` | `/proc/self/{cgroup,mountinfo}`, `cpu,cpuacct/cpu.cfs_{quota,period}_us` |
| glibc `nproc` | no file (`sched_getaffinity`) | same |
| `getconf _NPROCESSORS_ONLN`, `nproc --all` | `/sys/devices/system/cpu/online` | same |
| JVM **21.0.12.1, 17.0.20.1** (start-up) | **`statfs("/sys/fs/cgroup")` = `CGROUP2_SUPER_MAGIC`**, then `cgroup.controllers`, `/proc/self/{cgroup,mountinfo}`, `cpu.max`, `memory.max` | `statfs` = `TMPFS_MAGIC`, then **`/proc/cgroups`**, `/proc/self/{cgroup,mountinfo}`, `cpu,cpuacct/cpu.cfs_{quota,period}_us`, `memory/memory.limit_in_bytes` (17.0.20.1 also reads `memory.use_hierarchy`) |
| JVM **21.0.8, 17.0.12** (start-up) | **`/proc/cgroups`**, `/proc/self/{cgroup,mountinfo}`, `cpu.max`, `memory.max` (17.0.12 also reads `memory.current`) | `/proc/cgroups`, `/proc/self/{cgroup,mountinfo}`, `memory.use_hierarchy`, `cpu.cfs_{quota,period}_us`, `memory.limit_in_bytes` (17.0.12 also reads `memory.usage_in_bytes`) |
| `java -XshowSettings:system` (Java `Metrics` API) | adds `cpu.weight`, `cpuset.{cpus,mems}{,.effective}`, `memory.low`, `memory.swap.max`, `pids.max` | adds `cpu.shares`, `cpuset/cpuset.{cpus,mems,effective_cpus,effective_mems}`, `memory.{memsw.limit,soft_limit}_in_bytes`, `memory.swappiness`, `pids/pids.max` |

Under `cs4c2`:

- node, Go and glibc open exactly the same files as under `c4m3`.
- The JVM differs only in the memory part, because `cs4c2` has no
  `--memory`:
  - under v1, every JDK also reads `memory/memory.use_hierarchy` and
    `memory/memory.stat`, to find `hierarchical_memory_limit` when
    `memory.limit_in_bytes` is unlimited;
  - 17.0.12 stops reading `memory.usage_in_bytes` (v1) and `memory.current`
    (v2).

  (Comparison of the path sets in `disc-v*/{c4m3,cs4c2}/*-V0.trace.gz`.)

Observations:

- **No ergonomics path opens cpuset files**: not HotSpot, libuv, Go or glibc.
  Only Java's `Metrics` does.
- At V2s under v2, recent JDKs see `statfs` = `EXT2_SUPER_MAGIC`. They switch
  to the `/proc/cgroups` branch, find every controller at hierarchy 0, and
  stop: the trace log says `No relevant cgroup controllers mounted.`
  (`disc-v2/c4m3/jvm-t21-V2s.xlog`).
- Under v1, the files the JDK reads at V2s **are the same as at V0**. The only
  difference is that `statfs` returns EXT2 instead of TMPFS, and both values
  lead into the v1 branch.

### JVM ergonomics under `--cpus=4 --memory=3g` [measured]

Each cell is `availableProcessors` / `MaxHeapSize` (MiB). `Probe.java`'s
`maxMemory` equals `MaxHeapSize` in every cell. Source:
`out/vm/t1-v{1,2}.tsv`, logs in `out/vm/t1-v*/c4m3/<jdk>-<V>.log`.

| JDK | cgroup | V0 | V1 | **V2s** | V2s-nocg | V2s-v2set |
|---|---|---|---|---|---|---|
| 21.0.12.1 (`21-jdk`) | v2 | 4 / 768 | 8 / 2486 | **8 / 2486** | 8 / 2486 | — |
| | v1 | 4 / 768 | 8 / 2486 | **4 / 768** | 8 / 2486 | 8 / 2486 |
| 21.0.8 | v2 | 4 / 768 | 8 / 2486 | **4 / 768** | 8 / 2486 | — |
| | v1 | 4 / 768 | 8 / 2486 | **4 / 768** | 8 / 2486 | 8 / 2486 |
| 17.0.20.1 (`17-jdk`) | v2 | 4 / 768 | 8 / 2486 | **8 / 2486** | 8 / 2486 | — |
| | v1 | 4 / 768 | 8 / 2486 | **4 / 768** | 8 / 2486 | 8 / 2486 |
| 17.0.12 | v2 | 4 / 768 | 8 / 2486 | **4 / 768** | 8 / 2486 | — |
| | v1 | 4 / 768 | 8 / 2486 | **4 / 768** | 8 / 2486 | 8 / 2486 |

`ParallelGCThreads` follows the CPU count (4 or 8). 2486 MiB is a quarter of
the VM's 9940 MiB of RAM. The first meaningful `-Xlog:os+container=debug`
line (`out/vm/t1-*/c4m3/*.log`):

| Case | Log |
|---|---|
| V0 v2, every JDK; V2s v2 with 21.0.8/17.0.12 | `Detected cgroups v2 unified hierarchy` (recent JDKs find it through `cgroup.controllers`, older ones through `/proc/cgroups`) |
| V2s v2 with 21.0.12.1/17.0.20.1 | No detection line. At trace level: `No relevant cgroup controllers mounted.` |
| V0 v1 and **V2s v1, every JDK** | `Detected cgroups hybrid or legacy hierarchy, using cgroups v1 controllers` and `is_containerized() = true` |
| V2s-v2set v1 | v1 is detected, but then `Open of file /sys/fs/cgroup/cpu,cpuacct/cpu.cfs_quota_us failed, No such file or directory` |
| V1, V2s-nocg | `Can't open /proc/cgroups, No such file or directory` |

At V2s under v2, JDK 21.0.12.1 shows a small inconsistency **[measured]**:
`-XshowSettings:system` prints `System not containerized`, but
`OperatingSystemMXBean.getTotalMemorySize()` returns 3072 MiB, while the heap
is still a quarter of the VM's RAM. Java's `Metrics` code detects cgroups
independently of HotSpot.

**By JDK version:**

| JDK | cgroup detection | cgroup v2 + V2s | cgroup v1 + V2s | Needs |
|---|---|---|---|---|
| Temurin **21.0.12.1** | `statfs` (JDK-8349988 **[external]**, see experiment 14) | **Not fixable.** No static file can fake `f_type` | **Fixed**, = V0 | `/proc/cgroups` + 4 v1 files + `/proc/self/{cgroup,mountinfo}` |
| Temurin **17.0.20.1** | `statfs`: the backport is in the 17 line (**[measured]**) | **Not fixable** | **Fixed** | as above |
| Temurin **21.0.8** | `/proc/cgroups` | **Fixed**: kernel 5.15 lists `cpuset`/`memory` with hierarchy 0, so the snapshot sends the JDK down the v2 branch | **Fixed** | `/proc/cgroups` + the set for the running version |
| Temurin **17.0.12** | `/proc/cgroups` | **Fixed** | **Fixed** | as above |

- On kernel 7.0, the real `/proc/cgroups` lacks `cpuset`/`memory`, so older
  JDKs ignore the limits **already at V0** (**[measured]** in experiment 14).
  The snapshot reproduces V0 faithfully, that is, it ignores them too.
  - The snapshot is therefore always the "same as V0" choice, on every kernel
    **[inferred]**.
  - This confirms experiment 14's **[inferred]** claim about kernels up to
    6.x, now measured on 5.15.
- The exact backport boundary within the 21 line (between 21.0.8 and
  21.0.12.1) and the 17 line (between 17.0.12 and 17.0.20.1) **was not looked
  up**.

### Maven and Gradle builds under `c4m3` [measured]

Source: `out/vm/t1b-v{1,2}.tsv`. Each cell is the median wall time of 3 rounds
(range) | JVM figures:

- Maven: `availableProcessors` / `maxMemory` (MiB) of the **surefire fork
  JVM**;
- Gradle: `maxWorkers` / daemon CPU count / `availableProcessors` of the
  **test worker JVM**.

| Command | cgroup | V0 | V1 | V2s |
|---|---|---|---|---|
| `mvn -B package` | v2 | 2.59 s (2.58–2.65) \| 4 / 768 | 2.67 s \| **8 / 2486** | 2.64 s \| **8 / 2486** |
| | v1 | 2.62 s (2.57–2.65) \| 4 / 768 | 2.68 s \| **8 / 2486** | 2.65 s \| **4 / 768** |
| `gradle build --no-daemon` | v2 | 4.36 s (4.28–4.43) \| 4 / 4 / 4 | 4.46 s \| **8 / 8 / 8** | 4.35 s \| **8 / 8 / 8** |
| | v1 | 4.25 s (4.17–4.32) \| 4 / 4 / 4 | 4.49 s \| **8 / 8 / 8** | 4.38 s \| **4 / 4 / 4** |

- Every build reports `BUILD SUCCESS(FUL)` in every variant. The Temurin image
  already ran `ldconfig`, so `libjli` is found (experiment 14).
- The heap of the Gradle daemon and test workers is **512 MiB in every
  cell**. That is Gradle's default `-Xmx512m`, not ergonomics.
- The project is small, so the timings do not separate. The cost of running 8
  workers on a 4-CPU quota in a large build **was not measured**.

### jest, webpack+terser, `next build` of W1 [measured] (`c4m3`, 3 rounds)

Source: `out/vm/t2-v{1,2}.tsv`, logs in `out/vm/t2-v*/c4m3/`, `.next`
comparison in `out/vm/t2-v*/c4m3/compare.txt`. Each cell gives the worker
count and the median wall time (range). Every cell has rc=0 except where
FAIL is shown.

| Command | cgroup | V0 | V1 | V2s |
|---|---|---|---|---|
| jest 29.7 (`maxWorkers` / test processes) | v2 | 3 / 3, 9.01 s (8.97–9.07) | **7 / 7, 7.21 s** | 3 / 3, 9.06 s |
| | v1 | **7 / 7**, 7.16 s (7.15–7.20) | 7 / 7, 7.05 s | 7 / 7, 7.10 s |
| webpack + terser-webpack-plugin 5.6.1 (minify threads) | v2 | 3, 7.04 s (6.68–7.07) | **7, 8.06 s** | 3, 7.04 s |
| | v1 | **7**, 8.31 s (8.28–8.59) | 7, 7.91 s | 7, 8.56 s |
| `next build` W1 (node processes / `jest-worker` workers) | v2 | 19 / 18, 31.53 s (31.23–32.91) | **FAIL rc=1, 0.37 s** | 19 / 18, 31.61 s |
| | v1 | 19 / 18, 31.02 s (30.77–31.20) | **FAIL rc=1, 0.37 s** | 19 / 18, 30.89 s |

Node and Go probe counts under `c4m3` (`counts`):

| Value | v2: V0 / V1 / V2s | v1: V0 / V1 / V2s / V2s-v2set |
|---|---|---|
| node `os.cpus().length` | 8 / **0** / 8 | 8 / **0** / 8 / 8 |
| node `availableParallelism()` | **4** / 8 / **4** | 8 / 8 / 8 / 8 |
| node `constrainedMemory` (MiB) | 3072 / **0** / 3072 | 3072 / **0** / 3072 / **0** |
| V8 `heap_size_limit` (MiB) | 1584 / **2096** / 1584 | 1584 / **2096** / 1584 / **2096** |
| Go `GOMAXPROCS` | 4 / **8** / 4 | 4 / **8** / 4 / **8** |

**Output.**

- jest: 20/20 tests pass in every run.
- webpack: the `sha256` of every `dist/*.js` is `85d2aaecec75…` in **every**
  variant under both cgroup versions, the same hash as experiment 12a on the
  host.
- next: every successful build produces 2032 files, 70.7 MB. Against `V0-1`,
  all 5 other builds of each cgroup version (V0 ×2, V2s ×3) are **2032/2032
  identical** after normalising the BUILD_ID, preview keys and server-actions
  keys (experiment 14). Before normalisation, 2012 files are byte-identical.
- The V1 error: `[Error: ENOENT: no such file or directory,
  uv_resident_set_memory]` right after
  `Creating an optimized production build ...`. V1 leaves only 5 `.next`
  files.

Observations:

- **V2s = V0 for every count and every output** under both cgroup versions.
  The v1 file set restores exactly the cgroup-dependent values:
  `constrainedMemory`, the V8 heap and `GOMAXPROCS`. The v2 set (V2s-v2set)
  does not, as experiment 13 found.
- **Under v1, V0 itself ignores the CPU quota**: libuv reads a wrong path
  (see the file table above), so jest and webpack use 7 workers in every
  variant. Under v1, V1's different worker count is therefore **not caused by
  the missing `/proc`**. V1's real failures are `next build` and the heap
  limits.
- Next computes workers as `os.cpus().length − 1` = 7 and does not read
  cgroups, giving 19/18 under both versions. This is lower than experiment
  14's 68/66 because the VM has 8 CPUs instead of 32.
- On the 8-vCPU VM under v2, V1 (7 workers on a 4-CPU quota) is **faster**
  than V0 for jest (7.21 against 9.01 s) but **slower** for webpack (8.06
  against 7.04 s). The goal of the fake `/proc` is to **match V0**, not to be
  faster. The direction of the difference depends on the tool (experiment
  12a, on 32 CPUs, found V1 slower for both).

### W3 with parallel stages under cgroup v1 [measured]

Source: `out/vm/t3-v{1,2}.txt`. Times in seconds, within the VM, median of 3
rounds (range).

| cgroup | `-jobs 1` (sequential) | `-jobs 0` (default = 8) | Difference |
|---|---|---|---|
| **v1** | 21.39 (20.86–21.80) | **13.89** (13.61–14.16) | **−7.50 s, −35%** |
| v2 (same session, for comparison) | 21.29 (21.27–21.36) | 14.35 (14.19–15.16) | −6.94 s, −33% |
| v2 (experiment 13, another session; **[measured]** there) | 21.15 | 13.21 | −38% |

There is no significant difference between the two cgroup versions.

Correctness (`out/vm/t3-imgcheck-v{1,2}.txt`):

| cgroup | Rounds | Paths | Metadata differences | Content differences |
|---|---|---|---|---|
| v1 | 1, 2, 3 | 7,295 = 7,295 | **0** | 4–5: `/out/{a,b,c,all}.txt`, `/etc/hostname` |
| v2 | 1, 2, 3 | 7,295 = 7,295 | **0** | 4–5, same group |
| v1, `-jobs` default r1 against r2, r3 | | 7,295 | 0 | 4–5, same group |

- `/out/*.txt` holds the result of `du -sk node_modules`
  (`out/t3-out-txt-v1-r1.txt`): `-jobs 1` gives `543096` and the default
  gives `543092`, a difference of 4–8 KB. The package counts
  (`ls | wc -l` = 21/19/20) are the same.
- These files **also differ between two sequential rounds of the same
  configuration**, so parallelism does not cause the difference.
  `/etc/hostname` comes from the listing container.
- `/dev` is `mknod 6/6` in every build.
- Under cgroup v1, the parallel path gives the same image as the sequential
  build and is 35% faster, as under v2. How cgroup v1 charges page cache to
  the memory controller does not change the result at `--memory=8g`
  **[inferred]**. Tighter limits were not measured.

### cpuset [measured]

Source: `out/vm/t2-v*/cs4*/counts-*.log`, `jest-*.log`,
`out/vm/t1-v*/cs4*/`, `out/vm/disc-v*/cs4c2/`. V2s-cpuset gives **exactly the
same as V2s in every cell** under both cgroup versions, so it has no separate
column.

Order of values in each cell:

1. node `os.cpus().length`;
2. `availableParallelism()`;
3. `nproc`;
4. `nproc --all`;
5. `getconf _NPROCESSORS_ONLN`;
6. Go `NumCPU`;
7. Go `GOMAXPROCS`;
8. JVM `availableProcessors` (4 JDKs);
9. jest `maxWorkers`.

| cgroup | cfg | V0 | V1 | V2s (= V2s-cpuset) |
|---|---|---|---|---|
| v2 | `cs4` | 8 · 4 · 4 · 8 · 8 · 4 · 4 · 4 · 3 | **0** · 4 · 4 · **4** · **4** · 4 · 4 · 4 · 3 | 8 · 4 · 4 · 8 · 8 · 4 · 4 · 4 · 3 |
| v2 | `cs4c2` | 8 · **2** · 4 · 8 · 8 · 4 · **2** · **2** · **1** | **0** · 4 · 4 · **4** · **4** · 4 · 4 · 4 · 3 | 8 · 2 · 4 · 8 · 8 · 4 · 2 · **2 (older JDKs) / 4 (recent JDKs)** · 1 |
| v1 | `cs4` | 8 · 4 · 4 · 8 · 8 · 4 · 4 · 4 · 3 | **0** · 4 · 4 · **4** · **4** · 4 · 4 · 4 · 3 | 8 · 4 · 4 · 8 · 8 · 4 · 4 · 4 · 3 |
| v1 | `cs4c2` | 8 · **4** · 4 · 8 · 8 · 4 · **2** · **2** · **3** | **0** · 4 · 4 · **4** · **4** · 4 · 4 · 4 · 3 | 8 · 4 · 4 · 8 · 8 · 4 · 2 · 2 · 3 |

jest under cpuset (2 rounds, median, 20/20 tests pass in every run):

| cgroup | cfg | V0 | V1 | V2s | V2s-cpuset |
|---|---|---|---|---|---|
| v2 | `cs4` | 3 workers, 9.07 s | 3, 9.02 s | 3, 9.10 s | 3, 9.10 s |
| v2 | `cs4c2` | **1 worker, 24.32 s** (runs in a single process) | **3, 13.21 s** | 1, 24.38 s | 1, 24.36 s |
| v1 | `cs4` | 3, 9.23 s | 3, 9.17 s | 3, 9.16 s | 3, 9.18 s |
| v1 | `cs4c2` | 3, 13.03 s | 3, 13.07 s | 3, 12.98 s | 3, 13.06 s |

Answers to the cpuset question:

1. **Affinity passes through `chroot`.** `sched_getaffinity` is a process
   attribute and does not depend on `/proc`. `nproc`, Go `NumCPU`, libuv
   `availableParallelism` and the JVM therefore all return 4 **already at
   V1**. Under cpuset, V1 is wrong only about the **quota** (`--cpus=2`), as
   under `c4m3`.
2. **The fake `/proc/stat`, `/proc/cpuinfo` and
   `/sys/devices/system/cpu/online` do not contradict V0.**
   - Docker does not virtualise these files: at V0 they list all 8 CPUs, so
     `os.cpus().length`, `nproc --all` and `getconf` return 8.
   - The snapshot reproduces exactly those 8.
   - At V1 the files are missing, so glibc falls back to
     `sched_getaffinity` and returns 4. That is "more correct" than V0 but
     **different from V0**, and `os.cpus()` returns 0.
   - The contradiction "8 CPUs in `/proc/stat` but an affinity of 4" is
     **Docker's own**. The fake `/proc` merely preserves it.
   - Under `--cpuset-cpus=0-3`, Next would start 7 workers at V0 and V2s
     alike, because it uses `os.cpus()` **[inferred]**.
3. **No cpuset files are needed.**
   - strace under `cs4c2` (file table above): only
     `java -XshowSettings:system` opens `cpuset.*`.
   - HotSpot (`active_processor_count`), libuv, Go and glibc use
     `sched_getaffinity`.
   - V2s-cpuset = V2s for every count, every JDK and every jest run.
   - Only code calling `jdk.internal.platform.Metrics` or
     `OperatingSystemMXBean` to read the cpuset CPU list would see a
     difference **[inferred]**. No build tool in the list does that.
4. **A side effect of libuv under v1:** under `cs4c2`, V0 under v1 still
   reports `availableParallelism` = 4, because the quota is ignored, so jest
   uses 3 workers. Under v2 it reports 2 and jest uses 1 worker, nearly twice
   as slow (24.3 against 13.0 s). This is jest/libuv behaviour at V0, unrelated
   to the fake `/proc`: V2s follows V0 on both sides.

### Final fake `/proc` set per cgroup version [measured]

`t5-minimal.sh` (`out/vm/t5-v{1,2}.tsv`) re-ran V0 and V2s under `c4m3` with
a reduced cgroup part, checking the 4 JDKs, node and Go.

| Cgroup part | v2: node (`availableParallelism` / `constrainedMemory` / V8 heap) | v2: Go | v2: older JDKs / recent JDKs | v1: node | v1: Go | v1: 4 JDKs |
|---|---|---|---|---|---|---|
| **min1**: v2 `cpu.max memory.max`; v1 `cpu.cfs_{quota,period}_us`, `memory.limit_in_bytes` | **8 / 0 / 2096** (wrong) | 4 ✓ | 4/768 ✓ / 8/2486 (as V2s) | 8 / **0 / 2096** (wrong) | 4 ✓ | 4/768 ✓ |
| **min2**: v2 `cgroup.controllers cpu.max memory.max memory.high`; v1 min1 + `memory.soft_limit_in_bytes` | 4 / 3072 / 1584 ✓ | 4 ✓ | 4/768 ✓ / 8/2486 (as V2s) | 8 / 3072 / 1584 ✓ | 4 ✓ | 4/768 ✓ |

V0 control (same run): v2 node 4 / 3072 / 1584; v1 node 8 / 3072 / 1584;
Go 4; every JDK 4/768.

Each smaller set breaks something:

- v2 without `cgroup.controllers` or `memory.high`: libuv loses the quota.
- v1 without `memory.soft_limit_in_bytes`: libuv loses `constrainedMemory`.

The final set has **14 files per version**. The min2 set was measured; the
17-file V2s used in the other tests is min2 plus 3 files with no effect.

| # | File | v2 | v1 |
|---|---|---|---|
| 1–3 | `/proc/stat`, `/proc/cpuinfo`, `/proc/meminfo` | ✓ | ✓ |
| 4–6 | `/proc/self/cgroup`, `/proc/self/mountinfo`, `/proc/self/stat` | ✓ (`0::/`) | ✓ (14 lines) |
| 7–9 | `/sys/devices/system/cpu/{online,possible,present}` | ✓ | ✓ |
| 10 | **`/proc/cgroups`** (snapshot) | ✓: needed by older JDKs | ✓: needed by **every JDK** |
| 11–14 | The process's cgroup directory | `<D>/cgroup.controllers`, `<D>/cpu.max`, `<D>/memory.max`, `<D>/memory.high` | `<Dcpu>/cpu.cfs_quota_us`, `<Dcpu>/cpu.cfs_period_us`, `<Dmem>/memory.limit_in_bytes`, `<Dmem>/memory.soft_limit_in_bytes` |

Locating the directory (as `_cg_dir` in `fakeproc16.sh` does, which is also
how the JDK and Go do it):

- **v2:** take the mount point of the `cgroup2` line in
  `/proc/self/mountinfo` and append the path from the `0::<P>` line of
  `/proc/self/cgroup` after removing the mount root (column 4). With Docker's
  private cgroupns, `<D>` = `/sys/fs/cgroup`.
- **v1:** take the `cgroup` mount whose super options contain the controller
  name (`cpu`, `memory`) and append the path from the `N:…<ctl>…:<P>` line
  after removing the mount root. With Docker under v1, `<Dcpu>` =
  `/sys/fs/cgroup/cpu,cpuacct` and `<Dmem>` = `/sys/fs/cgroup/memory`.
- Measured only on these two layouts: v2 with a private cgroupns; v1 with a
  host cgroupns and mount root equal to the path.

Not needed:

- **cpuset files** (cpuset section).
- Files read only by Java's `Metrics` or for logging. Missing them causes only
  `ENOENT` and does not change ergonomics:
  - v2: `cpu.weight`, `memory.low`, `memory.swap.max`, `pids.max`,
    `memory.current`;
  - v1: `cpu.shares`, `memory.memsw.limit_in_bytes`, `memory.use_hierarchy`,
    `memory.swappiness`, `memory.usage_in_bytes`, `pids.max`.
- `memory.use_hierarchy` and `memory.stat`. The v1 JDK opens them only when
  there is **no `--memory`**, to read `hierarchical_memory_limit`.
  - **[measured]** with Docker v1 under `cs4`/`cs4c2`: V2s lacks
    `memory.stat` yet reports a 2486 MiB heap like V0, because Docker's parent
    cgroup has no limit either.
  - On Kubernetes with v1, if the limit is set only on a parent cgroup (pod or
    QoS), V0 would see it through `memory.stat` and V2s would not. A snapshot
    of `memory.stat` (a cheap 15th file under v1) would cover that case
    **[inferred]**. Not measured.
- The v1 symlinks `/sys/fs/cgroup/{cpu,cpuacct,net_cls,net_prio}`. `fake_up`
  creates them, but strace shows no tool opening a path through them: Go and
  the JDK follow the mount points in `mountinfo`. They can probably be dropped
  **[inferred]**, but every measured run had them.

Note for [survey chapter 06](../survey/06-overlay-and-parallelism.md)
**[inferred]**:

- Replace "13 files + `/proc/cgroups`" with "**9 common files +
  `/proc/cgroups` + 4 cgroup files for the running version**, derived from
  `mountinfo`/`cgroup` at run time".
- Add that under v2, JDKs using `statfs` (recent 21.0.x, 17.0.20 and later)
  still do not see the limits, while under v1 every JDK does.

## Findings

1. Under cgroup v1, V2s with the v1 file set and a `/proc/cgroups` snapshot
   gives every JDK the V0 values (4 CPUs, 768 MiB); without `/proc/cgroups`
   or with the v2 set it gives 8 CPUs and 2486 MiB (JVM ergonomics table).
2. Under cgroup v2, JDKs 21.0.12.1 and 17.0.20.1 cannot be fixed by fake
   files because they detect cgroup v2 with `statfs`; 21.0.8 and 17.0.12 can
   (JVM ergonomics table; by-JDK table).
3. The `statfs`-based detection (JDK-8349988) is already in Temurin 17.0.20.1
   (file table; by-JDK table).
4. jest, webpack and `next build` of W1 at V2s match V0 in workers, exit codes
   and output (2032/2032 `.next` files) under both cgroup versions; V1's
   `next build` fails after 0.37 s (jest/webpack/next table).
5. libuv 1.51 ignores the CPU quota under cgroup v1 even at V0, because it
   reads `cpuacct:/docker/<id>/cpu.cfs_quota_us` (file table; counts table).
6. W3 with parallel stages under v1 produces the same image as `-jobs 1`
   (7,295 paths, 0 metadata differences) and is 35% faster (W3 tables).
7. CPU affinity passes through `chroot`; the fake set needs no cpuset files,
   and V2s-cpuset equals V2s everywhere (cpuset tables).
8. The minimal sufficient cgroup part is 4 files per version; dropping
   `cgroup.controllers`/`memory.high` (v2) or `memory.soft_limit_in_bytes`
   (v1) breaks libuv (minimal-set table).

## Implications for ranikust

Kaniko mode is sequential by default and `--parallel-stages` is an opt-in
([ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)); these
points concern only the opt-in path. They are the recommendations recorded at
the time **[inferred]**, based on the results above; see
[kaniko mode](../../design/kaniko-mode.md) for the current design.

1. **Generate the fake `/proc` set at run time per cgroup version**, as the
   14 files of the final set:
   - detect the version with ranikust's own `statfs("/sys/fs/cgroup")`, or by
     the presence of a `cgroup2` line in `mountinfo`;
   - take the directories from `/proc/self/{cgroup,mountinfo}`;
   - skip files that do not exist (as `fake_up` does), without an error.
2. **Always copy `/proc/cgroups`.** Under v1 it is the deciding file for
   **every** JDK: without it, 8 CPUs and 2486 MiB.
3. **Do not copy cpuset files.** Affinity passes through `chroot`, and the
   `/proc/stat`/`cpuinfo` snapshot preserves V0's view.
4. **Warn specifically about the JVM under cgroup v2**: "JVMs 21.0.x and
   17.0.20 or later inside `RUN` will not see `--cpus`/`--memory`".
   - The recommendation at the time was not to fix this automatically,
     because `statfs` cannot be faked and `JAVA_TOOL_OPTIONS` has side
     effects (experiment 14, workaround c3). **Superseded:**
     [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md) adopts
     conditional injection of `JAVA_TOOL_OPTIONS` on the parallel path under
     cgroup limits, based on [experiment 15](15-java-next-concurrency.md),
     which measured deterministic failures of Java builds under tight memory
     limits without it.
   - The warning is not needed under v1.
   - Cheap detection: the stage rootfs contains `*/bin/java` and ranikust's
     `/sys/fs/cgroup` is `cgroup2fs`.
5. The parallel path works correctly under cgroup v1 (W3: identical image to
   the sequential build, −35%). No special handling for v1 is needed beyond
   the file set.

## Limitations and open questions

- **Real Kubernetes** (containerd, pod cgroupns, nested QoS cgroups). There
  the mount root may differ from the path in `/proc/self/cgroup`, for example
  v1 with a host cgroupns where `/sys/fs/cgroup/<ctl>` is the real root.
  - The rule "mount point + (path − mount root)" handles this
    **[inferred]**, but it was not measured.
  - Docker v2 with `--cgroupns=host`
    (`0::/system.slice/docker-<id>.scope`) was not measured either.
- **LXCFS**: `/proc/{cpuinfo,stat,meminfo}` are already virtualised to the
  limits, so the snapshot carries LXCFS's numbers and still matches V0
  **[inferred]**.
- Kernels other than 5.15:
  - v1 on kernel 6.x;
  - newer systemd/kernels that drop v1 entirely;
  - kernel 7.0 with older JDKs under v2 (measured at V0 on the host in
    experiment 14, not in rootful Docker).
- `--cpu-shares` and `--memory-swap`. A memory limit set only on a parent
  cgroup (the v1 JDK's `hierarchical_memory_limit` path through
  `memory.stat`). The no-`--memory` case was measured, but only on Docker.
- **The backport boundary of JDK-8349988** in 21.0.x and 17.0.x. Other
  vendors' JDKs: Corretto, Zulu, Microsoft, Liberica, UBI/Debian/Alpine. JDK
  25.
- Large Java builds: many workers on a small quota, OOM kills when heap = a
  quarter of VM RAM under a tight `--memory` (v2 + recent JDK).
- jest 29.3 and earlier and terser-webpack-plugin 5.3.13 and earlier (which
  use `os.cpus()`) under v1 and cpuset. Python and Rust under v1 (experiment
  12a measured only v2).
- W1 with parallel stages under v1 (only W3 was run). Several stages in
  parallel, each with its own fake `/proc`.
- A Rust implementation of `fake_up`/`fake_down`. Here it is still bash.
- libuv versions other than 1.51 under v1 (the `cpuacct:/docker/…` path bug).

## Reproduction

PoC sources are archived outside this repository under
`docs-rust/poc/16-vm-cgroupv1-cpuset/`.

**VM setup.** The VM is a copy of experiment 13's setup with its own name,
user, key and SSH port:

- `scripts/10-make-seed.sh` verifies the sha256 of experiment 13's Ubuntu
  22.04 cloud image, builds a NoCloud seed ISO (`cidata`, xorriso) from
  `cloud-init/user-data` and `meta-data`, and creates a 40 GB qcow2 overlay
  backed read-only by that base image.
- `cloud-init/user-data` creates user `k16` with passwordless sudo and the
  dedicated SSH key, installs `docker.io`, `socat`, `strace` and `jq` from the
  Ubuntu archive, writes the host registry as an insecure registry into
  `/etc/docker/daemon.json`, and enables Docker.
- `scripts/20-boot-vm.sh` boots QEMU/KVM (`-cpu host`, 8 vCPUs, 10 GB, q35,
  virtio disk and network) with user-mode networking and SSH forwarded to
  `127.0.0.1:2216`. It runs in the background, so the VM lives exactly as long
  as the task.
- Inside the VM, `setup.sh` (from experiment 13) starts socat forwarders on
  the Docker bridge gateway to the host registry and verdaccio at
  `10.0.2.2`; `set-cgroup-v1.sh` adds `systemd.unified_cgroup_hierarchy=0` to
  the kernel command line for the next boot.

| File | Content |
|---|---|
| `scripts/10-make-seed.sh` | `cidata` seed (xorriso) + 40 GB qcow2 overlay. The backing file is experiment 13's base image (read-only, sha256 checked) |
| `scripts/20-boot-vm.sh` | QEMU 8 vCPUs, 10 GB, SSH `127.0.0.1:2216`. Runs in the background: the VM lives exactly as long as the task |
| `scripts/30-push.sh`, `31-sync-scripts.sh` | Copy `~/k16` into the VM: experiment 12's `chrun` and `ovbuild` (read-only), `scripts/invm`, `fixtures`, `nextjs-kaniko/app-par`, the sources of `nextjs-kaniko/app` |
| `scripts/40-run.sh <script> [args]` | Runs one script in the VM and logs host load before and after into `out/hostload.txt` |
| `scripts/99-teardown.sh` | Deletes the `k16-*` repositories (API DELETE plus repository directories, as in experiment 13). Shuts the VM down and checks QEMU through the PID file (no `pgrep -f` loop). Deletes the overlay and seed |
| `scripts/report.py` | Medians per (cgroup, cfg, test, variant) → `out/summary.txt` |
| `scripts/cloud-init/` | User `k16`, `docker.io socat strace jq`, `insecure-registries` |
| `scripts/invm/lib.sh`, `setup.sh`, `env.sh`, `set-cgroup-v1.sh` | Copied from experiment 13 (k13 → k16), with `hrun` and `lim_of` added |
| `scripts/invm/prep.sh` | Packages strace, pulls images, builds `gocpu`, exports the rootfs (`/dev` with `mknod`), installs fixtures through verdaccio |
| `scripts/invm/fakeproc16.sh` | `fake_up <rootfs> <parts>`, `fake_down`, `fake_list`. Cgroup set per version, derived from `mountinfo` |
| `scripts/invm/inner.sh` | `runv` for V0/V1/V2s/V2s-nocg/V2s-v2set/V2s-cpuset, `mkdev`, `TRACE=` |
| `scripts/invm/t0-disc.sh`, `stracesum16.py` | File table → `out/vm/disc-v*/` |
| `scripts/invm/t1-jvm.sh`, `inner-jvm.sh`, `jprobe.cmd` | JVM ergonomics, cpuset (JVM) → `out/vm/t1-v*.tsv`, `t1-v*/<cfg>/` |
| `scripts/invm/t1b-jbuild.sh`, `inner-jbuild.sh` | Maven/Gradle builds → `out/vm/t1b-v*` |
| `scripts/invm/t2-node.sh`, `inner-node.sh`, `nexthash.py` (experiment 14) | jest/webpack/next, cpuset (node) → `out/vm/t2-v*` |
| `scripts/invm/t3-build.sh` (from experiment 13), `t3-imgcheck.sh`, `cmp-listing.py` (experiment 12) | W3 → `out/vm/t3-v*` |
| `scripts/invm/t5-minimal.sh` | Final set → `out/vm/t5-v*.tsv` |
| `out/01-prep.log` … `04-v1-suite.log` | Per-phase logs. The additional t5 step logs only to `out/hostload.txt` and `out/vm/t5-*` |
| `out/t3-out-txt-v1-r1.txt` | Contents of `/out/*.txt` in the W3 image (v1, round 1), used in the W3 section |
| `out/vm/` | The VM's whole `~/out` (12 MB; strace traces and image listings gzipped) |

Re-run (about 45 minutes):

```sh
bash scripts/10-make-seed.sh
bash scripts/20-boot-vm.sh &                      # background; wait for cloud-init: scripts/vssh 'cloud-init status'
bash scripts/30-push.sh
for s in setup.sh env.sh prep.sh t0-disc.sh "t1-jvm.sh c4m3" "t1-jvm.sh cs4" "t1-jvm.sh cs4c2" \
         "t2-node.sh cs4 2 counts jest" "t2-node.sh cs4c2 2 counts jest" "t1b-jbuild.sh 3" \
         "t2-node.sh c4m3 3 counts jest webpack next" "t3-build.sh 3" "t3-imgcheck.sh 3 w3" t5-minimal.sh; do
  bash scripts/40-run.sh $s; done                 # cgroup v2
scripts/vssh 'bash ~/k16/scripts/set-cgroup-v1.sh && sudo systemctl reboot'
# wait for SSH, then repeat the for loop above (without prep.sh) for cgroup v1
scripts/vssh 'sudo chown -R k16 ~/out && tar -C ~/out -czf - .' | tar -xzf - -C out/vm && python3 -I scripts/report.py out/vm
bash scripts/99-teardown.sh
```

**Clean-up** (**[measured]**, end of session):

- The VM is shut down. `99-teardown.sh` reported "no k16 qemu process left"
  and 0 `qemu-system` processes for the user.
- `vm/overlay.qcow2`, `vm/seed.iso`, the monitor socket and the PID file
  **were deleted**. Only the `vm/k16_key*` keys remain (gitignored).
- The `k16-w3-*` repositories (12) were deleted (HTTP 202) and their
  directories removed. The `nk-registry` catalog is back to the 5
  repositories it had before the experiment.
- No image was pulled to the host: every `docker pull` ran in the VM.
- Experiment 13's base image is unchanged: sha256 `58be26aa…17ca0`, mtime
  20:50.

## References

1. [Experiment 13: Rootful Docker in a VM](13-docker-rootful-vm.md): the VM
   setup, the v1 file list, libuv's wrong v1 path, W3 under v2.
2. [Experiment 14: Real builds without /proc](14-real-builds-without-proc.md):
   JVM cgroup detection on kernel 7.0, `libjli.so` and `ldconfig`, `.next`
   normalisation, `JAVA_TOOL_OPTIONS` side effects.
3. [Experiment 15: Java, Next.js and concurrency](15-java-next-concurrency.md):
   Java under tight memory limits, the basis of ADR-0019.
4. [Experiment 12a: Fake /proc](12a-fake-proc.md): the original 13-file set,
   jest and webpack fixtures, earlier v2-only measurements.
5. [Experiment 12: Parallel stages in kaniko mode](12-kaniko-mode-parallel.md):
   `chrun`, `ovbuild`, `listing.sh`, `cmp-listing.py`.
6. [Survey chapter 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md),
   parallel stages in kaniko mode.
7. **[external]** JDK-8349988, "Change cgroup version detection logic to not
   depend on /proc/cgroups", cited through experiment 14.
