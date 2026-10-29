# Experiment 13: Rootful Docker in a VM

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Do the mode-selection model, the parallel kaniko-mode path and the fake `/proc` hold on real rootful Docker under cgroup v2 and cgroup v1, where [experiment 12](12-kaniko-mode-parallel.md) could only simulate Docker with rootless podman? |
| **Answer** | Yes for modes, proot and parallel stages: default Docker and `--cap-add SYS_ADMIN` land in kaniko mode, unconfined seccomp plus AppArmor opens rootless overlay, `--privileged` gives full overlay, identically under v1 and v2; `mknod` works everywhere, AppArmor does not block proot, and parallel stages cut W3 by 38% with an identical image. The fake `/proc` file list, however, must be generated per cgroup version. |
| **Related** | [ADR-0002](../../adr/0002-two-snapshot-modes.md), [ADR-0004](../../adr/0004-support-every-privilege-level.md), [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md), [ADR-0016](../../adr/0016-overlay-mount-options.md), [ADR-0018](../../adr/0018-no-proot-in-1-0.md); [modes and detection](../../design/modes-and-detection.md), [kaniko mode](../../design/kaniko-mode.md); experiments [12](12-kaniko-mode-parallel.md), [12a](12a-fake-proc.md), [16](16-cgroup-v1-cpuset.md) |
| **Original** | `docs-rust/poc/13-docker-rootful-vm/` (Vietnamese, not in this repository) |

## Summary

- Real rootful Docker 29.1.3 ran inside an Ubuntu 22.04 VM (kernel 5.15),
  first under cgroup v2 and then under cgroup v1. The goal was to re-check on
  real Docker what [experiment 12](12-kaniko-mode-parallel.md) had only
  simulated with rootless podman ("E-dock": the 14 default Docker capabilities
  plus the moby seccomp profile).
- The mode table of [survey chapter 06](../survey/06-overlay-and-parallelism.md)
  is correct. Default `docker run` and `--cap-add SYS_ADMIN` cannot mount
  overlay (seccomp blocks user namespaces in the first case, AppArmor
  `docker-default` blocks mounts in the second) and land in kaniko mode.
  Unconfined seccomp and AppArmor opens rootless overlay; `--privileged` gives
  full overlay. The cgroup version changes no cell of the probe table.
- The one difference from the podman simulation: `mknod` of character devices
  works in every variant, including the default, so kaniko mode has a real
  `/dev` on rootful Docker. AppArmor `docker-default` does not block proot's
  `ptrace`.
- The parallel kaniko-mode path produced the same image as the sequential path
  and made W3 38% faster on 8 vCPUs; W1 gained 8%.
- Kernel 5.15 mounts the full ranikust overlay option set with classic
  `mount(2)`, but `fsconfig("lowerdir+")` fails at `CMD_CREATE` with `EINVAL`.
- The fake `/proc` "self" set of [experiment 12a](12a-fake-proc.md) restores
  the V0 CPU and memory counts under cgroup v2 but not under v1. Under v1 the
  quota files have other names and paths, and the list must be derived at run
  time from `/proc/self/{cgroup,mountinfo}`.

## Questions

1. Which `docker run` variants land in which ranikust mode on real rootful
   Docker, does the cgroup version change that, and is the model of survey
   chapter 06 correct?
2. Does AppArmor `docker-default` block proot's `ptrace`? (Left open by
   experiment 12.)
3. Does the parallel kaniko-mode path (`--parallel-stages`) work in default
   Docker: is `/dev` real, is the image identical to the sequential build, and
   how much faster is it?
4. Does the fake `/proc` restore the V0 CPU and memory counts of Node and Go
   under real Docker, under cgroup v2 and under cgroup v1?
5. Does kernel 5.15 accept the ranikust overlay option set, and does
   incremental `fsconfig("lowerdir+")` work there?

## Environment

The survey host (see [`README.md`](../README.md)) has only rootless podman;
its `/usr/bin/docker` is a podman shim. Real rootful Docker and cgroup v1 were
therefore obtained in a VM. Nothing on the host was changed: QEMU ran as the
unprivileged user, with `/dev/kvm` granted through an ACL.

| | cgroup v2 (default boot) | cgroup v1 (`systemd.unified_cgroup_hierarchy=0`) |
|---|---|---|
| Kernel | `5.15.0-198-generic` | same |
| Docker | 29.1.3 (Ubuntu `0ubuntu3~22.04.2`), server Go 1.24.4, containerd 2.2.1 | same |
| Storage driver | `overlayfs` | same |
| Cgroup driver / version | `systemd` / **2** | `cgroupfs` / **1** (hybrid) ¹ |
| Security options | `apparmor`, `seccomp`, `cgroupns` | `apparmor`, `seccomp` (no `cgroupns`) |
| AppArmor | enabled, 38 profiles in enforce mode, `docker-default` present | same |
| overlay module parameters | `redirect_dir=N metacopy=N index=N nfs_export=N xino_auto=Y` | same |

¹ The environment table of the original lists `systemd` for both boots. Its
own text and the raw `docker info` output (`out/env-v1.txt`) record
`cgroupfs` under v1, and [experiment 16](16-cgroup-v1-cpuset.md) measured the
same; that value is used here. **[measured]**

- **VM:** Ubuntu 22.04 (jammy) cloud image under QEMU/KVM, 8 vCPUs, 10 GB RAM.
  Image `jammy-server-cloudimg-amd64.img` from
  `cloud-images.ubuntu.com/jammy/current`, SHA-256
  `58be26aa7452f3005a4f7370a43928bfb33a7f35a77a8f3b264bf00266a17ca0`
  (matches `SHA256SUMS`, recorded in `out/00-image.txt`), 695 MB. A 40 GB
  qcow2 overlay sat on top of it, with a NoCloud seed (volume label `cidata`,
  built with xorriso). `docker.io`, `socat` and `strace` were installed from
  the Ubuntu archive.
- **Privilege level:** Ubuntu's `docker.io` is real rootful Docker (the daemon
  runs as root), not podman. Container uid 0 is root in the initial user
  namespace, unlike rootless podman; this is what decides `mknod` and `/dev`.
- **Network:** host services, the registry (`nk-registry`, port 5001) and
  verdaccio (port 4873), reached the VM at `10.0.2.2` through QEMU user
  networking. Containers reached them through socat forwarders listening on
  the Docker bridge gateway `172.17.0.1`. Caches were warm, so pull and push
  cost almost nothing, as in experiment 12. The real network was not measured.
- **Load:** the host was shared with [experiment 14](14-real-builds-without-proc.md),
  which ran at the same time. Timings are compared only within the VM.

## Method

**Probe matrix.** The static probe binary from experiment 12
(`compat/tools/probe`) ran inside `docker run <variant>`, each test in its own
child process. A separate static probe (`fsplus.c`) tested the overlay option
set and `fsconfig(lowerdir+)`; the `escape` binary from experiment 12 tested
breaking out of `chroot`. Raw output: `out/t1-v2.txt`, `out/t1-v1.txt`.

| Variant | `docker run` flags |
|---|---|
| (a) default | none |
| (b) +SYS_ADMIN | `--cap-add SYS_ADMIN` |
| (c) unconfined | `--security-opt seccomp=unconfined --security-opt apparmor=unconfined` |
| (d) privileged | `--privileged` |

**proot.** The static proot binary from experiment 12 (`compat/tools/proot`,
v5.5.0) ran in a default `docker run`; `/proc/self/attr/current` confirmed the
profile `docker-default (enforce)`. Raw output: `out/t2-v2.txt`,
`out/t2-v1.txt`.

**Parallel path.** `ovbuild`, the static Go prototype from experiment 12's
performance part, ran with `-mode walk -sandbox nocap` in a default
`docker run` (no security flags), with a Docker volume (ext4) as workdir and
`--memory=8g`. Base images were pulled from, and results pushed to, the host
`nk-registry` through socat; pnpm used verdaccio. W3 is `nextjs-kaniko/app-par`
(three independent stages) and W1 is `nextjs-kaniko/app`. `-jobs 1`
(sequential) was compared with `-jobs 0` (the default, the number of cores,
8). 3 rounds; medians of wall time. Image correctness was checked on the host
with `listing.sh` and `cmp-listing.py` from experiment 12, comparing the
`-jobs 1` and default images. Raw output: `out/t3-v2.txt`, `out/t3-v2/`,
`out/t3-image-check.txt`.

**Fake `/proc`.** `probe-node.js` (libuv and V8) and a cgroup-aware Go probe
ran under a default `docker run`. The Go probe was built with Go 1.27.3 inside
the VM; Go 1.25 and later read `cpu.max` or `cfs_quota` for `GOMAXPROCS`. The
`/dev` of the rootfs was built with `mknod` (6/6 nodes, possible because
Docker is rootful). Raw output: `out/t4-v2.txt`, `out/t4-v1.txt`,
`out/t4-v1-union.txt`.

| Variant | How it runs |
|---|---|
| **V0** | Directly in the container, with the real `/proc` |
| **V1** | `chrun` (bare `chroot`), no `/proc` |
| **V2s** | V1 plus the fake `/proc` "self" set of `fakeproc.sh` from [experiment 12a](12a-fake-proc.md) |
| **V2sx** | V2s plus the extra cgroup files that strace of V0 showed being opened |

Limits: `c4m3` is `--cpus=4 --memory=3g`; `c32` is unlimited (the VM has 8
CPUs).

## Results

### Probe matrix, cgroup v2 (`out/t1-v2.txt`) [measured]

| Test | (a) default | (b) +SYS_ADMIN | (c) unconfined | (d) privileged |
|---|---|---|---|---|
| `chroot` | OK | OK | OK | OK |
| `mknod c 1 3` | **OK** | **OK** | **OK** | **OK** |
| `unshare(CLONE_NEWNS)` | FAIL `EPERM` | **OK** | FAIL `EPERM` | OK |
| `unshare(CLONE_NEWUSER)` | **FAIL `EPERM`** | **OK** | **OK** | OK |
| `mount(bind)` | FAIL `EPERM` | **FAIL `EACCES`** | FAIL `EPERM` | OK |
| `ptrace(SEIZE)` of a child | OK | OK | OK | OK |
| seccomp `RET_TRACE` / `USER_NOTIF` | OK | OK | OK | OK |
| `process_vm_readv` | OK | OK | OK | OK |
| `FICLONE` on ext4/tmpfs | FAIL `EOPNOTSUPP` | FAIL | FAIL | FAIL |
| `/proc/kcore,keys,timer_list` | masked (0 bytes) | masked | masked | **readable** |
| `/sys/firmware` | masked (0 entries) | masked | masked | **3 entries** |
| mount a new `proc` | FAIL `EPERM` | **FAIL** (AppArmor, "cannot mount proc") | FAIL `EPERM` | **OK** (4 pids) |
| ranikust overlay option set (no user namespace) | FAIL `EPERM` | **FAIL `EACCES`** (AppArmor) | FAIL `EPERM` | **OK** |
| ranikust overlay option set (inside `unshare -Urm`) | — (user namespace blocked) | — | **OK** (`userxattr`) | OK |
| `fsconfig(lowerdir+)` → `CMD_CREATE` | — | accepted, then **`EINVAL`** | — | accepted, then **`EINVAL`** |
| `chroot` escape reading a sibling rootfs (`escape`) | **yes** | yes | yes | yes |

- **`mknod` of a character device succeeds in every variant, including the
  default.** Rootful Docker grants `CAP_MKNOD`, and the default device cgroup
  allows *creating* nodes (only reading and writing them is restricted). The
  `/dev` gap of Approach A in experiment 12 (bare `chroot` per stage) therefore
  disappears on real Docker, as experiment 12 had **[inferred]**. Only rootless
  podman returns `EPERM`.
- **(a) default: `unshare(CLONE_NEWUSER)` fails with `EPERM`.** Experiment 12
  simulated this with the moby seccomp profile; it is now confirmed with
  Docker's real seccomp profile. No user namespace means no rootless overlay,
  so ranikust falls to kaniko mode. Mounting fails (`EPERM`), and so does
  mounting a new `proc`.
- **(b) `--cap-add SYS_ADMIN` alone still cannot mount: `mount` returns
  `EACCES`.** That is the signature of AppArmor `docker-default`, distinct from
  the `EPERM` of a missing capability. `unshare` of NEWNS and NEWUSER succeeds,
  but AppArmor still blocks overlay (`EACCES`) and a new `proc`. The result is
  still kaniko mode. This matches the documented statement that
  "`--cap-add SYS_ADMIN` alone is still blocked (AppArmor + seccomp)".
- **(c) unconfined seccomp and AppArmor: `unshare(CLONE_NEWUSER)` succeeds and
  the ranikust overlay option set mounts inside `unshare -Urm`** (rootless,
  `userxattr`). The rootless overlay path opens, as the row "Docker with
  seccomp=unconfined,apparmor=unconfined" of survey chapter 06 predicts.
- **(d) privileged: the ranikust overlay option set mounts, and the effective
  options read back as** `redirect_dir=nofollow,uuid=off,volatile,userxattr`
  (`metacopy`, `index` and `xino=off` are defaults and are not printed).
  `/proc` is not masked and a new `proc` mounts. Full overlay.
- **Kernel 5.15:** classic `mount(2)` with **the whole ranikust option set
  works**, both privileged and inside a user namespace. **`fsconfig("lowerdir+")`
  cannot create a mount**: the `SET_STRING` step reports no error, but
  `CMD_CREATE` returns `EINVAL`, in both (b) and (d). Incremental `lowerdir+`
  appends are therefore unusable before kernel 6.8, as predicted; on 5.15 the
  classic `mount(2)` with the `l/` symlink scheme of survey chapter 06 is
  required.
- `escape` breaks out of `chroot` and reads a sibling rootfs in **every**
  variant (root plus `CAP_SYS_CHROOT`). Bare `chroot` is not a security
  boundary; this confirms the isolation finding of experiment 12 on real
  Docker.

### Probe matrix, cgroup v1 (`out/t1-v1.txt`) [measured]

**The probe table under cgroup v1 is identical to v2 in every cell:**
`mknod` succeeds in every variant; (a) user namespace `EPERM`, mount `EPERM`;
(b) user namespace OK but mount `EACCES` (AppArmor), `fsconfig(lowerdir+)`
accepted but `CMD_CREATE` `EINVAL`; (c) user namespace OK, the ranikust
overlay option set mounts inside `unshare -Urm`; (d) privileged mounts the
ranikust option set, `/proc` not masked. As expected, the cgroup version is
orthogonal to seccomp, AppArmor and capabilities, so it does not change the
mode selection. The only differences are cgroup version 1 in `docker info`,
the `cgroupfs` driver, and the absence of `cgroupns` from the security
options.

### Is the model of survey chapter 06 correct?

**Yes.** On real rootful Docker, (a) default and (b) `--cap-add SYS_ADMIN`
both **cannot use overlay** (seccomp blocks user namespaces in (a); AppArmor
blocks mounts in (b)) and land in **kaniko mode**; (c) relaxing seccomp and
AppArmor gives **rootless overlay**; (d) privileged gives **full overlay**.
The only difference from the podman simulation is that **`mknod` works**, so
kaniko mode has a real `/dev`. **[measured]**

### proot under AppArmor `docker-default` (cgroup v2) [measured]

| Test | Result |
|---|---|
| Profile applied to the container | `docker-default (enforce)` |
| `proot -r <rootfs> -b /proc -b /dev -b /sys … ls /proc/self` | **OK**: the whole of `/proc/self` is visible (cgroup, exe, fd, maps, mountinfo, stat, …) |
| `readlink /proc/self/exe` under proot | OK (`/usr/bin/readlink`) |
| `/dev/null`, `/dev/urandom` under proot | **`crw-rw-rw-` 1,3** (real device, from `mknod` plus binding `/dev`); `urandom` readable |
| `apt-get update` under proot, **without** `APT::Sandbox::User=root` | **Hangs → timeout (rc=124)** |
| `apt-get update` under proot, with `-o APT::Sandbox::User=root` | **OK** (rc=0, fetched 9.4 MB) |
| `apt-get install hello` under proot with the root sandbox user | **OK**; `hello` prints `Hello, world!` |

**AppArmor `docker-default` does not block proot's `ptrace`.** proot runs
correctly in default Docker and provides `/proc`, `/dev` and path translation
as it did under E-dock in experiment 12. The apt trap **repeats exactly**: apt
hangs without `APT::Sandbox::User=root` and stops hanging with it. The point
experiment 12 left open (Kubernetes AppArmor might block `ptrace`) is
therefore answered "no" at least for `docker-default`. Kubernetes'
`cri-containerd.apparmor.d` was not tested; that profile is essentially the
same as `docker-default` **[external]**. The v1 run is in `out/t2-v1.txt`.

### Parallel kaniko-mode path in default Docker (cgroup v2) [measured]

**`/dev` and warnings.**

- **`dev_status = "mknod 6/6"` in every build** (null, zero, full, random,
  urandom, tty). On rootful Docker, the `nocap` bare `chroot` has a **real
  `/dev`**; the rootless-podman problem of `/dev/null` being a regular file is
  gone.
- **`xargs: '/dev/null'` warnings: 0** in every W1 build. Experiment 12 saw 2
  such warnings on rootless podman because `/dev/null` was missing; on real
  Docker they disappear, as experiment 12 had **[inferred]**.

**Image correctness: `-jobs 1` against the default** (`out/t3-image-check.txt`)

| Workload | Paths | Metadata differences | Content differences |
|---|---|---|---|
| **W3** | 7,295 = 7,295 | **0** | 4: `/out/{all,c}.txt` (contain `du -sk node_modules`, off by a few KB), `/etc/hosts`, `/etc/hostname` (created by the `listing.sh` container) |
| **W1** | 262,240 = 262,240 (1 npm log file name differs) | 11: file sizes in `/tmp/node-compile-cache` | 1,654: `/root/.local/share` 1,092, `/tmp/node-compile-cache` 553, `/root/.npm/_cacache` 2, `.modules.yaml`, `.pnpm-workspace-state-v1.json` |

The parallel build produces the same image as the sequential one, except for
**exactly the group of non-deterministic pnpm/npm files** already recorded in
[experiment 07](07-kaniko-vs-overlay.md) and experiment 12 (Node's compile
cache, the pnpm store, npm's cacache). Parallelism adds **no** differences.
This confirms experiment 12's correctness result on real Docker.

**Timing** (within the VM only, 8 vCPUs, median of 3 rounds, warm caches)

| Workload | `-jobs 1` (sequential) | `-jobs` default (= 8) | Difference |
|---|---|---|---|
| **W3** (`app-par`, 3 independent stages) | 21.15 s (20.48–26.39) | **13.21 s** (13.08–15.69) | **−7.9 s, −38%** |
| **W1** (`app`, `deps → final` chain) | 16.67 s (16.03–16.77) | 15.41 s (15.38–15.47) | −1.3 s, −8% |

- Parallel stages in kaniko mode **pay off strongly on W3** (−38%) and
  **barely on W1** (−8%), the same shape as experiment 12 (W3 −41 to −44%,
  W1 −2 to −7% on the 32-CPU host). Absolute times are slower than in
  experiment 12 because the 8-vCPU VM shared the host; **compare only within
  the VM**.
- No build failed, and no `RUN` of W1 or W3 broke for lack of `/proc` (as in
  experiment 12).

The opt-in `--parallel-stages` path **works on real rootful Docker**: the
image matches the sequential build, `/dev` is real thanks to `mknod`, and W3
is 38% faster. What experiment 12 had to infer about `/dev` on rootful Docker
is now measured.

### Fake `/proc` under cgroup v2, `c4m3` (`out/t4-v2.txt`) [measured]

| Value | V0 | V1 | V2s | V2sx |
|---|---|---|---|---|
| node `os.cpus().length` | 8 | **0** | 8 | 8 |
| node `os.availableParallelism()` | **4** | **8** | **4** | **4** |
| node `constrainedMemory` (MiB) | 3072 | **0** | 3072 | 3072 |
| node V8 `heap_size_limit` (MiB) | **1584** | **2096** | **1584** | **1584** |
| Go `runtime.NumCPU` | 8 | 8 | 8 | 8 |
| Go `GOMAXPROCS` | **4** | **8** | **4** | **4** |
| `process.memoryUsage().rss` (MiB) | 44 | **throws `ENOENT`** | 1 | 1 |
| `process.execPath` | `/usr/local/bin/node` | `node` | `node` | `node` |
| Go `os.Executable` | OK | **error** | **error** | **error** |

- **V1 repeats experiment 12a exactly:** `os.cpus()` is 0, and the quota is
  **ignored** (`availableParallelism` 8, `GOMAXPROCS` 8, V8 heap 2096 MiB,
  `constrainedMemory` 0) because `/proc/self/cgroup` and `/sys/fs/cgroup/*`
  are missing. `memoryUsage()` throws `ENOENT`.
- **V2s restores V0 exactly for every CPU and memory count:** `os.cpus()` 8,
  `availableParallelism` 4, `constrainedMemory` 3072, V8 heap 1584 MiB,
  `GOMAXPROCS` 4. This confirms on real rootful Docker what experiment 12a
  measured under E-dock.
- Not fixable, as in experiment 12a: `process.execPath` and `os.Executable`
  (both need `/proc/self/exe`), and the real RSS (V2s reports 1 MiB).
- **strace of V0 (libuv) shows these files opened:** `/proc/self/cgroup`,
  `/proc/self/stat`,
  `/sys/fs/cgroup/{cgroup.controllers,cpu.max,memory.max,memory.high}`, which
  is **exactly the "self" set of `fakeproc.sh`**. Go additionally reads
  `/proc/self/mountinfo` to find the cgroup2 mount. Under v2 no cgroup file
  outside that set is opened, so **V2sx = V2s**.
- `c32` (unlimited): V0 = V1 = V2s for every count (8 CPUs everywhere,
  `availableParallelism` 8), as expected when v2 imposes no limit.

### Fake `/proc` under cgroup v1, `c4m3` (`out/t4-v1.txt`) [measured]

Under cgroup v1, `/proc/self/cgroup` has **14 per-controller lines**
(`10:cpu,cpuacct:/docker/<id>`, `7:memory:/docker/<id>`, …), and
`/sys/fs/cgroup/` is a **tmpfs** with one mount directory per controller
(`cpu,cpuacct`, `memory`, …). The quota files **change both name and path**
compared with v2.

| Value | V0 | V1 | **V2s (v2 set)** | **V2sx (v1 set from strace)** |
|---|---|---|---|---|
| node `os.cpus().length` | 8 | 0 | 8 | 8 |
| node `availableParallelism()` | **8** (see below) | 8 | 8 | 8 |
| node `constrainedMemory` (MiB) | 3072 | 0 | **0 (not fixed)** | **3072 ✓** |
| node V8 `heap_size_limit` (MiB) | 1584 | 2096 | **2096 (not fixed)** | **1584 ✓** |
| Go `GOMAXPROCS` | **4** | 8 | **8 (not fixed)** | **4 ✓** (needs Go's extra cpu files) |

- **The v2 fake `/proc` set is not sufficient under v1.** The "self" set of
  `fakeproc.sh` copies `/sys/fs/cgroup/{cpu.max,memory.max,memory.high}`;
  these files **do not exist under v1**, so `fake_up` skips them and V2s
  **cannot restore** `constrainedMemory`, the V8 heap or `GOMAXPROCS`.
  Experiment 12a had **[inferred]** exactly this.
- **The v1 file list must be derived from `/proc/self/cgroup` and
  `/proc/self/mountinfo` (or from strace), not hard-coded after v2.** strace
  of V0 gives the files the tools **actually** open (union of node and Go):

  | v1 file | Read by | Replaces (v2) |
  |---|---|---|
  | `/sys/fs/cgroup/cpu,cpuacct/cpu.cfs_quota_us` | Go `GOMAXPROCS` | `cpu.max` |
  | `/sys/fs/cgroup/cpu,cpuacct/cpu.cfs_period_us` | Go | `cpu.max` |
  | `/sys/fs/cgroup/memory/memory.limit_in_bytes` | libuv `constrainedMemory`, V8 heap | `memory.max` |
  | `/sys/fs/cgroup/memory/memory.soft_limit_in_bytes` | libuv | `memory.high` |
  | `/proc/self/cgroup` (14 lines), `/proc/self/mountinfo` | both | `/proc/self/cgroup` (`0::/`) |

  The quota files sit at the **root of the controller mount** (no
  `/docker/<id>` branch), because Docker under v1 bind-mounts the container's
  cgroup directory onto `/sys/fs/cgroup/<controller>`.
- **With this v1 set copied, V2sx matches V0 for both node and Go:**
  `constrainedMemory` 3072, V8 heap 1584 MiB, `GOMAXPROCS` 4. A separate run
  using the strace union confirmed it (`out/t4-v1.txt` and the check command
  in `out/t4-v1-union.txt`).
- **libuv under v1:** node's `os.availableParallelism()` is **already 8 at V0**
  (it does not honour `--cpus=4`). strace shows libuv building **a wrong cpu
  path** under v1: `/sys/fs/cgroup/cpuacct:/docker/<id>/cpu.cfs_quota_us`
  (with a stray `:`, using the wrong field of `/proc/self/cgroup`). That file
  **does not exist**, so libuv ignores the CPU quota. The fake `/proc`
  therefore **need not and cannot** fix `availableParallelism` under v1, and
  it already equals V0. Go reads the correct
  `cpu,cpuacct/cpu.cfs_quota_us` and gets 4.

The fake `/proc` is now **measured on real rootful Docker**. The "self" set of
experiment 12a restores V0 **under cgroup v2**. **Under cgroup v1 the file
list must change** (cpu `cfs_quota_us`/`cfs_period_us`, memory
`limit_in_bytes`/`soft_limit_in_bytes`, the 14-line `/proc/self/cgroup`);
then V2sx also matches V0. The list must be **derived from
`/proc/self/{cgroup,mountinfo}`**, not hard-coded, as experiment 12a
predicted. Under v1 this is the 4 controller files above plus the v1
`/proc/self/cgroup` and `mountinfo`, instead of the 6–9 v2 files.

## Findings

1. Real rootful Docker confirms the mode table of survey chapter 06: default
   and `--cap-add SYS_ADMIN` → kaniko mode, unconfined seccomp+AppArmor →
   rootless overlay, `--privileged` → full overlay (probe matrix, cgroup v2).
2. `--cap-add SYS_ADMIN` alone fails to mount with `EACCES`, the AppArmor
   signature, not `EPERM` (probe matrix, row `mount(bind)`).
3. The cgroup version does not change any cell of the probe matrix (probe
   matrix, cgroup v1).
4. `mknod` of character devices works in every variant on rootful Docker, so
   kaniko mode gets a real `/dev` and the `xargs: '/dev/null'` warnings
   disappear (probe matrix; parallel path, `/dev` and warnings).
5. Kernel 5.15 mounts the full ranikust overlay option set with `mount(2)`,
   but `fsconfig("lowerdir+")` fails at `CMD_CREATE` with `EINVAL` after a
   silent `SET_STRING` (probe matrix, last rows).
6. Bare `chroot` is escapable in every variant (probe matrix, `escape` row).
7. AppArmor `docker-default` does not block proot's `ptrace`; the apt hang
   without `APT::Sandbox::User=root` repeats (proot table).
8. The parallel path produces the same image as the sequential path apart
   from known non-deterministic files, and cuts W3 by 38% and W1 by 8% on 8
   vCPUs (image-correctness and timing tables).
9. Under cgroup v2 the fake `/proc` "self" set restores every V0 CPU and
   memory count; under v1 it restores none of the cgroup-dependent ones
   until the v1 file list is used (fake `/proc` tables).
10. libuv 1.51 builds a wrong cpu quota path under v1, so Node ignores
    `--cpus` even with the real `/proc` (fake `/proc` under v1).

## Implications for ranikust

- **Mode detection.** The default-Docker and `--cap-add SYS_ADMIN` cases land
  in kaniko mode, and only relaxing seccomp and AppArmor, or `--privileged`,
  opens overlay. This is the behaviour recorded in
  [ADR-0002](../../adr/0002-two-snapshot-modes.md) and
  [ADR-0004](../../adr/0004-support-every-privilege-level.md) and described in
  [modes and detection](../../design/modes-and-detection.md); it now rests on
  real Docker rather than a simulation.
- **Overlay options and kernel support.** The option set of
  [ADR-0016](../../adr/0016-overlay-mount-options.md) works with classic
  `mount(2)` on kernel 5.15. Before kernel 6.8, ranikust has to use classic
  `mount(2)` with the `l/` symlink scheme instead of incremental `lowerdir+`.
  Support for `lowerdir+` cannot be detected from the `SET_STRING` step; it
  must be probed by actually creating a mount.
- **Parallel stages.** [ADR-0014](../../adr/0014-kaniko-mode-sequential-by-default.md)
  makes kaniko mode sequential by default, with `--parallel-stages` as an
  opt-in. This experiment shows the opt-in path is correct on real rootful
  Docker and worthwhile for builds shaped like W3 (independent stages), with
  little gain for chains like W1. See [kaniko mode](../../design/kaniko-mode.md).
- **`/dev` in kaniko mode.** On rootful Docker ranikust can create real
  device nodes with `mknod`; only rootless podman needs the fallback.
- **proot.** [ADR-0018](../../adr/0018-no-proot-in-1-0.md) keeps proot out of
  1.0. This experiment removes AppArmor `docker-default` as an obstacle and
  records the apt sandbox hang that any future proot path must handle.
- **Fake `/proc`.** The file set must be generated at run time per cgroup
  version from `/proc/self/{cgroup,mountinfo}`. The final set is defined in
  [experiment 16](16-cgroup-v1-cpuset.md) and in
  [kaniko mode](../../design/kaniko-mode.md).
- **Isolation.** Bare `chroot` is not a security boundary, consistent with
  [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) and the
  [sandbox design](../../design/sandbox.md).

## Limitations and open questions

- **Real Kubernetes** (containerd, AppArmor `cri-containerd.apparmor.d`,
  `procMount`, `hostUsers`, Pod Security Standards) was not tested; only
  rootful Docker was. `cri-containerd` is essentially the same profile as
  `docker-default` **[external]**, so proot probably runs there too
  **[inferred]**, but this was not measured.
- **jest, webpack, terser and `next build` under bare `chroot`** were
  measured under E-dock in experiment 12a. This experiment re-measured only
  the **core** that decides worker counts and heap sizes (libuv
  `os.cpus`/`availableParallelism`/`constrainedMemory`, the V8 heap, Go
  `GOMAXPROCS`), which drives those tools, and only under
  `--cpus=4 --memory=3g`. The full jest/webpack/next suite was not re-run in
  the VM.
- **Java/JVM under cgroup v1** was not measured here. Experiment 12a found that
  JVM 21 does **not** detect cgroups on kernel 7.0 because `/proc/cgroups`
  lacks the cpuset and memory lines. On kernel 5.15 with cgroup v1 the JVM may
  detect cgroups (full `/proc/cgroups`).
- **W3 in parallel under cgroup v1** (optional test 5) was not run, because
  the v1 fake `/proc` took priority. Correctness probably does not differ from
  v2 **[inferred]**; timing may differ because the v1 memory controller
  accounts page cache differently.
- **cpuset / `--cpuset-cpus`** was not tried. With cpuset, glibc and libuv
  fall back to `sched_getaffinity` and may give **more accurate** numbers than
  V0 **[inferred]**, as experiment 12a suggested.
- **`fsconfig(lowerdir+)`:** kernel 5.15 reports **no error** when the
  `lowerdir+` string is set (only `CMD_CREATE` returns `EINVAL`), so
  `SET_STRING` cannot be used to detect support; a real mount must be
  attempted. The exact kernel where `CMD_CREATE` starts to succeed was not
  pinned down (reportedly 6.8 **[external]**).
- **libuv's wrong cpu path under cgroup v1** (`cpuacct:/docker/...`) was seen
  with libuv 1.51 (Node 22.23). Other libuv versions were not checked; it may
  be a version-specific bug **[inferred]**.
- **Network:** the host registry and npm proxy were reached through
  `10.0.2.2` and socat, with warm caches; pull and push cost almost nothing,
  as in experiment 12. The real network was not measured.
- **Load:** the VM shared the host with experiment 14, which ran
  concurrently. Timings are comparable only within the VM, 3 rounds, without
  confidence intervals.

[Experiment 16](16-cgroup-v1-cpuset.md) picked up the open points on the JVM
under cgroup v1, the jest/webpack/next suite, W3 under cgroup v1 and cpuset.

## Reproduction

PoC sources are archived outside this repository under
`docs-rust/poc/13-docker-rootful-vm/`.

**VM setup (host side).** No host configuration is changed; QEMU runs as the
user.

- `scripts/00-fetch-image.sh` downloads `jammy-server-cloudimg-amd64.img` and
  `SHA256SUMS` from `cloud-images.ubuntu.com/jammy/current`, verifies the
  SHA-256 and records it in `out/00-image.txt`.
- `scripts/10-make-seed.sh` builds the NoCloud seed ISO (volume label
  `cidata`) from `cloud-init/user-data` and `meta-data` with xorriso, and a
  40 GB qcow2 overlay backed by the base image.
- `cloud-init/user-data` creates a passwordless-sudo user with a dedicated
  SSH key (`vm/k13_key`), installs `docker.io`, `socat` and `strace` from the
  Ubuntu archive, writes `/etc/docker/daemon.json` with the host registry as
  an insecure registry, enables Docker and adds the user to the `docker`
  group.
- `scripts/20-boot-vm.sh` boots QEMU in the foreground: KVM, `-cpu host`,
  8 vCPUs, 10 GB, q35, virtio disk and network, user-mode networking with SSH
  forwarded to `127.0.0.1:2222`, serial log and a QEMU monitor socket in
  `vm/`.
- `scripts/vssh` and `scripts/vscp` wrap SSH and SCP with the dedicated key.
- `scripts/40-host-image-check.sh` compares the images pushed to the host
  registry using experiment 12's `perf/listing.sh` and `cmp-listing.py`.
- `scripts/99-teardown.sh` deletes the `k13-*` repositories from the host
  registry, powers the VM off through the monitor socket (with an SSH
  `poweroff` as backup), checks that no QEMU process remains, and deletes the
  overlay and seed. The verified base image (695 MB) is kept in `vm/`.
- `scripts/fsplus.c` is the static overlay option set and
  `fsconfig(lowerdir+)` probe.

**Inside the VM** (`scripts/invm/`, copied to `~/k13/scripts/`):

- `setup.sh` (run after each boot) waits for Docker, adds `registry` and
  `verdaccio` as `172.17.0.1` to `/etc/hosts`, starts socat forwarders as
  transient systemd units from `172.17.0.1:5001` and `:4873` to
  `10.0.2.2:5001` and `:4873`, and pulls the base `node:22-bookworm-slim`
  image from the host registry.
- `env.sh`, `t1-probe.sh`, `t2-proot.sh`, `t3-build.sh`, `t4-fakeproc.sh`
  run the tests; `fakeproc.sh` is copied from experiment 12a.
- `set-cgroup-v1.sh` adds `systemd.unified_cgroup_hierarchy=0` to the GRUB
  command line; a reboot switches the VM to cgroup v1 (kernel 5.15 with
  systemd 249 still supports it).
- Binaries reused from experiment 12: `compat/tools/{probe,escape,chrun,proot}`
  and `perf/bin/ovbuild`.

```sh
bash scripts/00-fetch-image.sh && bash scripts/10-make-seed.sh
nohup bash scripts/20-boot-vm.sh &                 # VM in background; SSH 127.0.0.1:2222, key vm/k13_key
# copy ~/k13 (binaries + fixtures + scripts) into the VM, then:
bash scripts/vssh 'bash ~/k13/scripts/setup.sh'
for t in env t1-probe t2-proot t3-build t4-fakeproc; do bash scripts/vssh "bash ~/k13/scripts/$t*.sh v2"; done
bash scripts/40-host-image-check.sh 1              # compare images on the host
bash scripts/vssh 'bash ~/k13/scripts/set-cgroup-v1.sh && sudo reboot'   # switch to cgroup v1
# wait for SSH, then re-run setup.sh and t1/t2/t4 with argument v1
bash scripts/99-teardown.sh                        # stop the VM and clean up
```

Raw output in `out/`: `env-v{1,2}.txt`, `t1-v{1,2}.txt`, `t2-v{1,2}.txt`,
`t3-v2.txt` with `t3-v2/summary.txt`, `t3-image-check.txt`, `t4-v{1,2}.txt`,
`t4-v1-union.txt`, `00-image.txt`. The qcow2 overlay and seed were deleted at
teardown.

## References

1. [Experiment 12: Parallel stages in kaniko mode](12-kaniko-mode-parallel.md):
   E-dock, Approach A (bare `chroot`) and Approach C (proot), isolation,
   performance, `ovbuild`, `listing.sh`.
2. [Experiment 12a: Fake /proc](12a-fake-proc.md): the "self" file set,
   `fakeproc.sh`, JVM on kernel 7.0.
3. [Experiment 07: kaniko vs overlay](07-kaniko-vs-overlay.md):
   non-deterministic pnpm/npm files.
4. [Experiment 14: Real builds without /proc](14-real-builds-without-proc.md).
5. [Experiment 16: cgroup v1 and cpuset](16-cgroup-v1-cpuset.md): JVM under
   v1, the final per-version fake `/proc` set.
6. [Survey chapter 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md):
   mode table, `l/` symlink scheme for lowerdirs.
7. Ubuntu 22.04 cloud images, https://cloud-images.ubuntu.com/jammy/current/
   (accessed 2026-10-29).
8. **[external]** Similarity of `cri-containerd.apparmor.d` and
   `docker-default`, and kernel 6.8 as the first kernel accepting
   `lowerdir+` through `fsconfig`: the original records these as web lookups
   without citing the source.
