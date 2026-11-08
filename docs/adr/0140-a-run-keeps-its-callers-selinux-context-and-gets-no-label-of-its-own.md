---
status: accepted
date: 2026-11-08
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0140: A `RUN` keeps its caller's SELinux context and gets no label of its own; labelling is revisited in Phase 6

## Context and Problem Statement

[#87](https://github.com/tran4774/ranikust/issues/87) measured ranikust on
AlmaLinux 9.8 with SELinux enforcing (policy `targeted`): root gets overlay
mode and a regular user with subordinate ranges gets rootless overlay mode, and
no denial was logged while either leg built. A `RUN` there keeps the SELinux
context of the process that started ranikust, `unconfined_t` for an
interactive user, because ranikust sets none. Container runtimes give each
container `container_t` with an MCS pair of its own, so on a host with SELinux
enforcing their containers are confined by policy and a `RUN` in ranikust is
not. The sandbox relies on namespaces, a seccomp profile and capabilities cut
to Docker's defaults, and applies no LSM label or profile (AppArmor included).
Should a `RUN` get an SELinux container label on such a host? Asked in
[#196](https://github.com/tran4774/ranikust/issues/196).

## Considered Options

- (a) Keep the caller's context and document it as a design property: the
  threat model and the book say a `RUN` gets no SELinux label of its own.
  No cost now, no extra confinement on SELinux hosts.
- (b) When SELinux is enforcing, give each `RUN` `container_t` with an MCS
  pair of its own, set through `setexeccon` before `execve`, with the rootfs
  labelled to match or mounted with `context=`. Stronger on SELinux hosts,
  but it depends on the host's policy shipping `container_t`
  (container-selinux), needs labelling or `context=` mounts per stage, and
  adds a failure mode where the policy is missing.
- (c) Opt-in labelling later, behind a flag (for example `--selinux-label`),
  designed in Phase 6. The default stays (a).

(a) was the recommendation, with a revisit as (b) or (c) in Phase 6 hardening
([#89](https://github.com/tran4774/ranikust/issues/89)), once the attack suite
across modes exists.

## Decision Outcome

A `RUN` keeps the SELinux context of the process that started ranikust and
gets no label of its own; the threat model and the book say so, and Phase 6
hardening ([#89](https://github.com/tran4774/ranikust/issues/89)) revisits
labelling as (b) or (c).

The maintainer's answer, 2026-11-08: "Ghi rõ, xét lại P6 (khuyến nghị)"
("State it plainly, revisit in Phase 6 (recommended)"), for option (a) as
the question's body gave it at the time.

- **Now.** ranikust sets no exec context, labels no root file and mounts
  nothing with `context=`. The `RUN` is in the caller's context in every mode;
  where ranikust itself runs in a container, that is the container's label,
  so the `RUN` is confined as the container is **[inferred]**.
- **Stated.** [Threat model](../security/threat-model.md#6-accepted-risks)
  section 6, [`RUN` sandbox](../design/sandbox.md#differences-from-docker),
  [modes and detection](../design/modes-and-detection.md#where-environments-land)
  and the book's [where to run it](../book/src/where-to-run.md).
- **Not decided.** Whether Phase 6 builds (b), (c) or neither, the flag's
  name, and anything about AppArmor, which is not applied either today.

### Consequences

- Good, because nothing changes for any build, and no failure mode appears
  where the host's policy lacks `container_t`: AlmaLinux 9.8 builds pass with
  no denial logged **[measured]**.
- Good, because the choice stays open: a later (b) or (c) adds a label and
  removes nothing a build relies on.
- Bad, because on a host with SELinux enforcing a `RUN` is held only by the
  sandbox (namespaces, seccomp profile, capabilities), not by policy as a
  container is: a kernel bug or a retained capability that crosses the
  sandbox is not stopped by SELinux the way it would be for `container_t`.
- Bad, because a build started from an unconfined login shell runs its `RUN`s
  as `unconfined_t`, and a stricter policy than `targeted` is measured nowhere.

## Evidence

- `just test-vm`'s `el9` suite ([#87](https://github.com/tran4774/ranikust/issues/87),
  [modes and detection](../design/modes-and-detection.md#where-environments-land)):
  AlmaLinux 9.8, kernel 5.14, SELinux enforcing, policy `targeted`, `auditd`
  running. Each leg's `RUN` prints `/proc/self/attr/current` and the suite
  fails unless it is the caller's `id -Z`; the audit log holds no denial from
  while the legs built; the control, a container reading a home directory
  bound without relabelling, shows the log would record one **[measured]**.
- ranikust's code calls no `setexeccon`, writes neither
  `/proc/self/attr/exec` nor `attr/current` and passes no `context=` mount
  option, and the sandbox applies no AppArmor profile **[code]**.
- Container runtimes give a container `container_t` with an MCS pair of its
  own on an SELinux host **[external]** (container-selinux).
- The costs of (b) are the question's analysis, not measured **[inferred]**.

## More Information

Answers [#196](https://github.com/tran4774/ranikust/issues/196). Builds on
[ADR-0003](0003-dockerfile-trust-and-default-isolation.md) (default isolation
about that of `docker build`) and on row 9e of
[ADR-0100](0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md)'s
privilege matrix, which #87 measured. Revisit in Phase 6 hardening (#89), once
the host-safety attack suite across modes exists; a decision for (b) or (c)
is a new ADR.
