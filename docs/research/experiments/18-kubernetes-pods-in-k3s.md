# Experiment 18: ranikust in Kubernetes pods, measured in k3s

| | |
|---|---|
| **Status** | Complete, 2026-11-08 |
| **Question** | Which mode does `--mode=auto` pick, or why does it refuse, in the Kubernetes rows of the privilege matrix, and does a write to `/proc/sys` fail in a `hostUsers: false` pod with an unmasked `/proc`? |
| **Answer** | A default pod builds in kaniko mode on `/`; `hostUsers: false` pods with `procMount: Unmasked` are refused, AppArmor confined or not; a restricted pod is refused at `unshare` with `EPERM`. In the unconfined one, a write of `kernel/core_pattern` fails with `EACCES`. |
| **Related** | [ADR-0100](../../adr/0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md) (rows 7a to 7d), [ADR-0107](../../adr/0107-rootless-podman-without-a-volume-builds-on-slash-and-unmasked-proc-pods-stay-refused.md) (case (ii)), [ADR-0054](../../adr/0054-refuse-kaniko-on-root-where-root-may-mount-and-no-effect-context-flags.md); [modes and detection](../../design/modes-and-detection.md#where-environments-land); [testing](../../testing.md#privilege-matrix-in-kubernetes) |
| **Original** | none: the T2 `k8s` suite, `tests/vm/guest/k8s.sh` |

## Summary

- The `k8s` suite runs each row as a pod of a one-node k3s cluster in the
  T2 VM and asserts the outcome (issue #86); this report records what it
  measured, including what it prints but does not assert.
- Rows 7a, 7c and 7d land where ADR-0100 expects them **[measured]**.
- Row 7b does not: ADR-0100 expected rootless overlay mode with a fresh
  `proc`, but ranikust's routing (`rootless_kept_off`, #194) keeps rootless
  overlay mode off wherever `/proc/sys` is writable, which applies ADR-0107's
  case (ii), written for the default AppArmor profile, to an unconfined pod
  too, and kaniko mode on `/` is refused there (ADR-0054) **[measured]**.
- A write of `kernel/core_pattern`'s own value fails with `EACCES` in the
  unconfined `procMount: Unmasked` pod (row 7b) **[measured]**: the first
  datum ADR-0107 waits for before deciding its option (d). In the confined
  one (row 7c) the AppArmor profile alone accounts for the same `EACCES`.

## Questions

1. What does `--mode=auto` do in each of rows 7a to 7d, with which step and
   errno?
2. In a `hostUsers: false` pod with `procMount: Unmasked`, does a write to
   `/proc/sys` fail (ADR-0107, case (ii))?
3. In a `hostUsers: false` pod, where does the user namespace probe stop,
   with Ubuntu's `kernel.apparmor_restrict_unprivileged_userns` at 1 and at
   0?

## Environment

- The T2 VM (`just test-vm k8s`): Ubuntu 24.04, kernel 6.8.0-147-generic,
  12 vCPUs, 16 GiB, cgroup v2, AppArmor on,
  `kernel.apparmor_restrict_unprivileged_userns=1`.
- k3s `v1.37.2+k3s1` (Kubernetes 1.37.2, containerd 2.3.7, runc 1.4.2), one
  node, the pause image `registry.k8s.io/pause:3.10.1` by digest; the
  kubelet sets no default seccomp profile.
- ranikust from `main` at 3464a501 (#194), static musl, release; the suite
  of branch `test/privilege-matrix-k3s` (#86): run 1 at 63e66a82, run 2
  with 43bd452d's fix of the pods' image, run 3 at c23daa83.

## Method

Each pod runs the `ranikust` binary, as root unless the row says otherwise,
on a context of `FROM scratch`, `COPY busybox`, `RUN busybox true`, with
`--mode=auto --no-push`, after `tests/vm/guest/k8s/pod.sh` prints the pod's
uid, AppArmor label, `uid_map`, seccomp mode, `no_new_privs`, effective
capabilities and the mount of `/proc/sys`. The `hostUsers: false` pods get an
`emptyDir` at the state directory, made `0700`, and the restricted pod a
state directory on `/dev/shm`. In the root pods `pod.sh` first writes
`kernel/core_pattern`'s own value back and prints the result. Three runs of
the suite, one pod per row each: the second and third agree on every row; in
the first, rows 7a and the masked ones failed on a harness bug (the pods'
image left `/var/lib` writable by its group, which ranikust refuses) and
rows 7b to 7d agree.

## Results

| Row | Pod | Outcome | Basis |
|---|---|---|---|
| 7a | default: the host's users, `cri-containerd.apparmor.d`, masked `/proc`, `/proc/sys` read-only, no seccomp filter | kaniko mode on `/`, built; root may not make a private mount namespace; the probe, reported only, stops at `proc` (`make / a recursive slave mount: EACCES`) after `unshare(CLONE_NEWUSER)`, the id map and `chown` pass | **[measured]** |
| 7b | `hostUsers: false`, `procMount: Unmasked`, AppArmor unconfined | refused: "rootless overlay mode is not tried as root where /proc/sys is writable (ADR-0107)", then kaniko mode on `/` refused for the writable `/proc/sys`; `core_pattern` write: `EACCES` | **[measured]** |
| 7c | the same, `cri-containerd.apparmor.d` | refused the same way; `core_pattern` write: `EACCES` | **[measured]** |
| 7d | `restricted` Pod Security Standard: uid 1000, no capability, `RuntimeDefault` seccomp, `no_new_privs` | refused: the probe stops at `unshare` (`unshare(CLONE_NEWUSER): EPERM`) | **[measured]** |
| — | `hostUsers: false`, AppArmor unconfined, masked `/proc`, switch at 1 | the probe stops at `chown` (`EPERM`), `unshare(CLONE_NEWNS)` `EPERM`; kaniko mode on `/`, built | **[measured]** |
| — | the same, switch at 0 | the probe passes all but `proc` (`mount proc: EPERM`) and `sysfs`; overlay passes; refused without `--allow proc.rbind`, so kaniko mode on `/`, built | **[measured]** |

A root pod in the `restricted` namespace is refused at admission
(`violates PodSecurity "restricted:latest"`) **[measured]**.

## Findings

1. Rows 7a, 7c and 7d match ADR-0100 (Results, rows 7a, 7c, 7d).
2. Row 7b is refused, not rootless overlay mode: the routing keeps rootless
   off for the writable `/proc/sys` whatever the AppArmor profile (row 7b).
3. Root in a `hostUsers: false` pod cannot write `kernel/core_pattern` even
   with `/proc/sys` mounted read-write and AppArmor unconfined (row 7b). In
   row 7c containerd's profile, which like Docker's denies writes below
   `/proc/sys/kernel` but for `shm*`, explains the `EACCES` without the
   kernel's check **[inferred]**.
4. On an Ubuntu 24.04 node, an unconfined pod's own user namespaces have no
   capabilities while the switch is 1 (the masked row at 1); at 0 only the
   masked `/proc` stops the probe (the masked row at 0).

## Implications for ranikust

- The suite asserts today's routing for row 7b; whether ADR-0107 takes its
  option (d), and whether root in such a pod then tries rootless overlay
  mode, needs a decision (an ADR).
- Row 7b as ADR-0100 expected it would need that decision and a node where
  the pod's own user namespaces get capabilities (the switch at 0, or a
  distribution without the restriction) **[inferred]**.

## Limitations and open questions

- Only `kernel/core_pattern` was written; other kernel-wide sysctls
  (`kernel/modprobe`, `kernel/hotplug`, `vm/*`, `fs/*`) and the namespaced
  ones were not swept.
- One node, one distribution, one kernel; the GitHub-hosted runner's job of
  the same suite has not run yet. Managed clusters (GKE Autopilot, EKS on
  Bottlerocket, OpenShift), SELinux nodes and a kubelet with
  `seccompDefault` were not run.
- The 7d refusal's hint names AppArmor's switch, while there the seccomp
  profile blocks `unshare` (a defect of the hint, to be tracked as an issue).

## Reproduction

`sg kvm -c 'just test-vm k8s'`; the results are in
`target/vm/results/noble/k8s.log` and `k8s/`. k3s and its pin are described
in [testing](../../testing.md#privilege-matrix-in-kubernetes). The VM is
deleted after the run.

## References

1. k3s release `v1.37.2+k3s1`, `https://github.com/k3s-io/k3s/releases`,
   read 2026-11-08.
