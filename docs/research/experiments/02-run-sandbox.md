# Experiment 02: RUN sandbox

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Does the `RUN` sandbox sequence in survey chapter 02 §3 work on real root filesystems, what is missing or wrong, which privileges does it need, and what does it cost per `RUN`? |
| **Answer** | It works after corrections: `pivot_root` instead of `chroot` (root escaped a `chroot`), `PR_SET_PDEATHSIG`, a capability drop, stub cleanup and several fallbacks. The lowest privilege that ran everything was a default rootless podman container plus `--security-opt unmask=ALL`, and the overhead is 1–2.5 ms per `RUN`. |
| **Related** | [ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md), [ADR-0004](../../adr/0004-support-every-privilege-level.md), [ADR-0010](../../adr/0010-run-network-host-or-none.md); [sandbox](../../design/sandbox.md), [modes-and-detection](../../design/modes-and-detection.md); experiments [01](01-overlay-semantics.md), [03](03-privilege-matrix.md), [06a](06a-root-rerun.md), [13](13-docker-rootful-vm.md) |
| **Original** | `docs-rust/poc/02-sandbox/` (Vietnamese, not in this repository) |

## Summary

- A Go prototype ran the sandbox sequence of
  [survey chapter 02](../survey/02-kaniko-mode-requirements.md) §3 on
  `alpine:3.20`, `debian:latest` and `node:22-bookworm-slim` root
  filesystems inside rootless podman: `apk add`, `apt-get install`,
  `npm --version` and `USER` switching all worked.
- `chroot` is not enough: root inside `RUN` escaped it with the classic
  `chroot(subdir)` + `chdir("..")` trick. `pivot_root` (or `MS_MOVE` +
  `chroot` where seccomp blocks `pivot_root`) held.
- Three steps were missing from the survey: `PR_SET_PDEATHSIG` (without it a
  `RUN` child outlives a killed build and keeps writing), dropping the
  capability bounding set (without it `RUN` could unmount `resolv.conf` and
  write the rootfs), and stub tracking (stub files created for bind targets
  leak into the layer and survive `SIGKILL`).
- The lowest privilege that ran every test was a default rootless podman
  container plus `--security-opt unmask=ALL`, with fallbacks: a new user
  namespace, an `rbind` of `/sys`, and no hostname.
- Overhead was 1–2.5 ms per `RUN` (`/bin/true` × 200), negligible against
  any real `RUN`.
- Everything ran as root of podman's user namespace; the real-root re-run is
  [experiment 06a](06a-root-rerun.md).

## Questions

1. Does each step of survey 02 §3 work as written, and what has to change?
   (section numbers refer to the original chapter)
2. What is the lowest container privilege at which the sandbox runs, and
   what are the fallbacks below it?
3. Which syscall and which capability does each step need, and what blocks
   it?
4. What does the sandbox add to each `RUN`?

## Environment

- Host: Ubuntu, Linux 7.0.0, x86_64. No sudo.
- Podman 5.7.0 rootless, runtime `crun`. Every test ran inside a podman
  container. "Root" below is root **in rootless podman's user namespace**
  (uid 0 → host uid 1000; 1..65536 → subordinate uids), not real root.
- Seccomp: podman's default profile, or Docker's default profile
  (`moby/profiles` `seccomp/default.json`, downloaded at run time) loaded
  into podman to simulate Docker. Real Docker was not available.
- AppArmor: rootless podman applies no profile (`crun (unconfined)`), so
  AppArmor could not be measured inside containers. The host row in Table 2
  ran with Ubuntu's `apparmor_restrict_unprivileged_userns=1`.
- Root filesystems: `podman export` of `alpine:3.20`, `debian:latest` and
  `node:22-bookworm-slim`, extracted to `/work` inside the test container
  (on the container's own overlay; the host is not touched).
- Overhead timing: no CPU limit was set on the test container; page cache
  state not recorded; machine load not recorded.

## Method

**Prototype.** `sandbox.go`, about 840 lines of Go, built as a static
binary. It has three layers: `build` → `stage1` (the build's mount
namespace) → `init` (PID 1 of the sandbox). The main flags of
`sandbox build` select the variants:

| Flag | Effect |
|---|---|
| `--rootfs --user --workdir --env` | The `RUN` context |
| `--chroot` | Use `chroot` instead of `pivot_root` |
| `--msmove` | Use `MS_MOVE` + `chroot` |
| `--drop-caps` | Reduce the bounding set to Docker's default capabilities |
| `--resolv ro\|copy\|none` | How `resolv.conf` is provided |
| `--init` | PID 1 is a small init instead of the command |
| `--no-pdeathsig` | Do not set `PR_SET_PDEATHSIG` |
| `--naive-symlink` | Resolve bind targets naively |
| `--no-stub-cleanup` | Leave stub mountpoints in place |
| `--repeat N` | Run N times, for timing |
| `--baseline` | Only `chroot` + `exec` |

After each run, `stage1` prints a `POSTCHECK`: the contents of
`proc sys dev tmp run` in the rootfs, the hashes of `etc/resolv.conf`,
`etc/hosts` and `etc/hostname` before and after, the number of mounts left
under the rootfs in `/proc/self/mountinfo`, and whether a `sleep 1000`
process is still alive.

**Test groups** (run inside the container): `basic`, `net`, `user`,
`orphan`, `parentdeath`, `resolv`, `mount`, `escape`, `symlink`, `bench`.
They were run at `--privileged`, at the minimum level
(`--security-opt unmask=ALL`), and across a privilege matrix that runs
`sandbox build --rootfs alpine --user nobody -- id` under each container
configuration of Table 2.

**Overhead.** `/bin/true` × 200 on Alpine, timed from `fork` to `wait` in
`stage1`; mean and median (p50) reported. Variants: **baseline** (`chroot` +
`exec`, no namespaces, no mounts), **full/pivot_root**, **full/chroot**,
**+drop-caps**, **+init**.

## Results

All results are **[measured]** unless labelled otherwise.

### Step-by-step verdict

Table 1. Each step of survey 02 §3, and the two steps it lacked.

| Step | Verdict | Evidence, and what to change |
|---|---|---|
| 1. `unshare(CLONE_NEWNS)` + `/` rprivate for the whole build | **Holds, needs a condition** | Must be called while the process **is still single-threaded** (before starting tokio). Measured: in a Go process with 6 threads, `unshare(CLONE_NEWNS)` **succeeds** but moves only 1 of 6 threads to the new namespace; `unshare(CLONE_NEWUSER)` returns `EINVAL`. Mounts disappear when the build dies: holds. **Disk writes** (stub mountpoints) do not, see step 3d. |
| 2. Fork a child with `NEWNS\|NEWPID\|NEWUTS\|NEWIPC`, shared network | **Holds, misses two points** | (a) `PR_SET_PDEATHSIG(SIGKILL)` is required. Measured: `SIGKILL` to the build during `RUN sleep 1000` → with PDEATHSIG, 0 processes left; without it, `sleep` **survives** and keeps writing to the rootfs. (b) Without `CAP_SYS_ADMIN`, `CLONE_NEWUSER` is also needed (Table 2), mapping the **whole id range** available and allowing `setgroups`; otherwise `USER nobody` or `_apt` fails (`setgroups: EPERM`). Debian's `apt-get` switches to `_apt` (uid 42), so this matters. |
| 3a. `proc` | **Holds** | Works at every level where the sandbox runs. The one failure: the container's `/proc` is masked **and** the sandbox must create a new user namespace → `EPERM` (`mount_too_revealing`). Fix: the container needs `unmask=ALL` (Kubernetes: `procMount: Unmasked`). |
| 3b. `sysfs` read-only | **Needs a fallback** | Mounting `sysfs` needs privilege over the user namespace that **owns the network namespace**. `EPERM` with `--network=host`, or when the sandbox creates its own user namespace. Fallback: `rbind /sys` + remount read-only (as rootless runc does) works. |
| 3c. tmpfs `/dev` + bind device nodes | **Holds; "bind" is mandatory** | `mknod /dev/null` = `EPERM` at **every** level (even `--privileged`), because `CAP_MKNOD` must be held in the initial user namespace. Binding nodes from the outer `/dev` works (they show owner `nobody` because host root is unmapped; harmless). `devpts` with `newinstance,ptmxmode=0666,mode=0620,gid=5` needs gid 5 mapped; otherwise drop `gid=5`. Also needed: symlinks `ptmx→pts/ptmx`, `fd`, `stdin`, `stdout`, `stderr`. |
| 3d. Read-only bind of `resolv.conf`/`hosts` | **Needs correction (3 traps)** | (1) Remount with `MS_BIND\|MS_REMOUNT\|MS_RDONLY` returns `EPERM` in a user namespace: the locked flags of the source mount (`statfs().f_flags`: nosuid/nodev/noexec/relatime) must be OR-ed in. (2) **The target may not exist**: `alpine:3.20` has no `/etc/resolv.conf`, `debian:latest` has no `/etc/hosts` → a stub must be created **on the real rootfs**, and it leaks into the layer unless cleaned; if the build is `SIGKILL`ed the stub **is left behind** (observed). (3) The target may be a **symlink** → resolve it inside the rootfs (securejoin, or `openat2(RESOLVE_IN_ROOT)`), never with `Path::join`. Also, read-only breaks `RUN echo nameserver ... > /etc/resolv.conf`, which Docker allows (into a private copy) **[external]**. Proposal: bind a writable **copy** (`--resolv copy` worked: writable, rootfs unchanged). |
| 3e. `RUN --mount` | Not tested | Out of scope. The symlink and stub traps of 3d apply unchanged. |
| 3f. `chroot` **or** `pivot_root` | **Needs correction: must be `pivot_root`** | With `chroot`, root inside `RUN` (holding `CAP_SYS_CHROOT`, which is in Docker's default set) **escapes** with `chroot(subdir); chdir("..")×N; chroot(".")` and sees the container's `/poc` and `/work`. With `pivot_root(".", ".")` + `umount2(".", MNT_DETACH)` it does not. The rootfs must be bind-mounted onto itself before `pivot_root`. Docker's default seccomp **blocks `pivot_root` even with `CAP_SYS_ADMIN`** → fallback `mount(rootfs, "/", MS_MOVE)` + `chroot(".")` (runc `--no-pivot`): tested, the same trick does not escape. |
| *(missing)* Drop capabilities | **Missing** | Without a drop, `RUN` (as root) holds `CAP_SYS_ADMIN`: it ran `mount -t tmpfs`, then `umount /etc/resolv.conf`, then **wrote straight into the rootfs's `resolv.conf`** (the rootfs hash changed). With `--drop-caps` (bounding and inheritable sets reduced to Docker's 14 default capabilities): `mount`/`umount` = `EPERM`, the write is blocked. `USER nobody` has `CapEff=0` but a full `CapBnd` → a setuid binary in the image regains `CAP_SYS_ADMIN`; the bounding set must be dropped **for every USER**. |
| *(missing)* `sethostname` | **Missing** | `NEWUTS` is created but §3 sets no hostname. Podman's and Docker's default seccomp block `sethostname` when the container lacks `CAP_SYS_ADMIN`, even inside a private UTS namespace → tolerate the error (skip). |
| 3g. `USER` from the rootfs's `/etc/passwd` → `setgroups/setgid/setuid` | **Holds** | `nobody`, `node` (+ `HOME`), `_apt` (gid `nogroup`), `1234:1234` all correct. A numeric uid absent from passwd → gid 0 (as in Docker). Supplementary groups come from `/etc/group`. |
| 3h. Look up the executable in `PATH` inside the rootfs | **Holds** | Look up **after** `pivot_root` and **after** `setuid`, so that `access(X_OK)` is evaluated for the right user. `npm --version` = 10.9.9 on `node:22-bookworm-slim`. |
| 4. PID 1 exits → kernel kills the rest | **Holds** | `setsid sleep 1000 &` then exit: 0 processes left, both with the command as PID 1 and with a small init. Note: a command running as PID 1 **ignores SIGTERM/SIGINT** unless it installs a handler (`kill -TERM $$` had no effect), and does not reap zombies. Docker and BuildKit also run `sh` as PID 1, so the behaviour matches **[external]**. |
| "No ignore list needed" | **Holds for mounts, wrong for stubs** | Seen from the build's namespace: `proc sys dev` are still empty, 0 mounts under the rootfs, `hosts`/`hostname` unchanged, no host content. But stubs (`etc/resolv.conf`, `etc/hosts`, `proc`… when the image lacks them) are real files. Record the stub list in `state/` **before** creating them, delete them after `RUN`, and clean up again on start after a crash. BuildKit does the same (`executor/stubs.go`, **[code]**, commit not recorded). |

### Minimum privilege

Table 2. Container configuration (rootless podman) and outcome.

| Container configuration | Outcome |
|---|---|
| `--privileged` | Everything runs. No new user namespace needed. |
| `--privileged --network=host` | Runs; `sysfs` needs the `rbind` fallback. |
| `--cap-add SYS_ADMIN` (podman seccomp, `/proc` masked) | **Everything runs.** No new user namespace is needed, so the `/proc` masks do not lock (`MNT_LOCKED` only applies when crossing into a less privileged user namespace **[external]**). |
| `--cap-add SYS_ADMIN` + **Docker's** seccomp (moby `default.json`) | `pivot_root` = `EPERM`. Runs with `--msmove`. |
| default (no `SYS_ADMIN`) | `CLONE_NEWNS` = `EPERM` → fallback `CLONE_NEWUSER\|CLONE_NEWNS` succeeds (podman seccomp allows it) → **`mount proc` = `EPERM`** because `/proc` is masked. No sandbox possible. |
| default + `--security-opt unmask=ALL` | **Everything runs** (apk, apt, npm, USER, orphan, drop-caps), with fallbacks: new user namespace, `sysfs` via `rbind`, no hostname. This is the lowest level measured. |
| default + `unmask=ALL` + Docker's seccomp | `unshare`/`clone` with namespace flags = `EPERM` → kaniko mode only. |
| Host, ordinary user, Ubuntu `apparmor_restrict_unprivileged_userns=1` | `clone(CLONE_NEWUSER)` **succeeds**, but every `mount` inside it = `EACCES` (AppArmor assigns the `unprivileged_userns` profile, which has no capabilities) → kaniko mode, unless an AppArmor profile granting `userns,` is installed for the binary. |

Seccomp: with **podman**, `seccomp=unconfined` is not needed (podman's
profile allows `mount`, `umount2`, `pivot_root`, `unshare` and `clone`
unconditionally). With **Docker**, either `CAP_SYS_ADMIN` (which opens
`mount`/`unshare`/`clone`) plus `MS_MOVE` instead of `pivot_root`, or
`seccomp=unconfined`. AppArmor: rootless podman applies no profile
(`crun (unconfined)`), so it could not be measured.

### Syscalls and capabilities per step

Table 3. Kernel requirements come from the man pages and runc/BuildKit
sources (**[external]**) and match the measurements; the "Blocked by" column
is **[measured]** except where labelled.

| Step | Syscall | Kernel requirement | Blocked by |
|---|---|---|---|
| Namespace for the build | `unshare`/`clone(CLONE_NEWNS)` | `CAP_SYS_ADMIN` in the current user namespace | Docker seccomp (without `SYS_ADMIN`) |
| Namespaces for `RUN` | `clone(NEWNS\|NEWPID\|NEWUTS\|NEWIPC)` | as above | as above |
| User namespace fallback | `clone(CLONE_NEWUSER\|…)` + write `uid_map`/`gid_map`/`setgroups` | No capability; mapping many ids needs `CAP_SETUID`/`CAP_SETGID` in the parent user namespace (root in a container has them; an ordinary user needs `newuidmap`) | Docker seccomp; Ubuntu AppArmor (created, but capabilities are stripped); `max_user_namespaces` **[external]** |
| rprivate, bind, read-only remount, tmpfs, devpts | `mount` | `CAP_SYS_ADMIN` over the user namespace owning the mount namespace; a remount must keep locked flags | Docker seccomp; AppArmor `docker-default` (`deny mount`) **[external]**, read only |
| proc | `mount("proc")` | `CAP_SYS_ADMIN` over the user namespace owning the **PID namespace**; outside the initial user namespace, an unmasked `/proc` must exist | Masked paths + new user namespace |
| sysfs | `mount("sysfs")` | `CAP_SYS_ADMIN` over the user namespace owning the **network namespace** | `--network=host`, new user namespace |
| `/dev` nodes | `mknod` | `CAP_MKNOD` in the **initial user namespace** | Every rootless container → use bind |
| pivot_root | `pivot_root`, `umount2(MNT_DETACH)` | `CAP_SYS_ADMIN` | Docker seccomp (always blocks) |
| `MS_MOVE` + chroot | `mount(MS_MOVE)`, `chroot` | `CAP_SYS_ADMIN`, `CAP_SYS_CHROOT` | — |
| Hostname | `sethostname` | `CAP_SYS_ADMIN` over the user namespace owning the UTS namespace | Podman and Docker seccomp when the container lacks `SYS_ADMIN` |
| Drop capabilities | `prctl(PR_CAPBSET_DROP)`, `capset` | `CAP_SETPCAP` | — |
| Switch user | `setgroups`, `setgid`, `setuid` | `CAP_SETGID`, `CAP_SETUID`; the ids must be mapped; `setgroups` must be "allow" | A user namespace that maps only one id |
| Orphan protection | `prctl(PR_SET_PDEATHSIG)` | None | — |

### Overhead per `RUN`

Table 4. `/bin/true` × 200 on Alpine, timed from `fork` to `wait` in
`stage1`. Values are mean / p50 over the 200 iterations. Ranges are as
reported in the original; how many repeated runs they span was not
recorded. No CPU limit.

| Variant | `--privileged` (mean / p50) | default + `unmask=ALL`, user namespace (mean / p50) |
|---|---|---|
| baseline: `chroot` + `exec`, no namespaces, no mounts | 0.20 / 0.15 ms | 0.13 / 0.13 ms |
| full sandbox, `pivot_root` | 2.1–2.4 / 1.9 ms | 1.02 / 1.00 ms |
| full sandbox, `chroot` | 1.9 / 1.7 ms | 0.93 / 0.92 ms |
| + drop-caps | 2.0–2.3 / 1.8–2.0 ms | 1.05 / 1.02 ms |
| + small init | 2.1 / 1.8–1.9 ms | 1.07 / 1.05 ms |

Setup inside `init` (mount → `execve`), measured separately, took 0.2–1 ms.
The rest is Go runtime start-up on re-exec. **[inferred]** A Rust
implementation that forks without re-exec will be faster. Overall: **1–2.5 ms
per `RUN`**, negligible against any real `RUN`.

### Evidence classes

**[measured]** (rootless podman, kernel 7.0.0):

- The whole sequence of Table 1 on `alpine:3.20`, `debian:latest` and
  `node:22-bookworm-slim`: `apk add curl`,
  `apt-get update && apt-get install -y curl`, `npm --version`,
  `USER nobody/node/_apt/1234:1234`.
- The threading behaviour of `unshare` in a multithreaded process.
- `mknod` refused; binding nodes works.
- Read-only remount needs the locked flags.
- Stub mountpoints, a stub left behind after `SIGKILL`, a symlink as bind
  target.
- Processes left behind when PID 1 dies; PDEATHSIG; PID 1 ignoring SIGTERM.
- `mount`/`umount` inside `RUN`, and the effect of drop-caps.
- The `chroot` escape, and `pivot_root` and `MS_MOVE` preventing it.
- The privilege matrix of Table 2, including Docker's seccomp (the
  `moby/profiles` profile loaded into podman).
- The overhead of Table 4.

**[external]**, documentation and source only (`mount_namespaces(7)`,
`pid_namespaces(7)`, `user_namespaces(7)`, `pivot_root(2)`, `prctl(2)`,
runc and BuildKit sources):

- `MNT_LOCKED` and `mount_too_revealing` explain the `proc` and remount
  failures. This matches the experiment, but the kernel 7.0 source was not
  read.
- `PR_SET_PDEATHSIG` fires when the parent **thread** dies, not the parent
  process. With tokio `spawn_blocking` (whose threads are reclaimed after
  about 10 s idle) it would kill `RUN` spuriously, so the fork must come from
  a thread that lives for the whole build. The Go documentation for
  `SysProcAttr.Pdeathsig` warns about the same thing; the PoC avoided it with
  `runtime.LockOSThread()` in `init()`.
- AppArmor `docker-default` has `deny mount`, so rootful Docker needs
  `apparmor=unconfined` together with `CAP_SYS_ADMIN`.
- Kubernetes: `procMount: Unmasked` is usable only with `hostUsers: false`.
- runc uses `filepath-securejoin` for mount targets; CVE-2021-30465 is a
  symlink race of exactly this kind.

## Findings

1. A bare `chroot` does not contain root in `RUN`; `pivot_root`, or
   `MS_MOVE` + `chroot` where seccomp blocks `pivot_root`, does (Table 1,
   step 3f).
2. Without `PR_SET_PDEATHSIG`, a `RUN` process outlives a `SIGKILL`ed build
   and keeps writing to the rootfs (Table 1, step 2).
3. Without dropping the bounding set, root in `RUN` can unmount the
   `resolv.conf` bind and modify the rootfs, and a setuid binary under any
   `USER` can regain `CAP_SYS_ADMIN` (Table 1, capability row).
4. Stub files created for missing bind targets are real files: they leak
   into the layer and survive `SIGKILL` unless tracked in `state/` (Table 1,
   step 3d and last row).
5. `unshare(CLONE_NEWNS)` in a multithreaded process moves only the calling
   thread, and `unshare(CLONE_NEWUSER)` fails with `EINVAL`, so namespaces
   must be set up before any thread starts (Table 1, step 1).
6. `mknod` is refused at every rootless level, `sysfs` needs an `rbind`
   fallback with host networking or a new user namespace, and a read-only
   remount must carry the source's locked flags (Table 1, steps 3b–3d).
7. The lowest level that ran everything is a default rootless podman
   container plus `--security-opt unmask=ALL`; with Docker's seccomp and no
   `CAP_SYS_ADMIN`, only kaniko mode remains (Table 2).
8. On an Ubuntu host with `apparmor_restrict_unprivileged_userns=1`,
   creating a user namespace succeeds but every mount in it fails with
   `EACCES`, so a successful `unshare` is not a usable probe (Table 2).
9. The sandbox costs 1–2.5 ms per `RUN` (Table 4).

## Implications for ranikust

The experiment proposed these corrections to survey 02 §3; they feed the
[sandbox design](../../design/sandbox.md), the default isolation of
[ADR-0003](../../adr/0003-dockerfile-trust-and-default-isolation.md) and the
detection rules of
[ADR-0004](../../adr/0004-support-every-privilege-level.md):

1. Step 1: call `unshare(CLONE_NEWNS)` (and `CLONE_NEWUSER` if needed)
   **before** creating any thread.
2. Step 2: add `PR_SET_PDEATHSIG(SIGKILL)` and check `getppid()` after the
   `prctl`. Fork from one fixed thread.
3. Step 3, in the order that worked: `mount --make-rslave /` → bind the
   rootfs onto itself → `proc` → read-only `sysfs` (fallback `rbind`) →
   tmpfs `/dev` + bound nodes + devpts + shm + symlinks → bind
   `resolv.conf`/`hosts` (a copy, target resolved inside the rootfs, remount
   keeping locked flags) → `RUN --mount` → `pivot_root(".", ".")` +
   `umount2(".", MNT_DETACH)` (fallback `MS_MOVE` + `chroot`) →
   `sethostname` (ignore errors) → drop the bounding and inheritable sets to
   Docker's default capabilities → `setgroups/setgid/setuid` →
   `chdir(WORKDIR)` → `PATH` lookup → `execve`.
4. Add a "stub mountpoint" step: record stubs in `state/` before creating
   them, delete them after `RUN`, clean up on start.
5. Remove "or `chroot`".
6. Survey 06 §1, step 1: a successful `unshare` is **not enough** (Ubuntu
   AppArmor); the probe must perform a real `mount`. Step 3 (mount `proc`)
   must run in **exactly the configuration that will be used** (with or
   without a new user namespace), because the `/proc` masks only block when
   a new user namespace is involved. Add probes for `pivot_root` (Docker
   seccomp) and `sysfs`.

`RUN` shares the network namespace with the build, as
[ADR-0010](../../adr/0010-run-network-host-or-none.md) later decided;
`--network=none` was not tested here.

## Limitations and open questions

- **Real root on the host** (no sudo): real `mknod`, `mount_too_revealing`
  in the initial user namespace and rootful Docker's AppArmor were not
  tested. [Experiment 06a](06a-root-rerun.md) re-ran this suite as real root
  via pkexec, and [experiment 13](13-docker-rootful-vm.md) used rootful
  Docker in a VM.
- **Rootful Docker and Kubernetes**: not available on the machine; only
  Docker's seccomp profile was simulated.
- `--network=none`, `RUN --mount`, cgroups, and seccomp for `RUN` itself:
  out of scope.
- TOCTOU races when resolving symlinks: no concurrent attack scenario could
  be built.
- Zombie reaping when the command is PID 1: no dedicated test was written.

## Reproduction

The PoC lived in `docs-rust/poc/02-sandbox/` (archived outside this
repository):

| File | Role |
|---|---|
| `sandbox.go` (+ `go.mod`) | The prototype; built with `CGO_ENABLED=0 go build`. |
| `in-container.sh` | The test groups, run inside the container; takes group names or `all`. |
| `run.sh` | Host driver: builds the PoC, downloads moby's `seccomp/default.json`, exports the three root filesystems with `podman create` + `podman export`, then runs the suite. `./run.sh priv` (`--privileged`), `./run.sh min` (`--security-opt unmask=ALL`), `./run.sh matrix` (Table 2), or no argument for all three. |

The test container is `alpine:3.20` running `sleep 3600`, with the artefact
directory mounted read-only at `/poc`; the root filesystems are extracted to
`/work` inside it. The real-root re-run used the same binary
(`stage/bin/sandbox`) and is described in
[experiment 06a](06a-root-rerun.md).

## References

1. [Survey chapter 02: kaniko-mode requirements](../survey/02-kaniko-mode-requirements.md), §3.
2. [Survey chapter 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md), §1.
3. [Experiment 01: Overlay semantics](01-overlay-semantics.md) (the `/proc`
   mask and `mount_too_revealing`).
4. [Experiment 06a: Root re-run](06a-root-rerun.md).
5. Docker default seccomp profile:
   <https://github.com/moby/profiles/blob/main/seccomp/default.json>
   (fetched 2026-10-29). **[external]**
6. Linux man pages `mount_namespaces(7)`, `pid_namespaces(7)`,
   `user_namespaces(7)`, `pivot_root(2)`, `prctl(2)`. **[external]**
7. Go `syscall.SysProcAttr.Pdeathsig` documentation. **[external]**
8. runc: `--no-pivot`, `filepath-securejoin`; CVE-2021-30465. **[external]**
9. BuildKit `executor/stubs.go`. **[code]**
