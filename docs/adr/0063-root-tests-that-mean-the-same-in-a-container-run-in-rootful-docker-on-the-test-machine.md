---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0063: Root tests that mean the same in a container run in rootful Docker on the test machine

## Context and Problem Statement

Every test that needs root (tier T2) ran in the one local QEMU/KVM VM, and
the VM lock made parallel branches wait for each other. The maintainer
added a VPS as a second test machine; it has no nested virtualisation, so
it cannot run the VM, but it has rootful Docker. Many root tests only need
root's capabilities inside a namespace of their own (`mknod`, `chown`,
`setgroups`, mounts in a private mount namespace, overlay mounts), which a
throwaway container gives. Others would not mean the same there: a
container's own guards (the device cgroup, Docker's seccomp profile and
capability set, AppArmor) block what they assert ranikust blocks, or they
need the host's own root (the `pivot_root` path, overlay as root on a
host), or a failure could harm the machine. The question: which root tests
may leave the VM, and on what terms.

## Considered Options

- Keep every root test in the VM.
- Run root tests on the VPS host itself as root.
- Run the root tests that mean the same in a container in throwaway rootful
  Docker containers on the VPS, and keep the others in the VM.

The first keeps the bottleneck. The second puts real root on a machine that
is not disposable. The third was proposed and accepted.

## Decision Outcome

Root tests that mean the same in a container are tier **T2c** and run in
throwaway rootful Docker containers on the disposable test machine (the
VPS); all other root tests stay tier T2, in the VM only. The VM's `unit`
suite keeps running both, so it remains the full reference.

Maintainer, 2026-11-01: "cho chạy docker rootful", and "ok" to the plan:
never `--privileged`, no bind mount of a host directory, `--rm` throwaway
containers, test binaries copied in; acceptance and benchmarks stay in the
VM.

- **Marker:** `#[ignore = "T2c: <profile>: <reason>"]`, with profile
  `default` (`docker run` as is), `sysadmin` (`--cap-add SYS_ADMIN
  --security-opt apparmor=unconfined`) or `netadmin` (`sysadmin` plus
  `--cap-add NET_ADMIN`); a test gets the smallest profile it needs.
  `#[ignore = "T2: <reason>"]` stays VM-only.
- **A test stays T2 (VM only) when** (1) a failure could harm the machine
  that runs it (escapes, deleting `/`, writes to `/proc/sys`, `modprobe`,
  opening real devices); (2) it asserts something is blocked that a
  container's guards would block anyway (a `nodev` mount, the capability
  bounding set, a device the device cgroup refuses), so it would pass
  without proving anything; (3) it needs the host's own root (the
  `pivot_root` path, overlay as root on a host, kaniko mode as root off
  `/`, `CAP_SYS_RESOURCE`, cgroup v1); (4) it is an acceptance run or a
  benchmark.
- **Control checks:** before the tests of a profile, the container proves
  it allows what they exercise (a 1:5 device node in `TMPDIR` opens, block
  nodes can be made, `chown`, `setgroups`; for `sysadmin`/`netadmin` a
  private mount namespace, a tmpfs and an overlay mount, a user namespace;
  for `netadmin` loopback up in a new network namespace) and holds no more
  than the profile says (Docker's default bounding set and no mount
  namespace for `default`; no `NET_ADMIN` for `sysadmin`). A failed check
  fails the run; nothing is skipped.
- **Containers:** never `--privileged`; no volume or bind mount of a host
  directory (the static test binaries are copied into an image built for
  the run); `--network none`; tmpfs work directories (`/work`, `/var/tmp`)
  mounted with device nodes allowed; `--rm`; memory, process and
  descriptor limits. The results
  are the containers' output, written by the shell on the test machine.
- **Where:** only on the disposable test machine:
  `tests/container/lane.sh` refuses unless `RANIKUST_CONTAINER_LANE=1` and
  refuses rootless Docker and Podman; `just test-container` (via
  `tests/container/remote.sh`) sends the committed branch there through
  the lane helper. Nothing privileged runs on a developer's machine.

### Consequences

- Good, because 25 of the 31 root tests can run beside the VM, so branches
  no longer queue for the one VM lock for them.
- Good, because the `default` profile is the environment kaniko mode on a
  container's `/` targets, so the Direct `RUN` tests run where users run
  ranikust.
- Bad, because a T2c test is checked against one kernel (the VPS's) and
  Docker's seccomp profile, which can differ from the VM's; the VM still
  runs every T2c test for that reason.
- Bad, because the `default` control check pins Docker's default bounding
  set (`00000000a80425fb`), so a Docker release that changes it fails the
  lane until the check is updated.
- Neutral, because the rootful Docker daemon is root-equivalent for the
  `docker` group on the test machine; that machine is the maintainer's
  disposable test host, not a developer's machine.

## Evidence

- Probe on the VPS (Ubuntu 24.04, kernel 6.8, Docker 29.9.2, cgroup v2,
  2026-11-01): in a default container `mknod` (char and block), opening
  1:5 in a `dev` tmpfs, `chown` and `setgroups` work and `unshare -m`
  fails; with `SYS_ADMIN` and AppArmor unconfined a private mount
  namespace, a tmpfs, an overlay mount and a user namespace work;
  `NET_ADMIN` is needed to bring loopback up in a new network namespace.
- Docker's `--tmpfs` is `nodev` by default, which would make a device-node
  test pass without opening anything; the lane mounts it with `dev` and a
  control check proves a node opens.

## More Information

Tiers, profiles, the harness and the "which test for what" table:
[docs/testing.md](../testing.md). The tiers themselves were set there, not
by an ADR; this is the first decision record about where a tier runs.
