# Choosing where to run it

> Overlay mode as root and kaniko mode are built; rootless overlay mode
> (Phase 3) is not, so a row that would use it shows what happens today.

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
| Podman rootless container (default options) | kaniko (rootless overlay planned, Phase 3, with a volume at the state directory) | The container's root has no `CAP_SYS_ADMIN`, so ranikust cannot mount and builds on the container's `/`, and keeps doing so without a volume ([ADR-0107](https://github.com/tran4774/ranikust/blob/main/docs/adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)). Rootless overlay mode would run in a user namespace and, where a fresh `/proc` cannot be mounted, let `RUN` see the container's processes; today a refused `proc` mount fails the `RUN`. An image with an owner outside the container's id map fails mid-build with a `chown` error today; planned, ranikust refuses it before the build and names the fix ([ADR-0098](https://github.com/tran4774/ranikust/blob/main/docs/adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)). | inferred |
| Docker container with `--security-opt seccomp=unconfined --security-opt apparmor=unconfined` | kaniko (rootless overlay planned, Phase 3) | Without `--cap-add SYS_ADMIN` the container's root cannot mount. | inferred |
| Docker container with default options, under cgroup v2 or v1 | kaniko | Docker's default seccomp profile blocks user namespaces, and AppArmor blocks mounts. | measured |
| GitHub Actions `container:` job, GitLab Kubernetes executor, ordinary Kubernetes pod, GKE Autopilot, Bottlerocket | kaniko | Same restrictions as a default Docker container. | inferred |
| Regular user on Debian 12+, Fedora, RHEL 9+ or Ubuntu 22.04 | none: refused today (rootless overlay planned, Phase 3) | ranikust builds as root only for now and says to run as root or in a container. Rootless overlay mode would need `newuidmap` and `/etc/subuid` and `/etc/subgid` entries for images with several users, and without them it would refuse such an image, as it would one with a user outside the ranges ([ADR-0098](https://github.com/tran4774/ranikust/blob/main/docs/adr/0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)). | inferred |
| Regular user on Ubuntu 24.04 or later | none | Refused, as is every regular user until Phase 3. There, user namespaces get no capabilities by default; a three-line AppArmor profile for the binary at a fixed path is the planned remedy. | measured |
| Kubernetes pod under the "restricted" Pod Security Standard, OpenShift `restricted-v2` | none | Neither root nor user namespaces. | inferred |

The measurements are in the research reports: the
[privilege matrix](https://github.com/tran4774/ranikust/blob/main/docs/research/experiments/03-privilege-matrix.md),
[rootful Docker in a VM](https://github.com/tran4774/ranikust/blob/main/docs/research/experiments/13-docker-rootful-vm.md)
and the
[mode selection chapter](https://github.com/tran4774/ranikust/blob/main/docs/research/survey/06-overlay-and-parallelism.md).
Real Kubernetes clusters have not been tested yet.

## An amd64 container on Apple Silicon

On an Apple Silicon Mac, OrbStack runs `linux/amd64` containers through
Rosetta, which lacks `openat2`, so the x86_64 release stops at start-up
there although the kernel is new enough; the message names the emulator
rather than the kernel (measured). Docker Desktop with its Rosetta setting on
should behave the same (inferred); with QEMU instead, it depends on the QEMU
version.

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
