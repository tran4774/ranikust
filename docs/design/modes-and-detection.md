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
| **overlay, rootless** (planned, Phase 3) | upperdir, user namespace | same | full sandbox in a user namespace; `/proc` rbind if a new `proc` is refused | parallel |
| **kaniko, can mount** | walk + hash | one directory per stage | full sandbox (bind mounts, `pivot_root`) | parallel by default |
| **kaniko, cannot mount** | walk + hash | `/` in a container, else a root in the state directory | directly, like kaniko (`Isolation::Direct`) | sequential on `/`; `--parallel-stages` refused (ADR-0060) |
| **refuse** | — | — | — | exit with guidance |

"kaniko, can mount" covers root where overlay is unusable: state directory on NFS or
FUSE, upper directory on the container's overlay root without a volume, or a
kernel without overlay.

**Built:** overlay, root; both kaniko outcomes; refuse. **Planned, Phase 3:**
overlay, rootless, and the user-namespace probe it needs. No code makes a
user namespace for the probe or the build: `enter_build_namespace` in
`crates/rnk-sandbox/src/setup.rs` does `unshare(CLONE_NEWNS)` only, the
sandbox probe (`crates/rnk-sandbox/src/probe.rs`) runs the `RUN` sequence in
an empty root with `pivot_root`, then `MS_MOVE`, and `Outcome` in
`crates/ranikust/src/setup.rs` has `OverlayRoot` alone (kaniko mode is set
up by `set_up_kaniko`). Until then, under `--mode=auto`, root lacking the
privilege to mount (`Error::Privilege`, `namespace_refusal`) goes to kaniko
mode, which builds on `/` in a container and is refused on a host, where
root may not mount (`host_without_mounts`, ADR-0050); a process that is not
root is refused with `NEEDS_ROOT`; and `--mode=overlay` is told to use kaniko
mode (`NEEDS_KANIKO_MODE`).

## Where environments land

From [experiment 03](../research/experiments/03-privilege-matrix.md) and
[experiment 13](../research/experiments/13-docker-rootful-vm.md); the full
table per CI system is in experiment 03 §6.

| Environment | Outcome |
|---|---|
| Host as root or via `sudo` (GitHub Actions with `sudo`, GitLab.com SaaS) | overlay, root |
| Container `--privileged`; podman `--cap-add SYS_ADMIN` | overlay, root (the state directory must be a volume or tmpfs) |
| Docker `--cap-add SYS_ADMIN` alone | kaniko: AppArmor `docker-default` denies `mount` with `EACCES` |
| Rootless podman (default); Docker with `seccomp=unconfined,apparmor=unconfined` | overlay, rootless (planned, Phase 3) |
| Unprivileged user on Debian, Fedora, RHEL 8+, Ubuntu ≤ 22.04 | overlay, rootless (planned, Phase 3; full uid range only with `newuidmap` and `/etc/subuid`) |
| Docker default, GitHub Actions `container:` jobs, Kubernetes pods on Ubuntu/COS nodes, GKE Autopilot, Bottlerocket | kaniko, cannot mount |
| Unprivileged user on Ubuntu ≥ 24.04, binary in `~/bin` | refuse |
| Pod Security Standards `restricted`, OpenShift `restricted-v2` | refuse |

Most locked-down CI pods land in kaniko mode. Overlay is available only where
BuildKit or buildah would also run. Until rootless mode is built, the two
rows marked planned end differently: root without `CAP_SYS_ADMIN` (the root
of a rootless podman container, or of a Docker container started without
`--cap-add SYS_ADMIN`) goes to kaniko mode under `--mode=auto`, which builds
on `/` in a container, with its own checks (ADR-0054), and is refused on a
host; an unprivileged user is refused.

## Probe order (`--mode=auto`)

Each crate probes its own capability, and the binary `ranikust` runs the
probes in the order below while the process is still single-threaded, then
picks the outcome:

- `rnk-sandbox`: the real `RUN` sequence in an empty root, as built
  (`probe.rs`): the namespaces, `proc`, `sysfs`, the `/dev` tmpfs and its
  node binds, `pivot_root` then `MS_MOVE`, the capability drop and the
  `USER` switch. No probe step tries a user namespace, the uid and gid maps
  or `mknod`; they are planned with rootless mode (Phase 3);
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
2. **Overlay in a user namespace** (planned, Phase 3; not built: no code
   creates a user namespace for a probe).
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
   read-only `sysfs` (the rbind `/sys` fallback is planned, Phase 3: a refused
   `sysfs` mount fails the probe today), so the sandbox knows which variant
   to use ([sandbox](sandbox.md)).
4. **Mount API.** Create a test overlay with `fsconfig(lowerdir+)`. Kernel
   5.15 accepts `FSCONFIG_SET_STRING` for `lowerdir+` and fails only at
   `FSCONFIG_CMD_CREATE` (`EINVAL`), so only a real create counts. On failure
   use `mount(2)` ([overlay-snapshot](overlay-snapshot.md)).
5. **No overlay path passed.**
   - If ranikust is root: **kaniko mode**. A bind-mount test decides between
     "can mount" (per-stage directories, full sandbox) and "cannot mount"
     (sequential on `/` in a container; a host where root may not mount is
     refused, ADR-0050).
   - Otherwise: **refuse** with guidance (below).
6. **Log the reason** for the chosen outcome, including each failed step and
   its errno (`ranikust`).

Built (ADR-0040, ADR-0059): `ranikust` enters the build's mount namespace
first, before anything is written; root refused there for lack of privilege
gets kaniko mode ("cannot mount"), with the reason logged. Otherwise the
overlay probes run as in Phase 1. Where they refuse because overlayfs
cannot use the state directory (the probe's mount refused, or a state
directory on overlayfs), `--mode=auto` outside a container builds in kaniko
mode with a directory per stage and the full sandbox ("can mount"), with
the reason logged; a failed step of the sandbox itself never routes there,
since that variant needs the same sandbox. In a container where root may
mount, kaniko mode on `/` is refused (ADR-0054), so the overlay refusal
stands with what makes overlay mode work (a volume at the state
directory). No refusal's guidance sends a user back to the mode they
used. `--ignore-path` is refused in overlay mode where it would change the
image, for good (ADR-0049); `--single-snapshot` is refused in every mode
for good (ADR-0065); `--materialize` is refused until Phase 4
(ADR-0064); `--ignore-var-run` is refused in both modes (ADR-0052 D4)
([ADR-0049](../adr/0049-security-and-performance-over-kaniko-compatibility.md)).

`--mode=overlay|kaniko` skips the choice but not the checks: a forced mode
that cannot work fails with the probe's diagnosis
([ADR-0023](../adr/0023-mode-flag.md)). `--snapshot-mode=full|redo|time`
keeps kaniko's meaning, what the kaniko-mode walk compares; in overlay mode
it is accepted and ignored with a warning.

## Build root and state directory

- There is no work-directory flag (ADR-0050). Kaniko mode builds on `/`,
  like kaniko, where kaniko's container check finds a container
  (`ranikust`'s `container.rs`, ported from `kaniko@44aa7ddc0
  pkg/util/proc/proc.go`, ADR-0048), and everywhere else in a directory per
  stage it makes in its state directory,
  `<state>/builds/<id>/stages/<n>/rootfs` (ADR-0059); it never builds on
  a machine's own `/`, and kaniko's `--force` is refused. Outside a
  container it needs mount privileges, for the full sandbox; a host where
  root may not mount is refused (Q-330).
- The state directory is `/var/lib/ranikust` in every mode; `--state-dir`
  replaces it ([cache-and-store](cache-and-store.md#store-layout)). It must
  be owned by the user running ranikust, not writable by others, below
  directories only root may write; ranikust makes it `0700`
  ([kaniko-mode](kaniko-mode.md#host-safety-checks)).
- Kaniko mode on `/` locks it (`flock` on `/` itself), one build
  per container. Overlay-mode builds and kaniko-mode builds on a host share
  the state directory and its store; each has its own `builds/<id>`.
- Overlay mode needs the state directory, where the upper directories live,
  on a filesystem that can hold an upper directory: ext4, xfs, btrfs or
  tmpfs. Not overlayfs (`EINVAL`: "not
  supported as upperdir"), not NFS. A container's own overlay root can be a
  lower layer but not the upper one. Overlay nests at most two deep. The
  probe in step 1 or 2 catches all of these, and the result is kaniko mode.
  A state directory on overlayfs is refused before the probe, telling to
  mount a volume there.

## Refusal and guidance

kaniko mode needs root (`chown`, `chroot`, switching users). An unprivileged
process with no usable user namespace cannot build, and ranikust says so
instead of producing a broken image. Built today: a process that is not root
is refused whatever its user namespaces allow, with the advice to run as root
or in a container (`NEEDS_ROOT`, `crates/ranikust/src/setup.rs`). Planned,
Phase 3, with rootless mode: refusing only where no user namespace is usable,
and offering, in order:

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

Rootless overlay mode and the guidance above that belongs to it are planned
for Phase 3. Today root that may not mount takes kaniko mode in a container
and is refused on a host (ADR-0050; a forced `--mode=overlay` is told to use
kaniko mode). Refusals also come from a process that is not root, a failed
probe step, kaniko mode on `/` where `refuse_unsafe_slash` stops it (root
may mount or a writable `/proc/sys`, ADR-0054; `--network=none`, ADR-0057) and a
container where root may mount but overlayfs is unusable
(`Route::RefuseInContainer`). A probe refusal gives the failing step with its
errno and the way out (`sudo` or a `--privileged` container for a process
that is not root). A host-side failure such as an unwritable state
directory is reported as it is, with nothing extra
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
