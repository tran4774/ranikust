# Experiment 14: Real builds without /proc

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Do real builds (`next build` of W1, Maven and Gradle on several JDKs, tsc, esbuild and vite) run under bare `chroot` without `/proc`, and with the fake `/proc`, with the same workers and output as with a real `/proc`? |
| **Answer** | `next build` of W1 always fails without `/proc` (after 0.50 s) and, with the 13-file fake `/proc`, matches V0 in time, process and thread counts and all 2032 `.next` files. Java depends on the image: Temurin runs but the JVM ignores `--cpus`/`--memory` (32 CPUs and a 7620 MiB heap instead of 4 and 768 MiB, 16–19% slower), while Debian, Alpine and UBI9 JDKs fail with rc=127 on `libjli.so`. TypeScript 7 panics on `/proc/self/exe`. |
| **Related** | [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md); [kaniko mode](../../design/kaniko-mode.md); experiments [12](12-kaniko-mode-parallel.md), [12a](12a-fake-proc.md), [13](13-docker-rootful-vm.md), [15](15-java-next-concurrency.md), [16](16-cgroup-v1-cpuset.md) |
| **Original** | `docs-rust/poc/14-next-java-proc/` (Vietnamese, not in this repository) |

## Summary

- On the opt-in parallel kaniko-mode path (`--parallel-stages`), `RUN` runs
  under a bare `chroot` into each stage's rootfs directory, with no `/proc`
  and no `/sys`. A "fake `/proc`" of 13 static files can be copied in
  ([experiment 12a](12a-fake-proc.md)). This experiment tested it on the real
  W1 app, on Maven and Gradle across JDK builds, and on tsc, esbuild and vite.
- **`next build` of W1** fails every time without `/proc` (`ENOENT …
  uv_resident_set_memory` after 0.50 s). With the 13-file fake `/proc` it
  completes and matches V0 in timing (24.6 / 21.1 s against V0's 22.4 /
  21.2 s, unlimited / `--cpus=4 --memory=3g`), in process and thread counts
  and in all 2032 `.next` files after normalising per-build random values.
- **Java depends on the image, not "Java always fails".** Official Temurin
  images run because they register the JDK library directory in
  `ld.so.cache`; Debian, Alpine (musl) and UBI9 JDKs fail with rc=127 because
  `ld.so` cannot resolve `$ORIGIN` without `/proc/self/exe`.
- **Even when Java runs, the JVM ignores the container limits** (32 CPUs and a
  7620 MiB heap instead of 4 CPUs and 768 MiB; Gradle `maxWorkers` 32 instead
  of 4). The fake `/proc` cannot fix this for recent JDKs, which detect cgroup
  v2 with `statfs`.
- Every automatic process-level workaround for `libjli.so` (`LD_ORIGIN_PATH`,
  `LD_LIBRARY_PATH`, a fake `/proc/self/exe`) works with one JDK but goes
  silently wrong with two. TypeScript 7 (`tsgo`) panics at start-up without
  `/proc/self/exe`.
- Recommendation at the time: keep the 13-file fake `/proc` and add a snapshot
  of `/proc/cgroups` (14 files), never fake `/proc/self/exe` or set `LD_*`,
  and classify tool families as safe or sequential-only.

## Questions

1. Does `next build` of the **real W1 app** (`nextjs-kaniko/app`: Next 15.5,
   React 19, MUI, tailwind, `output: 'standalone'`) run under bare `chroot`?
   If it does, are the worker counts and the output the same as with a real
   `/proc`?
2. Do Maven and Gradle run on different JDK builds (official Temurin, Debian
   OpenJDK, Alpine musl OpenJDK)? Which workarounds work, and what does each
   one break?
3. Does the JVM honour `--cpus` and `--memory`, at V0 and with the fake
   `/proc`?
4. Besides `java`, does Gradle need `/proc`? What about `tsc`, `esbuild` and
   `vite`?

## Environment

- **Host:** run on 2026-10-29, 20:45–21:30. Kernel 7.0.0-34, 32 CPUs, 29 GB
  RAM, podman 5.7.0 rootless, no sudo. The host was shared with a QEMU VM
  (8 vCPU, 10 GB) running a concurrent experiment. Load average during the
  runs: W1 1.7–6.8, Java
  0.5–3.5, JS 1.1–1.8 (recorded in `out/*/_env.txt` and in the `host_load1`
  column of each `results.tsv`). **Timings are for reference only.** The main
  results are exit codes, worker counts and output equality.
- **E-dock**, as defined in [experiment 12](12-kaniko-mode-parallel.md):
  rootless podman with `--cap-drop=ALL` plus the 14 default Docker
  capabilities, the moby seccomp profile (`compat/tools/moby-seccomp.json`,
  read-only), and `--network=host`. The `pdock` helper is in `scripts/lib.sh`.
- **Network:** verdaccio (`nk-verdaccio`) at 127.0.0.1:4873, proxying npmjs;
  Maven Central and `plugins.gradle.org` reached directly (HTTP 200 from the
  host).
- **Rootfs images** (`scripts/prep.sh`; each exported with
  `podman export | podman unshare tar -x`):

| Rootfs | Source | Versions |
|---|---|---|
| `r-node` | `node:22-bookworm-slim` + `npm i -g pnpm@10.34.7` + `/app` = sources of `nextjs-kaniko/app` + `pnpm install --frozen-lockfile` (as in the W1 Dockerfile) | Node 22.23.4, next 15.5.28, react 19.3.1, typescript 5.9.3. `node_modules` 1.4 GB. `pnpm install` took 3 s because verdaccio was warm; the store (1.4 GB) was deleted after installing |
| `r-maven` | `maven:3-eclipse-temurin-21` (Ubuntu 26.04) | Temurin 21.0.12.1, Maven 3.10.0, glibc 2.43 |
| `r-gradle` | `gradle:8-jdk21` | Temurin 21.0.12.1, Gradle 8.14.6 |
| `r-deb` | `debian:latest` (13.7) + `apt install default-jdk-headless maven` | OpenJDK 21.0.13, Maven 3.9.9 |
| `r-deb2` | as above + `openjdk-25-jdk-headless` (two JDKs; default `java`/`javac` is 25) | 21.0.13 and 25.0.5 |
| `r-alp` | `alpine:latest` (3.24.3) + `apk add openjdk21-jdk maven gradle` | OpenJDK 21.0.12 musl, Maven 3.9.16, Gradle 8.14.4 |
| `r-kc` | `keycloak:26.4.1-0` (UBI9) | OpenJDK 21.0.8 (used only for JVM container awareness) |

## Method

**Variants.**

| Variant | How it runs | `/proc`, `/sys` |
|---|---|---|
| **V0** | `podman run --rootfs stage/r-<x>` under E-dock: **the same file tree that V1/V2s chroot into**, command run directly | real |
| **V1** | harness container (`node:22-bookworm-slim`, E-dock) runs `compat/tools/chrun` (`chroot` + `exec`) into `/s/r-<x>` | empty directories |
| **V2s** | as V1, with `fake_up` before and `fake_down` after the command: the 13 files of experiment 12a, namely `/proc/{stat,cpuinfo,meminfo}`, `/proc/self/{cgroup,mountinfo,stat}`, `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max,memory.high}`, `/sys/devices/system/cpu/{online,possible,present}` | 13 static files |
| V2c | V2s plus a snapshot of `/proc/cgroups` (used for JVM container awareness) | 14 |
| V2cs | V2s plus a **generated** `/proc/cgroups` listing `cpuset`, `cpu`, `memory`, … with hierarchy 0, as kernels up to 6.x print it (used for JVM container awareness) | 14 |
| V2x | V2s plus a static symlink `rootfs/proc/self/exe -> <EXE>` (workaround b) | 13 + symlink |

**Details.**

- **`RUN` environment:** commands run as
  `env -i <image Config.Env> HOME=/root /bin/sh -c …`, with `Config.Env` taken
  from `podman image inspect`. An exported rootfs no longer carries the
  image's `ENV`, so it is re-applied the way ranikust applies it for `RUN`. V0
  uses `--env-file` with the same file.
- **`/dev` in the chroot:** `null`, `zero`, `random`, `urandom` and `tty` are
  real devices, placed with `podman --device /dev/X:/s/r-<x>/dev/X`. This
  simulates rootful Docker, where ranikust can `mknod` (experiment 12;
  confirmed in [experiment 13](13-docker-rootful-vm.md)). Experiment 12a used
  regular files instead. The `fd`, `stdin`, `stdout` and `stderr` symlinks
  point at `/proc/self/fd/*`, as in Docker.
- **Limits:** `c32` is unlimited. `c4m3` is `--cpus=4 --memory=3g`; inside the
  container `cpu.max` reads `400000 100000` and `memory.max` reads
  `3221225472`.
- **Measurement:** `stage/bin/runm` (`scripts/runm.c`, static) runs the
  command and samples `/proc` every 100 ms. For V1/V2s, `runm` runs in the
  harness, **outside the chroot**, in the same pid namespace, so it still sees
  the processes inside.
  - Output: exit code, wall time and, per `comm`, distinct pids / maximum
    concurrent / maximum threads of one process.
  - Wall time resolution is 0.1 s. Processes living less than 100 ms can be
    missed, so counts are lower bounds.
  - For Next, `NODE_OPTIONS=--require=/probes/pidlog.js` also logs **every**
    node process start. Those counts are exact.
- **Rounds:** W1, Maven, Gradle and JS ran 3 rounds with the variant order
  rotated each round; medians are reported. The JVM probes and the
  workarounds ran 1 round, as their results are deterministic.
- **Fixtures** (`fixtures/`):
  - `java/mvn`: `maven-archetype-quickstart` layout, one dependency
    (`commons-lang3`), one JUnit 5 test. The test prints `availableProcessors`
    and `maxMemory` of the **surefire fork JVM**. Plugin versions are pinned.
  - `java/gradle`: generated with
    `gradle init --type java-application --dsl kotlin`. Guava, JUnit Jupiter,
    toolchain 21 and the foojay plugin. `gradle.properties` enables the
    configuration cache.
  - `probes/Probe.java`: prints CPU count, heap,
    `ProcessHandle.info().command()` and tries to spawn `sh`.
  - `probes/gprobe.init.gradle`: prints `maxWorkers` and the daemon's CPU
    count.
  - `js/`: `tsc5`, `tsc7` and `esbuild` build 41 TS modules generated by
    `gen-ts-src.sh`; `vite` builds a minimal React app.

## Results

### `next build` of W1 [measured]

The command is `cd /app && pnpm build` (= `next build`). The app type-checks
(`ignoreBuildErrors: false`) and uses `output: 'standalone'`. Data:
`out/w1/results.tsv` and `out/w1/<cfg>/<V>-<rep>.{log,procs,pids,files.tsv.gz,keep/}`.
Medians of 3 rounds, warm caches.

| cfg | V0 | V1 | V2s |
|---|---|---|---|
| c32 | OK, **22.43 s** (20.83–25.14) | **FAIL rc=1, 0.50 s** | OK, **24.64 s** (21.33–25.05) |
| c4m3 | OK, **21.23 s** (21.04–21.83) | **FAIL rc=1, 0.50 s** | OK, **21.12 s** (20.92–21.44) |

- The V1 error is
  `> Build error occurred [Error: ENOENT: no such file or directory, uv_resident_set_memory]`,
  right after `Creating an optimized production build ...`. This is
  `process.memoryUsage()` reading `/proc/self/stat`, the same cause as in
  experiment 12a. `pnpm` itself still runs at V1.
- The c32 run coincided with a host load of 5.1–6.8. Two earlier runs were
  discarded because of the hash-normalisation bug described below, but gave
  timings in the same range: V0 20.5–22.5 s and V2s 20.4–21.4 s at c32; V0
  21.1–25.6 s and V2s 21.0–25.8 s at c4m3.
  - The difference between V0 and V2s is within the noise.
  - W1 is not slower under `--cpus=4`. Most of the time goes into the webpack
    compile (8.6 s, nearly single-threaded) and the type check
    **[inferred]**.

**Processes and threads** (from pidlog and `runm`; identical in all 3 rounds):

| cfg | Variant | node processes (pidlog) | of which `jest-worker/processChild.js` workers | max concurrent | max threads of one node process |
|---|---|---|---|---|---|
| c32 | V0, V2s | 68 | 66 | 64 | 43 |
| c4m3 | V0, V2s | 68 | 66 | 64 | **15** |
| both | V1 | 2 (`pnpm`, `next`) | 0 | 2 | 11 |

- Next derives the worker count from `os.cpus()` and does not read cgroups
  (**[code]**, experiment 12a), so under `--cpus=4` it still starts 66
  workers, at V0 and V2s alike.
- The thread count does follow the cgroup: 43 drops to 15, at V0 and V2s
  alike. These are pools sized by libuv's `availableParallelism()`, which
  reads `/proc/self/cgroup` and `cpu.max` from the fake set **[inferred]**.
- No OOM under 3 GB.

**Is the `.next` output equivalent?** `scripts/nexthash.py` hashes every file
under `.next` except `cache/` and `trace`; the comparison is in
`out/w1/compare.txt`.

- Every successful build produces **2032 files, 70.7 MB**. Most of them are in
  `standalone/`, the traced `node_modules` files.
- **The path lists are identical** across all 12 builds (6 V0, 6 V2s, both
  configurations), after replacing the BUILD_ID in `static/<id>/` with
  `<BID>`.
- 2010–2012 files are **byte-identical**. The other 20–22 differ only in
  per-build random values:
  - `BUILD_ID`, present in `build-manifest.json`, `*.html` and `*.rsc`. In the
    HTML `<!--id-->` comment, Next replaces `-` with `_`, since a comment may
    not contain `--`.
  - `previewModeId`, `previewModeSigningKey`, `previewModeEncryptionKey` in
    `prerender-manifest.json`.
  - The server-actions `encryptionKey` in
    `server/server-reference-manifest.json`.
  - The key order of `server/pages-manifest.json`, which differs between V0
    runs too, depending on which worker finishes first.
- After replacing those values and comparing JSON in canonical form (sorted
  keys), **2032/2032 files are identical** for every pair V0–V0, V0–V2s and
  c32–c4m3.
- V1 leaves only 5 files before it dies.

The 13-file fake `/proc` is **sufficient and correct** for `next build` of the
real app, including `standalone`, type checking and tailwind/postcss. Without
it the build always fails.

### Java: Maven and Gradle at V0, V1 and V2s [measured]

**By JDK.** Earlier, experiment 12a concluded that "Java does not start" from
a single UBI9 image (keycloak). That conclusion is **only partly right**:

| JDK in the rootfs | `java`/`mvn`/`gradle` at V1 and V2s |
|---|---|
| **Official Temurin** (`maven:3-eclipse-temurin-21`, `gradle:8-jdk21`) | **Runs.** The image adds `/etc/ld.so.conf.d/docker-openjdk.conf` and runs `ldconfig` **[code]**, so `libjli.so` is in `ld.so.cache` and `$ORIGIN` is not needed. Maven, `gradle --no-daemon` and Gradle with a daemon all build. Times at c32 equal V0 |
| Debian OpenJDK 21.0.13 (apt) | **Fails**, rc=127: `libjli.so: cannot open shared object file` |
| Alpine OpenJDK 21.0.12 (musl) | **Fails** the same way. `ld-musl` also resolves the main program's `$ORIGIN` through `readlink("/proc/self/exe")`; when that returns `ENOENT`, it skips the RPATH (**[measured]** with strace) |
| UBI9 OpenJDK 21.0.8 (keycloak, experiment 12a) | **Fails** |

**Build times.** Data: `out/java/results.tsv` and
`out/java/<cfg>/<test>-<root>-<V>-<rep>.{log,procs}`. Median of 3 rounds, in
seconds. Caches warm: one V0 run beforehand filled `/root/.m2` and
`/root/.gradle` inside the rootfs.

| Command | Rootfs | cfg | V0 | V1 | V2s |
|---|---|---|---|---|---|
| `mvn -B package` | Temurin (`r-maven`) | c32 | 2.10 | 2.10 | 2.10 |
| | | c4m3 | 2.10 | **2.50** (+19%) | **2.52** |
| | Debian (`r-deb`) | c32, c4m3 | 1.20 / 1.20 | **FAIL 127** `libjli.so` | **FAIL 127** |
| | Alpine (`r-alp`) | c32, c4m3 | 1.30 / 1.30 | **FAIL 127** `Error loading shared library libjli.so` | **FAIL 127** |
| `gradle build --no-daemon` | Temurin (`r-gradle`) | c32 | 3.10 | 3.11 | 3.11 |
| | | c4m3 | 3.10 | **3.60** (+16%) | **3.63** |
| | Alpine | c32, c4m3 | 3.50 / 3.40 | **FAIL 127** | **FAIL 127** |
| `gradle build` (daemon) + `--status` + `--stop` | Temurin | c32 | 3.80 | 3.90 | 3.81 |
| `mvn package` with an empty repository (downloads from Maven Central) | Temurin | c32 | 11.01 | 10.41 | 11.02 ¹ |
| Cold V0 run (fills the cache) | Debian / Alpine mvn; Temurin / Alpine gradle | c32 | 26.34 / 11.72; 18.22 / 33.36 | — | — |

¹ The first V2s attempt got `HTTP Status: 502` from `repo.maven.apache.org`
while downloading `maven-resolver-api`. That is a network error, not a
`/proc` error; the re-run succeeded.

**What the JVM sees at c4m3:**

| Measure | V0 | V1 | V2s |
|---|---|---|---|
| Surefire fork JVM: `availableProcessors` / `maxMemory` | 4 / 768 MiB | **32 / 7620 MiB** | **32 / 7620 MiB** |
| Gradle `maxWorkers` / daemon CPUs (`gprobe`) | 4 / 4 | **32 / 32** | **32 / 32** |
| Max threads of one `java` process: mvn / gradle | 34 / 83 | 64–66 / 177–179 | 64–65 / 175–177 |
| `ProcessHandle.current().info().command()` | `/opt/java/openjdk/bin/java` | `Optional.empty` | `Optional.empty` |

At c32 all three variants give the same numbers: 32 CPUs, 7620 MiB (a quarter
of host RAM), 63–64 threads (mvn) and 173–176 threads (gradle).

**Why Temurin runs and Debian and Alpine do not** **[code]**:

- `bin/java` in all three JDKs has `NEEDED libjli.so` and
  `RPATH [$ORIGIN:$ORIGIN/../lib]` (`readelf -d`).
- Without `/proc/self/exe`, both glibc's `ld.so` and `ld-musl` drop the
  `$ORIGIN` entries and search the default paths. strace
  (`out/java/c32/custom-strace-{deb,alp}-V1-1.trace`) shows:
  - glibc: `readlinkat("/proc/self/exe") = -1 ENOENT`, then
    `/lib/x86_64-linux-gnu/…` and `/usr/lib/x86_64-linux-gnu/…` in turn;
  - musl: `readlink("/proc/self/exe") = -1 ENOENT`, then `/lib/libjli.so`,
    `/usr/local/lib/libjli.so`, `/usr/lib/libjli.so`.
- The `eclipse-temurin` image (and the `maven`/`gradle` images built on it)
  runs
  `echo '/opt/java/openjdk/lib' > /etc/ld.so.conf.d/docker-openjdk.conf; …; ldconfig`
  (**[code]**: `podman image history`). `libjli.so` is therefore in
  `/etc/ld.so.cache`, and `ld.so` finds it without `$ORIGIN`.

A scan of every ELF executable in the rootfs images (`out/origin-scan.txt`)
found that **only `<JDK>/bin/*`** uses `$ORIGIN` for the main program (28–29
files per JDK). `node`, `esbuild`, `tsgo` and rolldown do not. **[measured]**

### Workarounds for `java` [measured] (c32, 1 round)

`J21=/usr/lib/jvm/java-21-openjdk-amd64` (Debian) and
`J21A=/usr/lib/jvm/java-21-openjdk` (Alpine).

| Workaround | Debian glibc (1 JDK) | Alpine musl | Debian with 2 JDKs (21 + 25, default 25) | Side effects observed |
|---|---|---|---|---|
| **(a) `LD_ORIGIN_PATH=$J21/bin`** for every `RUN` | `jprobe` OK; `mvn package` OK at V1 and V2s (1.20 s, same as V0) | **No effect**: still rc=127, musl has no `LD_ORIGIN_PATH` | `java` 25 runs but loads **JDK 21's** `libjli.so` (`LD_DEBUG=libs`). **`javac` 25 dies** with `NoSuchMethodError: getMainType`. At V0 both are correct | glibc only. Holds a **single** directory, so it overrides `$ORIGIN` for **every** main program with an `$ORIGIN` RPATH. **[code]**: glibc uses it only when reading `/proc/self/exe` fails, and ignores it in secure mode |
| **(b) Fake symlink `/proc/self/exe -> $J21/bin/java`** (V2x) | `mvn package` OK (1.20 s) | `mvn package` OK (1.40 s): musl follows the symlink | **`java` 25 and the default `java` silently run JDK 21** (print `21.0.13`) | Wrong for every other process. In `r-node`: `process.execPath` = `/usr/lib/jvm/…/java`; `spawnSync(process.execPath)` → `ENOENT`; **`next build` of W1 dies** with `uncaughtException [Error: spawn /usr/lib/jvm/java-21-openjdk-amd64/bin/java ENOENT]`, while V2s builds normally |
| **(c1) Call `ld.so` directly** (`/lib64/ld-linux-x86-64.so.2 $J21/bin/java`, `/lib/ld-musl-x86_64.so.1 $J21A/bin/java`) | `java -version` and `Probe.java` OK | OK | — | Works only for **that one process**. Wrapping `java` first on `PATH` lets Maven's JVM run and compile, but **the surefire fork `<java.home>/bin/java` → `libjli.so` → BUILD FAILURE**. `JAVACMD` is no help: Debian's `mvn` 3.9.9 overwrites it |
| **(c2) `LD_LIBRARY_PATH=<jdk>/lib`** | `mvn package` OK at V1 and V2s (1.30 s) | **OK** at V1 and V2s (1.30 / 1.40 s) | Same as (a): **`javac` 25 dies** at V2s, correct at V0 | glibc searches `DT_RPATH` before `LD_LIBRARY_PATH`; at V0 the `$ORIGIN` RPATH wins, in the chroot it is dropped and `LD_LIBRARY_PATH` decides for every JDK. The JDK lib directory may shadow system libraries of the same name **[inferred]** |
| **(c3) `JAVA_TOOL_OPTIONS=-XX:ActiveProcessorCount=4 -XX:MaxRAM=3221225472`** (c4m3, Temurin) | — | — | — | Ergonomics back to V0: 4 CPUs, heap 768 MiB, `ParallelGCThreads` 4, including in the surefire fork JVM. `mvn` takes 2.20 s instead of 2.50 s without it. But **every JVM prints `Picked up JAVA_TOOL_OPTIONS: …` on stderr** (2 lines per `mvn` run), the values are fixed, and they override the user's `JAVA_TOOL_OPTIONS` unless merged **[inferred]** |
| **(d) musl** | — | Also needs `/proc/self/exe` (strace above). Usable: (b), (c1), (c2), `/etc/ld-musl-x86_64.path`. Not usable: (a) | — | — |
| **(f) User fixes it in the Dockerfile**: `RUN echo $J21/lib > /etc/ld.so.conf.d/java.conf && ldconfig`, or write `/etc/ld-musl-x86_64.path` | **OK**: `ldconfig` itself runs in the chroot; `mvn` finishes in 1.30 s | **OK** | `ld.so.cache` keeps one path per soname `libjli.so`, so this **also works for only one JDK** **[inferred]**. Temurin images have only one JDK | Changes the image: one extra layer. This is the user's choice, not ranikust's job. Temurin images already do it |

**Who can apply a fix** **[inferred]**:

- Only ranikust could apply (a), (b) or (c2) automatically. All three **go
  silently wrong** when the rootfs has several JDKs (Gradle multi-toolchain
  builds, images with both JDK 17 and 21). (b) is also wrong for every
  non-`java` process: node, Go, Rust.
- No approach is both correct and general: `$ORIGIN` is **per-process**
  information, and a bare chroot has no source for it.

### Does the JVM see the container limits? [measured] (`jprobe`, c4m3)

| JDK | V0 | V1 | V2s (13 files) | V2c (+ snapshot `/proc/cgroups`) | V2cs (+ generated `/proc/cgroups`) |
|---|---|---|---|---|---|
| Temurin 21.0.12.1 | **4 CPUs, heap 768 MiB**, 4 GC threads | 32, 7620 MiB, 23 GC threads | 32, 7620 | 32, 7620 (`controller cpuset/memory is not enabled`) | 32, 7620 (`No relevant cgroup controllers mounted`) |
| Debian 21.0.13, Alpine 21.0.12 | 4, 768 | does not start | does not start | does not start | does not start |
| UBI9 21.0.8 (keycloak; the chroot needs `LD_LIBRARY_PATH`) | **32, 7620 MiB, 23 GC threads** (`One or more required controllers disabled at kernel level`) | — | 32, 7620 | 32, 7620 | **heap 768 MiB, 4 GC threads** (`Detected cgroups v2 unified hierarchy`) |

At c32 every cell is 32 CPUs and 7620 MiB. Data: `out/java/c4m3/jprobe-*`,
`custom-oldjdk2-kc-*`, `custom-newjdk-v2cs-*`,
`out/java/keycloak-jdk-21.0.8.txt`, and
`out/strace/jvm-maven-{V0,V2s}-c4m3.trace`.

**Why experiment 12a saw V0 ignore the limits too:**

- **It depends on the JDK build.** On this kernel 7.0, `/proc/cgroups` lists
  only the controllers usable with v1 (`cpu`, `cpuacct`, `blkio`, `pids`, …);
  the `cpuset` and `memory` lines are gone.
- JDK 21.0.8 detects cgroups through `/proc/cgroups`, sees `cpuset` and
  `memory` missing, and disables container support.
- JDK 21.0.12 (Temurin) and 21.0.13 (Debian) include JDK-8349988 "Change
  cgroup version detection logic to not depend on /proc/cgroups"
  **[external]** (hotspot-dev RFR, see references; which 21.0.x release got
  the backport was not looked up, only that 21.0.8 lacks it and 21.0.12 has
  it). strace at V0 shows they do not open `/proc/cgroups`. They call
  `statfs("/sys/fs/cgroup")`, get `CGROUP2_SUPER_MAGIC`, then read
  `cgroup.controllers`, `/proc/self/{cgroup,mountinfo}`, `cpu.max` and
  `memory.max` **[measured]**.

**Why the fake `/proc` cannot help recent JDKs:**

- In the chroot, `/sys/fs/cgroup` is a plain directory on ext4, so `statfs`
  returns `EXT2_SUPER_MAGIC`. The JDK treats that as "not v2" and switches to
  the v1 branch via `/proc/cgroups`:
  - the snapshot (V2c) lacks `cpuset` and `memory`;
  - the generated file (V2cs) finds no cgroup v1 mount in `mountinfo`.
- **No static file can fake `statfs`.** The only lever is a JVM flag; see
  workaround (c3).

**With older JDKs (21.0.8 and earlier):**

- A generated `/proc/cgroups` (V2cs) makes the JVM **detect** the limits,
  whereas V0 on this kernel does not. V2cs is therefore more correct than V0,
  which contradicts the fake `/proc` goal of "same as V0" and fabricates kernel
  data.
- On kernels up to 6.x, the real `/proc/cgroups` has `cpuset` and `memory`
  with hierarchy 0, so the **snapshot** (V2c) gives old JDKs exactly the V0
  result **[inferred]** (later measured on kernel 5.15 in
  [experiment 16](16-cgroup-v1-cpuset.md)).
- Adding a snapshot of `/proc/cgroups` is therefore cheap, matches V0 on
  every kernel, and is harmless for recent JDKs.

### Where Gradle needs `/proc` besides `java` [measured] (Temurin, c32)

`strace -f -e trace=%file` ran on both sides: inside V0, and in the harness
for V1. Files: `out/java/c32/{gradle,gradled,mvn}-strace-*-V{0,1}-1.trace`;
per-path summary in `out/strace-summary.txt`.

| Path | Read by | V0 | V1 |
|---|---|---|---|
| `/proc/self/{cgroup,mountinfo}`, `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max}`, `statfs /sys/fs/cgroup`, `/proc/stat`, `/sys/devices/system/cpu/{online,possible}`, `/proc/self/exe` | JVM (container support, CPU count) | OK | `ENOENT`, tolerated |
| `/proc/<pid>/stat` of **another process** (client and daemon) | Gradle (tracking the client/daemon process **[inferred]**) | OK | `ENOENT`, tolerated |
| `/proc/meminfo`, `/sys/fs/cgroup/memory.current` (then v1 `memory/memory.usage_in_bytes`) | Gradle (daemon memory monitor) **[inferred]** | OK | `ENOENT`, tolerated |
| `/proc/net/if_inet6` | JDK `NetworkInterface` (daemon and client talk over loopback) | OK | `ENOENT`, tolerated |
| `/proc/self/fd`, `/proc/self/map_files` | JDK (closing fds on spawn) and the daemon JVM | OK | `ENOENT`, tolerated |
| `/proc/mounts` | native-platform listing file systems for file watching **[inferred]** | — | `ENOENT`, tolerated |

- **`gradle build --no-daemon`** runs at V1 and V2s. Gradle still forks a
  single-use daemon ("To honour the JVM settings…") and a test worker: three
  `java` processes in total.
- **A normal daemon** (`gradle build`, then `--status` and `--stop`) also runs
  at V1 and V2s.
  - `--status` lists the daemon of the previous V0 run too, because the
    registry is in the shared rootfs's `/root/.gradle`. Gradle checks daemons
    through a socket, not through `/proc`.
  - native-platform loads normally.
  - No warnings in the log.
- **The only real consequence** measured is ergonomics: `maxWorkers` 32
  instead of 4 under `--cpus=4`.
- A daemon started by a `RUN` keeps running after the `RUN` exits. This is
  specific to running without a container per `RUN` (see experiment 12a).
  ranikust must kill the whole process tree, or at least wait for it to exit,
  before removing the fake `/proc` and taking the snapshot **[inferred]**.
- Under strace, V1 is much slower than V0: Maven 37.2 s against 2.0 s, Gradle
  11.4 s against 4.4 s, with the same number of syscalls. Without strace the
  times are equal. This is an artefact of running strace from the harness; it
  was not investigated further here (explained in
  [experiment 15](15-java-next-concurrency.md)).

### `tsc`, `esbuild`, `vite` [measured] (3 rounds, on `r-node`)

| Command | cfg | V0 | V1 | V2s | Output (sha256 of `dist/`) |
|---|---|---|---|---|---|
| `npx tsc -p .`, TypeScript 5.9.3 (JS) | c32, c4m3 | OK 0.5 s | OK 0.5 s | OK 0.5 s | `9047aa46…`, 82 files, identical in every variant |
| `npx tsc -p .`, **TypeScript 7.0.2** (Go, static; the node shim `execve`s the binary) | c32, c4m3 | OK 0.2 s | **FAIL rc=2**: `panic: vfs: failed to get executable path: readlink /proc/self/exe` (in `osvfs.init`) | **FAIL rc=2** | V0 gives `9047aa46…`, same as tsc 5 |
| | c32, V2x (`exe -> …/typescript-linux-x64/lib/tsc`) | | | **OK**; `tsc --version` = 7.0.2 | `9047aa46…` |
| `npx esbuild --bundle --minify`, esbuild 0.28.2 (Go) | c32, c4m3 | OK 0.1 s | OK 0.1–0.2 s | OK 0.1–0.2 s | `28b023f8…` in every variant |
| `npx vite build`, vite 8.3.5 (rolldown, Rust napi addon) + react 19.3.1 | c32 | OK 0.3 s, 68 threads | OK, 93 threads | OK, 69 threads | `08ebd0a7…` in every variant |
| | c4m3 | OK, **42** threads | OK, **93–94** threads | OK, **41–42** threads | as above |

- **TypeScript 7 is a new hard case.** `tsgo` calls `os.Executable()` during
  package initialisation to locate the `lib/*.d.ts` directory next to the
  binary, so even `tsc --version` panics. TypeScript 7 is the `latest` tag on
  npm, so any project that upgrades breaks on the parallel path. The fake
  `/proc` does not help. Only an `exe` symlink pointing at the right binary
  does, and the workaround table shows that such a symlink is wrong for other
  processes.
- esbuild does not call `os.Executable` on this path. Its threads could not be
  counted: the process lives less than 100 ms.
- vite (rolldown) sizes its threads from the cgroup. V1 ignores the quota; V2s
  restores it. The output does not change.

## Findings

1. Without `/proc`, `next build` of W1 always fails after 0.50 s with
   `uv_resident_set_memory`; with the 13-file fake `/proc` it matches V0 in
   time, in 68/66 processes/workers, in 43/15 threads and in 2032/2032 files
   (`next build` of W1).
2. Next sizes its workers from `os.cpus()` and starts 66 under `--cpus=4`, at
   V0 as well (`next build` of W1, processes and threads).
3. Whether Java starts under bare `chroot` depends on whether `libjli.so` is in
   `ld.so.cache`: Temurin images run, Debian, Alpine and UBI9 JDKs fail with
   rc=127 (Java, by JDK).
4. Running JVMs ignore `--cpus`/`--memory` at V1 and V2s (32 CPUs, 7620 MiB,
   `maxWorkers` 32), making `mvn package` 19% and `gradle build` 16% slower
   under `--cpus=4` (Java build times; what the JVM sees).
5. JDK 21.0.12 and later detect cgroup v2 with `statfs`, which no static file
   can fake; older JDKs read `/proc/cgroups`, which a snapshot can reproduce
   (JVM container limits).
6. Every automatic `libjli.so` workaround breaks with two JDKs or breaks other
   processes; only a user-side `ldconfig` (or `ld-musl-*.path`) is clean
   (workarounds table).
7. `JAVA_TOOL_OPTIONS` with `ActiveProcessorCount` and `MaxRAM` restores V0
   ergonomics but adds a `Picked up` line to every JVM's stderr
   (workaround c3).
8. Gradle's other `/proc` reads fail with `ENOENT` and are tolerated; the only
   measured consequence is `maxWorkers` (Gradle table).
9. tsc 5, esbuild and vite produce byte-identical output at V1 and V2s; vite's
   thread count is only right with the fake `/proc`; TypeScript 7 panics
   without `/proc/self/exe` (JS tools table).

## Implications for ranikust

Kaniko mode is sequential by default and `--parallel-stages` is an opt-in
([ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)); the points
below concern only the opt-in path. They are the recommendations recorded at
the time **[inferred]**, based on the results above; see
[kaniko mode](../../design/kaniko-mode.md) for the current design.

1. **Enable the 13-file fake `/proc` by default under `--parallel-stages`**,
   as experiment 12a proposed. The real W1 app confirms it: without it
   `next build` dies, with it the output is identical to V0.
2. **Add a snapshot of `/proc/cgroups` (14 files in total).**
   - Cost: one `cat`.
   - On kernels up to 6.x, JDKs up to 21.0.8 then match V0. With recent JDKs
     and on kernel 7.0 it has no effect and does no harm.
   - **Do not generate** `/proc/cgroups`: that fabricates data and differs
     from V0.
   - [Experiment 16](16-cgroup-v1-cpuset.md) later replaced the fixed v2 file
     list with a per-cgroup-version set of 14 files.
3. **Do not** fake `/proc/self/exe` automatically, and do not set
   `LD_ORIGIN_PATH`/`LD_LIBRARY_PATH`. All three have been shown to go
   silently wrong: wrong JDK loaded, `javac` dying, `next build` dying on
   `execPath`.
4. **Do not** inject `JAVA_TOOL_OPTIONS` automatically: it pollutes every
   JVM's stderr and overrides the user's value; document instead how users
   can set `ENV JAVA_TOOL_OPTIONS=…` or
   `-XX:ActiveProcessorCount`/`-XX:MaxRAMPercentage` in the Dockerfile.
   **Superseded:** [experiment 15](15-java-next-concurrency.md) showed that
   Java builds under tight memory limits fail deterministically without the
   flags, and [ADR-0019](../../adr/0019-java-tool-options-on-parallel-path.md)
   adopts conditional injection of `JAVA_TOOL_OPTIONS` on the parallel path
   under cgroup limits.
5. **Classify tool families** for user documentation and warnings:

   | Tool family | Parallel path (`chroot` + fake `/proc`) | Notes |
   |---|---|---|
   | Node: next (real W1), webpack/terser, jest, tsc 5, esbuild, vite/rolldown, pnpm/npm | **Safe** | Workers, threads and output match V0 (experiment 12a and this experiment) |
   | Java on images with `libjli` in `ld.so.cache` (`eclipse-temurin`, `maven:*-eclipse-temurin-*`, `gradle:*-jdk*`) | **Runs, but the JVM ignores `--cpus`/`--memory`** | +16–19% under `--cpus=4`; heap = a quarter of host RAM. May be OOM-killed in large builds under `--memory` **[inferred]**. A second JDK downloaded by a toolchain loads Temurin's `libjli` (see limitations) |
   | Java on JDKs installed with apt, apk or dnf (Debian/Ubuntu `openjdk-*`, Alpine `openjdk*`, UBI/RHEL), glibc and musl alike | **Needs sequential**, or the user adds `ldconfig`/`ld-musl-*.path` (workaround f) | Does not start: rc=127 |
   | TypeScript 7 (`tsgo`) and every Go/Rust binary calling `os.Executable`/`current_exe` at start-up **[inferred]** | **Needs sequential** | panic |
   | Scripts using `<(…)`, `/dev/stdin`, `/dev/fd`, `ps`, `/proc/<pid>` | **Needs sequential** | Experiment 12a |

6. **Early detection** (cheap, not exhaustive):
   - While building a stage's rootfs, if `*/bin/java` has an `$ORIGIN` RPATH
     and `libjli.so` is not in `ld.so.cache`, log a warning "stage X: java
     will not start under `--parallel-stages`". This catches Debian and
     Alpine; Temurin passes.
   - After a `RUN` fails, if stderr contains `libjli.so`,
     `readlink /proc/self/exe`, `uv_resident_set_memory` or `/dev/fd/`, print
     a hint to run without `--parallel-stages`. Do not retry automatically
     (experiment 12a).
7. The walk always skips `/proc`, `/sys` and `/dev`. After each `RUN`, wait
   for the whole process tree to exit (the Gradle daemon).

## Limitations and open questions

- **Real rootful Docker, Kubernetes pods, cgroup v1, LXCFS.** Here `/dev` was
  bound with `podman --device` to simulate rootful Docker's `mknod`. A JVM with
  `/dev/urandom` as an empty file (the reality on rootless podman) was not
  tried.
- **Real-size Java builds**: many modules, many tests, tight `--memory`. An
  OOM kill caused by heap = a quarter of host RAM at V1/V2s was not measured,
  nor the cost of Gradle running 32 workers on 4 CPUs in a build with many
  parallel tasks.
- **Gradle/Maven toolchains downloading another JDK** (foojay plugin,
  `~/.gradle/jdks`) on a Temurin image. In the chroot, the downloaded JDK's
  `java` will take `libjli.so` from `ld.so.cache`, that is Temurin's, and hit
  the same JDK-mixing error as (a)/(c2) **[inferred]**. The fixture here uses
  toolchain 21, the same JDK as the running one, so nothing was downloaded.
- Other JDKs: Corretto, Zulu, Liberica, Microsoft, GraalVM, official JDK
  17/25. It is unknown which images add `ld.so.conf.d` as Temurin does. sbt,
  the Kotlin daemon, Bazel (Bazel uses `/proc/self/exe` heavily
  **[inferred]**).
- A Gradle daemon still alive after a `RUN` in the chroot (life cycle,
  snapshot), and file watching by a long-lived daemon.
- `next build --turbopack`, Next 16, and Next's type check in a project using
  TypeScript 7.
- Several stages running in parallel, each with its own fake `/proc`.
- `go build`, `cargo build`, pip/maturin.
- Why strace from the harness makes V1 5–18 times slower (Gradle section).
- A Rust implementation of `fake_up`/`fake_down`. Here it is still bash, 17–29
  ms per `RUN` (column `fake_ms`).

[Experiment 15](15-java-next-concurrency.md) followed up on large Java builds
under tight memory, toolchain JDKs, Next 16 and Turbopack, concurrent stages
with fake `/proc` and the strace slowdown;
[experiment 16](16-cgroup-v1-cpuset.md) on rootful Docker and cgroup v1.

## Reproduction

PoC sources are archived outside this repository under
`docs-rust/poc/14-next-java-proc/`. Everything ran on the host as an
unprivileged user under rootless podman (E-dock); no VM.

| File | Content |
|---|---|
| `scripts/lib.sh` | `pdock` (E-dock); uses experiment 12's `compat/tools/{moby-seccomp.json,chrun}`, read-only |
| `scripts/prep.sh [tools node w1 js mvn gradle deb deb2 alp kc]` | Builds `stage/`: `bin/runm`, `bundle/` (strace and readelf with their glibc, run through `ld.so`), the `r-*` rootfs and `env-*` files. Generates `fixtures/java/gradle` if missing |
| `scripts/runm.c` | Measures wall time, counts processes and threads through `/proc`, writes `.log`/`.procs` |
| `scripts/fakeproc.sh` | `fake_up <root> 13\|13cg\|13cgs\|exe [target]`, `fake_down` |
| `scripts/inner.sh`, `scripts/run.sh` | One `RUN` per variant: `run1 <cfg> <V> <root> <out>`, with variables `SETUP`, `CMD`, `ENVADD` (`'A=b\|C=d e'`), `EXE`, `STRACE`, `DEV` |
| `scripts/w1.sh <cfg> <reps>`, `scripts/nexthash.py` | `next build` of W1 → `out/w1/`, `out/w1/compare.txt` |
| `scripts/java.sh <cfg> <reps> <test> <root> [V…]` | jprobe, mvn, mvncold, gradle, gradled, custom |
| `scripts/java-matrix.sh`, `scripts/mitig.sh`, `scripts/followup.sh` | Java, workarounds, JVM limits, Gradle → `out/java/` |
| `scripts/js.sh <cfg> <reps>`, `scripts/tsc7-exe.sh` | JS tools → `out/js/` |
| `scripts/stracesum.py`, `scripts/report.py` | `out/strace-summary.txt`, `out/summary.txt` |
| `out/00-prep*.log` … `05-followup-js.log` | Per-step logs, with host load |

Re-run (about 25 minutes; `stage/` takes about 6.1 GB):

```sh
bash scripts/prep.sh
bash scripts/w1.sh c32 3 && bash scripts/w1.sh c4m3 3
for c in c32 c4m3; do for r in maven deb alp; do bash scripts/java.sh $c 1 jprobe $r V0 V1 V2s V2c V2cs; done; done
bash scripts/java-matrix.sh && bash scripts/mitig.sh && bash scripts/followup.sh
bash scripts/js.sh c32 3 && bash scripts/js.sh c4m3 3 && bash scripts/tsc7-exe.sh
python3 scripts/report.py
```

Clean-up after the experiment:

- `stage/` (6.1 GB, above the 500 MB threshold) **was deleted** with
  `podman unshare rm -rf stage`; `prep.sh` rebuilds it.
- Images pulled for this experiment **were deleted**:
  `docker.io/library/maven:3-eclipse-temurin-21` (580 MB) and
  `docker.io/library/gradle:8-jdk21` (807 MB). `prep.sh` pulls them again
  when needed.
- The Debian, two-JDK Debian and Alpine rootfs were built in temporary
  `rk14-build-*` containers and exported. No image was created, and the
  containers were deleted.
- Images already on the machine were only read, never deleted:
  `node:22-bookworm-slim`, `debian:latest`, `alpine:latest`,
  `rust:1-bookworm` (used to build `runm`), `keycloak:26.4.1-0`.
- All containers were named `rk14-*` and ran with `--rm`.

## References

1. [Experiment 12a: Fake /proc](12a-fake-proc.md): the 13-file set, `next
   build` and JVM under bare `chroot`, `/dev/fd`, risks of per-`RUN`
   processes outliving the `RUN`.
2. [Experiment 12: Parallel stages in kaniko mode](12-kaniko-mode-parallel.md):
   E-dock, `chrun`, `/dev` under rootful Docker.
3. [Experiment 13: Rootful Docker in a VM](13-docker-rootful-vm.md).
4. [Experiment 15: Java, Next.js and concurrency](15-java-next-concurrency.md).
5. [Experiment 16: cgroup v1 and cpuset](16-cgroup-v1-cpuset.md).
6. [Survey chapter 05: Decisions](../survey/05-decisions.md), Q12, and
   [survey chapter 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md),
   parallel stages in kaniko mode.
7. **[external]** JDK-8349988, "Change cgroup version detection logic to not
   depend on /proc/cgroups", hotspot-dev review thread,
   https://mail.openjdk.org/pipermail/hotspot-dev/2025-February/102065.html
   (accessed 2026-10-29).
8. **[code]** `eclipse-temurin` image history (`podman image history`):
   `/etc/ld.so.conf.d/docker-openjdk.conf` and `ldconfig`; `readelf -d` of
   `<JDK>/bin/java`; glibc `LD_ORIGIN_PATH` handling. The original gives no
   commit or line references.
