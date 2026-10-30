# Modes and privilege detection

How ranikust decides, at start-up, which snapshot mode and which `RUN`
execution path it can use, and what it does when nothing works.

Decisions: [ADR-0002](../adr/0002-two-snapshot-modes.md),
[ADR-0004](../adr/0004-support-every-privilege-level.md),
[ADR-0014](../adr/0014-kaniko-mode-sequential-by-default.md),
[ADR-0022](../adr/0022-common-state-dir.md),
[ADR-0023](../adr/0023-mode-flag.md).

## Outcomes

The probe ends in exactly one of these outcomes:

| Outcome | Snapshot | Stage roots | `RUN` execution | Stages |
|---|---|---|---|---|
| **overlay, root** | upperdir | one overlay mount per stage in the state directory | full sandbox | parallel (DAG) |
| **overlay, rootless** | upperdir, user namespace | same | full sandbox in a user namespace; `/proc` rbind if a new `proc` is refused | parallel |
| **kaniko, can mount** | walk + hash | one directory per stage | full sandbox (bind mounts, `pivot_root`) | parallel by default |
| **kaniko, cannot mount** | walk + hash | the workdir itself (`/` in a container) | directly, like kaniko (`Isolation::Direct`) | sequential; `--parallel-stages` → one directory per stage, bare `chroot` |
| **refuse** | — | — | — | exit with guidance |

"kaniko, can mount" covers root where overlay is unusable: workdir on NFS or
FUSE, upper directory on the container's overlay root without a volume, or a
kernel without overlay.

## Where environments land

From [experiment 03](../research/experiments/03-privilege-matrix.md) and
[experiment 13](../research/experiments/13-docker-rootful-vm.md); the full
table per CI system is in experiment 03 §6.

| Environment | Outcome |
|---|---|
| Host as root or via `sudo` (GitHub Actions with `sudo`, GitLab.com SaaS) | overlay, root |
| Container `--privileged`; podman `--cap-add SYS_ADMIN` | overlay, root (workdir must be a volume or tmpfs) |
| Docker `--cap-add SYS_ADMIN` alone | kaniko: AppArmor `docker-default` denies `mount` with `EACCES` |
| Rootless podman (default); Docker with `seccomp=unconfined,apparmor=unconfined` | overlay, rootless |
| Unprivileged user on Debian, Fedora, RHEL 8+, Ubuntu ≤ 22.04 | overlay, rootless (full uid range only with `newuidmap` and `/etc/subuid`) |
| Docker default, GitHub Actions `container:` jobs, Kubernetes pods on Ubuntu/COS nodes, GKE Autopilot, Bottlerocket | kaniko, cannot mount |
| Unprivileged user on Ubuntu ≥ 24.04, binary in `~/bin` | refuse |
| Pod Security Standards `restricted`, OpenShift `restricted-v2` | refuse |

Most locked-down CI pods land in kaniko mode. Overlay is available only where
BuildKit or buildah would also run.

## Probe order (`--mode=auto`)

Each crate probes its own capability, and the binary `ranikust` runs the
probes in the order below while the process is still single-threaded, then
picks the outcome:

- `rnk-sandbox`: namespaces, the uid and gid maps, `proc`,
  `pivot_root`/`MS_MOVE`, `sysfs`, bind mounts, `mknod`;
- `rnk-snapshot`: the overlay mount with the
  [ADR-0016](../adr/0016-overlay-mount-options.md) options, and `fsconfig`
  with `lowerdir+`.

Each step performs the real operation. Many environments let an early step
succeed and fail a later one, so the probe never stops at the first success
and never compares kernel versions (Ubuntu and Debian patched overlay in user
namespaces before 5.11; SELinux needs 5.13).

The probe runs before any thread exists. Each attempt runs in a forked child,
which reports its result through a pipe and exits, so failed attempts leave no
namespace or mount behind [inferred]. The overlay probe as root
(`rnk_snapshot::probe`, steps 1 and 4) is the exception: `ranikust` has
already entered the build's private mount namespace, so it mounts there
directly and unmounts; a mount a failure leaves goes with the process. It
runs after the store is open, in `builds/<id>/overlay-probe` (Q-018,
Q-111).

1. **Overlay without a new user namespace.** `unshare(CLONE_NEWNS)`, mount a
   small overlay where the stage roots will live (the state directory) with
   the full option set ([ADR-0016](../adr/0016-overlay-mount-options.md)),
   then mount a new `proc` in a new PID namespace. With `CAP_SYS_ADMIN` this
   is the only way to get a new `proc` when `/proc` is masked: a new user
   namespace makes the kernel refuse it (`EPERM`).
2. **Overlay in a user namespace.**
   - `unshare(CLONE_NEWUSER | CLONE_NEWNS)`, then **write `uid_map` and
     `gid_map`**. Root inside a container writes a multi-range map itself
     (written once; each range must fit inside one range of the parent map,
     so `0 0 1` + `1 1 65536` works where `0 0 65536` is `EPERM`). An
     unprivileged host user needs `newuidmap`/`newgidmap` and `/etc/subuid`;
     without them only one uid maps, and `chown` to any other uid fails with
     `EINVAL`.
   - Mount the overlay (`userxattr` is implied in a user namespace).
   - Mount a new `proc`; on failure try an rbind of the existing `/proc`
     (`RUN` then sees the container's processes, like BuildKit's
     `--oci-worker-no-process-sandbox`).

   On Ubuntu ≥ 24.04 `unshare` succeeds and the process is moved to the
   `unprivileged_userns` AppArmor profile: writing the map and every mount
   then fail with `EACCES`. The probe must reach those steps.
3. **Sandbox capabilities.** Try `pivot_root` (Docker's seccomp profile
   blocks it even with `CAP_SYS_ADMIN`; fallback `MS_MOVE` + `chroot`) and a
   read-only `sysfs` (fallback: rbind `/sys`, remount read-only), so the
   sandbox knows which variant to use ([sandbox](sandbox.md)).
4. **Mount API.** Create a test overlay with `fsconfig(lowerdir+)`. Kernel
   5.15 accepts `FSCONFIG_SET_STRING` for `lowerdir+` and fails only at
   `FSCONFIG_CMD_CREATE` (`EINVAL`), so only a real create counts. On failure
   use `mount(2)` ([overlay-snapshot](overlay-snapshot.md)).
5. **No overlay path passed.**
   - If ranikust is root: **kaniko mode**. A bind-mount test decides between
     "can mount" (per-stage directories, full sandbox) and "cannot mount"
     (sequential on the workdir). `mknod` of a character device is tested
     for the `--parallel-stages` path: it works on rootful Docker and fails
     under rootless podman.
   - Otherwise: **refuse** with guidance (below).
6. **Log the reason** for the chosen outcome, including each failed step and
   its errno (`ranikust`).

Built so far (ADR-0040): `ranikust` enters the build's mount namespace
first, before anything is written; root refused there for lack of privilege
gets kaniko mode ("cannot mount"), with the reason logged. Otherwise the
overlay probes run as in Phase 1, and their refusal stands until the
"kaniko, can mount" variant (Phase 2 task 9). Kaniko mode's flags
(`--ignore-path`, `--ignore-var-run`, `--single-snapshot`, `--materialize`)
are refused in overlay mode where they would change the image
[open: Q-260].

`--mode=overlay|kaniko` skips the choice but not the checks: a forced mode
that cannot work fails with the probe's diagnosis
([ADR-0023](../adr/0023-mode-flag.md)). `--snapshot-mode=full|redo|time`
keeps kaniko's meaning, what the kaniko-mode walk compares; in overlay mode
it is accepted and ignored with a warning.

## Workdir rules

- Without `--workdir`, kaniko mode builds on `/`, like kaniko, and only
  where kaniko's container check finds a container (`ranikust`'s
  `container.rs`, ported from `kaniko@44aa7ddc0 pkg/util/proc/proc.go`), or
  with `--force`, kaniko's meaning
  ([ADR-0040](../adr/0040-kaniko-mode-selection-container-check-and-flags.md)).
  A `--workdir` must not be `/` or a system directory
  ([kaniko-mode](kaniko-mode.md#host-safety-checks)); it needs no
  container.
- The state directory defaults to `<workdir>/.ranikust` in overlay mode; in
  kaniko mode to `/ranikust/state` on `/` and `<cwd>/.ranikust` with a
  `--workdir` (`ADR-0038`); `--state-dir` replaces either
  ([cache-and-store](cache-and-store.md#store-layout)).
- Only kaniko mode locks the workdir (`flock`), because there it is the root
  being built. Overlay-mode builds may share a workdir and its store.
- Overlay mode needs the state directory, where the upper directories live,
  on a filesystem that can hold an upper directory: ext4, xfs, btrfs or
  tmpfs. Not overlayfs (`EINVAL`: "not
  supported as upperdir"), not NFS. A container's own overlay root can be a
  lower layer but not the upper one. Overlay nests at most two deep. The
  probe in step 1 or 2 catches all of these, and the result is kaniko mode.

## Refusal and guidance

kaniko mode needs root (`chown`, `chroot`, switching users). An unprivileged
process with no usable user namespace cannot build, and ranikust says so
instead of producing a broken image. The message offers, in order:

1. Run with `sudo`.
2. Install the AppArmor profile, when
   `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` is `1`: a
   three-line profile granting `userns,` with `flags=(unconfined)`, bound to
   the binary's absolute path. The binary must live in a root-owned path such
   as `/usr/local/bin/ranikust` (or come from a package that ships the
   profile, like podman); a profile for a user-writable path lets anyone swap
   the binary.
3. Run ranikust inside a container.

Inside a plain `chroot`, `/` is not a mount root: the build namespace
cannot make it a slave mount, and `MS_MOVE` would let `..` climb out of a
stage root ([Q-077](../open-questions.md#q-077-how-is-the-ms_move-fallback-kept-from-a-chroot-escape)).
ranikust refuses there before changing anything, and says to run on the
host or in a container, or to bind-mount the chroot directory onto itself
first. Where the kernel does not report whether `/` is a mount root
(`STATX_ATTR_MOUNT_ROOT` missing from the mask), it does not refuse on that
ground and does not fall back to `MS_MOVE`.

Phase 1 builds only the "overlay, root" outcome: every other environment
gets the failing probe step with its errno and the way out: `sudo` or a
`--privileged` container without root; kaniko mode (Phase 2) for root the
environment still refuses, as with Docker's defaults; nothing extra for a
host-side failure such as an unwritable state directory
(`crates/ranikust/src/setup.rs`).

## Failure handling

- A probe step that fails with an unexpected errno is reported verbatim; the
  probe does not guess.
- The probe result is fixed for the whole build. A later mount failure in the
  chosen mode is a build error, not a reason to switch modes mid-build.
- When an overlay-mode `RUN` fails with `EXDEV`, ranikust suggests rerunning
  with `--mode=kaniko` ([ADR-0016](../adr/0016-overlay-mount-options.md),
  [ADR-0023](../adr/0023-mode-flag.md)).

## Open questions

- [Q-039](../open-questions.md#q-039-which-environments-must-the-probe-be-tested-in) Which environments must the probe be tested in?
- [Q-040](../open-questions.md#q-040-what-happens-to-a-rootless-user-without-subuid-ranges) What happens to a rootless user without subuid ranges?
- [Q-018](../open-questions.md#q-018-where-does-the-probe-mount-its-test-overlay) Where does the probe mount its test overlay?
- [Q-017](../open-questions.md#q-017-which-type-carries-the-probe-outcome-to-the-libraries) Which type carries the probe outcome to the libraries?

## References

- [Survey 06](../research/survey/06-overlay-and-parallelism.md) §1;
  [survey 02](../research/survey/02-kaniko-mode-requirements.md) §1–§2.
- [Experiment 01](../research/experiments/01-overlay-semantics.md) §2 (privilege
  matrix, masked `/proc`, single-uid map),
  [experiment 02](../research/experiments/02-run-sandbox.md) §2 (minimum
  privileges), [experiment 03](../research/experiments/03-privilege-matrix.md),
  [experiment 11](../research/experiments/11-root-extras.md) (AppArmor
  profile, nesting), [experiment 13](../research/experiments/13-docker-rootful-vm.md)
  (Docker 29.1, cgroup v1 and v2, kernel 5.15).
