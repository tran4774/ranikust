---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0100: The probe is tested in today's harnesses plus runner, VM and k3s legs and Alma 9; older kernels, Docker rootless and NFS or FUSE wait for Phase 6

## Context and Problem Statement

The probe picks a mode by performing each step
([ADR-0002](0002-two-snapshot-modes.md), [ADR-0004](0004-support-every-privilege-level.md)),
and [modes-and-detection](../design/modes-and-detection.md#where-environments-land)
lists where environments should land, most of it **[inferred]**. Three
harnesses exist: the local QEMU/KVM VM (T2; Ubuntu 24.04 with kernel 6.8, or
22.04 with kernel 5.15 through `RANIKUST_VM_RELEASE`; Docker, no Podman), the
GitHub-hosted runner ([ADR-0090](0090-t2-also-runs-as-root-on-github-hosted-runners.md);
Ubuntu 24.04, AppArmor restriction on, passwordless `sudo`, user `runner`,
Docker and Podman), and throwaway rootful Docker containers (T2c,
[ADR-0063](0063-root-tests-that-mean-the-same-in-a-container-run-in-rootful-docker-on-the-test-machine.md);
profiles `default`, `sysadmin`, `netadmin`, never `--privileged`).
[testing](../testing.md#not-covered-yet) lists what none of them covers. Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)) adds the user-namespace
path, which runs in no tier yet. Which environments must the probe be tested
in (#11, Q-039)?

## Considered Options

- (a) Today's harnesses only: rows 1, 2, 2b, 3b, 5b and 9d of the matrix below.
  No new harness work; the rootless outcome and the refusal hint ship untested
  outside T0 and the developer's machine, and the book's rows stay inferred.
  Weakest for security: the user-namespace path, the one Phase 3 adds, runs in
  no tier.
- (b) Plus the Phase 3 harness: runner jobs for rows 5, 6 and 6b; VM legs for
  rows 2c, 3a, 4, 6c and 6d; a `k8s` suite with k3s for rows 7a to 7d. Every
  outcome Phase 3 adds runs in T1 or T2 on both kernels the VM boots; costs one
  runner job per environment and a k3s install in the VM or on the runner.
- (c) Plus older kernels, other distributions and Docker rootless: (b) with rows
  8, 8b, 9a, 9b, 9e and 10. Needs the VM harness to boot images other than
  Ubuntu's, which it cannot today (`RANIKUST_VM_RELEASE` takes `noble` or
  `jammy` only), plus rootlesskit and NFS set-ups.
- (d) Managed clusters and other CI services: need credentials and are not
  disposable (ADR-0063, ADR-0090); out for every option.

## Decision Outcome

The probe is tested in the rows of today's harnesses, the Phase 3 harness of
option (b) and row 9e (Alma or Rocky 9 with SELinux) taken from option (c) (the
maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as recommended); rows 8,
8b, 9a, 9b and 10 move to Phase 6, managed clusters are out, and Docker
variants run in the VM or on the runner instead of a new T2c profile, so
ADR-0063 stands.

- **Planned, Phase 3**, task 14 of #41 (the privilege matrix harness). Nothing
  is built by this ADR; the rows marked *new* below run in no tier today.
- **Required rows and their tiers.** "Basis" is how the expected outcome is
  known today. A row marked *today* is already asserted by the harness named.

| # | Environment | Expected outcome | Basis | Tier and harness |
|---|---|---|---|---|
| 1 | Host, root | overlay, root | measured | today: T2, VM and runner |
| 2 | Docker `--privileged`, state directory on a volume | overlay, root | measured | today: T2, VM and runner (`smoke`) |
| 2b | Docker `--privileged`, no volume | refused (`RefuseInContainer`) | built | today: T2, VM (`quick`, `rt_auto_privileged_container`) |
| 2c | Rootful Podman `--privileged` | overlay, root | inferred | new: T2, Podman in the VM (`tests/vm/vm.sh` packages) or on the runner |
| 3a | Docker `--cap-add SYS_ADMIN`, AppArmor `docker-default` kept | kaniko on `/` | first step measured (experiment 13), the user-namespace step inferred | new: T2, a Docker variant in the VM or on the runner |
| 3b | Docker `--cap-add SYS_ADMIN` and `apparmor=unconfined` | overlay, root (`MS_MOVE`) | measured | today: T2, VM and runner; T2c `sysadmin` |
| 4 | Docker, seccomp and AppArmor unconfined, no added capability | overlay, rootless | inferred | new: T2, a Docker variant in the VM or on the runner |
| 5 | Rootless Podman, default options, state directory on a volume | overlay, rootless, `/proc` rbind | steps measured (experiment 03) | new: T1 (`just test-rootless`), and a runner job in CI |
| 5b | Rootless Podman, default options, no volume | kaniko on `/` ([ADR-0107](0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)) | inferred | today: T1 kaniko leg |
| 6 | Ubuntu 24.04, regular user, no AppArmor profile | refused, with the AppArmor hint | measured | new: T2, a runner job (the `runner` user is this environment) and a VM leg |
| 6b | Ubuntu 24.04, regular user, `uidmap` and ranges, the sample profile for `/usr/local/bin/ranikust` | overlay, rootless, fresh `proc` | mounting and `chown` measured, the build inferred | new: T2, a runner job and a VM leg |
| 6c | Ubuntu 22.04 (also Debian 12, Fedora), regular user, `uidmap` and ranges | overlay, rootless, fresh `proc` | Ubuntu 22.04: namespace, map and rename measured, the overlay mount inferred; others inferred | new: T2, a VM leg on Ubuntu 22.04 |
| 6d | Ubuntu 22.04, regular user without `uidmap` (Ubuntu's default) or without ranges | one id maps: images with one owner build, several owners refused ([ADR-0098](0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)) | map measured, the refusal not built | new: T2, a VM leg on Ubuntu 22.04 |
| 7a | Kubernetes, default pod, AppArmor node | kaniko on `/` | inferred (containerd's profile denies `mount`) | new: T2, a `k8s` suite with k3s on the runner or in the VM |
| 7b | `hostUsers: false`, `procMount: Unmasked`, AppArmor unconfined | overlay, rootless, fresh `proc` | inferred | new: T2, the `k8s` suite; needs Kubernetes 1.33 or later, containerd 2.0, runc 1.2, kernel 6.3 (**[external]**, see Evidence) |
| 7c | `hostUsers: false`, `procMount: Unmasked`, default AppArmor | refused (ADR-0054, writable `/proc/sys`; [ADR-0107](0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)) | inferred | new: T2, the `k8s` suite |
| 7d | Pod Security Standard `restricted` | refused | inferred | new: T2, the `k8s` suite |
| 9d | RHEL 8 (kernel 4.18) | refused at start-up (ADR-0028) | built | today: T0, by fault injection |
| 9e | Alma or Rocky 9 (kernel 5.14, SELinux enforcing) | root: overlay, root; regular user with ranges: overlay, rootless | inferred (SELinux labels on the overlay unverified) | new: T2, a VM harness that boots a non-Ubuntu image (`dnf`, Podman) |

- **Rows not required.** Rows 8 (Docker rootless, default container) and 8b
  (Docker rootless, `--privileged`), 9a and 9b (kernel 5.10, Debian 11, as root
  and as a regular user) and 10 (state directory on NFS or FUSE) move to
  Phase 6. Row 9c (kernel 6.1) adds nothing over 5.15 and 6.8 and needs no
  harness. fuse-overlayfs is not a row: no code path uses it. Managed
  clusters (GKE Autopilot, EKS Bottlerocket, OpenShift) are not rows: their
  expected outcomes are the Kubernetes rows'.
- **Where the Docker variants run.** Rows 3a and 4 run in the VM or on the
  runner, not as a T2c profile, so ADR-0063 stays as it is.

### Consequences

- Good, because each outcome Phase 3 adds is tested where it will run: the
  runner is the very Ubuntu 24.04 regular user of rows 6 and 6b, and rootless
  Podman (row 5) is the commonest way to run ranikust without root in CI
  **[inferred]**.
- Good, because row 9e is the one RHEL-family target the docs name that
  ADR-0028 lets start (RHEL 8 is refused, row 9d), and SELinux is the one
  confinement the other rows never meet.
- Good, because rootless overlay runs in the VM on both kernels it boots (rows
  6b on 6.8 and 6c on 5.15), the AppArmor refusal on 6.8 (row 6) and the
  ADR-0098 refusal on 5.15 (row 6d).
- Bad, because it costs one runner job per environment, which is time on each CI
  run that touches the probe (by the lanes of `scripts/ci.py`), and a k3s
  install in the VM or on the runner.
- Bad, because the VM harness must learn to boot a non-Ubuntu image for row 9e.
- Bad, because rows 8, 8b, 9a, 9b and 10 stay inferred until Phase 6, and Debian
  11's kernel 5.10 stays untested.
- Neutral, because Debian 11's LTS ended in August 2026 **[unverified]**, and
  Docker rootless lands in rows already covered (kaniko on `/`, overlay as root
  in a namespace).

## Evidence

- [modes-and-detection](../design/modes-and-detection.md#where-environments-land),
  [experiment 03](../research/experiments/03-privilege-matrix.md) and
  [experiment 13](../research/experiments/13-docker-rootful-vm.md): the expected
  outcomes and which steps were measured (the Basis column).
- `poc-runner` in the VM, 2026-11-04, Ubuntu 22.04 and 24.04, **[measured]**
  (details in [ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md#evidence)):
  - M1: a `0555` directory renames inside a user namespace when its uid and gid
    are both mapped.
  - M2: outside the namespace the user cannot rename, `chmod` or unlink entries
    of subordinate ids.
  - M3: on Ubuntu 24.04 (`apparmor_restrict_unprivileged_userns=1`) a namespace
    made by an unconfined binary has no usable capabilities: with one uid the
    `uid_map` write fails with `EPERM`, with ranges there is no `chown` and no
    mount namespace; the sysctl at `0` or an AppArmor profile with `userns,`
    restores them (the profile was checked for mounting and `chown` only);
    Ubuntu 22.04 has no such restriction.
  - M4: Ubuntu does not install `uidmap` by default, so only one id maps.
- Row 7b's versions: pod user namespaces (`hostUsers: false`) are on by default
  from Kubernetes 1.33 and need containerd 2.0 or CRI-O 1.25, runc 1.2 or crun
  1.9, kernel 6.3, and `procMount: Unmasked` is usable only with them
  (**[external]**, high; [experiment 03](../research/experiments/03-privilege-matrix.md),
  [Kubernetes user namespaces](https://kubernetes.io/docs/concepts/workloads/pods/user-namespaces/)).
- `RANIKUST_VM_RELEASE` takes `noble` or `jammy` only, and the VM's packages are
  `docker.io zstd pigz busybox-static`, so no Podman **[code]**
  (`tests/vm/vm.sh`). The runner has Podman **[external]**.

## More Information

Answers #11 (Q-039), which blocked Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Related: ADR-0063 and
ADR-0090 (the harnesses, unchanged), ADR-0095 (the CI lanes the runner jobs
join), [ADR-0098](0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)
(row 6d), [ADR-0107](0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md)
(rows 5b and 7c).

Follow-ups, with task 14 of #41: one task issue per harness (runner jobs, VM
legs, the `k8s` suite, the non-Ubuntu VM image). When each lands, change
[modes-and-detection](../design/modes-and-detection.md#where-environments-land)
(a Basis column), [testing](../testing.md) (the privilege-matrix row and "Not
covered yet") and the book's [where to run it](../book/src/where-to-run.md)
(each row marked measured once its test runs), and check
`scripts/ci.py` for the lanes of each new path.
