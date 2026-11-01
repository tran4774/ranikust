# Choosing where to run it

> This page describes the design. ranikust cannot build images yet. The
> expected modes come from experiments run before the code was written.

ranikust needs **Linux 5.8 or later**: it checks at start-up and stops
with a clear message on older kernels, such as RHEL/CentOS 8, Debian 10,
Ubuntu 20.04 without the HWE kernel or Amazon Linux 2 with kernel 4.14
([ADR-0028](https://github.com/tran4774/ranikust/blob/main/docs/adr/0028-minimum-kernel-linux-5-8.md)).

ranikust picks its mode from what the environment allows (see
[How ranikust builds](how-it-builds.md)). The table lists the mode each common
environment is expected to get. *Measured* means an experiment observed it;
*inferred* means it follows from the platform's documented defaults but has
not been tried.

| Environment | Expected mode | Notes | Basis |
|---|---|---|---|
| Host or VM, as root or with `sudo` | overlay | Every step of the sandbox works without fallbacks. | measured |
| Docker or Podman container with `--privileged` | overlay | Put the state directory (`/var/lib/ranikust`) on a volume or tmpfs: overlay cannot use the container's own overlay root filesystem. | measured |
| Podman container with `--cap-add SYS_ADMIN` | overlay | | measured |
| Docker container with `--cap-add SYS_ADMIN` only | kaniko | Docker's default AppArmor profile still blocks mounts. | measured |
| Podman rootless container (default options) | overlay, rootless | `RUN` sees the container's processes, because a fresh `/proc` cannot be mounted. | measured |
| Docker container with `--security-opt seccomp=unconfined --security-opt apparmor=unconfined` | overlay, rootless | | measured |
| Docker container with default options, under cgroup v2 or v1 | kaniko | Docker's default seccomp profile blocks user namespaces, and AppArmor blocks mounts. | measured |
| GitHub Actions `container:` job, GitLab Kubernetes executor, ordinary Kubernetes pod, GKE Autopilot, Bottlerocket | kaniko | Same restrictions as a default Docker container. | inferred |
| Regular user on Debian 11+, Fedora, RHEL 9+ or Ubuntu 22.04 | overlay, rootless | Images with several users need `newuidmap` and entries in `/etc/subuid` and `/etc/subgid`. | inferred |
| Regular user on Ubuntu 24.04 or later | none | User namespaces get no capabilities by default. With the binary installed at a fixed path and a three-line AppArmor profile for it, rootless overlay mode works. | measured |
| Kubernetes pod under the "restricted" Pod Security Standard, OpenShift `restricted-v2` | none | Neither root nor user namespaces. | inferred |

The measurements are in the project's research reports: the
[privilege matrix](https://github.com/tran4774/ranikust/blob/main/docs/research/experiments/03-privilege-matrix.md),
[rootful Docker in a VM](https://github.com/tran4774/ranikust/blob/main/docs/research/experiments/13-docker-rootful-vm.md)
and the
[mode selection chapter](https://github.com/tran4774/ranikust/blob/main/docs/research/survey/06-overlay-and-parallelism.md).
Real Kubernetes clusters have not been tested yet.

## Picking an environment

- **Fastest builds:** an environment that gets overlay mode, root or
  rootless.
- **Locked-down CI:** kaniko mode works where kaniko works. Builds there are
  sequential on a container's `/`; `--parallel-stages` is not offered yet
  (see [How ranikust builds](how-it-builds.md#kaniko-mode)).
- **Untrusted Dockerfiles:** prefer a disposable VM, or a container made for
  that one build. In kaniko mode the container is the only boundary between
  `RUN` and everything else in it.
- **Never run kaniko mode on a machine you care about.** It treats the
  container's root filesystem as the build root and empties it between
  stages.
