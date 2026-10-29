# Experiment 12: Parallel stages in kaniko mode

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | In kaniko mode (root, but unable to mount or create a user namespace, as in a default Docker container or an ordinary Kubernetes pod), can each stage get its own rootfs directory inside the workdir so that stages run in parallel, and what does that gain and cost? |
| **Answer** | Yes, with no mount at all and an image identical to the sequential build: on W3, bare `chroot` with parallel stages took 7.99 s against 14.04 s sequential at 32 CPUs (−41 to −44%) and 13.74 s against 17.14 s at `--cpus=4` (−21%). The price is a `RUN` without `/proc` (and without `/dev` under rootless podman) unless proot is used, at +26–56% wall time. |
| **Related** | [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md), [ADR-0018](../../adr/0018-no-proot-in-1-0.md), [ADR-0013](../../adr/0013-jobs-default-available-parallelism.md), [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md); [kaniko mode](../../design/kaniko-mode.md), [scheduler](../../design/scheduler.md); experiments [02](02-run-sandbox.md), [07](07-kaniko-vs-overlay.md), [08](08-parallel-stages-lazy-export.md), [12a](12a-fake-proc.md), [13](13-docker-rootful-vm.md), [14](14-real-builds-without-proc.md) |
| **Original** | `docs-rust/poc/12-kaniko-mode-parallel/` (`README.md`, `compat/README.md`, `perf/README.md`, `perf/results/tables.md`; Vietnamese, not in this repository) |

## Summary

- The experiment had two parts that ran at the same time on the same machine.
  The **compatibility part** probed which mechanisms survive in an emulated
  default Docker container, tried every available way to give each stage its
  own root, and tested package managers, rootfs preparation and isolation. The
  **performance part** built W1 and W3 with an extended `ovbuild` that has
  `-jobs` and three sandboxes, 5 rounds per configuration.
- Bare `chroot` into one directory per stage works with no mount and needs only
  `CAP_SYS_CHROOT`. W3 got 41–44% faster at 32 CPUs and 21% faster at 4 CPUs,
  more than parallel stages gained in overlay mode
  ([experiment 08](08-parallel-stages-lazy-export.md)). W1, which is mostly
  one chain, gained only 0.3–1.0 s. With lazy export, the parallel walk
  (5.38 s) beat parallel overlay lazy (6.29 s). Every image matched the
  sequential build. **[measured]**
- The cost is in the sandbox. `RUN` has no `/proc`: some commands fail, others
  silently return wrong results (an empty `ps`, `os.cpus().length == 0`). There
  is no real `/dev` under rootless podman, although `mknod` works under rootful
  containers. A malicious `RUN` can escape `chroot` into a sibling stage.
  proot restores `/proc` and `/dev` but costs +26–56% wall time, hangs apt
  unless `APT::Sandbox::User=root` is set, and is a third-party GPL-2.0 binary.
  **[measured]**
- A per-stage rootfs must be a real copy: tar extract or `cp -a`, 0.05–0.17 s
  per stage. A hardlink farm (`cp -al`) is 3–6× faster, but one `echo >>` in a
  stage corrupted the store and the sibling stage. On xfs and btrfs, plain
  `cp -a` reflinks and costs 3–12 MB of disk per stage instead of 231 MB.
  **[measured]**
- Memory, including page cache, is the binding constraint: parallel W3 needs
  about 8.2 GB and hit the 8 GB cgroup limit in 46 of 60 runs. **[measured]**
- Outcome: kaniko mode stays sequential on `/` by default, and
  `--parallel-stages` opts in to one directory per stage with bare `chroot`
  ([ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)).
  Per-stage rootfs is copied, never hardlinked
  ([ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md)), and
  proot is not part of 1.0 ([ADR-0018](../../adr/0018-no-proot-in-1-0.md)).

## Questions

Until this experiment, the design ran kaniko mode sequentially, because the
workdir is `/` and there is only one rootfs
([survey 06](../survey/06-overlay-and-parallelism.md) §5). The experiment asked:

1. Which building blocks are still available inside a default Docker container
   (Docker's 14 default capabilities plus its seccomp profile), where `mount`,
   `unshare` and `pivot_root` are not?
2. Which way of giving each stage its own root works there: bare `chroot`,
   path translation with proot, a seccomp user-notify supervisor, `LD_PRELOAD`
   fakechroot, a hybrid with one stage on the real `/`, or linking `/proc` and
   `/dev` into the rootfs?
3. Which common `RUN` commands (package managers, user management, process and
   CPU queries) work under each approach, as root and as `USER nobody`?
4. How should a per-stage rootfs be prepared (tar extract, `cp -a`, hardlink
   farm, reflink), and what does each strategy cost in time, disk and safety?
5. How well are parallel stages isolated from each other, against accidental
   writes and against a malicious `RUN`?
6. How much does running stages in parallel gain over the sequential walk on
   W1 and W3, compared with optimized kaniko and with overlay mode?
7. What does a per-stage rootfs cost in time, disk and memory, and how much
   does proot cost on real builds?
8. Is the default `--jobs` (the number of cores) still right in this mode?
9. Is the image identical to the one built sequentially?

## Environment

| | |
|---|---|
| Host | AMD Ryzen 9 9950X3D (16 cores / 32 threads), 29–30 GB RAM, NVMe with ext4 `/home`, Linux 7.0.0-34, cgroup v2; shared development machine |
| Runtime | podman 5.7.0 rootless, no sudo. The real-root steps ran once through `pkexec` (`rootful.sh`, 2026-10-29 19:00–19:01, root in the initial user namespace) |
| Privilege model under test | "E-dock": Docker's default container emulated in rootless podman (below) |
| Seccomp profile | moby `default.json` from `https://raw.githubusercontent.com/moby/profiles/main/seccomp/default.json` (accessed 2026-10-29), sha256 `6416b47770785a41ac59073cdc77d9fe98517df2799dc83ef207e622de3053f6`, `defaultAction=SCMP_ACT_ERRNO` |
| Registry and npm | `nk-registry` and `nk-verdaccio` from the `nextjs-kaniko` compose project; verdaccio cache warm |
| Filesystems | Workdir on ext4 `/home` (performance part: podman volume `k12p-work`); tmpfs for part of the compatibility cells; xfs and btrfs loop images for the reflink test |
| Workloads | W1 = `nextjs-kaniko/app/Dockerfile` (`deps` → `final` is one chain; only `assets` is independent). W3 = `nextjs-kaniko/app-par` (three independent `pnpm install` stages plus a small `final`) |

**E-dock.** Both parts emulated Docker's default container as:

```sh
podman run --cap-drop=ALL \
  --cap-add=CHOWN,DAC_OVERRIDE,FSETID,FOWNER,MKNOD,NET_RAW,SETGID,SETUID,SETFCAP,SETPCAP,NET_BIND_SERVICE,SYS_CHROOT,KILL,AUDIT_WRITE \
  --security-opt seccomp=moby-seccomp.json …
```

Inside the container: `CapEff=00000000a80425fb` (exactly Docker's 14
capabilities), `Seccomp: 2` (filter mode), `Seccomp_filters: 1`; `mount`,
`unshare(CLONE_NEWNS)`, `unshare(CLONE_NEWUSER)` and `mknod c 1 3` all return
`EPERM`; `chroot` and `ptrace` work. **[measured]** The compatibility part used
`--network=host` plus `--add-host` entries for `verdaccio` and `registry`; the
performance part used the network `nextjs-kaniko_internal`.

**Where rootless E-dock differs from rootful Docker.**

- `mknod` of a character device returns `EPERM` even with `CAP_MKNOD`,
  because a rootless container runs in a user namespace other than the initial
  one. Rootful Docker allows it; this was confirmed later with rootful podman
  (see [Capability probe](#capability-probe)).
- `unshare(CLONE_NEWUSER)` is blocked by the moby seccomp profile (it requires
  `CAP_SYS_ADMIN`), while podman's own default profile allows it. Docker's
  seccomp profile, not podman, is what pushes this environment into kaniko
  mode. **[measured]**
- In the performance part, the `bind` and overlay configurations ran in
  `--privileged`, while `nocap` and `proot` ran in E-dock.

**Timing and load.**

- Compatibility part: 2026-10-29 15:40–18:10, load 0.1–5.0. Its timing
  measurements (proot overhead, rootfs preparation) ran before the performance
  part started: proot overhead at load about 1.6–2.0, rootfs preparation at
  about 1.5–3.0. Its compatibility matrix was re-run at 17:54, after the
  performance part had finished (load 0.2–1.9).
- Performance part: 15:42–17:48. Sets in order: `w3-cpu32` (15:53–16:19),
  `w3-cpu4` (16:29–16:59), `w1-cpu32` (16:59–17:17), `w1-cpu4` (17:17–17:37),
  then `w3-jobs-cpu32` (17:37–17:42) and `w3-mem16-cpu32` (17:42–17:48).
- The two parts shared the machine. The compatibility part's "heavy work done"
  marker existed from 15:48, before the performance matrix began at 15:53, but
  at that point the compatibility matrix (container `rk12-matrix`, apt and pnpm
  in tmpfs) was still running. The wait-for-idle step before each run was the
  only protection; no round deviated abnormally.
- 1-minute loadavg before each performance run: median 2.5–3.4 and maximum
  5.6–9.6 per set, mostly the tail of the previous build (loadavg decays
  slowly). The runs with the highest load were not timing outliers. At the end
  the machine had 8.9 GB free and 20.8 GB available of 30 GB.

**Images and tools.**

- Base rootfs tarballs (alpine, debian, node) from the
  [root re-run](06a-root-rerun.md) stage directory; compatibility harness image
  `debian:latest`.
- proot v5.5.0, static, 1.8 MB, from
  `https://github.com/proot-me/proot/releases/download/v5.5.0/proot`, sha256
  `db185300a9daabbce86aca7632d8c7fdc4249f6f153171954013b70c5f491ec0`
  (matches the official `SHA256SUMS`).
- `ovbuild` built with Go 1.26.9 in the golang image `5d8d6618be1c`.
- Optimized kaniko: `localhost/kaniko-bench:stream` (41 flags plus
  `STREAM_TAR_COMPRESS`), as in experiments [07](07-kaniko-vs-overlay.md) and
  [08](08-parallel-stages-lazy-export.md).
- Reflink test: xfs and btrfs images created with `mkfs` inside a debian
  container, because the host has no `mkfs.xfs` or `mkfs.btrfs` (xfsprogs
  6.13.0, btrfs-progs 6.14); ext4 through the host's `mkfs.ext4`; host
  coreutils 9.7.

## Method

### Compatibility part

Seven sub-experiments, each with its own script:

1. **Capability probe.** `probe.c`, a static binary; each test runs in a child
   process so that side effects do not leak. It ran in E-dock, in podman with
   its default profile, as an unprivileged host user, and (with real root) in
   rootful podman E-dock.
2. **Approach A, bare `chroot`.** alpine, debian and node were extracted into
   separate rootfs directories. `/etc/hosts` and `resolv.conf` were copied in
   (and must later be excluded from snapshots). There is no `/proc` and no
   `/sys`; `/dev` is created with `mknod` where possible, otherwise `/dev/null`
   falls back to a regular file. Commands ran through `chrun.c` (`chroot` +
   `setuid` + `exec`) as root and as `USER nobody`. Each matrix cell ran on a
   fresh `cp -a` copy of the rootfs with its own timeout (300 s; apt 120 s).
   Times come from a single run with a warm registry and verdaccio cache and
   are **not benchmarks**.
3. **Approach C, path translation with proot.** The static proot v5.5.0 as
   `proot -r <rootfs> -b /proc -b /dev -b /sys -b /etc/resolv.conf`, in the
   same matrix as approach A, plus a micro-benchmark of its overhead with and
   without seccomp acceleration (`PROOT_NO_SECCOMP=1`). A separate script,
   `dbg-apt.sh`, chased the apt hang.
4. **Isolation.** Two alpine stages, `store-A` and `store-B`;
   `store-B/secret.txt` is the secret. `escape.c` (the `chroot` escape from
   [experiment 02](02-run-sandbox.md): `chroot(sub)`, `chdir("..")` N times,
   `chroot(".")`) ran inside stage A.
5. **Rootfs preparation.** Four strategies: (a) tar extract, (b) `cp -a` from
   a store, (c) `cp -al` hardlink farm, (d) `cp --reflink=always`, on ext4
   `/home` and on tmpfs, for debian (126 MB, 5449 entries) and node (228 MB,
   7293 entries), with a warm page cache. Then a corruption demo with two
   hardlinked stages, and an inode test on a `cp -al` copy (same inode after
   the command means an in-place write) with `strace` of `openat` and
   `rename`.
6. **Other ideas.** Feasibility checks only, not built into tools: the
   seccomp user-notify building blocks, fakechroot, the hybrid, a `/proc`
   symlink, `mknod` under rootful Docker.
7. **Real root** (`rootful.sh`, through `pkexec`): `mknod` of character, block
   and FIFO nodes as real root; the probe in rootful podman E-dock; reflink
   (`FICLONE`) on xfs and btrfs loop images and on ext4, with rootfs
   preparation timed for the node rootfs (223 MB, 7293 entries), warm page
   cache, one measurement. Disk use was measured with `df` before and after,
   because `du` counts reflinked files at full size. The first run had no
   xfs/btrfs images, so the reflink part was skipped and the ext4 line was
   wrong (`out/rootful-reflink-1-noimg.txt`); the script was fixed and only
   the filesystem part was re-run.

### Performance part

`ovbuild`, the Go prototype builder from
[experiment 07](07-kaniko-vs-overlay.md), was copied and extended; experiments
07 and 08 were left untouched. Overlay mode is unchanged. `walk` mode
simulates ranikust's kaniko mode: a plain rootfs, and after each command a
`syncfs`, a walk with parallel `lstat` and HighwayHash plus a stat cache, and
tar + pgzip immediately after the command. The extension added:

| Flag | Meaning |
|---|---|
| `-jobs N` | Stages run along the DAG as in overlay mode: one goroutine per stage, waiting for its parent stage (`FROM <stage>`) and for the source stages of `COPY --from`. `N` is a semaphore around each **unit that touches a rootfs**: rootfs preparation (base extract + baseline scan), and each `RUN`/`COPY` **including** the `syncfs` + walk/hash + tar/gzip right after it (the next command will modify the rootfs, so the snapshot must finish first). Default `N` = GOMAXPROCS (32, or 4 under `--cpus=4`). **`-jobs 1` is therefore equivalent to the sequential walk of experiments 07 and 08** (checked: W3 `-jobs 1` 13.92 s, `-seq` (the old loop) 14.09 s, experiment 08 `walk` 14.32 s; W1 `-jobs 1` 11.22 s, experiment 07 `walk` 11.29 s) |
| `-eager` (on) | A stage whose base is an image prepares its rootfs immediately, without waiting for its `COPY --from` sources (its rootfs is private, so nothing conflicts). In W3, `final` extracts its base at the same time as `a`, `b` and `c` |
| (reuse rule) | `FROM <stage>` reuses the parent's rootfs (like kaniko's "squash", experiment 07) **only if** the parent has exactly one child and is not a `COPY --from` source; otherwise the parent rootfs is copied with `cp -a` to a new directory. W1 (`FROM deps AS final`) takes the reuse path; neither W1 nor W3 needs a copy |
| `-sandbox bind` | Unchanged: `chroot` plus bind mounts of `/proc`, `/dev` (rbind), `/etc/hosts` and `/etc/resolv.conf`. Needs `CAP_SYS_ADMIN`; runs in `podman --privileged` |
| `-sandbox nocap` | **No mounts at all**, only `chroot` (`CAP_SYS_CHROOT` is in Docker's default set). Before each `RUN`, the container's `/etc/hosts` and `/etc/resolv.conf` contents are copied into the rootfs (a symlink is temporarily replaced by a file); after `RUN`, the originals are restored (content, mode, owner, mtime; a symlink keeps its target) **together with the mtime of `/etc`** (see [Bugs found](#bugs-found)). The walk already skips these two paths. At rootfs preparation it tries `mknod` for `null zero full random urandom tty` and skips them on failure (`-null-file` creates `/dev/null` as an empty regular file instead). No `/proc` |
| `-sandbox proot` | `RUN` goes through `proot -r <rootfs> -b /proc -b /dev -b /etc/hosts -b /etc/resolv.conf -w <workdir> /bin/sh -c …`, using the static proot v5.5.0 from the compatibility part, mounted read-only at `/tools`. proot's default mode (seccomp acceleration on) |
| `-prep copy` | Extract the base **once** into `work/base-<digest>`, then `cp -a` it into each stage's rootfs. The default, `extract`, extracts the base separately for each stage (as in experiments 07 and 08) |
| `-syncfs=false` | Skip `syncfs` before the walk (a "cheap improvement" variant) |
| `-export final` | (from experiment 08, `walk-lazy`) Stages that are not part of the final image get no baseline scan, no snapshot and no export |
| `-seq` | The old sequential loop of experiment 07, for cross-checking |
| `-logs DIR` | `RUN` logs are written outside the workdir so that they survive its deletion |

`ovbuild.json` also records per-stage rootfs preparation time and semaphore
wait, the **peak space used on the workdir filesystem** (`statfs` every
100 ms, peak minus start; on ext4 delalloc blocks not yet written are
subtracted too), the cgroup's `memory.peak`, and the `/dev` status for
`nocap`.

Configurations:

| Name | What | Container |
|---|---|---|
| `k-stream` | Optimized kaniko, exactly as in experiments 07 and 08 (`kaniko-bench:stream`, 41 flags + `STREAM_TAR_COMPRESS`) | As in experiment 07's harness |
| `ov-j0` | `ovbuild -mode overlay -export final -jobs 0` (parallel overlay lazy, experiment 08) | `--privileged` |
| `wb-jN` | `-mode walk -export all -jobs N -sandbox bind` | `--privileged` |
| `wn-jN` | `-mode walk -export all -jobs N -sandbox nocap` | **E-dock** |
| `wp-jN` | `-mode walk -export all -jobs N -sandbox proot` | **E-dock** |
| Suffixes `-lazy`, `-nosync`, `-copy`, `-nullf` | Add `-export final`, `-syncfs=false`, `-prep copy`, `-null-file` | |

`N = 0` is the default (the number of cores: 32, or 4 under `--cpus=4`).
"Export all layers" (`-export all`) is kaniko's behaviour: every command is
snapshotted and tar+gzipped, including stages only used through
`COPY --from`.

Measurement:

- `run.sh` (copied from experiment 07): T0 right before `podman run -d`, T1
  when `podman wait` returns. Removing the container and the workdir is timed
  separately (teardown) and not counted. Registry `nk-registry`, npm through
  `nk-verdaccio` (warm cache), `--memory=8g` (except in the memory sets), 32
  CPUs or `--cpus=4`. Each build pushes to a fresh repository
  `k12p-<set>-<config>-<round>`.
- Rounds: one warm-up round (discarded) plus 5 measured rounds;
  configurations interleaved, order rotated every round. 216 builds in the
  four main sets (including warm-up), 28 builds in the two extra sets (3
  rounds plus warm-up), and 15 kept smoke tests; no build failed. Values are
  median (min–max) over the rounds; differences are paired by round and given
  as median (min; max).
- Peak memory: the container cgroup's `memory.peak` read from the host
  (`libpod-<id>.scope`), sampled every 0.3 s, the same gauge for kaniko and
  `ovbuild`. The memory cgroup includes page cache. Reaching 8192 MB means the
  kernel reclaimed inside the cgroup, not an OOM kill; no build was killed.
- Peak disk: `statfs` inside `ovbuild` (every 100 ms), plus `du -sxm` of the
  workdir after the build in round 1 (untimed) and `du` of each `rootfs-*`
  (`du.txt`).
- Idle wait: before each run, wait until the CPU is less than 10% busy over
  3 s, three times in a row (as in experiment 07). loadavg before and after
  each run is in `run.json`; `uptime` at the start and end of each set is in
  `run.log` and `results/all.log`.

Correctness:

- `check.sh` runs experiment 07's `listing.sh` (every path in the final image:
  type, mode, uid:gid, size, link target, sha256), compares it with a
  reference run using `cmp-listing.py`, and also compares per-layer statistics
  from `ovbuild.json` (files, directories, whiteouts, tar size), including the
  intermediate layers of `a`, `b` and `c` that are not in the final image.
- Smoke tests: one run per variant at 32 CPUs, compared with `wb-j1`. Selected
  builds from round 1 of the main matrix were checked the same way.
  `layercheck.py` compared the layer statistics of every walk run.

## Results

All values in this section are **[measured]** unless labelled otherwise.

### Capability probe

Output: `compat/out/01-probe-edock.txt`, `compat/out/rootful-podman-edock.txt`.

| Test | E-dock (14 caps + moby) | podman default | Unprivileged host user | Rootful podman E-dock | Cause (read from moby `default.json`) |
|---|---|---|---|---|---|
| `chroot` | **OK** | OK | FAIL `EPERM` | OK | moby allows it with `CAP_SYS_CHROOT` (E-dock has it); the host user lacks the capability |
| `mknod c 1 3` (char null) | **FAIL `EPERM`** | FAIL `EPERM` | FAIL `EPERM` | **OK** | moby allows `mknod` unconditionally; the `EPERM` comes from the **kernel**, which forbids creating devices in a non-initial user namespace even with `CAP_MKNOD`. **The rootless divergence** |
| `unshare(CLONE_NEWNS)` | **FAIL `EPERM`** | FAIL `EPERM` | FAIL `EPERM` | FAIL `EPERM` | moby: `unshare` needs `CAP_SYS_ADMIN` (E-dock lacks it). Host: Ubuntu AppArmor |
| `unshare(CLONE_NEWUSER)` | **FAIL `EPERM`** | **OK** | OK | FAIL `EPERM` | moby blocks it (needs `CAP_SYS_ADMIN`); podman's default allows it. **Docker's own seccomp locks user namespaces** |
| `mount` (bind) | **FAIL `EPERM`** | FAIL `EPERM` | FAIL `EPERM` | FAIL `EPERM` | Needs `CAP_SYS_ADMIN` |
| `ptrace(PTRACE_SEIZE)` on a child | **OK** | OK | OK | OK | moby allows `ptrace` (rule `minKernel 4.8`) **without** `CAP_SYS_PTRACE`, so proot can run |
| seccomp `RET_TRACE` (through `prctl`) | **OK** | OK | OK | OK | `prctl(PR_SET_SECCOMP)` + `NO_NEW_PRIVS`, no capability needed |
| seccomp `RET_USER_NOTIF` (`seccomp()` `NEW_LISTENER`) | **OK** | OK | OK | OK | moby allows `seccomp` unconditionally, so a user-notify supervisor can be built |
| `process_vm_readv` (read a child's memory) | **OK** | OK | OK | OK | moby allows it (rule `minKernel 4.8`) |
| `FICLONE` (reflink) on ext4 `/home` | **FAIL `EOPNOTSUPP` (95)** | — | FAIL (95) | — | ext4 here has no reflink, so strategy (d) is unusable |
| `FICLONE` on tmpfs | **FAIL `EOPNOTSUPP` (95)** | — | FAIL (95) | FAIL (95) | tmpfs does not support reflink |

Inside E-dock, what remains for separating one rootfs per stage is `chroot`
(with the capability) and `ptrace`, `process_vm_readv` and `seccomp` (enough
for proot or a supervisor). Everything that needs a mount or a namespace is
gone: overlay, `pivot_root`, bind-mounting `/proc` and `/dev`; reflink is
unavailable on ext4 and tmpfs.

As real root in the initial user namespace (`compat/out/rootful-mknod.txt`),
`mknod` of a character device (`c 1 3`), a block device (`b 7 0`) and a FIFO
all succeeded. In rootful podman E-dock, `mknod_c_1_3` succeeded and every
other row matched the rootless run. **On rootful Docker, bare `chroot` can
therefore have a real `/dev`** by creating the standard nodes with `mknod`;
the `/dev` gap below exists only under rootless podman. One gap remains on
both: **no `/proc`**.

### Approaches tried

| Approach | Result | `/proc` | `/dev` | Speed | Notes |
|---|---|---|---|---|---|
| **Bare `chroot`, one directory per stage** (A) | **Feasible** | **No** | Yes on rootful Docker (`mknod`); no under rootless podman | As fast as bind mounts | apk, apt/dpkg, npm, pnpm, `useradd`, `USER nobody` all work. Commands that read `/proc` either fail or **return wrong results without an error**: empty `ps`, `os.cpus().length == 0` |
| **proot** (path translation through ptrace) (C) | **Feasible** | Yes | Yes | +26–56% wall; micro-benchmarks 3–6× | **apt hangs** and needs `-o APT::Sandbox::User=root`. A third binary under GPL-2.0 **[external]**, which conflicts with the single-binary goal |
| seccomp user-notify supervisor | Building blocks present, **not built** | Yes | Yes | **[inferred]** see note below | Would have to be written at proot's scale: `ADDFD`, `*at()`, symlinks, TOCTOU |
| fakechroot (`LD_PRELOAD`) | **No** | No | No | | Static binaries ignore the preload and read the container's real files |
| Hybrid: one stage runs on the real `/` | **No** **[inferred]** | | | | Which stage gets `/proc` depends on scheduling; the filesystem must be deleted while other stages run |
| Symlink or hardlink `/proc` and `/dev` into the rootfs | **No** **[inferred]** | | | | After `chroot` a symlink resolves inside the rootfs; a hardlink across mounts returns `EXDEV` |

The parts disagree on the speed of a user-notify supervisor, and both views
are **[inferred]**. The compatibility part expected it could be much faster
than proot, since only path syscalls would be trapped and there is no
ptrace stop for every syscall. The performance part expected about the same
cost as proot in seccomp mode, since a filter cannot read path strings and
every `open*` and `stat*` call must still be trapped. The summary of the
original rated it "about proot's speed".

Ranking by the compatibility part:

| # | Approach | Feasible | Strengths | Limits |
|---|---|---|---|---|
| **1** | **A: bare `chroot`, one rootfs per stage (extract or `cp -a`)** | **Yes**, measured in both parts | Fast, close to bind mounts. Needs only `CAP_SYS_CHROOT`, which is in the 14 default capabilities. Package managers (apk, apt, npm, pnpm) and `USER` work | **No `/proc`**: commands that read it either fail or **return wrong results silently** (`ps` empty, `os.cpus()` = 0). **No `/dev`** under rootless podman (`mknod` `EPERM`): only a regular-file `/dev/null`, no `/dev/zero` or `/dev/urandom`; rootful Docker can `mknod`. **No isolation against malicious code** (escape into another stage measured), but current kaniko mode has none either. `hosts`, `resolv.conf` and the mtime of `/etc` must be copied in and restored. RAM and disk grow with the number of concurrent stages |
| **2** | **C: proot** | **Yes**, measured | `/proc`, `/dev` and `resolv.conf` through path translation. Blocks naive `chroot` escapes and accidental access to other stages | **Slow**: +26–56% wall, micro-benchmarks 3–6×. **apt hangs** without `APT::Sandbox::User=root`, and other traps may exist. Requires shipping the proot binary (1.8 MB static, third party) or writing an equivalent. **Not a security boundary** **[inferred]** |
| 3 | seccomp user-notify supervisor | Building blocks present (measured); **not built** | **[inferred]** Possibly much faster than proot, because only path syscalls are trapped | A proot-sized project (`ADDFD`, `*at()`, symlinks, TOCTOU). Deferred; only worth doing if A and C are not enough |
| ✗ | fakechroot | **No** | Does not need `CAP_SYS_CHROOT` | Static, musl and Go binaries bypass the preload (measured) |
| ✗ | Hybrid (one stage on `/`) | **No** | | Non-deterministic; the filesystem must be deleted exactly while other stages run |

Detail of the other ideas (feasibility only, `compat/out/06-other.txt`):

| Idea | What was checked | Result | Assessment |
|---|---|---|---|
| **seccomp user-notify supervisor** (path translation without ptrace) | `seccomp()` + `NEW_LISTENER`, `ptrace`, `process_vm_readv` in E-dock | **All three OK** (moby allows `seccomp` unconditionally; `ptrace` and `process_vm_readv` under the `minKernel 4.8` rule) | **[inferred]** Buildable. It would have to read paths with `process_vm_readv`, open files on the process's behalf and return the fd with `SECCOMP_IOCTL_NOTIF_ADDFD`, handle `*at()`, symlinks, `..`, `execve`, and the TOCTOU race when reading arguments. A proot-sized project, not a few hundred lines |
| **fakechroot** (`LD_PRELOAD`) on debian | `fakechroot chroot <rootfs>` as root and as `nobody` | Both work; `nobody` needs no `CAP_SYS_CHROOT` (it sees the target rootfs's `/etc/os-release`) | **Unusable for arbitrary `RUN`.** A static binary (measured with `escape`) ignores the preload and reads the container's real `/etc/hostname`. musl (alpine), static Go and anything that issues syscalls directly behave the same **[inferred]**. No `/proc`, no `/dev` |
| **Hybrid**: one stage on the real `/`, the others in `chroot` | — | **[inferred]** | The stage on `/` has a real `/proc` and `/dev` and the others do not, so behaviour depends on scheduling. The stage on `/` must also delete the filesystem as kaniko does while other stages run in subdirectories of `/`, which must then be excluded from the walk and the deletion. Complex, little gain |
| Symlink `rootfs/proc` → an fd of `/proc` opened before `chroot` | — | **[inferred]** (the same idea as the performance part's) | Does not work: after `chroot` there is no `/proc/self/fd` to resolve the symlink. Passing an inherited fd does not help commands that open `/proc/...` paths |
| `mknod` of `/dev` on **rootful** Docker | `rootful.sh` (rootful podman E-dock) | See [Capability probe](#capability-probe) | **[inferred]** at the time, following the performance part; `mknod` was then confirmed with rootful podman. Rootful Docker grants `CAP_MKNOD` in the initial user namespace and its device cgroup allows `mknod`, so a real `/dev/null` can be created; approach A's `/dev` gap disappears on real Docker and remains only under rootless podman |

The performance part considered, but did not build, the same alternatives
**[inferred]**: hardlinking the container's `/dev` nodes into the rootfs fails
with `EXDEV` (`/dev` is a separate tmpfs in the container, and a bind from the
host under rootless podman); a `rootfs/proc` → `/proc/self/fd/N` symlink has
no `/proc/self` to resolve after `chroot`; an `LD_PRELOAD` approach does not
apply to static and Go binaries and changes libc behaviour; a user-notify
supervisor; the hybrid, where which stage gets `/proc` depends on scheduling.

### Compatibility matrix

**`/dev` under bare `chroot`.** `mknod /dev/null c 1 3` returns `EPERM` under
rootless podman, and without mounts a real `/dev/null` cannot be
bind-mounted. Only two options remain, both bad:

- a regular file at `/dev/null`: **dangerous**, because it is not a sink.
  Every write to `/dev/null` grows the file (filling the disk), and reading it
  returns old data instead of EOF. It ends up in a layer unless cleaned up;
- nothing: any command that opens `/dev/null` gets `ENOENT`.

**[inferred]** Approach A therefore has no correct way to provide `/dev` under
rootless podman. This is inherent, and affects `/dev/zero` and `/dev/urandom`
too, which some runtimes read. The matrix below ran with `/dev/null` as a
regular file, and apk, apt, npm and pnpm all passed. The performance part saw
one W1 message `xargs: '/dev/null': No such file or directory` (GNU xargs
opens `/dev/null` as stdin for its children); it is only a warning and the
layer was correct. Under rootful Docker `mknod` works and the problem goes
away.

Matrix: `compat/out/02-matrix.txt`, per-cell output in `compat/out/cells/`
(load 0.2–1.9). Each cell ran on a fresh `cp -a` copy with its own timeout
(300 s; apt 120 s). In every `chroot` cell `/dev/null` was a regular file.
Times are single runs with warm registry and verdaccio caches, **not
benchmarks**.

| Command | Base | `chroot`, root | `chroot`, `nobody` | proot, root | Cause of failure |
|---|---|---|---|---|---|
| `apk add curl` | alpine | OK (1.4 s) | FAIL: `Failed to open apk database: Permission denied` | OK (2.8 s) | `nobody` cannot write apk's database; the same under Docker |
| `adduser -D` | alpine | OK | — | OK | |
| `ps aux` | alpine | **rc=0 but an empty list** (header only) | — | OK, processes visible | busybox `ps` reads `/proc`; without it, it **silently** returns nothing |
| `cat /proc/self/status` | all 3 | **FAIL** `ENOENT` | FAIL `ENOENT` | OK | No `/proc` |
| `apt-get update` | debian | OK (0.6 s) | — | **TIMEOUT 120 s**. With `-o APT::Sandbox::User=root`: OK (0.5 s) | apt drops its download methods to user `_apt`; under proot that hangs (see [proot](#proot)) |
| `apt-get install -y curl` | debian | OK (2.4 s) | FAIL: cannot take the dpkg lock | TIMEOUT. With `Sandbox::User=root`: OK (2.5 s) | As above; `nobody` lacks write permission |
| `apt-get install -y curl` | node | OK (1.9 s) | — | TIMEOUT. With `Sandbox::User=root`: OK (2.7 s) | As above |
| `useradd t1` | debian, node | OK | — | OK | |
| `ps aux` | debian, node | `ps: not found` | — | `ps: not found` | The image has no `procps`; unrelated to the sandbox |
| `npm --version` | node | OK (10.9.9) | OK | OK | |
| `node -e 'os.cpus().length'` | node | **rc=0 but `cpus=0`** | `cpus=0` | OK (`cpus=32`) | `os.cpus()` reads `/proc/cpuinfo` and `/proc/stat`; it returns 0 instead of failing |
| `npm install -g pnpm@10.34.7` | node | OK (0.4 s) | — | OK (0.6 s) | |
| `pnpm install`, project with 1 dependency | node | OK (0.5 s) | FAIL rc=243 | OK (0.3 s) | **[inferred]** `nobody` cannot write `/work` or `HOME=/root` (both owned by root); rc 243 is npm/pnpm's permission error code |
| `pnpm install --frozen-lockfile`, W3 `app-par/a` | node | OK (1.3 s) | — | OK (3.9 s, about 3×) | |
| gcc | — | Skipped: none of the three images has a compiler | | | |

- **Package managers work under bare `chroot`**: apk, apt/dpkg (including the
  switch to user `_apt`), npm, pnpm with W3's real lockfile, `useradd` and
  `adduser`. None of them needs `/proc` or a real `/dev/null`. The
  performance part saw the same for every `RUN` of W1 and W3.
- **A missing `/proc` breaks things in two ways**: a visible error
  (`cat /proc/...`) and, more dangerously, **a wrong result without an error**
  (empty `ps`, `os.cpus().length == 0`). A build that sizes its workers from
  `os.cpus().length` (jest, terser, thread-loader…) may run with 0 workers or
  divide by zero **[inferred, not tested here; see
  [experiment 12a](12a-fake-proc.md)]**.
- **`USER nobody`** through `setuid` inside the `chroot` behaves correctly:
  failures come only from missing write permission, as under Docker.

### proot

**proot runs in E-dock (moby seccomp + 14 capabilities).**
`proot -r <rootfs> -b /proc -b /dev -b /sys -b /etc/resolv.conf` succeeds, and
`/proc`, `/dev/null` and `/etc/resolv.conf` all look correct **without any
mount**, because proot translates paths through ptrace. This is the key
point: proot fills exactly the three gaps of approach A (`/proc`, `/dev`,
binds).

Overhead (`compat/out/03-proot-overhead.txt`, load about 1.6–2.0):

| Workload | Bare `chroot` | proot (seccomp acceleration) | proot `PROOT_NO_SECCOMP=1` |
|---|---|---|---|
| `/bin/true` × 200 (fork+exec) | 0.40 ms/exec | 1.11 ms/exec (**about 2.8×**) | 0.71 ms/exec (about 1.8×) |
| `find /` on node (7.3k entries) | 0.005 s | 0.030 s (**about 6×**) | 0.098 s (about 20×) |
| `tar -x` of debian.tar (5.4k entries, write-heavy) | 0.041 s | 0.133 s (**about 3.2×**) | — |

- For **syscall-heavy** jobs (`find`, `tar`), proot is **3–6×** slower even
  with seccomp acceleration, and much worse without it (20×). This is the
  price of ptrace: every path-related syscall is trapped.
- For **tiny execs** (`/bin/true`), seccomp acceleration is
  **counter-productive** (installing the filter costs more than it saves) and
  `PROOT_NO_SECCOMP` is faster. **[inferred]** A real `RUN` (thousands of
  syscalls) should keep acceleration on.
- On real builds (see [proot on real builds](#proot-on-real-builds)): +26–56%
  wall time (W3 +3.5 s, W1 +4.2–6.2 s); `pnpm install` 1.6–3.1×.

**apt hangs under proot.** `apt-get update` and `install` under proot **hang
until the timeout** (60 s and 120 s; `compat/out/02-matrix.txt`,
`compat/scripts/dbg-apt.sh`), while the same commands finish in 0.6–2.4 s
under bare `chroot`. With `-o APT::Sandbox::User=root`, proot finishes in
0.5–2.7 s. The cause is therefore apt **switching its download methods to
user `_apt`** while traced by proot. It was not chased further
(`setgroups`/`setresuid` under ptrace) **[inferred]**.

Consequence: with proot, the tool would have to apply this option to every
`RUN` that uses apt, without letting it **leak into the layer** (no file in
the rootfs's `/etc/apt/apt.conf.d/`). An `APT_CONFIG` variable pointing at a
file outside the rootfs, bound in with `-b`, might work **[inferred, not
tested]**. This is the kind of per-program trap that proot always brings
along.

### Rootfs preparation

Output: `compat/out/05-prep.txt` (load about 1.5–3.0). Extracted sizes: debian
126 MB / 5449 entries, node 228 MB / 7293 entries. Warm page cache.

| Filesystem | Base | (a) tar extract | (b) `cp -a` from store | (c) `cp -al` hardlinks | (d) `cp --reflink=always` |
|---|---|---|---|---|---|
| ext4 `/home` | debian | 0.074–0.076 s | 0.080–0.083 s | **0.026 s** | **FAIL** `EOPNOTSUPP` |
| ext4 | node | 0.122–0.172 s | 0.116–0.170 s | **0.032–0.044 s** | **FAIL** |
| tmpfs | debian | 0.044–0.047 s | 0.051–0.056 s | **0.012–0.014 s** | **FAIL** |
| tmpfs | node | 0.066–0.068 s | 0.079–0.124 s | **0.018–0.031 s** | **FAIL** |

- **Disk:** (a) and (b) cost a full copy per stage (debian about 120 MB, node
  about 223 MB). With (c), `du` of store plus copy together **still equals one
  store** (about 0 bytes extra).
- **`cp -al` is the fastest (about 3–6× faster than extract or `cp -a`) and
  almost free on disk**, but it shares inodes; see below.
- **Reflink (d) is unavailable on ext4 and tmpfs**, as expected.

**Hardlink farm corruption.** Two stages were made as `cp -al` copies of one
store. Stage 1 ran `echo >> /etc/profile`; the file grew **in the store and in
stage 2** as well (debian 828 → 861 B, node 769 → 802 B). Hardlinks share the
inode, so **an in-place write corrupts the store and the sibling stages**.

**In-place writes versus temp file + rename** (`compat/out/05b-inplace.txt`;
inode measured on a `cp -al` copy, where the same inode after the command
means an in-place write):

| Command | File watched | Inode before → after | Verdict | `strace` (`openat`/`rename`) |
|---|---|---|---|---|
| `echo x >> /etc/profile` | `etc/profile` | 97407 → 97407 | **In place → corrupts the store** | — |
| `sed -i '1i #x' /etc/profile` | `etc/profile` | 103699 → 109876 | Temp + rename, safe | `rename("/etc/seds54zy4", "/etc/profile")` |
| `apk add curl` | `lib/apk/db/installed`, `etc/apk/world` | 172 → 1342; 1443 → 2684 | Temp + rename, safe | Every file: `renameat(".apk.<hash>" → target)` |
| `apt-get install -y curl` (dpkg) | `var/lib/dpkg/status` | 8095 → 11705 | Temp + rename, safe | `rename(updates/tmp.i → updates/0000)`; package files go through `*.dpkg-new`/`*.dpkg-tmp` |
| `npm i -g`, `pnpm install` | — | **Not measured** (the npm test watched the wrong path: the node image's npm lives in `/usr/local/lib`) | **[inferred]** Mostly create new files in `node_modules`, rarely modify base files | — |

- **The big package managers (apk, dpkg) are safe with a hardlink farm**: they
  always write a temporary file and `rename` it, which breaks the link and
  leaves the store intact.
- **The risk lies in shell commands and metadata changes** **[inferred, a
  property of hardlinks, not measured]**: `>>`, `truncate`,
  `dd conv=notrunc`, programs that open files `O_RDWR` (sqlite…). More
  importantly, **`chmod`, `chown` and `touch` on a hardlinked file change the
  shared inode's metadata**. One `RUN chown -R node /usr/local` in one stage
  would modify the store and every other stage.
- **[inferred]** A `cp -al` farm is **safe only if** every `RUN` writes
  through temp + rename. In practice some commands overwrite in place
  (`echo >>`, `truncate`, some scripts). **Use a full `cp -a` per stage**, or
  hardlinks with the link broken before each write (manual copy-on-write,
  which is not possible without controlling `RUN`).

**Reflink as real root** (`compat/out/rootful-reflink.txt`). Node rootfs
223 MB, 7293 entries; warm page cache; one measurement; disk measured with
`df` before and after:

| Filesystem | `FICLONE` | `cp -a --reflink=always` | `cp -a` | `cp -al` | Write to the reflinked copy |
|---|---|---|---|---|---|
| xfs | **OK** | 0.105 s, +3 MB | 0.096 s, **+4 MB** | 0.030 s, +0 MB | Store unchanged (CoW) |
| btrfs | **OK** | 0.077 s, +12 MB | 0.077 s, **+12 MB** | 0.042 s, +3 MB | Store unchanged (CoW) |
| ext4 | `EOPNOTSUPP` | Error | 0.101 s, **+231 MB** | 0.034 s, +3 MB | — |

- On xfs and btrfs, **plain `cp -a` already reflinks**: coreutils ≥ 9.0
  defaults to `--reflink=auto` **[external]** (no source recorded in the
  original), and this was measured with the host's coreutils 9.7. `cp -a`
  therefore costs almost no disk there and stays safe to write.
- **Reflink is not faster than a full copy** (0.08–0.1 s), because the cost
  lies in the metadata of 7293 entries, not in the data. Its benefit is
  **disk**: +3–12 MB against +231 MB.
- Consequence: build each stage's rootfs with `cp -a` (or `FICLONE` per file)
  from the store. On xfs and btrfs this is as safe as a full copy and as cheap
  on disk as hardlinks; on ext4 and tmpfs it costs a full copy. Hardlink farms
  remain unusable.

### Isolation

Output: `compat/out/04-isolation.txt`.

| Approach | Can a malicious `RUN` escape its rootfs? | Can it read or write another stage's rootfs? |
|---|---|---|
| **A: bare `chroot`** | **Yes** (it still holds `CAP_SYS_CHROOT`) | **Yes: it read `secret.txt` and wrote `secret.txt.pwned` into stage B** |
| **C: proot** | No (the `chroot(".")` escape → `ENOENT`) | No (`open(/wtmp/store-B/...)` is translated into the rootfs → `ENOENT`) |
| **Accidental** writes (relative path, `echo > /etc/foo`) | — | No, under both: the write lands in the stage's own rootfs |

- **Accidental** writes (a command writing to the wrong absolute path inside
  its rootfs): **both A and C isolate**, because each stage is its own
  directory tree. *Important exception:* with a **`cp -al` hardlink farm**,
  an in-place write also corrupts other stages. That is a real accidental
  risk, but it comes from **how the rootfs is prepared**, not from the
  `chroot` mechanism.
- **Malicious** code (a `RUN` deliberately escaping): **approach A blocks
  nothing**. Escaping `chroot` is a textbook technique, and reading or writing
  another stage is easy. **proot blocks the naive escape** and any accidental
  access, **but proot is not a security boundary** **[inferred]**: its own
  documentation says so, and static code or direct syscalls can bypass it (the
  same mechanism as the fakechroot bypass above).
- **Context:** current kaniko mode **is not isolated either** (one rootfs,
  `RUN` as root inside it). One `chroot` per stage is therefore **no worse**
  against malicious code and **better** against accidents (separate rootfs).
  But secrets of one stage, for example `--mount=type=secret`, become readable
  from another stage running in parallel. Users who build untrusted code still
  need overlay mode with a user namespace, not kaniko mode.

### Performance: W3 and W1

Host wall time (s), median (min–max) of 5 rounds after one discarded warm-up;
`--memory=8g`; warm page cache and warm verdaccio cache. The full per-set
tables are in the [appendix](#appendix-full-tables).

W3 (`nextjs-kaniko/app-par`, three independent `pnpm install` stages plus a
small `final`):

| W3 | 32 CPUs | `--cpus=4` |
|---|---|---|
| Optimized kaniko (`k-stream`) | 19.88 (19.77–20.08) | 22.03 (21.95–22.21) |
| Walk bind, `--jobs 1` (= current sequential kaniko mode) | 14.04 (13.91–14.23) | 17.14 (17.04–17.37) |
| Walk bind, default `--jobs` | 8.35 (8.03–8.55) | 13.45 (13.39–13.72) |
| Walk nocap (E-dock), `--jobs 1` | 14.18 (13.97–14.47) | 17.24 (17.12–17.29) |
| **Walk nocap (E-dock), default `--jobs`** | **7.99** (7.80–8.29) | **13.74** (13.56–14.40) |
| Walk proot (E-dock), default `--jobs` | 11.58 (10.70–11.82) | 17.34 (17.26–17.46) |
| Walk nocap lazy, `--jobs 1` | 9.61 (9.51–9.65) | 12.20 (12.05–12.30) |
| **Walk nocap lazy, default `--jobs`** | **5.38** (5.21–5.89) | **10.02** (9.98–10.17) |
| Walk nocap, default `--jobs`, no `syncfs` | 7.30 (7.29–8.19) | 13.56 (13.32–14.55) |
| Walk nocap, default `--jobs`, `-prep copy` | 8.26 (7.85–8.78) | 13.82 (13.73–13.88) |
| Overlay lazy, default `--jobs` (same-session control) | 6.29 (5.77–6.60) | 10.64 (10.49–10.81) |

W1 (`nextjs-kaniko/app/Dockerfile`, `deps` → `final` is one chain and only
`assets` is independent):

| W1 | 32 CPUs | `--cpus=4` |
|---|---|---|
| Walk bind, `--jobs 1` | 11.22 (11.05–11.86) | 14.08 (13.63–14.75) |
| Walk bind, default `--jobs` | 10.51 (10.14–11.47) | 13.00 (12.98–13.12) |
| Walk nocap (E-dock), `--jobs 1` | 11.06 (10.94–11.18) | 14.26 (13.87–14.57) |
| Walk nocap (E-dock), default `--jobs` | 10.67 (10.38–12.10) | 13.80 (13.04–14.36) |
| Walk proot (E-dock), `--jobs 1` | 17.13 (17.07–17.47) | 18.77 (18.30–19.08) |
| Walk proot (E-dock), default `--jobs` | 16.44 (16.04–16.60) | 18.15 (17.81–18.38) |
| Walk nocap, default `--jobs`, no `syncfs` | 10.37 (9.92–11.01) | 12.90 (12.74–14.34) |

Differences paired by round (s), median (min; max), and percentage:

| | W3 32 CPUs | W3 4 CPUs | W1 32 CPUs | W1 4 CPUs |
|---|---|---|---|---|
| bind: default − `--jobs 1` | **−5.76** (−5.89; −5.49), −41% | **−3.66** (−3.75; −3.38), −21% | −0.63 (−1.65; +0.42), −6% | −0.96 (−1.65; −0.65), −7% |
| nocap: default − `--jobs 1` | **−6.12** (−6.67; −5.88), −44% | **−3.55** (−3.65; −2.84), −21% | −0.39 (−0.73; +1.12), −4% | −0.31 (−1.41; +0.22), −2% |
| nocap lazy: default − `--jobs 1` | −4.27 (−4.34; −3.72), −44% | −2.17 (−2.32; −1.88), −18% | – | – |
| proot: default − `--jobs 1` | – | – | −0.94 (−1.08; −0.57), −5% | −0.45 (−1.07; −0.15), −2% |
| nocap − bind (both at default `--jobs`) | −0.36 (−0.75; +0.26) | +0.22 (+0.08; +0.69) | +0.35 (−0.42; +0.99) | +0.79 (+0.06; +1.36) |
| proot − nocap (default `--jobs`) | **+3.53** (+2.76; +3.83), +44% | **+3.53** (+2.94; +3.87), +26% | **+5.46** (+4.50; +6.15), +51% | **+4.18** (+3.78; +5.34), +30% |
| proot − nocap (`--jobs 1`) | – | – | +6.18 (+5.99; +6.49), +56% | +4.51 (+4.16; +4.62), +31% |
| nocap: lazy − export all layers (default `--jobs`) | −2.76 (−2.91; −1.91) | −3.57 (−4.42; −3.54) | – | – |
| nocap: no `syncfs` − with `syncfs` | −0.63 (−0.99; +0.39) | −0.29 (−0.61; +0.99) | −0.75 (−1.15; +0.07) | −0.69 (−1.10; −0.02) |
| nocap: `-prep copy` − extract | +0.27 (−0.44; +0.98) | −0.00 (−0.53; +0.23) | – | – |
| Walk nocap default − overlay lazy default | +1.69 (+1.51; +2.17) | +3.10 (+2.83; +3.91) | – | – |
| **Walk nocap lazy default − overlay lazy default** | **−0.90** (−1.22; −0.23) | **−0.54** (−0.77; −0.47) | – | – |
| Walk nocap default − optimized kaniko | −11.83 (−12.05; −11.80), −60% | −8.28 (−8.65; −7.63), −38% | – | – |

The W1 4 CPU set is the one exception to "`nocap` is as fast as `bind`":
`nocap` was 0.79 s slower (+0.06; +1.36) because `pnpm install` took 0.7 s
longer. At `--jobs 1` in the same set there was no difference, so this is
unexplained; interference from the compatibility part running at the same
time is one possibility.

#### Where the gain comes from (W3)

In the sequential walk (`wn-j1`, 32 CPUs), each of the stages `a`, `b` and
`c` consists of: base extract 0.63 s, `pnpm install` 1.8–2.3 s, `syncfs`
0.2–0.3 s, walk + hash 0.3–0.55 s, tar + gzip 0.6–1.05 s. **Nearly half of a
stage's time is outside the `RUN` command.** In overlay mode
([experiment 08](08-parallel-stages-lazy-export.md)) the part outside the
command is only mount/umount plus the upperdir walk (0.2 s per stage).

| W3, Σ over the 3 `pnpm` stages (s) | 32 CPUs `--jobs 1` | 32 CPUs default | 4 CPUs `--jobs 1` | 4 CPUs default |
|---|---|---|---|---|
| Σ `pnpm install` command / longest command (nocap) | 6.41 / 2.34 | 12.01 / 4.40 | 9.15 / 3.33 | 26.24 / 9.09 |
| Σ `syncfs` + walk/hash + tar/gzip of the 3 `pnpm` layers (bind) | 0.68 + 1.29 + 2.57 = 4.54 | 0.29 + 1.85 + 3.94 = 6.08 | 0.69 + 1.50 + 2.75 = 4.94 | 0.24 + 2.88 + 5.74 = 8.86 |
| Base extract, Σ of 4 stages / all finished at | 2.56 / 2.69 | 3.49 / 0.95 | 2.51 / 2.63 | 3.75 / 1.03 |
| `final` stage starts at (nocap) | 13.69 | 7.39 | 16.74 | 13.14 |

- Running together, **each command gets slower**: the longest
  `pnpm install` goes from 2.3 to 4.4 s (32 CPUs) and from 3.3 to 9.1 s
  (4 CPUs), as in experiment 08. At 4 CPUs the commands themselves gain
  almost nothing (Σ 9.2 s sequential against a longest command of 9.1 s in
  parallel).
- The gain comes from **overlapping the steps outside the command**: the base
  extracts of the 4 stages finish after 0.95–1.03 s instead of 2.6–2.7 s, and
  one stage's walk, hash and tar run while another stage runs
  `pnpm install`. Total snapshot CPU rises (4.5 → 6.1 s at 32 CPUs, 4.9 →
  8.9 s at 4 CPUs, from CPU and disk contention) but is no longer serial.
- That is why parallel stages gain **more** in walk mode than in overlay mode:
  −5.8 to −6.1 s against −2.5 s (32 CPUs), −3.6 s against −0.5 s (4 CPUs).
  Walk lazy (no snapshot of `a`, `b`, `c`) gains −4.3 s and −2.2 s, close to
  overlay, because only the base extract is left outside the command.
- W1: `final` must wait for `deps` (it reuses that rootfs), so only `assets`
  (extract 0.65 s + command 0.15 s + snapshot) overlaps: `deps` finishes at
  6.9–7.0 s instead of 7.6 s at 32 CPUs. The gain of −0.4 to −1.0 s is of the
  same size as the noise.

#### Cost of a per-stage rootfs

| | W3 32 CPUs | W3 4 CPUs | W1 32 CPUs |
|---|---|---|---|
| Base extract (node, 5 layers, 232 MB, 7.3k entries) per stage, sequential | 0.64 (0.62–0.67) s | 0.62 (0.61–0.65) s | 0.64 s |
| Same, 4 stages at once (including `syncfs` + baseline scan before the first command) | 0.87 (0.87–0.88) s | 0.94 (0.93–0.95) s | 0.73 s (2 stages) |
| `-prep copy`: one extract + `cp -a` per stage (parallel) | 0.59 + 0.41 s | 0.58 + 0.41 s | – |
| Disk: each rootfs (`du`, round 1) | `a` 833, `b` 689, `c` 1067, `final` 232 MB | Same | `assets` 246, `deps`+`final` 1779 MB |
| Disk: workdir peak (`statfs`), export all / lazy / overlay lazy | 3278 / 2819 / 2128 MB | Same | 2727 MB (2376 at the end of the build, because `final` deletes files) |
| Disk: layer blobs (tar.gz) in the workdir, export all | 460 MB | 460 MB | 352 MB |
| `-prep copy` adds | +232 MB (shared base copy) | +232 MB | – |
| Peak cgroup memory (host), `--jobs 1` → default | 5.3–5.6 → **7.9–8.19 GB; hit the 8 GB limit in 27/30 parallel walk runs** (lazy 5/5, proot 2/5) | 5.3–5.5 → 7.8–8.19 GB; hit the limit in 19/30 (lazy 5/5, proot 0/5) | 4.9–5.2 → 4.8–5.1 GB |
| Overlay lazy, default (same session) | 7.57 GB | 7.48 GB | – |

- **Time:** a private rootfs costs one base extract per stage (0.62–0.64 s
  with the node base). In parallel the extracts overlap, so only about 1 s
  remains on the critical path. kaniko avoids this with `REUSE_BASE_ROOTFS`
  (experiment 08: 1.1 s for 4 stages), but only because it runs sequentially
  on one rootfs.
- **`cp -a` from a shared extracted copy is not faster** than extracting from
  the gzip blob on this machine (0.41 s against 0.62 s per stage sequentially,
  plus 0.58 s for the shared extract; in parallel they tie: W3 +0.27 /
  −0.00 s). The compatibility part measured the same direction
  (`compat/out/05-prep.txt`: node 0.15 s for a plain tar extract against
  0.15–0.17 s for `cp -a`). `cp --reflink` is not available on ext4 or tmpfs;
  `cp -al` is 3–4× faster but **corrupts the shared copy when a `RUN` writes in
  place**, so it is unusable.
- **Disk:** each stage adds one copy of the base (232 MB) plus what it writes.
  On W3 the parallel walk used 3.28 GB against 2.13 GB for overlay (overlay
  extracts the base once, and lazy has no intermediate blobs). **[inferred]**
  Sequential kaniko on `/` keeps only one stage at a time (it deletes the
  filesystem between stages and saves the files needed for `COPY --from`), so
  its disk peak is lower by roughly the sum of the rootfs of stages that do
  not run at the same time.
- **Memory is the real constraint.** The memory cgroup includes the page cache
  of every file just written. Three concurrent `pnpm install` runs write about
  2.6 GB, plus 4 × 232 MB of base, plus blobs; the parallel walk on W3 hit the
  8 GB limit in 46 of 60 runs (kernel reclaim inside the cgroup, no OOM). The
  measurable consequence: export had to **re-read from disk** (median 0–233 MB
  depending on the configuration, at most 384 MB in one run) in the parallel
  walk that exports every layer, and always 0 MB in the sequential walk; the
  same as in experiment 08. Overlay lazy peaked at 7.5 GB and never hit the
  limit. W1 did not change (4.9–5.2 GB), because almost nothing runs
  concurrently.

#### proot on real builds

| | W1 32 CPUs | W1 4 CPUs | W3 32 CPUs | W3 4 CPUs |
|---|---|---|---|---|
| proot − nocap, `--jobs 1` | +6.18 s (+56%) | +4.51 s (+31%) | – | – |
| proot − nocap, default `--jobs` | +5.46 s (+51%) | +4.18 s (+30%) | +3.53 s (+44%) | +3.53 s (+26%) |
| proot default − bind `--jobs 1` (current sequential) | +4.91 s | +3.73 s | **−2.47 s** | +0.23 s |

Per command (W1, `--jobs 1`, median, nocap → proot):

| `RUN` | 32 CPUs | 4 CPUs | Note |
|---|---|---|---|
| `pnpm install --frozen-lockfile` | 1.99 → 6.11 s (×3.1) | 4.24 → 6.69 s (×1.6) | At 4 CPUs the command is CPU-bound, so ptrace is a smaller share |
| `npm install -g pnpm` | 0.72 → 0.95 s (×1.3) | 0.72 → 0.91 s (×1.3) | |
| `find node_modules … -exec rm` / `-delete` (250k files) | 0.25 → 1.15 s (×4.5) | 0.26 → 1.17 s (×4.6) | Many `getdents`/`lstat`/`unlink` |
| `find … \| xargs sed -i` + `find -exec touch` | 0.22 → 1.11 s (×5.1) | 0.22 → 1.11 s (×5.0) | |
| `for` loop of 3000 `printf` + 500 `cp` | 0.15 → 0.29 s (×2.0) | 0.15 → 0.30 s (×2.0) | fork/exec |
| `mkdir` + 5000 `echo >` + `chmod -R` | 0.05 → 0.18 s (×3.5) | 0.05 → 0.18 s (×3.5) | |

- proot (v5.5.0, default mode with seccomp filtering) ran every `RUN` of W1
  and W3 correctly, with identical images. The overhead scales with the number
  of path syscalls: large tree walks are 4.5–5× slower, `pnpm install`
  1.6–3.1×.
- On parallel W3, proot's overhead (longest `pnpm install` 4.40 → 8.27 s at
  32 CPUs, 9.09 → 13.66 s at 4 CPUs) eats almost all of the parallel gain: at
  4 CPUs parallel proot **ties** the sequential walk (+0.23 s); at 32 CPUs it
  still gains 2.5 s.
- Snapshots under proot were slightly faster (walk/hash + tar/gzip 4.0 against
  5.9 s on W3 at 32 CPUs), because slower commands overlap less. This is not
  a real advantage.

#### Cheap variants

| Variant | W3 32 CPUs | W3 4 CPUs | W1 32 CPUs | W1 4 CPUs | Verdict |
|---|---|---|---|---|---|
| Lazy (`-export final`) − export all layers, nocap default | **−2.76** (−2.91; −1.91) | **−3.57** (−4.42; −3.54) | – | – | Large, correct gain (already in experiment 08). Condition: the intermediate stages' layers are not needed (no `--cache`, that stage is not pushed) |
| No `syncfs` − with `syncfs` | −0.63 (−0.99; +0.39) | −0.29 (−0.61; +0.99) | −0.75 (−1.15; +0.07) | −0.69 (−1.10; −0.02) | Small gain; the min–max range contains 0 in 3 of 4 sets. Layers unchanged. `syncfs` before the walk costs 0.4–0.8 s in total; when 4 stages extract at once, the `syncfs` before the baseline scan slows each extract by 0.24 s |
| `-prep copy` − extract | +0.27 (−0.44; +0.98) | −0.00 (−0.53; +0.23) | – | – | No gain, 232 MB more disk |

On `syncfs`: kaniko calls it before every scan because "mtime may lag without
a sync" **[code]** (`kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:240-266`).
**[inferred]** On local filesystems (ext4, xfs, btrfs, tmpfs, the container's
overlay), `stat` reads the in-memory inode and does not lag; `syncfs` is only
needed on network or FUSE filesystems that push metadata late. It can be
skipped when the workdir is on a local filesystem (read `statfs.f_type`). The
gain on W1 and W3 is small (0.3–0.75 s). One side effect is notable: without
`syncfs`, freshly written pages stay dirty and are not reclaimed early, so
export re-reads less (experiment 07 §4.3); in the main matrix the effect was
not stable (123 MB against 185 MB re-read).

One more improvement was considered and **not built**: asynchronous export of
a stage's last layer once its rootfs is no longer modified (W3 `a`, `b`, `c`).
The build must still wait for every export before pushing, and W3's `final`
takes only 0.1–0.25 s, so the critical path would not shrink **[inferred from
the timeline]**.

#### `--jobs` and memory

Two extra sets, W3 at 32 CPUs, nocap, 3 rounds plus warm-up, run back to back:
`--memory=8g` as in the main matrix, then `--memory=16g`.

| W3, 32 CPUs | `--jobs 1` | `--jobs 2` | Default `--jobs` (32) | Lazy, default |
|---|---|---|---|---|
| Wall, `--memory=8g` (s) | 14.11 (14.08–14.17) | 10.24 (10.18–10.83) | 8.76 (8.71–8.77) | – |
| Peak cgroup memory, 8g (MB) | 5604 | 6071 | 7651 (7097–7879) | – |
| Re-read on export, 8g (MB) | 0 | 0 | 443 (361–1185) | – |
| Wall, `--memory=16g` (s) | 14.13 (14.13–14.18) | 10.06 (10.05–10.20) | 7.69 (7.66–7.71) | 5.19 (5.16–5.23) |
| Peak cgroup memory, 16g (MB) | 5922 | 6567 | 8245 (8179–8269) | 8233 |
| Re-read on export, 16g (MB) | 0 | 0 | 263 (95–351) | 0 |

| Paired difference (s) | 8g | 16g |
|---|---|---|
| `--jobs 2` − `--jobs 1` | −3.87 (−3.99; −3.25) | −4.07 (−4.08; −3.98) |
| Default − `--jobs 2` | **−1.47** (−2.12; −1.42) | **−2.37** (−2.53; −2.34) |
| Default − `--jobs 1` | −5.37 (−5.41; −5.34), −38% | −6.44 (−6.51; −6.42), −46% |

- **The default beats `--jobs 2` at both memory limits** (−1.5 and −2.4 s),
  and `--jobs 2` beats `--jobs 1` (−3.9 to −4.1 s). At no point did lowering
  `--jobs` make the build faster.
- The natural memory peak of the parallel walk on W3 is **8.2–8.3 GB**
  (measured under a 16 GB limit, including page cache), just above the main
  matrix's 8 GB limit. `--jobs 2` needs 6.0–6.6 GB, `--jobs 1` 5.4–6.0 GB.
- Raising the limit to 16 GB made the default 1.1 s faster (7.69 against
  8.76 s, two consecutive sets; main matrix at 8 GB: 7.99 s). Export still
  re-read 95–351 MB even when the cgroup was below its limit, so part of the
  reclaim is **machine-wide** (shared machine, about 9 GB free), as suspected
  in experiment 07 §4.3. This 8 GB set re-read more than the main matrix (443
  against 185 MB) and was 0.8 s slower: the machine-wide page-cache state
  changes over time and cannot be reproduced exactly.
- **[inferred]** In walk mode each parallel stage keeps in page cache the
  extracted base (232 MB), what its command wrote (0.7–1.1 GB for W3), and
  then reads all of it back for hashing and tar. A memory-aware scheduler
  should estimate "dirty bytes plus bytes still to be re-read" for each
  running stage, not just RSS.

### Correctness

Smoke tests at 32 CPUs, one run per variant (raw data
`perf/results/smoke/<W>-<config>/`, comparison in
`perf/results/smoke/image-check.txt`):

| W | Compared with `wb-j1` | Paths | Metadata differences | Content differences | Layer statistics (files, dirs, whiteouts, tar size) |
|---|---|---|---|---|---|
| W1 | `wb-j0`, `wn-j0`, `wn-j0-nullf`, `wp-j0` | 262,240 = 262,240 (one npm log file name differs) | 10–14, all file sizes in `/tmp/node-compile-cache` | 1654, exactly the non-deterministic group of experiment 07 §3 (`/root/.local/share` 1092, `/tmp/node-compile-cache` 553, `/root/.npm/_cacache` 2, `.modules.yaml`, `.pnpm-workspace-state-v1.json`, `/etc/hostname`) | 8/8 layers **identical** |
| W3 | `wb-j0`, `wn-j0`, `wp-j0`, `wb-j0-copy`, `wb-j0-nosync` | 7295 = 7295 | 0 | 5–6: `/out/{a,b,c,all}.txt` (contain `du -sk node_modules`, which differs by a few KB between runs as in experiment 08 §2; the package counts 21/19/20 are the same in every variant), `/etc/hosts`, `/etc/hostname` (from the `listing.sh` container) | 10/10 layers **identical**, including the 3 intermediate `pnpm install` layers (142,276 / 71,782 / 105,966 files) |
| W3 | `wb-j0-lazy` | As above | 0 | As above | The 4 layers of `final` identical; the 6 layers of `a`/`b`/`c` are not produced (as lazy intends) |

Additional checks on main-matrix builds (W1 32 CPUs round 1: `wb-j0`,
`wn-j0`, `wp-j0`, `wn-j0-nosync`; W3 4 CPUs round 1: `wn-j0`, `wp-j0`,
`wn-j0-lazy`, `wn-j0-copy`; each against `wb-j1` of the same set) gave the
same result (`perf/results/image-check-matrix.txt`).

**Running stages in parallel on private rootfs does not change image
content**, in all three sandboxes; parallelism changes only the timing, not
the diff of each command. Across the main matrix
(`perf/results/layer-check.txt`, `layercheck.py`), the file, directory and
whiteout counts of every layer were **identical** to `wb-j1` round 1 in all
192 walk runs (including warm-up). Tar sizes differed by 512–2048 bytes in 9
runs, including sequential configurations (`wb-j1`, `wn-j1`), so the cause is
the non-deterministic pnpm/npm content above, not parallelism.

No `RUN` of W1 or W3 broke for lack of `/proc` (node, npm, pnpm, find, sed,
xargs, touch, chmod, du, cat).

### Bugs found

1. **`nocap` added an `/etc` directory entry to every `RUN` layer.** Copying
   `/etc/hosts` and `/etc/resolv.conf` in and restoring them changes the mtime
   of `/etc`; the walk hashes directory mtimes, so it saw `/etc` as changed in
   every `RUN` (W1: one extra directory per layer; W3: tar 512 bytes larger
   per layer). Fix: record the mtime of `/etc` before copying (m0) and after
   copying (m1); after the `RUN`, if the mtime is still m1 (the command did
   not touch `/etc`), restore m0, otherwise keep the value the command set.
   After the fix, layer statistics matched `bind` byte for byte. **Lesson for
   ranikust:** everything the tool places in the rootfs during a `RUN`
   (`hosts`, `resolv.conf`, stub mount points, `/dev` nodes) must be restored
   **including the parent directory's metadata**, or it leaks into the layer.
   kaniko does not hit this because it uses the container's own mounts.
2. **`nocap` lacked `/dev/null`** (`mknod` returns `EPERM` under rootless
   podman). W1's `RUN` (b) printed `xargs: '/dev/null': No such file or
   directory` twice. GNU xargs opens `/dev/null` as stdin for its children;
   the error does not stop xargs (it continues with stdin closed), so `sed`
   still ran, the layer was correct and the `RUN` returned 0. A command that
   **writes** `> /dev/null` creates a regular file `/dev/null` in the rootfs
   (ignored, because `/dev` is in the ignore list, but it keeps growing); a
   command that **reads** `/dev/zero` or `/dev/urandom` breaks. `-null-file`
   (an empty regular `/dev/null`) silenced xargs and left the image unchanged.
   **[inferred, not tested on real rootful Docker]** Rootful Docker grants
   `CAP_MKNOD` and its default device cgroup allows `mknod` of any character
   device (only read and write are restricted), so `nocap` can create a real
   `/dev` there. Rootless podman blocks it because device `mknod` needs
   `CAP_MKNOD` in the initial user namespace.
3. **Memory.** Parallel W3 needs about 8.2 GB including page cache. Under an
   8 GB limit it hit the ceiling in 46 of 60 runs, so export had to re-read
   from disk (see [Cost of a per-stage rootfs](#cost-of-a-per-stage-rootfs)).

## Findings

1. Without any mount, a private rootfs directory per stage with bare `chroot`
   runs W1 and W3 correctly in an emulated default Docker container; only
   `CAP_SYS_CHROOT` is needed ([Approaches tried](#approaches-tried),
   [Correctness](#correctness)).
2. Parallel stages with private rootfs cut W3 by 41–44% at 32 CPUs (−5.8 to
   −6.1 s) and 21% at 4 CPUs (−3.6 s), more than in overlay mode (−2.5 s,
   −29% and −0.5 s, −5% in experiment 08), because base extract, walk/hash
   and tar/gzip overlap ([Where the gain comes from](#where-the-gain-comes-from-w3)).
3. On W1, a chain of stages, parallelism hardly helps (−0.3 to −1.0 s,
   −2 to −7%; the min–max range contains 0 for nocap at both CPU levels and
   for bind at 32 CPUs): only `assets` (0.9 s) and its base extract overlap
   ([Performance](#performance-w3-and-w1)).
4. Bare `chroot` is as fast as bind mounts (−0.36 to +0.35 s; W1 4 CPUs
   +0.79 s unexplained) and produces the same image
   ([Performance](#performance-w3-and-w1), [Correctness](#correctness)).
5. Lazy plus parallel is the fastest W3 configuration, faster than parallel
   overlay lazy (5.38 against 6.29 s at 32 CPUs, 10.02 against 10.64 s at
   4 CPUs): overlay slows the `pnpm install` itself (longest command 4.99
   against 4.15 s), and lazy walk does not snapshot intermediate stages.
   Without lazy, the parallel walk that exports every layer is still 1.7–3.1 s
   slower than parallel overlay lazy. Overlay wins only when stages need
   layers (W1, experiment 07)
   ([Performance](#performance-w3-and-w1), [appendix](#appendix-full-tables)).
6. Without `/proc`, commands either fail (`cat /proc/self/status`) or silently
   return wrong values (`ps` empty, `os.cpus().length == 0`); under rootless
   podman `/dev` cannot be created, while rootful containers can `mknod` it
   ([Compatibility matrix](#compatibility-matrix),
   [Capability probe](#capability-probe)).
7. proot restores `/proc` and `/dev` and runs every `RUN` of W1 and W3
   correctly, but costs +26 to +56% wall (+3.5 to +6.2 s), ×1.3 (npm) to ×5
   (`find` over 250k files) per command, and hangs apt without
   `APT::Sandbox::User=root`; on W3 at 4 CPUs it erases the parallel gain.
   It is a correct fallback (full `/proc` and `/dev`), not a fast path
   ([proot](#proot), [proot on real builds](#proot-on-real-builds)).
8. fakechroot cannot work for arbitrary `RUN` (static binaries bypass it); a
   seccomp user-notify supervisor is possible but would be a proot-sized
   project ([Approaches tried](#approaches-tried)).
9. A hardlink farm is unsafe: one `echo >>` in one stage changed the store and
   the sibling stage; apk, dpkg and `sed -i` are safe because they write a
   temp file and rename it ([Rootfs preparation](#rootfs-preparation)).
10. Per-stage extract or `cp -a` costs 0.05–0.17 s in the compatibility
    measurements and 0.62–0.64 s per node base in the builds (about 1 s on
    the critical path when overlapped); `cp -a` from a shared copy is not
    faster; on xfs and btrfs, `cp -a` reflinks and adds only 3–12 MB
    ([Rootfs preparation](#rootfs-preparation),
    [Cost of a per-stage rootfs](#cost-of-a-per-stage-rootfs)).
11. With bare `chroot`, a malicious `RUN` escaped and read and wrote a
    parallel stage's rootfs; accidental writes stay inside the stage's own
    rootfs ([Isolation](#isolation)).
12. On CPU alone the default `--jobs` is right: it was never slower than
    `--jobs 1` beyond the noise, in all 4 main sets and all three sandboxes,
    and `--jobs 2` was always slower than the default (by 1.5–2.4 s). Memory,
    including page cache, binds first: parallel W3 needs 8.2–8.3 GB against
    7.5 GB for overlay lazy and 5.4–6.0 GB sequential, and under an 8 GB
    limit export re-read hundreds of MB
    ([`--jobs` and memory](#--jobs-and-memory)).
13. Anything the tool writes into the rootfs around a `RUN` must have its
    parent directory's metadata restored, or it leaks into every layer
    ([Bugs found](#bugs-found)).
14. Skipping `syncfs` and `-prep copy` are not worth making default: the
    `syncfs` gain (0.3–0.75 s) has a min–max range containing 0 in 3 of 4
    sets, and `-prep copy` is no faster and costs 232 MB more; lazy is the
    real improvement (−2.8 to −3.6 s) ([Cheap variants](#cheap-variants)).

## Implications for ranikust

**Final decision.** Kaniko mode is **sequential on `/` by default**, as in
kaniko, so that `RUN` has a real `/proc` and `/dev`. **`--parallel-stages`**
opts in to one directory per stage with bare `chroot`
([ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)). When the
flag is set, the tool logs at build start that `RUN` has no `/proc`, and the
user documentation says so. The maintainer briefly chose parallel by default
on 2026-10-29 and then reverted it because of the `/proc` risk
([survey 05](../survey/05-decisions.md), Q12).

**Superseded recommendations.** The original reports proposed, at different
points: a `--kaniko-parallel=chroot|proot` flag with `chroot` recommended and
`proot` for stages that need `/proc`; enabling parallelism automatically when
the DAG has two or more heavy independent stages **[inferred]**, after a wider
compatibility matrix (pip, Java, cargo…); and at one stage parallel by
default. All of these are **superseded** by ADR-0014: there is one opt-in flag,
`--parallel-stages`, no automatic enabling, and no proot option
([ADR-0018](../../adr/0018-no-proot-in-1-0.md)).

Kaniko mode splits into two cases, depending on whether mounting works
**[inferred]**:

| Case | Example | Stage rootfs | Parallel |
|---|---|---|---|
| Root, **can mount** but cannot use overlay | Workdir on NFS or FUSE; upper on the container's overlay without a volume; kernel without overlay | One directory per stage with a full sandbox (bind `/proc`, `/dev`, `pivot_root`), as in overlay mode | **Default**; loses nothing against overlay apart from walk + hash |
| Root, **cannot mount** | Default Docker, ordinary Kubernetes pod | Default: one rootfs, `/`, as in kaniko. With `--parallel-stages`: one directory per stage plus bare `chroot` | **Opt-in** with the flag ([ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)) |

The three paths for `RUN` in kaniko mode, measured here:

| Path | Needs | `/proc` | `/dev` | Speed (W1/W3) |
|---|---|---|---|---|
| Rootfs = the container's `/`, sequential (kaniko today; ranikust default) | Root | Yes (the container's) | Yes | Baseline; cannot run stages in parallel |
| Subdirectory + bare `chroot` (`nocap`; `--parallel-stages`) | Root + `CAP_SYS_CHROOT` | **No** | `mknod` where allowed (rootful Docker: yes; rootless podman: no) | Same as `bind`; parallel −41 to −44% on W3 |
| Subdirectory + proot (not in 1.0) | `ptrace` (allowed by Docker's default seccomp from kernel 4.8 **[external]**, moby profile above; ran in E-dock) | Yes (path translation) | Yes | +26 to +56% against `chroot` |

Design consequences:

1. **Drop the statement "kaniko mode lets only one stage touch the rootfs at a
   time"** from [survey 06](../survey/06-overlay-and-parallelism.md) §5. Kaniko
   mode can place each stage's rootfs under the workdir and follow the DAG as
   overlay mode does. Its `StageRootfs` is "one plain directory per stage",
   and `release()` deletes that directory
   ([kaniko mode](../../design/kaniko-mode.md)).
2. **Per-stage rootfs by extract or `cp -a`** (reflink where the filesystem
   offers it), **never a hardlink farm**
   ([ADR-0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md)).
3. **proot stays out of 1.0**: a third binary under GPL, slow, and with its
   own traps such as apt ([ADR-0018](../../adr/0018-no-proot-in-1-0.md)).
4. **Lazy export is the default in every mode** (proposed in experiment 08).
   It matters even more in parallel walk mode: −2.8 to −3.6 s and 460 MB less
   peak disk.
5. **The scheduler must limit concurrency by memory, including page cache**,
   not only by `--jobs` ([scheduler](../../design/scheduler.md),
   [survey 06](../survey/06-overlay-and-parallelism.md) §4.3, experiment 08).
   In walk mode the page cache of private rootfs and blobs weighs more than in
   overlay mode (W3: 8.2–8.3 GB parallel walk, 7.5 GB parallel overlay lazy,
   5.4–6.0 GB sequential walk; `--jobs 2` 6.0–6.6 GB). Keep the default of
   one job per core ([ADR-0013](../../adr/0013-jobs-default-available-parallelism.md)),
   but reduce parallelism when `memory.current` approaches `memory.max` (or
   `MemAvailable` is low).
6. **With `--parallel-stages`, the tool must**: try `mknod` for the `/dev`
   nodes and otherwise create a regular-file `/dev/null` and log a warning;
   restore `/etc` metadata after each `RUN`; warn that `RUN` has no `/proc`;
   limit concurrent stages by memory.
7. **Everything the tool places in the rootfs during a `RUN` must be restored
   including its parent directory's metadata** (bug 1), and kaniko-mode layers
   need a test that "a `RUN` that changes nothing yields an empty layer".
8. **A fake `/proc` on the `chroot` path** ([experiment 12a](12a-fake-proc.md),
   measured there). Without `/proc`, `next build` fails immediately and Java
   cannot start because it needs `/proc/self/exe`; under `--cpus=4`, jest and
   webpack ignore the CPU quota (jest uses 20 workers instead of 3, webpack is
   51% slower). Copying 13 static files from `/proc` and `/sys` into the
   rootfs fixes `next build`, the CPU count and the quota, but not `/dev/fd`
   or `ps`. On real builds ([experiment 14](14-real-builds-without-proc.md)),
   `next build` of the W1 app was correct with the fake `/proc`; Java ran on
   the Temurin image but not with a JDK installed from apt or apk; TypeScript
   7 failed.
9. **Isolation:** parallel stages do not make kaniko mode worse against
   malicious code, but a secret of one stage (for example a future
   `--mount=type=secret`) is readable from another. Untrusted code belongs in
   overlay mode with a user namespace
   ([ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md)).
10. `syncfs` before the walk can be skipped when the workdir is on a local
    filesystem; the gain is small, so this is low priority.

## Limitations and open questions

- **E-dock is rootless podman emulating Docker**, not real rootful Docker:
  `mknod` was blocked here but is allowed under rootful Docker (confirmed with
  rootful podman using the same capabilities and seccomp profile). Real
  Kubernetes pods (containerd, AppArmor `cri-containerd.apparmor.d`) were not
  tested; AppArmor might block proot's `ptrace` **[external, not tested;
  no source recorded in the original]**. Follow-up:
  [experiment 13](13-docker-rootful-vm.md) ran real rootful Docker in a VM.
- In the performance part, `bind` and overlay ran in `--privileged` and
  `nocap`/`proot` in E-dock, so `nocap − bind` mixes the sandbox difference
  with the container's seccomp and capability difference. The result was close
  to 0, so the two were not separated.
- `ovbuild` is not a full builder (experiment 07 §6): no cache, no
  `.dockerignore`, no `USER`, and an ignore list of only 5 fixed paths. W1 and
  W3 contain no command that needs a real `/proc`; other workloads may break
  under `nocap`.
- One machine, one NVMe drive, a loopback registry and a warm verdaccio cache.
  **No real network** was measured, where parallel base downloads might gain
  more. W3 has only 3 independent stages, so more stages than cores was not
  tested.
- The machine was shared between the two parts during the measurements; only
  the wait-for-idle step protected each run.
- 5 rounds per cell (3 in the `--jobs` and memory sets); no bootstrap
  confidence intervals.
- proot was measured only in its default mode (seccomp filtering) on real
  builds; `PROOT_NO_SECCOMP` was slower on syscall-heavy commands in the
  micro-benchmark.
- Compatibility with pip, maven/gradle, cargo, go, jest/terser (tools that
  choose worker counts from `os.cpus()`), and commands that read
  `/dev/urandom` or `/dev/zero` under bare `chroot`, were not tested here.
  Follow-up: [experiment 12a](12a-fake-proc.md) and
  [experiment 14](14-real-builds-without-proc.md).
- Whether npm and pnpm write in place was not measured (the inode test
  watched the wrong path).
- Only the naive proot escape was shown to be blocked; concrete bypasses were
  not tried.
- `nobody` under proot does not work: `-i` only fakes the uid and does not
  apply kernel permissions.
- The detailed cause of apt hanging under proot was not found.

## Reproduction

PoC sources are archived outside this repository, under
`docs-rust/poc/12-kaniko-mode-parallel/`.

**Compatibility part** (`compat/`, rootless podman, no root except
`rootful.sh`):

- `lib.sh` defines the shared `edock` wrapper (14 capabilities, moby seccomp,
  host network, `verdaccio`/`registry` host entries, the base tarballs mounted
  read-only).
- Tools, each source with its static build under `tools/`: `probe.c`
  (`tools/probe`, capability and syscall probe), `chrun.c` (`tools/chrun`,
  `chroot` + `setuid` + `exec` for approach A), `escape.c` (`tools/escape`,
  chroot escape); also `tools/proot` with `tools/proot.url`, and
  `tools/moby-seccomp.json`.
- Scripts, one per sub-experiment, each with an inner script run inside the
  container: `scripts/01-probe.sh` (probe), `scripts/02-matrix.sh` and
  `02-matrix-inner.sh` (matrix for approaches A and C; the matrix container's
  log is `out/02-matrix.log`), `scripts/03-proot.sh` (proot overhead),
  `scripts/04-isolation.sh`, `scripts/05-prep.sh` (rootfs preparation),
  `scripts/05b-strace.sh` (in-place versus temp + rename),
  `scripts/06-other.sh` (other ideas), `scripts/dbg-apt.sh` (apt under proot).
- `rootful.sh` (run through `pkexec`): real-root `mknod`, rootful podman
  E-dock probe, reflink on xfs/btrfs loop images.
- `fixtures/`: `package.json` and lockfiles for pnpm (W3 `app-par/a` and a
  small project). Raw output in `out/`, per-cell logs in `out/cells/`.

**Performance part** (`perf/`, rootless podman, no real root):

```sh
./run.sh build                          # bin/ovbuild (Go 1.26.9 in golang image 5d8d6618be1c)
./all.sh                                # 4 main sets: w3-cpu32 w3-cpu4 w1-cpu32 w1-cpu4 (5 rounds + warm-up)
./all.sh w3-jobs-cpu32 w3-mem16-cpu32   # --jobs and memory sets (3 rounds + warm-up)
./run.sh one results/smoke/w3-wn-j0 wn-j0 app-par 0          # a single run
./check.sh results/smoke/check results/smoke/w1-wb-j1 results/smoke/w1-wn-j0   # compare images
python3 -I layercheck.py results/w3-cpu32                    # compare layer statistics of all runs
python3 -I summarize.py results/w3-cpu32 results/w3-cpu4 results/w1-cpu32 results/w1-cpu4 results/w3-jobs-cpu32 results/w3-mem16-cpu32 > results/tables.md
python3 -I tl.py results/w3-cpu32/wn-j0-1                    # timeline of one run
./run.sh clean                          # delete volumes k12p-work, k12p-gomod and k12p-* repositories
```

- Requires the `nextjs-kaniko` compose project running (`nk-registry`,
  `nk-verdaccio`, network `nextjs-kaniko_internal`), the image
  `localhost/kaniko-bench:stream` (for `k-stream`), and
  `../compat/tools/proot` for `wp-*`.
- `check.sh` needs the images to still be in the registry. The `k12p-*`
  repositories were deleted with `./run.sh clean` after the measurements
  (orphaned blobs remain in `nk-registry`; no GC was run, as in experiment
  07). The intermediate listing files (about 460 MB per comparison) were also
  deleted; only the comparison results were kept
  (`results/smoke/image-check.txt`, `results/image-check-matrix.txt`).
- Files: `main.go` (extended `ovbuild`), `run.sh` (harness), `all.sh`,
  `summarize.py`, `tl.py` (timeline), `check.sh` + `listing.sh` +
  `cmp-listing.py` (image comparison), `layercheck.py`, `moby-seccomp.json`;
  results in `results/<set>/<config>-<round>/` (`run.json`, `build.log`,
  `ovbuild.json` or `metrics.json`, `RUN` logs in `logs/`, `du.txt` in round
  1), `results/<set>/run.log`, `results/all.log`, `results/tables.md`,
  `results/layer-check.txt`, `results/smoke/`.

## References

1. [Experiment 02: RUN sandbox](02-run-sandbox.md): the `chroot` escape and
   the moby seccomp download.
2. [Experiment 06a: Root re-run](06a-root-rerun.md): base rootfs tarballs.
3. [Experiment 07: kaniko vs overlay](07-kaniko-vs-overlay.md): `ovbuild`,
   harness, listing, non-deterministic content (§3), re-reads (§4.3),
   limitations (§6).
4. [Experiment 08: Parallel stages and lazy export](08-parallel-stages-lazy-export.md):
   overlay-mode parallel gains, lazy export, memory.
5. [Experiment 12a: Fake /proc](12a-fake-proc.md).
6. [Experiment 13: Rootful Docker in a VM](13-docker-rootful-vm.md).
7. [Experiment 14: Real builds without /proc](14-real-builds-without-proc.md).
8. [Survey 05: Decisions](../survey/05-decisions.md), Q12.
9. [Survey 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md),
   §4.3 and §5.
10. ADRs: [0003](../../adr/0003-dockerfile-trust-and-default-isolation.md),
    [0013](../../adr/0013-jobs-default-available-parallelism.md),
    [0014](../../adr/0014-kaniko-mode-sequential-by-default.md),
    [0017](../../adr/0017-stage-rootfs-by-copy-not-hardlinks.md),
    [0018](../../adr/0018-no-proot-in-1-0.md).
11. `kaniko@44aa7ddc0 pkg/snapshot/snapshot.go:240-266` (`syncfs` before
    each scan).
12. moby seccomp profile,
    `https://raw.githubusercontent.com/moby/profiles/main/seccomp/default.json`,
    accessed 2026-10-29.
13. proot v5.5.0,
    `https://github.com/proot-me/proot/releases/download/v5.5.0/proot`,
    accessed 2026-10-29.

## Appendix: full tables

Full per-set results of the performance part, from `perf/results/tables.md`.
Median (min–max) over the numbered rounds, warm-up discarded. Differences are
paired by round, median (min; max). In the breakdown, "baseline scan" is the
scan that records the reference state before a stage's first command, and the
`pnpm` "span" runs from the first `pnpm install` start to the last one's end.

### `w3-cpu32` (5 rounds)

| Configuration | Host wall (s) | Process (s) | Peak cgroup memory (MB, host) | Peak workdir usage (MB, statfs) | Final workdir `du` (MB) |
|---|---|---|---|---|---|
| optimized kaniko (`k-stream`) | 19.88 (19.77–20.08) | 19.67 (19.56–19.86) | 4230 (4166–4256) | – | – |
| overlay lazy, default `--jobs` | 6.29 (5.77–6.60) | 5.93 (5.54–6.25) | 7572 (7215–7642) | 2128 (2128–2129) | 2129 |
| walk bind, `--jobs 1` (= sequential) | 14.04 (13.91–14.23) | 13.84 (13.72–14.02) | 5288 (5228–5931) | 3278 (3278–3278) | 3279 |
| walk bind, default `--jobs` | 8.35 (8.03–8.55) | 8.15 (7.82–8.35) | 8192 (8173–8192) | 3278 (3278–3278) | 3279 |
| walk nocap (E-dock), `--jobs 1` | 14.18 (13.97–14.47) | 14.00 (13.77–14.27) | 5578 (5533–5934) | 3278 (3278–3279) | 3278 |
| walk nocap (E-dock), default `--jobs` | 7.99 (7.80–8.29) | 7.80 (7.59–8.08) | 8192 (8185–8192) | 3278 (3278–3278) | 3279 |
| walk proot (E-dock), default `--jobs` | 11.58 (10.70–11.82) | 11.39 (10.50–11.63) | 8115 (7878–8192) | 3278 (3278–3278) | 3279 |
| walk nocap lazy, `--jobs 1` | 9.61 (9.51–9.65) | 9.42 (9.34–9.47) | 5322 (4513–5354) | 2819 (2818–2819) | 2820 |
| walk nocap lazy, default `--jobs` | 5.38 (5.21–5.89) | 5.19 (5.03–5.70) | 8192 (8192–8192) | 2819 (2819–2819) | 2819 |
| walk nocap, default `--jobs`, no syncfs | 7.30 (7.29–8.19) | 6.97 (6.93–7.78) | 8192 (8192–8192) | 3278 (3278–3278) | 3279 |
| walk nocap, default `--jobs`, `-prep copy` | 8.26 (7.85–8.78) | 8.06 (7.64–8.58) | 8192 (8192–8192) | 3509 (3509–3509) | 3510 |

Host wall-time differences, paired by round, median (min; max):

| | Δ (s) | Δ % |
|---|---|---|
| bind: default − `--jobs 1` | −5.76 (−5.89; −5.49) | −41 (−42; −39) |
| nocap: default − `--jobs 1` | −6.12 (−6.67; −5.88) | −44 (−46; −42) |
| nocap lazy: default − `--jobs 1` | −4.27 (−4.34; −3.72) | −44 (−45; −39) |
| nocap default − bind `--jobs 1` (parallel + sandbox + E-dock) | −6.13 (−6.29; −5.63) | −43 (−44; −40) |
| nocap − bind (both at default `--jobs`) | −0.36 (−0.75; +0.26) | −4 (−9; +3) |
| proot − nocap (default `--jobs`) | +3.53 (+2.76; +3.83) | +44 (+35; +48) |
| proot default − bind `--jobs 1` | −2.47 (−3.53; −2.29) | −18 (−25; −16) |
| nocap: lazy − export all layers | −2.76 (−2.91; −1.91) | −34 (−35; −24) |
| nocap: no syncfs − with syncfs | −0.63 (−0.99; +0.39) | −8 (−12; +5) |
| nocap: `-prep copy` − extract | +0.27 (−0.44; +0.98) | +3 (−5; +13) |
| walk nocap default − overlay lazy default | +1.69 (+1.51; +2.17) | +26 (+24; +38) |
| walk nocap lazy default − overlay lazy default | −0.90 (−1.22; −0.23) | −14 (−19; −4) |
| walk bind `--jobs 1` − overlay lazy default | +7.72 (+7.32; +8.46) | +123 (+111; +147) |
| walk nocap default − optimized kaniko | −11.83 (−12.05; −11.80) | −60 (−61; −59) |
| walk nocap lazy default − optimized kaniko | −14.57 (−14.70; −13.96) | −73 (−74; −70) |
| walk proot default − optimized kaniko | −8.28 (−9.07; −8.05) | −42 (−46; −41) |

Breakdown (s, median; summed over all stages, so a parallel total can exceed
the wall time):

| Configuration | Rootfs prep (Σ) | Baseline scan (Σ) | RUN/COPY commands (Σ) | RUN pnpm (Σ / longest / span) | Other RUN (Σ) | syncfs (Σ) | Walk+hash (Σ) | tar+gzip (Σ) | Last stage starts at | Push | Re-read on export (MB) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| overlay lazy, default `--jobs` | 0.00 | 0.00 | 13.41 | 13.41 / 4.99 / 4.99 | 0.00 | 0.00 | 0.66 | 0.00 | 5.73 | 0.17 | 0 (0–0) |
| walk bind, `--jobs 1` (= sequential) | 2.54 | 0.12 | 6.44 | 6.43 / 2.32 / 9.65 | 0.00 | 0.79 | 1.32 | 2.57 | 13.57 | 0.15 | 0 (0–0) |
| walk bind, default `--jobs` | 3.47 | 0.27 | 13.30 | 13.29 / 4.70 / 4.70 | 0.00 | 0.46 | 1.88 | 3.94 | 7.77 | 0.15 | 49 (0–121) |
| walk nocap (E-dock), `--jobs 1` | 2.56 | 0.12 | 6.41 | 6.41 / 2.34 / 9.14 | 0.00 | 0.80 | 1.31 | 2.61 | 13.69 | 0.15 | 0 (0–0) |
| walk nocap (E-dock), default `--jobs` | 3.49 | 0.26 | 12.03 | 12.01 / 4.40 / 4.40 | 0.00 | 0.70 | 2.03 | 3.88 | 7.39 | 0.15 | 185 (64–384) |
| walk proot (E-dock), default `--jobs` | 3.48 | 0.26 | 21.07 | 21.06 / 8.27 / 8.27 | 0.00 | 0.51 | 1.36 | 2.69 | 11.07 | 0.15 | 0 (0–0) |
| walk nocap lazy, `--jobs 1` | 2.49 | 0.03 | 6.51 | 6.50 / 2.43 / 6.50 | 0.00 | 0.16 | 0.03 | 0.00 | 9.03 | 0.15 | 0 (0–0) |
| walk nocap lazy, default `--jobs` | 2.78 | 0.03 | 11.90 | 11.88 / 4.15 / 4.15 | 0.00 | 0.18 | 0.04 | 0.00 | 4.80 | 0.15 | 0 (0–0) |
| walk nocap, default `--jobs`, no syncfs | 2.51 | 0.25 | 11.24 | 11.22 / 3.99 / 4.00 | 0.00 | 0.00 | 2.11 | 3.83 | 6.75 | 0.17 | 123 (46–203) |
| walk nocap, default `--jobs`, `-prep copy` | 1.65 | 0.25 | 12.62 | 12.60 / 4.37 / 4.37 | 0.00 | 0.45 | 1.78 | 4.00 | 7.67 | 0.16 | 233 (79–360) |

1-minute loadavg right before each run: median 3.40, min 1.24, max 5.63.

### `w3-cpu4` (5 rounds)

| Configuration | Host wall (s) | Process (s) | Peak cgroup memory (MB, host) | Peak workdir usage (MB, statfs) | Final workdir `du` (MB) |
|---|---|---|---|---|---|
| optimized kaniko (`k-stream`) | 22.03 (21.95–22.21) | 21.83 (21.74–22.00) | 3110 (3102–3131) | – | – |
| overlay lazy, default `--jobs` | 10.64 (10.49–10.81) | 10.28 (10.16–10.45) | 7480 (7450–7608) | 2128 (2128–2128) | 2129 |
| walk bind, `--jobs 1` (= sequential) | 17.14 (17.04–17.37) | 16.94 (16.86–17.18) | 5345 (5176–5854) | 3278 (3278–3278) | 3279 |
| walk bind, default `--jobs` | 13.45 (13.39–13.72) | 13.25 (13.19–13.50) | 8148 (7980–8192) | 3278 (3278–3278) | 3279 |
| walk nocap (E-dock), `--jobs 1` | 17.24 (17.12–17.29) | 17.05 (16.93–17.10) | 5531 (5226–5919) | 3278 (3278–3278) | 3279 |
| walk nocap (E-dock), default `--jobs` | 13.74 (13.56–14.40) | 13.54 (13.37–14.20) | 8192 (7996–8192) | 3278 (3278–3278) | 3279 |
| walk proot (E-dock), default `--jobs` | 17.34 (17.26–17.46) | 17.14 (17.08–17.26) | 7929 (7805–8059) | 3278 (3278–3278) | 3279 |
| walk nocap lazy, `--jobs 1` | 12.20 (12.05–12.30) | 12.02 (11.87–12.12) | 4841 (4482–5362) | 2819 (2819–2819) | 2819 |
| walk nocap lazy, default `--jobs` | 10.02 (9.98–10.17) | 9.84 (9.80–9.99) | 8192 (8192–8192) | 2819 (2819–2819) | 2820 |
| walk nocap, default `--jobs`, no syncfs | 13.56 (13.32–14.55) | 13.16 (12.89–14.17) | 8192 (8129–8192) | 3278 (3278–3278) | 3279 |
| walk nocap, default `--jobs`, `-prep copy` | 13.82 (13.73–13.88) | 13.62 (13.52–13.67) | 8192 (8192–8192) | 3509 (3509–3510) | 3510 |

Host wall-time differences, paired by round, median (min; max):

| | Δ (s) | Δ % |
|---|---|---|
| bind: default − `--jobs 1` | −3.66 (−3.75; −3.38) | −21 (−22; −20) |
| nocap: default − `--jobs 1` | −3.55 (−3.65; −2.84) | −21 (−21; −16) |
| nocap lazy: default − `--jobs 1` | −2.17 (−2.32; −1.88) | −18 (−19; −16) |
| nocap default − bind `--jobs 1` (parallel + sandbox + E-dock) | −3.30 (−3.55; −2.97) | −19 (−21; −17) |
| nocap − bind (both at default `--jobs`) | +0.22 (+0.08; +0.69) | +2 (+1; +5) |
| proot − nocap (default `--jobs`) | +3.53 (+2.94; +3.87) | +26 (+20; +29) |
| proot default − bind `--jobs 1` | +0.23 (−0.04; +0.32) | +1 (−0; +2) |
| nocap: lazy − export all layers | −3.57 (−4.42; −3.54) | −26 (−31; −26) |
| nocap: no syncfs − with syncfs | −0.29 (−0.61; +0.99) | −2 (−4; +7) |
| nocap: `-prep copy` − extract | −0.00 (−0.53; +0.23) | −0 (−4; +2) |
| walk nocap default − overlay lazy default | +3.10 (+2.83; +3.91) | +29 (+26; +37) |
| walk nocap lazy default − overlay lazy default | −0.54 (−0.77; −0.47) | −5 (−7; −4) |
| walk bind `--jobs 1` − overlay lazy default | +6.40 (+6.36; +6.88) | +60 (+59; +66) |
| walk nocap default − optimized kaniko | −8.28 (−8.65; −7.63) | −38 (−39; −35) |
| walk nocap lazy default − optimized kaniko | −12.05 (−12.19; −11.85) | −55 (−55; −54) |
| walk proot default − optimized kaniko | −4.74 (−4.78; −4.62) | −21 (−22; −21) |

Breakdown (s, median; summed over all stages, so a parallel total can exceed
the wall time):

| Configuration | Rootfs prep (Σ) | Baseline scan (Σ) | RUN/COPY commands (Σ) | RUN pnpm (Σ / longest / span) | Other RUN (Σ) | syncfs (Σ) | Walk+hash (Σ) | tar+gzip (Σ) | Last stage starts at | Push | Re-read on export (MB) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| overlay lazy, default `--jobs` | 0.00 | 0.00 | 26.92 | 26.91 / 9.39 / 9.39 | 0.00 | 0.00 | 0.66 | 0.00 | 10.12 | 0.19 | 0 (0–0) |
| walk bind, `--jobs 1` (= sequential) | 2.51 | 0.12 | 9.02 | 9.01 / 3.28 / 12.04 | 0.00 | 0.81 | 1.53 | 2.75 | 16.62 | 0.15 | 0 (0–0) |
| walk bind, default `--jobs` | 3.76 | 0.37 | 25.43 | 25.42 / 8.85 / 8.85 | 0.00 | 0.42 | 2.92 | 5.74 | 12.87 | 0.15 | 0 (0–10) |
| walk nocap (E-dock), `--jobs 1` | 2.51 | 0.12 | 9.16 | 9.15 / 3.33 / 12.21 | 0.00 | 0.80 | 1.55 | 2.75 | 16.74 | 0.16 | 0 (0–0) |
| walk nocap (E-dock), default `--jobs` | 3.75 | 0.35 | 26.25 | 26.24 / 9.09 / 9.09 | 0.00 | 0.46 | 2.88 | 5.93 | 13.14 | 0.16 | 52 (0–81) |
| walk proot (E-dock), default `--jobs` | 3.77 | 0.36 | 36.21 | 36.20 / 13.66 / 13.66 | 0.00 | 0.45 | 2.02 | 3.53 | 16.83 | 0.16 | 0 (0–0) |
| walk nocap lazy, `--jobs 1` | 2.46 | 0.03 | 9.14 | 9.13 / 3.34 / 9.13 | 0.00 | 0.20 | 0.03 | 0.00 | 11.61 | 0.16 | 0 (0–0) |
| walk nocap lazy, default `--jobs` | 3.09 | 0.04 | 25.64 | 25.62 / 8.81 / 8.81 | 0.00 | 0.12 | 0.05 | 0.00 | 9.52 | 0.15 | 0 (0–0) |
| walk nocap, default `--jobs`, no syncfs | 2.90 | 0.28 | 26.54 | 26.52 / 9.24 / 9.25 | 0.00 | 0.00 | 2.93 | 6.06 | 12.90 | 0.17 | 130 (0–236) |
| walk nocap, default `--jobs`, `-prep copy` | 1.64 | 0.35 | 26.00 | 25.99 / 9.00 / 9.00 | 0.00 | 0.55 | 3.25 | 5.88 | 13.20 | 0.15 | 89 (31–128) |

1-minute loadavg right before each run: median 2.56, min 1.47, max 9.55.

### `w1-cpu32` (5 rounds)

| Configuration | Host wall (s) | Process (s) | Peak cgroup memory (MB, host) | Peak workdir usage (MB, statfs) | Final workdir `du` (MB) |
|---|---|---|---|---|---|
| walk bind, `--jobs 1` (= sequential) | 11.22 (11.05–11.86) | 11.00 (10.81–11.65) | 5059 (4951–5418) | 2727 (2727–2727) | 2376 |
| walk bind, default `--jobs` | 10.51 (10.14–11.47) | 10.29 (9.93–11.25) | 5105 (4878–5396) | 2727 (2727–2727) | 2376 |
| walk nocap (E-dock), `--jobs 1` | 11.06 (10.94–11.18) | 10.85 (10.71–10.96) | 5194 (5084–5285) | 2727 (2727–2727) | 2376 |
| walk nocap (E-dock), default `--jobs` | 10.67 (10.38–12.10) | 10.47 (10.18–11.86) | 5119 (4969–5175) | 2727 (2727–2727) | 2376 |
| walk proot (E-dock), `--jobs 1` | 17.13 (17.07–17.47) | 16.90 (16.85–17.26) | 5149 (4782–5401) | 2727 (2727–2727) | 2376 |
| walk proot (E-dock), default `--jobs` | 16.44 (16.04–16.60) | 16.24 (15.84–16.39) | 4935 (4725–5156) | 2727 (2727–2727) | 2376 |
| walk nocap, default `--jobs`, no syncfs | 10.37 (9.92–11.01) | 9.98 (9.51–10.74) | 4977 (4625–5156) | 2727 (2727–2727) | 2376 |

Host wall-time differences, paired by round, median (min; max):

| | Δ (s) | Δ % |
|---|---|---|
| bind: default − `--jobs 1` | −0.63 (−1.65; +0.42) | −6 (−14; +4) |
| nocap: default − `--jobs 1` | −0.39 (−0.73; +1.12) | −4 (−7; +10) |
| proot: default − `--jobs 1` | −0.94 (−1.08; −0.57) | −5 (−6; −3) |
| nocap default − bind `--jobs 1` (parallel + sandbox + E-dock) | −0.67 (−0.87; +1.05) | −6 (−8; +9) |
| nocap − bind (both at default `--jobs`) | +0.35 (−0.42; +0.99) | +3 (−4; +10) |
| proot − nocap (default `--jobs`) | +5.46 (+4.50; +6.15) | +51 (+37; +59) |
| proot − nocap (`--jobs 1`) | +6.18 (+5.99; +6.49) | +56 (+54; +59) |
| proot default − bind `--jobs 1` | +4.91 (+4.58; +5.55) | +44 (+39; +50) |
| nocap: no syncfs − with syncfs | −0.75 (−1.15; +0.07) | −7 (−10; +1) |

Breakdown (s, median; summed over all stages, so a parallel total can exceed
the wall time):

| Configuration | Rootfs prep (Σ) | Baseline scan (Σ) | RUN/COPY commands (Σ) | RUN pnpm (Σ / longest / span) | Other RUN (Σ) | syncfs (Σ) | Walk+hash (Σ) | tar+gzip (Σ) | Last stage starts at | Push | Re-read on export (MB) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| walk bind, `--jobs 1` (= sequential) | 1.29 | 0.06 | 3.59 | 2.85 / 2.14 / 3.06 | 0.68 | 0.39 | 2.79 | 1.91 | 7.60 | 0.71 | 0 (0–0) |
| walk bind, default `--jobs` | 1.46 | 0.08 | 3.70 | 2.89 / 2.15 / 3.02 | 0.68 | 0.39 | 2.81 | 1.92 | 6.89 | 0.75 | 0 (0–0) |
| walk nocap (E-dock), `--jobs 1` | 1.28 | 0.06 | 3.45 | 2.72 / 1.99 / 2.83 | 0.67 | 0.44 | 2.79 | 1.92 | 7.55 | 0.74 | 0 (0–0) |
| walk nocap (E-dock), default `--jobs` | 1.48 | 0.08 | 4.06 | 3.22 / 2.46 / 3.33 | 0.69 | 0.44 | 2.82 | 1.92 | 6.96 | 0.73 | 0 (0–0) |
| walk proot (E-dock), `--jobs 1` | 1.29 | 0.06 | 9.85 | 7.04 / 6.11 / 7.32 | 2.75 | 0.42 | 2.57 | 1.94 | 11.84 | 0.74 | 0 (0–0) |
| walk proot (E-dock), default `--jobs` | 1.46 | 0.08 | 10.25 | 7.47 / 6.46 / 7.58 | 2.72 | 0.41 | 2.44 | 1.95 | 11.20 | 0.76 | 0 (0–0) |
| walk nocap, default `--jobs`, no syncfs | 1.20 | 0.08 | 4.12 | 3.29 / 2.53 / 3.36 | 0.69 | 0.00 | 2.82 | 1.92 | 6.80 | 0.74 | 0 (0–0) |

1-minute loadavg right before each run: median 2.52, min 1.33, max 5.47.

### `w1-cpu4` (5 rounds)

| Configuration | Host wall (s) | Process (s) | Peak cgroup memory (MB, host) | Peak workdir usage (MB, statfs) | Final workdir `du` (MB) |
|---|---|---|---|---|---|
| walk bind, `--jobs 1` (= sequential) | 14.08 (13.63–14.75) | 13.86 (13.40–14.52) | 5008 (4721–5248) | 2727 (2727–2727) | 2376 |
| walk bind, default `--jobs` | 13.00 (12.98–13.12) | 12.78 (12.76–12.90) | 5047 (5022–5105) | 2727 (2727–2727) | 2376 |
| walk nocap (E-dock), `--jobs 1` | 14.26 (13.87–14.57) | 14.03 (13.65–14.35) | 4931 (4910–5138) | 2727 (2727–2727) | 2376 |
| walk nocap (E-dock), default `--jobs` | 13.80 (13.04–14.36) | 13.58 (12.84–14.14) | 4996 (4917–5270) | 2727 (2727–2727) | 2376 |
| walk proot (E-dock), `--jobs 1` | 18.77 (18.30–19.08) | 18.56 (18.09–18.87) | 5049 (4834–5254) | 2727 (2727–2728) | 2376 |
| walk proot (E-dock), default `--jobs` | 18.15 (17.81–18.38) | 17.93 (17.59–18.16) | 4823 (4774–5152) | 2727 (2727–2727) | 2376 |
| walk nocap, default `--jobs`, no syncfs | 12.90 (12.74–14.34) | 12.57 (12.33–13.91) | 4901 (4793–5066) | 2727 (2727–2727) | 2376 |

Host wall-time differences, paired by round, median (min; max):

| | Δ (s) | Δ % |
|---|---|---|
| bind: default − `--jobs 1` | −0.96 (−1.65; −0.65) | −7 (−11; −5) |
| nocap: default − `--jobs 1` | −0.31 (−1.41; +0.22) | −2 (−10; +2) |
| proot: default − `--jobs 1` | −0.45 (−1.07; −0.15) | −2 (−6; −1) |
| nocap default − bind `--jobs 1` (parallel + sandbox + E-dock) | −0.51 (−0.95; +0.65) | −4 (−6; +5) |
| nocap − bind (both at default `--jobs`) | +0.79 (+0.06; +1.36) | +6 (+0; +10) |
| proot − nocap (default `--jobs`) | +4.18 (+3.78; +5.34) | +30 (+26; +41) |
| proot − nocap (`--jobs 1`) | +4.51 (+4.16; +4.62) | +31 (+29; +33) |
| proot default − bind `--jobs 1` | +3.73 (+3.51; +4.76) | +26 (+24; +35) |
| nocap: no syncfs − with syncfs | −0.69 (−1.10; −0.02) | −5 (−8; −0) |

Breakdown (s, median; summed over all stages, so a parallel total can exceed
the wall time):

| Configuration | Rootfs prep (Σ) | Baseline scan (Σ) | RUN/COPY commands (Σ) | RUN pnpm (Σ / longest / span) | Other RUN (Σ) | syncfs (Σ) | Walk+hash (Σ) | tar+gzip (Σ) | Last stage starts at | Push | Re-read on export (MB) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| walk bind, `--jobs 1` (= sequential) | 1.27 | 0.06 | 5.71 | 4.88 / 4.14 / 5.23 | 0.67 | 0.34 | 3.35 | 2.25 | 10.17 | 0.70 | 0 (0–0) |
| walk bind, default `--jobs` | 1.46 | 0.10 | 5.63 | 4.84 / 4.11 / 4.96 | 0.68 | 0.36 | 3.33 | 2.27 | 9.15 | 0.70 | 0 (0–0) |
| walk nocap (E-dock), `--jobs 1` | 1.28 | 0.06 | 5.74 | 4.97 / 4.24 / 5.11 | 0.68 | 0.35 | 3.54 | 2.27 | 10.27 | 0.70 | 0 (0–0) |
| walk nocap (E-dock), default `--jobs` | 1.45 | 0.11 | 6.37 | 5.56 / 4.81 / 5.68 | 0.68 | 0.36 | 3.36 | 2.28 | 9.95 | 0.69 | 0 (0–0) |
| walk proot (E-dock), `--jobs 1` | 1.27 | 0.06 | 10.59 | 7.69 / 6.69 / 7.81 | 2.77 | 0.38 | 3.14 | 2.30 | 13.13 | 0.72 | 0 (0–0) |
| walk proot (E-dock), default `--jobs` | 1.46 | 0.10 | 10.87 | 8.08 / 7.14 / 8.20 | 2.75 | 0.39 | 3.01 | 2.29 | 12.62 | 0.73 | 0 (0–0) |
| walk nocap, default `--jobs`, no syncfs | 1.19 | 0.10 | 5.72 | 4.90 / 4.14 / 4.97 | 0.69 | 0.00 | 3.44 | 2.28 | 9.00 | 0.73 | 0 (0–0) |

1-minute loadavg right before each run: median 1.71, min 0.89, max 4.11.

### `w3-jobs-cpu32` (3 rounds)

| Configuration | Host wall (s) | Process (s) | Peak cgroup memory (MB, host) | Peak workdir usage (MB, statfs) | Final workdir `du` (MB) |
|---|---|---|---|---|---|
| walk nocap (E-dock), `--jobs 1` | 14.11 (14.08–14.17) | 13.91 (13.89–13.98) | 5604 (5382–5942) | 3278 (3278–3278) | 3278 |
| walk nocap (E-dock), `--jobs 2` | 10.24 (10.18–10.83) | 10.04 (9.99–10.64) | 6071 (6027–6526) | 3278 (3278–3278) | 3279 |
| walk nocap (E-dock), default `--jobs` | 8.76 (8.71–8.77) | 8.56 (8.52–8.58) | 7651 (7097–7879) | 3278 (3278–3278) | 3279 |

Host wall-time differences, paired by round, median (min; max):

| | Δ (s) | Δ % |
|---|---|---|
| nocap: default − `--jobs 1` | −5.37 (−5.41; −5.34) | −38 (−38; −38) |
| nocap: `--jobs 2` − `--jobs 1` | −3.87 (−3.99; −3.25) | −27 (−28; −23) |
| nocap: default − `--jobs 2` | −1.47 (−2.12; −1.42) | −14 (−20; −14) |

Breakdown (s, median; summed over all stages, so a parallel total can exceed
the wall time):

| Configuration | Rootfs prep (Σ) | Baseline scan (Σ) | RUN/COPY commands (Σ) | RUN pnpm (Σ / longest / span) | Other RUN (Σ) | syncfs (Σ) | Walk+hash (Σ) | tar+gzip (Σ) | Last stage starts at | Push | Re-read on export (MB) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| walk nocap (E-dock), `--jobs 1` | 2.56 | 0.12 | 6.34 | 6.34 / 2.37 / 9.17 | 0.00 | 0.81 | 1.31 | 2.59 | 13.63 | 0.15 | 0 (0–0) |
| walk nocap (E-dock), `--jobs 2` | 2.89 | 0.16 | 8.65 | 8.64 / 3.53 / 6.88 | 0.00 | 0.52 | 1.46 | 3.11 | 9.72 | 0.15 | 0 (0–0) |
| walk nocap (E-dock), default `--jobs` | 3.47 | 0.27 | 13.95 | 13.93 / 5.20 / 5.20 | 0.00 | 0.50 | 1.98 | 3.92 | 8.17 | 0.15 | 443 (361–1185) |

1-minute loadavg right before each run: median 2.59, min 2.11, max 4.72.

### `w3-mem16-cpu32` (3 rounds)

| Configuration | Host wall (s) | Process (s) | Peak cgroup memory (MB, host) | Peak workdir usage (MB, statfs) | Final workdir `du` (MB) |
|---|---|---|---|---|---|
| walk nocap (E-dock), `--jobs 1` | 14.13 (14.13–14.18) | 13.95 (13.95–13.98) | 5922 (5562–6028) | 3278 (3278–3278) | 3279 |
| walk nocap (E-dock), `--jobs 2` | 10.06 (10.05–10.20) | 9.87 (9.85–10.00) | 6567 (6022–6616) | 3278 (3278–3278) | 3279 |
| walk nocap (E-dock), default `--jobs` | 7.69 (7.66–7.71) | 7.49 (7.48–7.50) | 8245 (8179–8269) | 3278 (3278–3278) | 3279 |
| walk nocap lazy, default `--jobs` | 5.19 (5.16–5.23) | 5.02 (4.98–5.06) | 8233 (8064–8292) | 2819 (2819–2819) | 2819 |

Host wall-time differences, paired by round, median (min; max):

| | Δ (s) | Δ % |
|---|---|---|
| nocap: default − `--jobs 1` | −6.44 (−6.51; −6.42) | −46 (−46; −45) |
| nocap: `--jobs 2` − `--jobs 1` | −4.07 (−4.08; −3.98) | −29 (−29; −28) |
| nocap: default − `--jobs 2` | −2.37 (−2.53; −2.34) | −24 (−25; −23) |
| nocap: lazy − export all layers | −2.48 (−2.54; −2.47) | −32 (−33; −32) |

Breakdown (s, median; summed over all stages, so a parallel total can exceed
the wall time):

| Configuration | Rootfs prep (Σ) | Baseline scan (Σ) | RUN/COPY commands (Σ) | RUN pnpm (Σ / longest / span) | Other RUN (Σ) | syncfs (Σ) | Walk+hash (Σ) | tar+gzip (Σ) | Last stage starts at | Push | Re-read on export (MB) |
|---|---|---|---|---|---|---|---|---|---|---|---|
| walk nocap (E-dock), `--jobs 1` | 2.54 | 0.12 | 6.40 | 6.39 / 2.34 / 9.20 | 0.00 | 0.81 | 1.30 | 2.59 | 13.63 | 0.15 | 0 (0–0) |
| walk nocap (E-dock), `--jobs 2` | 2.85 | 0.16 | 7.85 | 7.84 / 3.12 / 6.87 | 0.00 | 0.85 | 1.89 | 2.93 | 9.59 | 0.14 | 0 (0–0) |
| walk nocap (E-dock), default `--jobs` | 3.43 | 0.27 | 11.03 | 11.01 / 3.90 / 3.90 | 0.00 | 0.56 | 2.17 | 3.97 | 7.06 | 0.15 | 263 (95–351) |
| walk nocap lazy, default `--jobs` | 2.77 | 0.03 | 11.07 | 11.06 / 3.95 / 3.96 | 0.00 | 0.22 | 0.04 | 0.00 | 4.59 | 0.15 | 0 (0–0) |

1-minute loadavg right before each run: median 2.41, min 1.84, max 4.79.
