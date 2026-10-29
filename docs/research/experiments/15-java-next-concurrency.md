# Experiment 15: Java, Next.js and concurrency

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | On the parallel kaniko-mode path with the 14-file fake `/proc`, do large Java builds survive tight memory limits, do toolchain JDKs start, does Next 16/Turbopack need more fake files, and do concurrent stages with their own fake `/proc` stay correct? |
| **Answer** | Java under `--memory=2g` fails 0/3 at V1 and V2s and succeeds 16/16 once `-XX:ActiveProcessorCount` and `-XX:MaxRAM` are injected through `JAVA_TOOL_OPTIONS`, although Gradle still fails at 1.5 GiB. A toolchain JDK 17 on a Temurin 21 image fails visibly. Next 16/Turbopack needs no extra files. Three concurrent stages with per-stage fake `/proc` produce identical images with no leakage, 22% faster, at under 2 ms per `RUN`. |
| **Related** | [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md); [kaniko mode](../../design/kaniko-mode.md), [scheduler](../../design/scheduler.md); experiments [12](12-kaniko-mode-parallel.md), [12a](12a-fake-proc.md), [14](14-real-builds-without-proc.md), [16](16-cgroup-v1-cpuset.md) |
| **Original** | `docs-rust/poc/15-java-next-concurrency/` (Vietnamese, not in this repository) |

## Summary

- Five points left open by [experiment 14](14-real-builds-without-proc.md)
  were tested on the opt-in parallel path (`--parallel-stages`), where `RUN`
  runs under a bare `chroot` with a 14-file fake `/proc`.
- **Java under tight memory fails deterministically, and the fake `/proc`
  does not help.** A synthetic 3040-class project fails 0/3 under
  `--cpus=4 --memory=2g` at V1 and V2s (Maven OOM-killed, Gradle out of heap)
  and passes 3/3 at V0. The JVM sees 32 CPUs and sizes heaps from host RAM.
- **Injecting `-XX:ActiveProcessorCount=4 -XX:MaxRAM=<memory.max>` fixes it**:
  16/16 builds pass at 2 GiB with V0 time and peak memory. Both flags are
  needed. At 1.5 GiB Gradle still fails, because the JVM picks G1 instead of
  SerialGC, and injecting a GC flag is unsafe. Side effects: one
  `Picked up …` stderr line per JVM.
- **A toolchain JDK 17 on a Temurin 21 image loads Temurin 21's `libjli.so`**
  and fails visibly (`NoSuchMethodError: getMainType`, BUILD FAILED). The
  reverse mix (JDK 21 tools on a JDK 17 image) runs silently with the wrong
  launcher.
- **Next 16 (Turbopack) and Next 15.5 `--turbopack`** fail without `/proc`
  and match V0 with the 14-file fake `/proc`; the Turbopack native addon
  reads only the three cgroup files of Rust's `available_parallelism`.
- **Three stages in parallel, each with its own fake `/proc`**, built 9/9,
  saw only their own snapshot, leaked nothing into layers, produced identical
  images, and ran 22% faster than `-jobs 1` (32.2 s against 41.5 s; peak
  memory 7235 against 6189 MiB). The fake `/proc` costs under
  2 ms per `RUN`. strace's large V1 slowdown comes from the JDK closing every
  fd up to `RLIMIT_NOFILE` when `/proc/self/fd` is missing.

## Questions

1. **Large Java builds under a tight `--memory`.** Under `chroot` the JVM does
   not see the cgroup limits (maximum heap 7620 MiB under `--memory=3g`). Is
   the build OOM-killed? Which mitigations can ranikust apply itself, and
   with what side effects?
2. **A JDK downloaded by a toolchain** on a Temurin image (Gradle + foojay).
   Does it start? Whose `libjli.so` does it load?
3. **Next 16 and Turbopack.** Does Turbopack's native binary read
   `/proc/self/exe` or cgroup files?
4. **Several stages at once, each with its own fake `/proc`.** Does anything
   leak into layers, mix between stages, or change the image?
5. (Optional) Why does strace make V1 so much slower than V0?

## Environment

- **Host:** run on 2026-10-29, 21:46–23:00. Kernel 7.0.0-34, 32 CPUs, 29 GB
  RAM, podman 5.7.0 rootless, no sudo. The host was shared with a QEMU VM
  (8 vCPU, RSS 10.5 GB) running a concurrent experiment. Load average
  (column `host_load1` of each
  `results.tsv`, `out/*/_env.txt`): Java 3–23 (at the peak, Next builds
  limited to 4 CPUs ran in parallel), Next 2.5–13.5, multi-stage 1.8–6.5,
  strace 2–3. **Timings are for reference only.** The main results are exit
  codes, OOM events, worker counts and output equality.
- **E-dock**, as defined in [experiment 12](12-kaniko-mode-parallel.md):
  rootless podman with `--cap-drop=ALL` plus the 14 default Docker
  capabilities and the moby seccomp profile (`compat/tools/moby-seccomp.json`,
  read-only). The `capflags` and `lim_of` helpers are in `scripts/lib.sh`.
- **Swap off** (`--memory-swap` = `--memory`), as on Kubernetes nodes and
  most CI VMs. Exceeding the limit makes the kernel reclaim and then
  OOM-kill; nothing is swapped.
- **Rootfs images** (`scripts/prep.sh`):

| Rootfs | Source | Versions |
|---|---|---|
| `r-maven` | `maven:3-eclipse-temurin-21` (pulled for this experiment) | Temurin 21.0.12.1, Maven 3.10.0, Ubuntu 24.04 |
| `r-gradle`, `r-gtc` | `gradle:8-jdk21` (pulled for this experiment). `r-gtc` is a second export with a clean cache, for the toolchain tests | Temurin 21.0.12.1, Gradle 8.14.6. `/root/.gradle` is a symlink to `/home/gradle/.gradle` |
| `r-mtc` | `maven:3-eclipse-temurin-21` + `/opt/jdk17` (Temurin 17.0.21+9, copied from the JDK Gradle downloaded into `r-gtc`) + `~/.m2/toolchains.xml` | |
| `r-node` | `node:22-bookworm-slim` + pnpm 10.34.7. `/app` is W1 (`nextjs-kaniko/app`, Next 15.5.28) installed with `pnpm install --frozen-lockfile`. `/app16` has the same sources but `next` and `eslint-config-next` = **16.4.3** (`next@latest` on npm, released 2026-10-23), reinstalled without a lockfile | Node 22.23.4, React 19.3.1 |

The Temurin image has `/etc/ld.so.conf.d/docker-openjdk.conf` listing
`/opt/java/openjdk/lib` and `/opt/java/openjdk/lib/server` **[code]**, so
**JDK 21's** `libjli.so` and `libjvm.so` are in `ld.so.cache`.

## Method

**Variants.**

| Variant | How it runs | `/proc`, `/sys` |
|---|---|---|
| **V0** | `podman run --rootfs stage/r-<x>` under E-dock: the same file tree that V1/V2s chroot into | real |
| **V1** | harness container (`node:22-bookworm-slim`, E-dock) runs `compat/tools/chrun` (`chroot` + `exec`) into `/s/r-<x>` | empty directories |
| **V2s** | as V1, with `fake_up <rootfs> 13cg` before and `fake_down` after the command (`scripts/fakeproc.sh`, copied unchanged from experiment 14). This is the 14-file set recommended by experiment 14: the 13 files of [experiment 12a](12a-fake-proc.md) plus a snapshot of `/proc/cgroups` | 14 static files |

**Limits** (`lim_of`): `c4m2` = `--cpus=4 --memory=2g --memory-swap=2g`;
`c4m15` = `--cpus=4 --memory=1536m --memory-swap=1536m`; `c4m3` = 3 GiB;
`c32` = unlimited.

**Memory and OOM** (`scripts/run.sh`, function `ctr`):

- The container runs in the background. The host reads `memory.peak` and
  `memory.events` (`oom`, `oom_kill`) of the `libpod-<id>.scope` every 0.2 s
  until the scope disappears.
- **`oom_kill` is a lower bound.** If the container exits within 0.2 s of the
  last kill, the last sample can be missed. The reliable signal is `rc=137`,
  or `Killed`/`daemon disappeared` in the log, together with `memory.peak`
  equal to the limit. For example `mvn-jto-apc` shows `Killed`, rc 137 and a
  2048 MiB peak but records `oom_kill=0`.

**Processes and threads:** `stage/bin/runm` (`scripts/runm.c` from experiment
14) samples `/proc` every 100 ms, outside the chroot. For Next,
`pidlog.js` is also loaded through `NODE_OPTIONS`.

**`RUN` environment:** `env -i <image Config.Env> HOME=/root`, as in
experiment 14.

**Rounds:** the main Java matrix ran 3 rounds at 2 GiB and 2 rounds at
1.5 GiB, with the variant order rotated each round. Mitigations ran 2 rounds,
functional checks 1 round. Next ran 3 rounds; multi-stage builds 3 rounds per
configuration where shown.

**Java project** (`fixtures/java/gen.py` generates the same sources for both
build tools, `stage/java/{mvn,gradle}`):

- **Sources:** `core` has 80 Lombok models and 80 services. Eight modules
  `m01`–`m08` each have 180 Lombok models and 180 services and depend on
  `core`. **3040 classes** in total. Annotation processing with
  **Lombok 1.18.48**.
- **Tests:** 256 JUnit 5 classes × 3 tests.
  - Each class has a `@BeforeAll` that writes the `pid`,
    `availableProcessors` and `maxMemory` of the **test JVM** to
    `/tmp/jprobe.log`, then keeps 5000 objects in a static registry, like
    Spring's test-context cache.
  - Each test allocates 20,000 objects through builders, streams, grouping
    and sorting.
- **Parallelism follows the CPU count.** Both settings are common and
  documented:
  - Maven: surefire `forkCount=1C`, `reuseForks=true`. `javac` runs inside
    Maven's JVM; modules build sequentially.
  - Gradle: `org.gradle.parallel=true` and the daemon's default JVM arguments
    (512 MiB heap). Tests use `maxParallelForks = availableProcessors()/2`,
    the snippet from the Gradle user guide. `maxWorkers` defaults to
    `availableProcessors()`.
- **Calibration** (`out/java/calib/notes.txt`): the configuration was chosen
  so that **V0 passes at 2 GiB**.
  - With heavier tests (200,000 / 40,000 objects), V0 itself was OOM-killed
    even at 3 GiB: 5 JVMs × 25% of the limit = 125% of the limit. Per-JVM
    ergonomics do not add up across JVMs.
  - Gradle with `org.gradle.jvmargs=-Xmx1g` had its daemon OOM-killed at
    2 GiB, even at V0.

**Mitigations.** Variables are added only to the environment of the `RUN`
process. `N` = `ceil(quota/period)` of `cpu.max` = 4; the limit is
`memory.max`. `jto` = `JAVA_TOOL_OPTIONS=-XX:ActiveProcessorCount=4
-XX:MaxRAM=<limit>`; `jdk` = `JDK_JAVA_OPTIONS=<same flags>`.

**Toolchains.** Every process of the `RUN` logs `ld.so` activity
(`LD_DEBUG=libs,files`, `LD_DEBUG_OUTPUT`), showing which `libjli.so` and
`libjvm.so` each launcher loads.

**Next.** `./node_modules/.bin/next build [--turbopack]`, 3 rounds. To
attribute `/proc` reads, `strace -f` traced `%file,%process,readlink*,statfs`
at c32 (`out/next/strace-<app>-<V>.txt`); `strace -k`, filtered with `-P` by
path, attributed each syscall to the ELF file on the stack
(`scripts/kstack.py` → `out/next/kstack-summary.txt`).

**Concurrent stages.** `ovbuild/main.go` is copied from experiment 12's
performance prototype (the original was not modified), with three additions:

- **`-fakeproc`** (only with `-sandbox nocap`). Before each `RUN`:
  - ovbuild reads the 14 files from **its own** `/proc` and `/sys` with
    `os.ReadFile` (that is, to EOF);
  - it writes them into that stage's `<rootfs>/proc` and `<rootfs>/sys` with
    mode 0444, and records the sha256 of (path ‖ content) in `ovbuild.json`
    (`instrs[].fakeproc_sha`).
  - After the `RUN` it counts surviving processes whose `/proc/<pid>/root`
    is the rootfs (`survivors`), deletes **everything** under `proc/` and
    `sys/`, and restores the atime/mtime of both directories with
    `utimensat`. Then the existing `nocap` step restores `/etc/hosts` and
    `resolv.conf`.
- **`-walk-procsys`**: the walk does **not** skip `/proc` and `/sys`. This is
  a strict leak test: any leftover file or changed directory mtime would end
  up in a layer.
- **`-jvm-env tool|jdk`**: adds `JAVA_TOOL_OPTIONS` or `JDK_JAVA_OPTIONS`
  (N and the limit read from the cgroup) to the `RUN` environment, not to
  the stage's `ENV`.

Build context: `scripts/ms.sh prep` → `stage/ctx`. Dockerfile
`fixtures/multistage/Dockerfile`:

- `next`: `pnpm install` for W1, then `next build` (Next 15.5.28 webpack,
  `standalone`).
- `jest`: jest 29.7, 20 CPU-heavy test files (fixture from experiment 12a).
- `wp`: webpack 5.112 + terser-webpack-plugin 5.6.1, 12 entries (fixture
  from experiment 12a).
- `final`: `COPY --from` of three small files: `next.tsv` (normalised hash of
  `.next`; `k15/nexthash.js` is a Node port of `nexthash.py`), `jest.txt` and
  `wp.sha`.
- Each heavy `RUN` prints:
  - `FPCHECK start`/`end`: the sha of the fake files that **this `RUN`**
    sees, with the same byte order as ovbuild (`k15/fpcheck.sh`);
  - `NODEPROBE`: `os.cpus()`, `availableParallelism()`, heap and
    `constrainedMemory`;
  - `WORKERS`: the real worker count, from pidlog or the jest/terser log.

Run under E-dock (`scripts/ms.sh one`):

- `-mode walk -sandbox nocap -export all`, so **every** layer of every stage
  is tarred.
- Limit `--memory=10g` (the host was shared with a 10 GB VM), 32 CPUs.
- Each build is pushed to `registry:5000/k15-<label>`. The host samples the
  container's `memory.peak`.
- After the build, every blob in the workdir is listed with `tar -tzv`
  (`out/ms/<label>/layers.txt`).

**strace slowdown.** `scripts/strace-why.sh`, c32, `r-maven`, with
`strace -f -c` counts.

## Results

### Java under a tight `--memory` [measured]

Summary under `--cpus=4 --memory=2g`, no swap (3040 classes, Lombok, 256 test
classes):

| | V0 (real `/proc`) | V1 (bare `chroot`) | V2s (`chroot` + 14-file fake `/proc`) |
|---|---|---|---|
| `mvn verify` (surefire `forkCount=1C`) | **3/3 OK**, 38.0 s, peak 1535 MiB | **0/3**: Maven OOM-killed (rc 137) after 11–13 s | **0/3**, as V1 |
| `gradle build --parallel --no-daemon` | **3/3 OK**, 32.0 s, peak 2023 MiB | **0/3**: daemon `OutOfMemoryError: Java heap space` (GC thrashing) after 43–64 s | **0/3**, as V1 |

At `--memory=1.5g` the outcome is the same: V0 2/2 OK, V1 and V2s 0/2.

**Main matrix.** Data: `out/java/results.tsv`,
`out/java/<cfg>/<test>-<V>-<rep>.{log,procs}`, summarised in
`out/summary.txt`. Times are medians. "kill" is the sum of `oom_kill`, a lower
bound only.

| cfg | Command | V | OK | wall (s) | peak (MiB) | kill | `java` JVM (max heap / initial heap / GC) | Parallelism | Concurrent `java` |
|---|---|---|---|---|---|---|---|---|---|
| c4m2 | mvn | V0 | 3/3 | 37.99 | 1535 | 0 | 512 / 32 MiB, G1, 4 GC threads | 4 forks per module (32 total), forks see 4 CPUs / 512 MiB | 5 |
| | | V1 | **0/3** rc 137 | 13.03 | **2048** | ≥5 | **7620 / 480 MiB, G1, 23 GC threads** | **32 forks** in module `m01` | **33** |
| | | V2s | **0/3** rc 137 | 12.85 | **2048** | ≥5 | as V1 | as V1 | 33 |
| c4m2 | gradle | V0 | 3/3 | 31.97 | 2023 | 0 | daemon 512 MiB (set by Gradle), test worker 512 MiB | `maxWorkers=4`, 16 test JVMs | 6 |
| | | V1 | **0/3** rc 1 | 44.30 | 1301 | 0 | daemon 512 MiB | **`maxWorkers=32`**, 8 concurrent `compileJava` in the daemon | 2 |
| | | V2s | **0/3** rc 1 | 43.59 | 1236 | 0 | as V1 | as V1 | 2 |
| c4m15 | mvn | V0 | 2/2 | 45.02 | 1214 | 0 | 384 / 24 MiB, **SerialGC** | as c4m2 | 5 |
| | | V1, V2s | **0/2 + 0/2** rc 137 | 11.0 | 1536 | ≥2 | 7620 / 480 MiB, G1 | 32 forks | 33 |
| c4m15 | gradle | V0 | 2/2 | 33.63 | 1507 | 0 | daemon 494 MiB (SerialGC reports this figure for `-Xmx512m`) | `maxWorkers=4` | 6 |
| | | V1, V2s | **0/2 + 0/2** rc 1 | 43.0 | 1290 | 0 | | `maxWorkers=32` | 2 |

How the builds fail:

- **Maven at V1/V2s:** surefire starts 32 JVMs at once for `m01`. The cgroup
  hits its limit **before the first test class runs** (`/tmp/jprobe.log` is
  empty). The kernel kills the largest JVM, Maven's own: the log shows
  `Killed` and `mvn` exits with 137.
- **Gradle at V1/V2s:** no kernel OOM. The daemon (`-Xmx512m`, set by
  Gradle) runs 8 `compileJava` in parallel because `maxWorkers=32`, then
  prints `The Daemon will expire immediately since the JVM garbage collector
  is thrashing` or `java.lang.OutOfMemoryError: Java heap space` inside
  `javac`. BUILD FAILED after 41–64 s.
- **V2s equals V1 in every cell.** JDK 21.0.12 detects cgroups with
  `statfs("/sys/fs/cgroup")`, which fake files cannot satisfy (experiment
  14).

Two mechanisms are at work, and both must be fixed:

- **Parallelism follows the CPU count the JVM sees**: 32 instead of 4 at
  V1/V2s. Surefire starts 32 JVMs at once; Gradle sets `maxWorkers=32`, so 8
  `javac` run at once inside a 512 MiB daemon.
- **Heap ergonomics.** The maximum heap is a quarter of host RAM (7620 MiB).
  The initial heap is 1/64 of host RAM (480 MiB instead of 32 MiB), so every
  JVM takes several hundred MB as soon as it starts.

### Mitigations ranikust can apply itself [measured]

| cfg | Command | Mitigation | V | OK | wall (s) | peak (MiB) | Notes |
|---|---|---|---|---|---|---|---|
| c4m2 | mvn | jto | V1 / V2s | **2/2 / 2/2** | 38.73 / 37.96 | 1492 / 1501 | As V0: heap 512 MiB, 4 forks, 5 concurrent `java` |
| | | jdk | V1 / V2s | **2/2 / 2/2** | 39.49 / 39.12 | 1472 / 1458 | |
| | | jto | V0 (control) | 1/1 | 42.12 | 1500 | Flags equal to what V0 computes itself |
| c4m2 | gradle | jto | V1 / V2s | **2/2 / 2/2** | 32.84 / 33.82 | 1982 / 1916 | `maxWorkers=4` |
| | | jdk | V1 / V2s | **2/2 / 2/2** | 30.76 / 29.70 | 1997 / 1840 | |
| | | jto | V0 | 1/1 | 30.22 | 1805 | |
| c4m15 | mvn | jto / jdk | V2s | **2/2 / 2/2** | 39.60 / 39.64 | 1526 / 1520 | Close to the limit: G1 instead of Serial |
| c4m15 | gradle | jto / jdk | V2s | **0/2 / 0/2** | 14.5 | 1536 | OOM-killed (≥5 / ≥3): `Gradle build daemon disappeared` |
| c4m15 | gradle | jto **+ `-XX:+UseSerialGC`** | V2s | **2/2** | 32.05 | 1536 | As V0 |
| c4m15 | mvn | jto + `-XX:+UseSerialGC` | V2s | 2/2 | 41.31 | 1152 | Peak as V0 (1214) |
| c4m15 | gradle | `-XX:+UseG1GC` (forcing G1 at V0) | **V0** | **0/2** | 14.4 | 1536 | V0 also OOMs with G1 |
| c4m15 | mvn | `-XX:+UseG1GC` | V0 | 2/2 | 37.80 | 1530 | Close to the limit |
| c4m2 | mvn | jto **`ActiveProcessorCount` only** | V2s | **0/1** rc 137 | 12.7 | 2048 | 4 forks, but each fork has a 7620 MiB max heap and 480 MiB initial heap |
| | | jto **`MaxRAM` only** | V2s | **0/1** rc 137 | 14.5 | 2048 | 512 MiB heap, but 32 forks |
| c4m2 | gradle | jto `ActiveProcessorCount` only | V2s | **0/1** | 14.3 | 2048 | `maxWorkers=4`. Gradle caps test workers at `-Xmx512m`, but the initial heap is still min(480, 512) MiB, and the daemon is OOM-killed |
| | | jto `MaxRAM` only | V2s | **0/1** | 41.9 | 1303 | `maxWorkers=32`: the daemon runs out of heap as at V1 |

- **Both flags are required.** `ActiveProcessorCount` fixes the fork count
  and `maxWorkers`. `MaxRAM` fixes the maximum heap **and the initial heap**:
  1/64 of `MaxRAM` is 32 MiB instead of 480 MiB.
- **`MaxRAM` does not replace container detection.** HotSpot picks SerialGC
  when `os::physical_memory() < 1792 MiB` or with fewer than 2 CPUs. At V0
  that value is the cgroup limit; under `chroot` it is host RAM, and
  `MaxRAM` does not change it (**[code]** `is_server_class_machine`;
  **[measured]** `UseSerialGC=true` at V0 c4m15 against `UseG1GC=true` at V2s
  with jto).
  - At any limit below 1792 MiB, multi-JVM builds therefore use more memory
    than at V0, and Gradle at 1.5 GiB runs out of memory.
  - Other HotSpot code paths that use `physical_memory()` diverge from V0 in
    the same way **[inferred]**.
- **Never inject a GC flag.** `JAVA_TOOL_OPTIONS=-XX:+UseSerialGC` together
  with `java -XX:+UseG1GC` stops the JVM from starting:
  `Error occurred during initialization of VM: Multiple garbage collectors
  selected` (`out/jvmside/flags-V0.log`). Many projects set the GC in
  `MAVEN_OPTS`, `org.gradle.jvmargs` or `argLine`.
- **Unknown flags kill older JVMs:** `JAVA_TOOL_OPTIONS=-XX:BogusFlag=1`
  gives `Unrecognized VM option … Could not create the Java Virtual Machine`.
  - With `-XX:+IgnoreUnrecognizedVMOptions` the JVM starts **[measured]**.
  - `-XX:ActiveProcessorCount` exists only from JDK 8u191 and 10
    **[external]**. Older JVMs die without `IgnoreUnrecognizedVMOptions`.

### Side effects of injecting the flags [measured] (`out/jvmside/`, `out/tc/`)

Measured at V0 c32 and at V2s c4m2. The "none" column is the control.

| Check | none | `JAVA_TOOL_OPTIONS` | `JDK_JAVA_OPTIONS` |
|---|---|---|---|
| `java -version 2>&1 \| head -1 \| cut -d'"' -f2` | `21.0.12.1` | **`Picked up JAVA_TOOL_OPTIONS: …`** | **`NOTE: Picked up JDK_JAVA_OPTIONS: …`** |
| `awk -F'"' '/version/{print $2}'`, `-XshowSettings:properties` | correct | correct | correct |
| Extra stderr lines from `java -version` | 0 | 1 | 1 |
| Ergonomics at V2s c4m2 (CPUs / heap) | 32 / 7620 MiB | 4 / 512 MiB | 4 / 512 MiB |
| User has `JAVA_TOOL_OPTIONS=… -XX:ActiveProcessorCount=2` | 2 | **2**: the user's value wins if ranikust puts its flags **first** | **4**: `JDK_JAVA_OPTIONS` counts as command line, so it **overrides** the user's `JAVA_TOOL_OPTIONS` |
| Command-line flags (`-XX:ActiveProcessorCount=3 -Xmx256m`) | win | win | win |
| `javac` launcher (`javac -J-XX:+PrintFlagsFinal`) | 32 / 7620 | 4 / 512 | **32 / 7620**: `javac` does not read this variable |
| `mvn -Dmaven.compiler.fork=true verify` (toolchain 17, V0) | OK, 0 warnings | OK, **`[WARNING] Unrecognized output from [/opt/jdk17/bin/javac, -version]: Picked up …`** | OK, 0 warnings |
| `Picked up` lines in the log of one `mvn` / `gradle` build (c4m2) | 0 | 33 / 17 | 33 / 17 |
| Surefire `Corrupted channel`, extra `[WARNING]` | — | 0 | 0 |
| Gradle toolchain (probing JDK 17, V0) | OK | **OK**, major 61 | OK |
| `Env` in the image config (`ovbuild -jvm-env`, `out/ms/jvmenv-*`) | — | **absent**, `history` unchanged | **absent** |
| File written by the `RUN` (`echo $JAVA_TOOL_OPTIONS > /jto.txt`) | — | value is in the layer | — |
| `~/.gradle/daemon/8.14.6/daemon-<pid>.out.log` | — | contains `Picked up JAVA_TOOL_OPTIONS: …` | contains `NOTE: Picked up JDK_JAVA_OPTIONS: …` |

**`JDK_JAVA_OPTIONS` does not reduce the noise**: the `java` launcher still
prints a `NOTE:` line, and each build still produces 33/17 lines. In exchange
it loses three things:

- the `javac` launcher and JVMs created through JNI do not see the flags;
- it overrides the user's `JAVA_TOOL_OPTIONS`;
- JDK 8 ignores it **[external]**.

Its only advantage is that maven-compiler with `fork=true` prints no warning.

In summary, the side effects of `JAVA_TOOL_OPTIONS` are: one `Picked up …`
line per JVM on stderr (33 per `mvn` run, 17 per `gradle` run); parsing in the
style `java -version 2>&1 | head -1 | cut -d'"' -f2` breaks; maven-compiler
with `fork=true` prints `[WARNING] Unrecognized output` but the build
completes. The image config does not pick up the variable, but the Gradle
daemon log (`~/.gradle/daemon/*/daemon-*.out.log`) contains the value, so an
image that keeps `~/.gradle` carries it in a layer file.

### Toolchain JDKs on a Temurin image [measured]

Data: `out/tc/results.tsv`, `out/tc/c32/*.log`.

| Test | V0 | V1 | V2s |
|---|---|---|---|
| **Gradle toolchain 17 + foojay 0.10.0, JDK not yet present** (`tc17-fresh`) | OK 13 s. Downloads Temurin **17.0.21+9** into `~/.gradle/jdks/eclipse_adoptium-17-amd64-linux.2`. Tests run on 17.0.21. **major 61** | **BUILD FAILED** 11 s: `Cannot find a Java installation … foojay (… Unpacked JDK archive does not contain a Java home …)` | **BUILD FAILED**, as V1 |
| Gradle, JDK 17 already installed (`tc17-warm`) | OK, major 61 | **BUILD FAILED**: the probe fails, Gradle downloads again, then fails as above | **BUILD FAILED** |
| `libjli` / `libjvm` loaded by JDK 17 | `jdk17/bin/../lib/libjli.so` + `jdk17/lib/server/libjvm.so` | **`/opt/java/openjdk/lib/libjli.so` (Temurin 21)** + `jdk17/lib/server/libjvm.so` | as V1 |
| `jdk17/bin/java -version` | `17.0.21` | **`17.0.21`, rc 0**: runs with a mixed launcher without any message | as V1 |
| `jdk17/bin/javac -version`; `jdk17/bin/java -cp … tc.App` | OK | **`NoSuchMethodError: getMainType`** | as V1 |
| **maven-toolchains**, `/opt/jdk17` preinstalled (`mtc17`) | OK 12.5 s, major 61 | **BUILD FAILURE** 1.9 s: `Could not retrieve version from /opt/jdk17/bin/javac … NoSuchMethodError: getMainType` | **BUILD FAILURE** |
| **Reverse**: simulated Temurin 17 image (`ld.so.conf.d` → `/opt/jdk17/lib`, `ldconfig`), running JDK 21 (`mix21`) | `java` / `javac` / `mvn -v` use 21, with 21's `libjli` (`$ORIGIN` beats the cache) | **All run, rc 0, print 21.0.12.1**, but load **`/opt/jdk17/lib/libjli.so`** + 21's `libjvm`. **Silent mix** | — |
| Gradle toolchain + `JAVA_TOOL_OPTIONS`/`JDK_JAVA_OPTIONS` (warm, V0) | OK, major 61. Gradle's probe tolerates the `Picked up` line | — | — |

Why (**[code]** and **[measured]**):

- Every JDK's `bin/java` has `RPATH $ORIGIN/../lib`. Without
  `/proc/self/exe`, `ld.so` drops `$ORIGIN` and takes `libjli.so` from
  `ld.so.cache`, that is, the image's JDK (experiment 14).
- `libjli` then locates `libjvm` from the launcher's own path, so it gets JDK
  17's `libjvm`.
- `libjli` 21 calls `sun.launcher.LauncherHelper.getMainType`. That method
  exists from JDK 21 (JEP 445) **[inferred]** from its name and from
  experiment 14 (JDK 25 also lacks it), so **`libjli` 21 with JDK 17 or 25
  dies when loading the main class**. `-version` alone does not load a main
  class.
- `libjli` 17 does not call that method, so **`libjli` 17 with JDK 21 runs**:
  a silent mix.

Conclusions:

- On a Temurin 21 image, a JDK 17 downloaded by a toolchain (Gradle/foojay)
  or preinstalled (maven-toolchains) **does not run** at V1/V2s. The failure
  is **visible** (BUILD FAILED). No wrong output was observed.
- The **silent** mix exists in the reverse direction: the image's `libjli`
  is older than the toolchain's JDK. In that direction the commands tried all
  ran and gave correct results, but through another JDK's launcher.
- The fake `/proc` changes nothing: V2s = V1.

### Next 16 and Turbopack [measured]

Data: `out/next/results.tsv`, `out/next/<cfg>/*.{log,procs,pids,files.tsv.gz}`,
`out/next/compare.txt`. 3 rounds, medians.

| App | cfg | V0 | V1 | V2s |
|---|---|---|---|---|
| `/app16`: **Next 16.4.3** (Turbopack by default) | c32 | OK **5.82 s** (5.72–5.91), peak 1804 MiB | **FAIL rc 1, 0.2 s** | OK **5.71 s** (5.63–5.74), 1798 MiB |
| | c4m3 | OK 7.14 s, 1471 MiB | FAIL 0.3 s | OK 6.82 s, 1465 MiB |
| `/app`: **Next 15.5.28 `--turbopack`** | c32 | OK 14.12 s, 1241 MiB | FAIL 0.3 s | OK 14.02 s, 1203 MiB |
| | c4m3 | OK 18.33 s, 1027 MiB | FAIL 0.3–0.4 s | OK 18.63 s, 1044 MiB |
| `/app`: Next 15.5.28 webpack (control, 1 round, with strace) | c32 | OK 27.1 s | FAIL 0.4 s | OK 26.9 s |

- **The V1 error** is the same as with webpack and in experiment 14:
  `Error: ENOENT: no such file or directory, uv_resident_set_memory` right
  after `Creating an optimized production build`. Next 16 also calls
  `process.memoryUsage()` at the start of the build.
- **Workers** (pidlog; identical at V0 and V2s in every round):
  - Next 15.5 `--turbopack`: 66 node processes, of which 64 `jest-worker`
    workers, at both c32 and c4m3. This count follows `os.cpus()`.
  - **Next 16**: 8 node processes: 4 workers, 2 `tsc` (type checking runs in
    a separate process), 1 `turbopack-node` pool for postcss, and 1 `next`.
    The log prints `Generating static pages using 4 workers`.
  - **[code]** `next/dist/build/index.js`
    `getNumberOfWorkers(config, totalPageCount)`: Next 16 still takes `cpus`
    from `os.cpus()`, but **caps the worker count at the number of pages
    + 1**. W1 has 3 pages, so the result is 4 regardless of CPU count.
- **Threads:** Next 16's main process (comm `next-build (v16…`) peaks at
  **52 threads at c32 and 24 at c4m3**, the same at V0 and V2s. At V1 (c4m3)
  it reaches **43** before dying, because Turbopack's thread pool is sized for
  32 CPUs when it cannot read the cgroup.
- **Output** (experiment 14's `scripts/nexthash.py`, per-build random values
  removed; `out/next/compare.txt`):
  - Next 15.5 `--turbopack`: **2175/2175 files identical** for every pair
    V0–V0, V0–V2s and c32–c4m3.
  - Next 16: **1364/1365**. The differing file is `trace-build`, a JSON timing
    file (`duration`, `timestamp`) new in Next 16. It also differs between two
    V0 runs.

**Who reads what in `/proc`** (V0, c32; count of accesses):

| Path | `node` (Next 16 / 15.5 Turbopack / 15.5 webpack) | `next-swc.linux-x64-gnu.node` (Turbopack + SWC) |
|---|---|---|
| `/proc/self/cgroup` | 12 / 71 / 74 | **10 / 10 / 11** |
| `/proc/self/mountinfo` | — | **1 / 1 / 3** |
| `/sys/fs/cgroup/cpu.max` | 1 / 1 / 1 | **9 / 9 / 8** |
| `readlink /proc/self/exe` | 9 / 67 / 69 | **0** |
| `statfs /sys/fs/cgroup` | 0 | **0** |
| `/proc/meminfo`, `/proc/stat`, `/proc/self/stat` | yes | 0 |

- The Turbopack addon reads only the three cgroup files used by Rust's
  `std::thread::available_parallelism`, and **all three are in the 14-file
  set**. At V2s it sees the 4-CPU quota (c4m3: 24 threads, as V0). The
  webpack build of 15.5 reads the same files, because SWC lives in the same
  `.node` file.
- Nothing in the addon calls `readlink("/proc/self/exe")` or `current_exe`;
  every such call comes from `node` itself. This differs from `tsgo`
  (TypeScript 7, experiment 14).
- At V2s a few harmless `ENOENT`s remain: `/proc/self/maps` (glibc
  `pthread_getattr_np`), `/proc/filesystems`, `/proc/mounts`,
  `/proc/version*`, `cpufreq`.

Next 16 and Turbopack work on the parallel path with the 14-file fake
`/proc`: output, worker counts and thread counts match V0, and no extra fake
file is needed.

### Concurrent stages, each with its own fake `/proc` [measured]

Data: `out/ms/<label>/{run.json,ovbuild.json,build.log,logs/,layers.txt}`,
`out/ms/report.txt` (`scripts/ms-report.py`), `out/ms/image-check.txt`.

| Run | rc | wall (s) | peak (MiB) | Stage done at (s): wp / jest / next | Workers: next (node / jest-worker) / jest / terser | Fake `/proc`: match / `RUN` | Blobs with `proc/`, `sys/`, `etc/` |
|---|---|---|---|---|---|---|---|
| `-jobs 1` ×3 | 0 ×3 | **41.47** (41.45–41.51) | 6189 (6081–6430) | 16.5–19.9 / 17.1–19.8 / 41.0 | 68/66 / 20 / 12 | 3/3, 0 mismatches | 0/16 ×3 |
| `-jobs` default (32) ×3 | 0 ×3 | **32.19** (32.19–32.40) | 7235 (7235–7286) | 9.2–9.6 / 10.4–11.1 / 31.8 | 68/66 / 20 / 12 | 3/3, 0 mismatches | 0/16 ×3 |
| `--cpus=4 --memory=6g`, default | 0 | 41.86 | 5027 | 18.1 / 24.3 / 41.4 | **68/66 / 3 / 3** | 3/3 | 0/16 |
| `-walk-procsys` (strict), default | 0 | 31.90 | 7350 | 9.2 / 10.5 / 31.5 | 68/66 / 20 / 12 | 3/3 | **0/16** |
| Without `-fakeproc` (= V1), default | **1** | 11.13 | 7283 | wp and jest finish; `next` fails | — / 20 / 12 | — | 0/11 |

- **Every stage completed** in the 9/9 builds with a fake `/proc`, including
  the smoke run. Without it, the `next` stage fails with
  `uv_resident_set_memory`. jest and webpack at c32 still run, with
  `os.cpus()=0` and `availableParallelism()=32`.
- **No mixing between stages:**
  - Each `RUN` gets its own snapshot: the 8 `RUN`s of a build give 8 distinct
    hashes, because `/proc/stat` and `/proc/self/stat` change over time.
  - In all 27 heavy `RUN`s (9 builds × 3 stages), the hash seen by the `RUN`
    at start and at end **equals** the hash ovbuild recorded for that `RUN`,
    even with three stages running at once.
  - `survivors=0` for every `RUN`.
- **No leakage into layers.** 9 builds × 16 blobs (18 layers, of which 3
  identical `COPY k15/` layers):
  - **No** `proc/`, `sys/` or `etc/` entry at all. `/etc` is unchanged
    because `nocap`'s mtime restoration still works.
  - In the `-walk-procsys` run, the walk sees `/proc` and `/sys` and still
    records nothing for them. `fake_down` therefore restores the parent
    directories' metadata correctly; the ignore list is not what saves it.
- **Identical images** (`ovbuild/listing.sh` + `cmp-listing.py`, copied from
  experiment 12):
  - The final images of 7 runs (all three `-jobs 1`, all three default, c4m6,
    strict), each compared with `j1-r1`: **7294/7294 paths, 0 metadata
    differences**.
  - Content differs only in `/etc/hostname` and `/etc/hosts`, created by the
    `listing.sh` container, as in experiment 12.
  - `/result/next.tsv` (normalised hashes of the 2032 `.next` files),
    `jest.txt` and `wp.sha` are byte-identical.
- **Layer statistics** (files, directories, whiteouts, tar size; against
  `j1-r1`): 18/18 layers have the same number of files, directories and
  whiteouts.
  - Tar size differs only in the `next build` layer, by 0.5–32 KB
    (`.next/cache`, `trace`, per-build IDs).
  - In the c4m6 run, tar size also differs in two `npm install` layers, by
    1.5–13 KB (`_cacache`, npm logs).
  - Both are the known non-deterministic groups (experiment 12).
- **Workers:**
  - Identical between `-jobs 1` and the default, since every stage sees the
    same snapshot.
  - Under `--cpus=4`: jest runs 3 test processes and terser 3 threads
    (`availableParallelism()=4` from the fake `cpu.max`), with
    `constrainedMemory` = 6144 MiB. `next build` still uses 66 workers,
    because Next 15.5 uses `os.cpus()` (experiment 12a).
  - **All three stages share the 4-CPU quota**, so jest takes 15.8 s instead
    of 2.4–3.0 s and webpack 12.2 s instead of 2.7–3.7 s, as experiment 12a
    predicted.
- **Timing:**
  - The default `-jobs` is **9.3 s (−22%)** faster than `-jobs 1`. The
    `next build` command barely slows down when running alongside the other
    two stages: 20.0–20.2 s against 20.2–20.4 s. jest and webpack slow down
    by 0.5–1 s.
  - Peak cgroup memory (including page cache) rises from 6.2 to 7.2 GiB.
- **Cost of the Go fake `/proc`:** 0.6–1.2 ms to set up and 0.2–0.5 ms to
  remove, per `RUN`. The bash version in experiment 14 took 17–29 ms.
- **Note on `/proc/self/stat`:** the snapshot is **ovbuild's own** (by design:
  ranikust reads its own `/proc`). `process.memoryUsage().rss` of every Node
  process in a `RUN` therefore returns the builder's RSS, 85–531 MiB depending
  on the moment. In experiment 12a it was the 1 MiB of `cat`. No tool in this
  set uses that number to make decisions.
- **JVM variables do not leak into the image** (`ovbuild -jvm-env tool|jdk`,
  `out/ms/jvmenv-*`):
  - The `RUN` sees
    `JAVA_TOOL_OPTIONS=-XX:ActiveProcessorCount=4 -XX:MaxRAM=2147483648`.
  - The image's `config.Env` contains only `PATH`, `NODE_VERSION`,
    `YARN_VERSION`, `FOO=bar`.
  - The file `/jto.txt` written by the `RUN` itself contains the value, as
    expected.

A per-stage fake `/proc` is correct when several stages run at once: no
mixing, no leakage, an unchanged image, the same worker counts, and under
2 ms per `RUN`.

### Why strace makes V1 so much slower [measured] (optional)

`scripts/strace-why.sh`, c32, `r-maven`. Data: `out/strace/results.tsv`,
`out/strace/c32/*-count-*.strace` (`strace -f -c`).

| Command | no strace V0 / V1 | `-e trace=none` V0 / V1 | `-e trace=%file` V0 / V1 | `close()` V0 / V1 (errors) |
|---|---|---|---|---|
| `java -version` | 0.1 / 0.1 s | 0.1 / 0.1 | 0.1 / 0.1 | — |
| `java Probe.java` (spawns one `sh`) | 0.3 / 0.3 s | 0.3 / **3.4** | 0.3 / **3.6** | 142 / **524,368 (524,286 `EBADF`)** |
| `mvn -o -q -pl core package -DskipTests` | 3.0 / 2.9 s | 3.9 / **33.3** | 3.9 / **32.5** | 3322 / **5,245,552 (5,242,201 `EBADF`)** |

- `ulimit -n` is **524288** in all three variants
  (`out/strace/c32/ulimit-*.log`). `/proc/self/fd` exists at V0 but not at
  V1 and V2s.
- When creating a child process (jspawnhelper or `posix_spawn`), the JDK
  closes every extra fd:
  - normally by listing `/proc/self/fd` (**[code]** `childproc.c`
    `closeDescriptors`);
  - if that directory cannot be opened, by calling `close()` on every fd from
    3 to `sysconf(_SC_OPEN_MAX)`.
  - Measured: about 524,000 syscalls per spawn, and `mvn` on `core` spawns
    about 10 times.
- Without strace a failing `close()` is very cheap, so the difference is not
  measurable. Under ptrace each syscall becomes two stops (entry and exit),
  even with `trace=none`. `futex` time (other threads waiting) also rises from
  50 to 412 µs per call.
- Failed `openat` (113 against 62) and failed `readlink` (9272 against 9246)
  are nearly equal. The cause is therefore **not** repeated `ENOENT`s or
  file fallback paths.
- Consequences **[inferred]**:
  - Outside strace, every JVM spawn inside a bare `chroot` costs an extra
    loop of about 524,000 syscalls. The figure scales with `RLIMIT_NOFILE`;
    Docker commonly sets 1,048,576 **[external]**.
  - Builds whose JVMs spawn thousands of child processes will slow down
    noticeably.
  - The fake `/proc` cannot fix this: `/proc/self/fd` is per-process.
  - ranikust could lower the soft `RLIMIT_NOFILE` for `RUN`, but that changes
    behaviour relative to Docker and is not advisable.

## Findings

1. Under `--cpus=4 --memory=2g`, a multi-module Java build fails 0/3 at V1 and
   V2s (Maven OOM-killed, Gradle out of heap) and passes 3/3 at V0; the fake
   `/proc` changes nothing (Java summary and main matrix).
2. The failures come from two mechanisms: CPU-based parallelism (32 forks,
   `maxWorkers=32`) and host-RAM heap ergonomics (7620 MiB maximum, 480 MiB
   initial) (main matrix).
3. `-XX:ActiveProcessorCount=4 -XX:MaxRAM=<limit>` in `JAVA_TOOL_OPTIONS` or
   `JDK_JAVA_OPTIONS` makes 16/16 builds pass at 2 GiB with V0 time and peak;
   either flag alone fails (mitigations table).
4. Below 1792 MiB the JVM without container detection picks G1 instead of
   SerialGC, so Gradle at 1.5 GiB still fails 4/4 with the flags; adding
   `-XX:+UseSerialGC` fixes it but conflicts with user-chosen GCs
   (mitigations table).
5. `JAVA_TOOL_OPTIONS` beats `JDK_JAVA_OPTIONS`: it lets the user's value win
   and reaches `javac`, whereas `JDK_JAVA_OPTIONS` overrides the user's
   `JAVA_TOOL_OPTIONS` and is ignored by JDK 8; both add one stderr line per
   JVM and neither enters the image config (side-effects table).
6. A toolchain JDK 17 on a Temurin 21 image loads Temurin 21's `libjli.so`
   and fails with `getMainType`; the reverse mix runs silently (toolchain
   table).
7. Next 16 and Next 15.5 `--turbopack` fail at V1 and match V0 at V2s
   (2175/2175 and 1364/1365 files); the Turbopack addon reads only the three
   `available_parallelism` cgroup files (Next tables).
8. Three concurrent stages with per-stage fake `/proc` built correctly, saw
   only their own snapshot (27/27), leaked nothing (0/16 blobs, also with
   `-walk-procsys`) and produced identical images, 22% faster (multi-stage
   table).
9. The Go fake `/proc` costs 0.6–1.2 ms to set up and 0.2–0.5 ms to remove per
   `RUN` (multi-stage results).
10. Without `/proc/self/fd`, the JDK closes every fd up to `RLIMIT_NOFILE` on
    each spawn (about 524,000 `close()` calls), which is cheap natively but
    slow under strace (33.3 s against 3.9 s for `mvn` on `core`) (strace
    table).

## Implications for ranikust

Kaniko mode is sequential by default and `--parallel-stages` is an opt-in
([ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)); all points
below concern the opt-in path. They are the recommendations recorded at the
time **[inferred]**, based on the results above and on experiments 12a and
14. See [kaniko mode](../../design/kaniko-mode.md) for the current design.

1. **Keep the 14-file fake `/proc`, enabled by default under
   `--parallel-stages`.** This experiment adds Next 16 (Turbopack), Next 15.5
   `--turbopack` and three concurrent stages to the evidence. No extra file is
   needed. ([Experiment 16](16-cgroup-v1-cpuset.md) later made the cgroup part
   of the set depend on the cgroup version.)
2. **Implementation** (verified in Go in `ovbuild`; ranikust does the same in
   Rust):
   - ranikust reads the 14 files from its own `/proc` and `/sys` and writes
     them into **that stage's** rootfs, mode 0444.
   - After the `RUN`: wait for the whole process tree to exit (check
     `survivors`; a Gradle daemon outlives its `RUN`, experiment 14). Then
     delete **everything** under `proc/` and `sys/` and restore the atime and
     mtime of both directories.
   - The walk still always skips `/proc`, `/sys` and `/dev`. The
     `-walk-procsys` test shows that the clean-up alone is sufficient, so the
     ignore list is a second line of defence rather than the only one.
   - Cost: about 1 ms per `RUN`.
3. **Java on the parallel path.** This reverses experiment 14's advice not to
   inject `JAVA_TOOL_OPTIONS`, and is the basis of
   [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md):
   - **Conditions:** running with `--parallel-stages`; ranikust's cgroup has
     `cpu.max` or `memory.max` other than `max`; the stage rootfs contains
     `*/bin/java`.
   - **Value:**
     `JAVA_TOOL_OPTIONS="-XX:+IgnoreUnrecognizedVMOptions -XX:ActiveProcessorCount=<N> -XX:MaxRAM=<memory.max> $USER_JTO"`,
     with ranikust's flags placed **before** the stage's existing
     `JAVA_TOOL_OPTIONS` so the user's values win.
   - **Scope:** only the environment of the `RUN` process, never `ENV` or the
     image config.
   - **Logging:** one line per stage stating which variable was injected and
     why.
   - **Never** inject a GC flag; **never** use `JDK_JAVA_OPTIONS`.
   - Measured effect: 0/16 becomes 16/16 at 2 GiB, with V0 time and peak
     memory.
4. **Warn before the build** if `memory.max < 1792 MiB` and a stage contains
   Java: "the JVM will use G1 instead of SerialGC as it would with `/proc`;
   multi-JVM builds may run out of memory; run without `--parallel-stages` or
   set the GC yourself". At 1.5 GiB Gradle failed 4/4 even with the flags.
5. **User documentation** lists the side effects: the `Picked up` stderr line,
   `head -1` style parsing, the maven-compiler warning when forking, and the
   Gradle daemon log.
6. **Pre-build detection of several JDKs in one rootfs** (extending the
   detection proposed in experiment 14):
   - For each `*/bin/java`, compare `<jdk>/lib/libjli.so` with the path
     `ld.so.cache` returns for `libjli.so`. A mismatch means that launcher
     will load another JDK's `libjli` under `chroot`: a `getMainType` failure
     or a silent mix.
   - In that case, log "stage X: JDK at P will load libjli of Q under
     --parallel-stages".
   - A JDK downloaded by a toolchain **during** a `RUN` cannot be detected;
     it fails visibly (BUILD FAILED). Add
     `Unpacked JDK archive does not contain a Java home` and `getMainType` to
     the list of error strings that trigger the hint to run without
     `--parallel-stages`.
7. **Tool families** (update of experiment 14's table):

   | Tool family | Parallel path (`chroot` + fake `/proc`) | Notes |
   |---|---|---|
   | Node: next 15.5 (webpack, `--turbopack`), **next 16 (Turbopack)**, webpack/terser, jest, tsc 5, esbuild, vite | **Safe** | Output, worker and thread counts as V0. Also checked with 3 stages running at once |
   | Java on a Temurin image, **one JDK**, no cgroup limits | Runs | As in experiment 14 |
   | Java on a Temurin image, with `--cpus`/`--memory` | **Fails deterministically** without the fix (0/16 at 2 GiB). **Runs** when ranikust injects `JAVA_TOOL_OPTIONS` (16/16 at 2 GiB) | Below 1792 MiB it can still run out of memory, because the JVM picks G1 instead of Serial |
   | **Other toolchain JDKs** (Gradle foojay, maven-toolchains, images with two JDKs) | **Needs sequential** | The image JDK's `libjli`: `getMainType`/BUILD FAILED, or a silent mix |
   | JDKs installed with apt/apk/dnf | Needs sequential | Experiment 14 |
   | TypeScript 7 (`tsgo`) and binaries calling `os.Executable`/`current_exe` at start-up | Needs sequential | Experiment 14. The Turbopack addon does **not** call them |
   | `<(…)`, `/dev/fd`, `ps`, `/proc/<pid>` | Needs sequential | Experiment 12a |

8. **Scheduler:** three parallel stages all see the full CPU quota (jest 5–6
   times slower under `--cpus=4`) and push page cache to 7.2 GiB. The memory
   limit remains the main constraint (experiment 12; see
   [scheduler](../../design/scheduler.md)).

## Limitations and open questions

- **Real rootful Docker, Kubernetes pods, cgroup v1, LXCFS.** Still E-dock on
  rootless podman. V1/V2s got `/dev` through `podman --device`, while
  `ovbuild nocap` cannot `mknod`, so it has no real `/dev/null`.
- **Real Java projects.** The project here is synthetic: Lombok, no Kotlin, no
  Spring. The Kotlin daemon, sbt, Bazel, Android, and builds longer than 10
  minutes were not tried.
- **Java with limits of 3–8 GiB**, where G1 versus Serial no longer matters
  but fork and worker counts are still wrong. Only 2 GiB and 1.5 GiB were
  measured.
- **JDKs other than Temurin** (Corretto, Zulu, Liberica, GraalVM, JDK
  8/11/17/25 as the image JDK). The mix "`libjli` 21 + JDK 22–24" was not
  tried; it probably works because those releases still have `getMainType`
  **[inferred]**.
- **The mitigation on JVMs up to 8u181** (no `ActiveProcessorCount`) and on
  OpenJ9 was not run; only `-XX:+IgnoreUnrecognizedVMOptions` with a bogus
  flag was checked.
- **Other HotSpot code paths using `physical_memory()`** (besides GC
  selection) under `-XX:MaxRAM`.
- **Next 16 on an app with many pages** (where the worker count is no longer
  capped at 4), Next 16 with `--webpack`, and `next build` in a project using
  TypeScript 7.
- **More than 3 stages, or more stages than cores**; several Java stages at
  once with `JAVA_TOOL_OPTIONS` (each stage's JVMs get the same N and the
  same limit, so the total exceeds the limit; only memory-based limiting can
  prevent that **[inferred]**).
- **A Rust implementation** of setting up and removing the fake `/proc`. Here
  it is Go, about 1 ms.
- **`RLIMIT_NOFILE` = 1,048,576** (Docker) and the cost of the `close()` loop
  when a Java build spawns many processes without strace.

## Reproduction

PoC sources are archived outside this repository under
`docs-rust/poc/15-java-next-concurrency/`. Everything ran on the host as an
unprivileged user under rootless podman (E-dock); no VM.

| File | Content |
|---|---|
| `scripts/lib.sh`, `scripts/run.sh`, `scripts/inner.sh` | E-dock, `run1` for V0/V1/V2s, host-side sampling of `memory.peak`/`memory.events` (`ctr`), `rec` → `results.tsv` |
| `scripts/fakeproc.sh` | Copied unchanged from experiment 14. V2s = level `13cg` (14 files) |
| `scripts/prep.sh [tools node w1 w16 gradle maven gtc mtc javagen]` | Builds `stage/`. `mtc` needs `r-gtc` to already contain JDK 17, so `tc.sh c32 fresh V0` must run first |
| `fixtures/java/gen.py`, `gen.sh` | Synthetic Java project. `GEN_ALLOC`/`GEN_KEEP` (defaults 20000/5000) |
| `scripts/java.sh`, `java-matrix.sh`, `java-gc.sh` | Main matrix and mitigations → `out/java/`. Calibration notes and discarded rows: `out/java/calib/notes.txt` |
| `scripts/jvm-side.sh`, `jvm-flags.sh` | Side effects → `out/jvmside/` |
| `fixtures/java/tc17`, `fixtures/java/mtc17`, `scripts/tc.sh`, `tc-mvn.sh`, `tc-mix.sh` | Toolchains → `out/tc/` |
| `scripts/next.sh`, `nexthash.py`, `stracesum.py`, `kstack.py` | Next → `out/next/`. Raw strace files over 2 MB were moved to `stage/traces` and deleted; summaries are in `out/next/strace-*.txt` and `kstack-summary.txt` |
| `ovbuild/main.go` (+ `go.mod`, `go.sum`, `listing.sh`, `cmp-listing.py`) | Concurrent stages. Build: `podman run --rm -v k15-gomod:/go/pkg/mod -v $PWD/ovbuild:/src -w /src 5d8d6618be1c sh -c 'CGO_ENABLED=0 go build -o bin/ovbuild .'` |
| `fixtures/multistage/`, `fixtures/jvmenv/`, `scripts/ms.sh`, `ms-all.sh`, `ms-report.py` | Concurrent stages → `out/ms/` |
| `scripts/strace-why.sh` | strace slowdown → `out/strace/` |
| `scripts/report.py` | `out/summary.txt` |
| `out/0*.log` | Per-step logs, with host load |

Re-run (about 75 minutes; `stage/` takes about 8.2 GB):

```sh
bash scripts/prep.sh tools node w1 w16 gradle maven gtc javagen
cp -a stage/java/mvn stage/r-maven/gen/ && cp -a stage/java/gradle stage/r-gradle/gen/   # (mkdir -p .../gen first)
TAG=cold bash scripts/java.sh c4m2 1 mvn V0 && TAG=cold bash scripts/java.sh c4m2 1 gradle V0   # warm the caches
bash scripts/java-matrix.sh && bash scripts/java-gc.sh
bash scripts/tc.sh c32 fresh V0 V1 V2s && bash scripts/tc.sh c32 fresh V0 && bash scripts/tc.sh c32 warm V1 V2s
bash scripts/prep.sh mtc && bash scripts/tc-mvn.sh c32 && bash scripts/tc-mix.sh
bash scripts/jvm-side.sh V0 && bash scripts/jvm-side.sh V2s c4m2 && bash scripts/jvm-flags.sh
bash scripts/next.sh c4m3 3 w16 && bash scripts/next.sh c4m3 3 w1tp && bash scripts/next.sh c32 3 w16 && bash scripts/next.sh c32 3 w1tp
for t in w16 w1tp w1wp; do STRACE=proc TAG=strace bash scripts/next.sh c32 1 $t; STRACE=kstack TAG=kstack bash scripts/next.sh c32 1 $t V0; done
bash scripts/ms.sh prep && bash scripts/ms-all.sh && python3 -I scripts/ms-report.py out/ms/j1-r1 out/ms/j0-r*
bash scripts/strace-why.sh c32 1 && python3 -I scripts/report.py
bash scripts/ms.sh clean
```

Two measurement errors were recorded and corrected:

- **`scripts/inner.sh` was edited while a container was reading it.** bash
  reads scripts incrementally, so the `c4m15 gradle V2s` run, repetition 1,
  hit a syntax error after the build finished. `fake_down` did not run, the
  fake files stayed in `r-gradle`, and the next run (`V1` repetition 2) was
  effectively V2s. Both rows were discarded and re-run as repetition 3
  (`out/java/calib/notes.txt`). Lesson for ranikust: never depend on a file
  that another process may modify mid-run.
- **Wrong path when deleting `~/.gradle/jdks`.** The first attempt deleted
  `$R/root/.gradle` (an absolute symlink) from the harness side, which removed
  nothing. That run was repeated with `/home/gradle/.gradle/jdks`; the old
  results are in `out/tc/old/`.

Clean-up after the experiment:

- `stage/` (8.2 GB, above the 500 MB threshold) **was deleted** with
  `podman unshare rm -rf stage`. `prep.sh` and `ms.sh prep` rebuild it.
- Images pulled for this experiment **were deleted**:
  `docker.io/library/maven:3-eclipse-temurin-21` (580 MB,
  `sha256:c665c104…`) and `docker.io/library/gradle:8-jdk21` (807 MB,
  `sha256:b3453e88…`).
- Images already on the machine were only read: `node:22-bookworm-slim`,
  `debian:latest`, `alpine:latest`, `rust:1-bookworm`, `golang`
  `5d8d6618be1c`.
- The `k15-*` repositories in `nk-registry` **were deleted** (11
  repositories: `k15-smoke`, `k15-j{0,1}-r{1,2,3}`, `k15-c4m6-j0`,
  `k15-strict-j0`, `k15-jvmenv-{tool,jdk}`). Orphan blobs remain, as no
  garbage collection was run, as in experiment 12.
- The `k15-work` and `k15-gomod` volumes **were deleted**.
- Containers were named `k15-*` and all were removed (`podman rm` after
  measuring).
- `ovbuild/bin/` (gitignored) was deleted; the build command is in the table
  above.

## References

1. [Experiment 14: Real builds without /proc](14-real-builds-without-proc.md):
   the 14-file set, `libjli.so` and `$ORIGIN`, JVM cgroup detection with
   `statfs`, Gradle daemon life cycle.
2. [Experiment 12a: Fake /proc](12a-fake-proc.md): the 13-file set, jest and
   webpack fixtures, `next build` worker counts, quota sharing between
   stages.
3. [Experiment 12: Parallel stages in kaniko mode](12-kaniko-mode-parallel.md):
   E-dock, `ovbuild`, `listing.sh`, known non-deterministic files, memory as
   the scheduling constraint.
4. [Experiment 16: cgroup v1 and cpuset](16-cgroup-v1-cpuset.md).
5. [Survey chapter 05: Decisions](../survey/05-decisions.md), Q12, and
   [survey chapter 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md),
   parallel stages in kaniko mode.
6. **[code]** HotSpot `is_server_class_machine` (SerialGC below 1792 MiB or
   2 CPUs); JDK `childproc.c` `closeDescriptors`; Next.js
   `next/dist/build/index.js` `getNumberOfWorkers`. The original gives no
   commit or line references.
7. **[external]** `-XX:ActiveProcessorCount` available from JDK 8u191 and 10;
   JDK 8 ignores `JDK_JAVA_OPTIONS`; Docker's common `RLIMIT_NOFILE` of
   1,048,576. The original records these without citing the source.
