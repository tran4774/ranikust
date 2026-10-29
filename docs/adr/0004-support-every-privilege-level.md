---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0004: Support every privilege level: detect, fall back, or refuse

## Context and Problem Statement

ranikust is not tied to one environment. It may run as root on a host, inside
a privileged or rootless container, in a locked-down CI pod, or as an
unprivileged user on a workstation. Each of these allows a different subset
of `unshare`, `mount`, `pivot_root` and `mknod`, and some environments allow
nothing useful. Which privilege levels must ranikust support, and how does it
find out where it is?

## Considered Options

- Require root or a privileged container, like buildah and BuildKit.
- Require running as root inside a container, like kaniko.
- Support every level: probe at run time, fall back step by step, and refuse
  with guidance where no mode can work.

## Decision Outcome

ranikust 1.0 supports overlay as real root, overlay in a user namespace and
kaniko mode, picks among them by actually performing each privileged step,
and refuses with specific guidance where none of them works.

- "Everywhere" means everywhere kaniko or BuildKit/buildah can run.
- The probe tries the real operations in a fixed order (write the uid map,
  mount overlay, mount `proc`, `pivot_root`, `sysfs`) and never compares
  kernel versions. Order and fallbacks:
  [modes-and-detection](../design/modes-and-detection.md).
- Environments where no mode works are accepted as a known limitation
  (maintainer, 2026-10-29): an unprivileged user on Ubuntu ≥ 24.04 with the
  binary in a user-writable path, Pod Security Standards `restricted` pods,
  OpenShift `restricted-v2`. kaniko mode needs root; there is no silent
  degradation to a mode that cannot `chown`.
- The refusal names the fix: run with `sudo`, install the AppArmor profile for
  a root-owned binary path, or run inside a container. It reads
  `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` to choose the hint.
- The probe is a core component with tests for every privilege level.

### Consequences

- Good, because one binary builds in the widest range of environments.
- Good, because failures are explicit and actionable rather than half-working
  builds.
- Bad, because three code paths and a privilege test matrix (rootless podman,
  rootful Docker in a VM, real root, Kubernetes) must be maintained.
- Bad, because some users, notably on stock Ubuntu ≥ 24.04 without `sudo`, get
  a refusal instead of a build.

## Evidence

- [Experiment 03](../research/experiments/03-privilege-matrix.md): environment
  → mode table; Ubuntu ≥ 24.04 lets `unshare(CLONE_NEWUSER)` succeed and then
  strips every capability, so a probe that stops at `unshare` is wrong.
- [Experiment 01](../research/experiments/01-overlay-semantics.md): in a
  default rootless container, overlay in a user namespace works but a new
  `proc` fails with `EPERM` because `/proc` is masked; with `CAP_SYS_ADMIN`,
  `proc` mounts only without a new user namespace.
- [Experiment 11](../research/experiments/11-root-extras.md): a three-line
  AppArmor profile (`userns,` with `flags=(unconfined)`) bound to the binary's
  absolute path gives an unprivileged user full rootless overlay, including a
  new `proc`; the same binary at another path stays blocked.
- [Experiment 13](../research/experiments/13-docker-rootful-vm.md): on Docker
  29.1 rootful, the default profile blocks `CLONE_NEWUSER` (`EPERM`) and
  `--cap-add SYS_ADMIN` alone is still denied `mount` by AppArmor (`EACCES`);
  results are identical under cgroup v1 and v2.

## More Information

Kubernetes, other kernels (5.10, 6.1, RHEL 8/9), SELinux and Docker rootless
were not tested in the survey and are phase 3 work. The AppArmor hint is safe
only for a root-owned path such as `/usr/local/bin/ranikust`, never `~/bin`.

A mode is forced with `--mode=overlay|kaniko`, which still runs the checks
([ADR-0023](0023-mode-flag.md)).
