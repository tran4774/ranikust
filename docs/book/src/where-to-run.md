# Choosing where to run it

> Overlay mode as root, rootless overlay mode and kaniko mode are built. The
> AppArmor advice for Ubuntu 24.04 and `--userns` (Phase 3) are not.

ranikust needs **Linux 5.8 or later**. It checks at start-up and stops with a
clear message on older kernels, such as RHEL/CentOS 8, Debian 10, Ubuntu 20.04
without the HWE kernel or Amazon Linux 2 with kernel 4.14 ([ADR-0028]).

ranikust picks its mode from what the environment allows (see
[How ranikust builds](how-it-builds.md)). The table lists the mode each common
environment gets. *Measured* means an experiment observed it; *inferred* means
it follows from the platform's documented defaults and has not been tried.

| Environment | Mode | Notes | Basis |
|---|---|---|---|
| Host or VM, as root or with `sudo` | overlay | Every step of the sandbox works without fallbacks. | measured |
| Docker or Podman container with `--privileged` | overlay | Put the state directory (`/var/lib/ranikust`) on a volume or tmpfs: overlay cannot use the container's own overlay root filesystem. | measured |
| Podman container with `--cap-add SYS_ADMIN` | overlay | | measured |
| Docker container with `--cap-add SYS_ADMIN` only | kaniko | Docker's default AppArmor profile still blocks mounts. | measured |
| Podman rootless container (default options) | overlay, rootless, with a volume at the state directory and `--allow proc.rbind`; kaniko otherwise | The container's root has no `CAP_SYS_ADMIN`, so ranikust cannot mount; it enters a user namespace of its own, where it can. The state directory must not be on the container's own root: without a volume there (`-v ranikust-state:/var/lib/ranikust`) `--mode=auto` builds on the container's `/` in kaniko mode and logs a hint to mount one ([ADR-0107](https://github.com/tran4774/ranikust/blob/main/docs/adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)). Where a fresh `/proc` cannot be mounted (Podman's masked `/proc`), rootless overlay mode refuses unless the operator passes `--allow proc.rbind`: `--mode=overlay` is refused, naming the flag, and `--mode=auto` builds in kaniko mode. With the flag a `RUN` sees every process of the container's PID namespace (the host's or other containers' too where it shares one) and reads the files of `/proc/<pid>` not under ptrace's check, such as `cmdline`, `status`, `stat`, `mountinfo`, `cgroup` and `net/*` (ranikust's too), but not their environment, files or memory ([ADR-0130](https://github.com/tran4774/ranikust/blob/main/docs/adr/0130-with-allow-proc-rbind-the-residual-is-the-measured-one-the-peer-scan-stays-and-the-build-uses-the-persistent-store.md); measured on Linux 5.15 and 6.8, and through ranikust's sandbox by tests); the bind is read-only, so a `RUN` cannot write `/proc`, its own `oom_score_adj` included, as it can with a fresh one ([ADR-0132](https://github.com/tran4774/ranikust/blob/main/docs/adr/0132-the-proc-rbind-is-bound-read-only-so-a-run-writes-nothing-under-it.md)); `RUN`s then run one at a time ([ADR-0127](https://github.com/tran4774/ranikust/blob/main/docs/adr/0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md)). Use it with no secrets in other processes' command lines, ranikust's included, and the container in a PID namespace of its own: ranikust refuses a PID namespace it shares with the host or another container ([ADR-0129](https://github.com/tran4774/ranikust/blob/main/docs/adr/0129-with-allow-proc-rbind-a-shared-pid-namespace-is-refused-and-the-store-is-anonymous.md), [ADR-0134](https://github.com/tran4774/ranikust/blob/main/docs/adr/0134-with-allow-proc-rbind-the-pid-namespace-check-reads-files-no-ptrace-check-guards.md)). An image with an owner outside the container's id map fails mid-build with a `chown` error today; planned, ranikust refuses it before the build and names the fix ([ADR-0098](https://github.com/tran4774/ranikust/blob/main/docs/adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)). | measured |
| Docker container with `--security-opt seccomp=unconfined --security-opt apparmor=unconfined` | overlay, rootless, with a volume at the state directory and `--allow proc.rbind` (Docker's `/proc` is masked too); kaniko otherwise | Without `--cap-add SYS_ADMIN` the container's root cannot mount. | inferred |
| Docker container with default options, under cgroup v2 or v1 | kaniko | Docker's default seccomp profile blocks user namespaces, and AppArmor blocks mounts. | measured |
| GitHub Actions `container:` job, GitLab Kubernetes executor, ordinary Kubernetes pod, GKE Autopilot, Bottlerocket | kaniko | Same restrictions as a default Docker container. | inferred |
| Regular user on Debian 12+, Fedora, RHEL 9+ or Ubuntu 22.04 | overlay, rootless | A user namespace of ranikust's own gives it the mounts it needs. An image with several owners needs `newuidmap` and `/etc/subuid` and `/etc/subgid` entries; without them ranikust maps one id and says so; planned, it then refuses such an image, as it does one with a user outside the ranges ([ADR-0098](https://github.com/tran4774/ranikust/blob/main/docs/adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)). | measured on Ubuntu 22.04, inferred for the rest |
| Regular user on Ubuntu 24.04 or later | none | Refused with the step of the probe that failed (the `chown`, in a user namespace without capabilities) and the AppArmor switch that causes it. There, user namespaces get no capabilities by default; a three-line AppArmor profile for the binary at a fixed path is the planned remedy, which the message does not name yet. | measured |
| Kubernetes pod under the "restricted" Pod Security Standard, OpenShift `restricted-v2` | none | Neither root nor user namespaces. | inferred |

The measurements are in the research reports: the
[privilege matrix](https://github.com/tran4774/ranikust/blob/main/docs/research/experiments/03-privilege-matrix.md),
[rootful Docker in a VM](https://github.com/tran4774/ranikust/blob/main/docs/research/experiments/13-docker-rootful-vm.md)
and the
[mode selection chapter](https://github.com/tran4774/ranikust/blob/main/docs/research/survey/06-overlay-and-parallelism.md).
Real Kubernetes clusters have not been tested yet.

## Cgroups of `RUN` commands

Where the cgroup tree ranikust starts in was delegated to it, each `RUN` gets
cgroups of its own, and when it ends everything it started is killed through
them, with any cgroup a command made below its own: in a
`docker run --privileged` container, or wherever ranikust starts at the root
of its own cgroup namespace with a writable cgroupfs, and in a systemd unit or
scope with `Delegate=yes` whose cgroup systemd marks as delegated with
`trusted.delegate` (Ubuntu 22.04's systemd 249 and 24.04's 255 do). In
rootless overlay mode, the root of its own cgroup namespace counts, and a
cgroup its user owns that systemd marks `user.delegate`, such as a system
unit with `Delegate=yes` and `User=` that user; a unit of the user's own
systemd manager (`systemd-run --user`) carries no mark. Anywhere else
nothing changes. They set no resource limit, only how many
cgroups a `RUN` may make below its own, and how deep: 4095 cgroups and 32
levels at most, past which `mkdir` fails with `EAGAIN`. ranikust logs which
layout it uses:

- **nested**, when ranikust is the only process in its cgroup and the `cpu`
  controller is delegated: ranikust moves into a cgroup of its own below it
  for the build, and each `RUN` gets one cgroup for its init and one for its
  command. `docker exec` into the container still works; its process joins
  ranikust's cgroup, and one still running when the build ends keeps that
  cgroup, which ranikust then leaves with a warning.
- **single** otherwise: the command alone gets a cgroup.

Either way the command runs in a cgroup namespace of its own, so a `RUN`
reads `0::/` in `/proc/self/cgroup`, as in a Docker container, and a file
that records it is the same whatever the build's `--jobs`. Its init's cgroup
in `/proc/1/cgroup` still differs between the two layouts, and where a `RUN`
gets no cgroup it reads ranikust's own, which depends on where ranikust
runs.

A ranikust killed with `SIGKILL` leaves these cgroups, empty, until its unit
or container ends; systemd and Docker then remove them.

## An amd64 container on Apple Silicon

On an Apple Silicon Mac, OrbStack runs `linux/amd64` containers through
Rosetta, which lacks `openat2`, so the x86_64 release stops at start-up
there although the kernel is new enough; the message names the emulator
rather than the kernel (measured). Docker Desktop with its Rosetta setting on
should behave the same (inferred). Under QEMU's user mode it stops at start-up
too: at the same kernel check with qemu-user 8.2.2, at its check that it runs
alone in its process with qemu-user 10.2.1, whose own thread fails it
(measured on Linux, not on Docker Desktop).

An untested workaround, not a supported set-up: there is no aarch64 release
and CI does not cover this use ([ADR-0009]). Build ranikust for
`aarch64-unknown-linux-musl` from a checkout
(`cargo zigbuild --release --locked --target aarch64-unknown-linux-musl -p ranikust`,
with the toolchain of the
[development set-up](https://github.com/tran4774/ranikust/blob/main/docs/development-setup.md))
and run it in an arm64 container. The image it builds is still `linux/amd64`,
the only platform accepted so far, and its `RUN` steps go through Rosetta.
[Experiment 17](https://github.com/tran4774/ranikust/blob/main/docs/research/experiments/17-apple-silicon-rosetta.md)
tried it once, on OrbStack 2.2.5 and macOS 26.7: an `alpine` image with one
`RUN`, in kaniko mode.

## A seccomp profile without `open_tree` and `move_mount`

Where `RUN` gets the full sandbox (overlay mode, kaniko mode off `/`) but a
seccomp profile refuses `open_tree` and `move_mount` (Linux 5.2; the profile
predates those calls), each `RUN --mount` and secret file is bound and
remounted by path instead. Its target may then not pass through a symbolic
link: a target under `/var/run` (a link to `/run` in many images), under a
usrmerge `/lib` or below any linked directory fails that `RUN` with "Too many
levels of symbolic links". Name the target by its real path, or allow those
two calls in the profile.

## Picking an environment

- **Fastest builds:** an environment that gets overlay mode: root on a host or
  in a privileged container.
- **Locked-down CI:** kaniko mode works where kaniko works. Builds there are
  sequential on a container's `/`; `--parallel-stages` is refused for good,
  since a bare `chroot` per stage would let a `RUN` reach the others (see
  [kaniko mode](how-it-builds.md#kaniko-mode)).
- **Untrusted Dockerfiles:** prefer a disposable VM, or a container made for
  that one build. In kaniko mode on `/` the container is the only boundary
  between `RUN` and everything else in it.
- **Never run kaniko mode on a machine you care about.** On a container's `/`
  it treats the root filesystem as the build root and empties it between
  stages.

[ADR-0009]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0009-x86-64-first-aarch64-ready.md
[ADR-0028]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0028-minimum-kernel-linux-5-8.md
