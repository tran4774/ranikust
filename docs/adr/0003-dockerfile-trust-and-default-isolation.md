---
status: accepted
date: 2026-10-29
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0003: Dockerfile trust is the user's call; default isolation like `docker build`

## Context and Problem Statement

Outside a container, ranikust runs `RUN` commands as real root on the host,
separated only by namespaces. A hostile Dockerfile could use a retained
capability or a kernel bug to reach the host. ranikust is an open-source tool
for end users, who decide what they build. How strong must the default
isolation be, and who decides whether a Dockerfile is trusted?

## Considered Options

- Assume trusted input and isolate minimally, as kaniko does.
- Assume hostile input and always enforce user namespaces, seccomp and cgroup
  limits. Breaks in many environments ranikust must support
  ([ADR-0004](0004-support-every-privilege-level.md)).
- Let the user decide; default to roughly `docker build` isolation and offer
  opt-in hardening.

## Decision Outcome

Trust is the user's call: by default `RUN` gets isolation roughly equivalent to
`docker build`, and users who build untrusted code can tighten it with
`--seccomp` and `--userns`.

- Default: mount, PID, UTS and IPC namespaces; `pivot_root` into the stage
  rootfs (`MS_MOVE` + `chroot` fallback); the capability bounding and
  inheritable sets reduced to Docker's 14 default capabilities for every
  `USER`; `PR_SET_PDEATHSIG`. A bare `chroot` is never used where mounting is
  possible. Details: [sandbox](../design/sandbox.md).
- `--seccomp=default|unconfined|<file>` loads a seccomp profile for `RUN`.
- `--userns` runs the build in a user namespace: uid 0 inside the build maps
  to an unprivileged uid on the host.
- The user documentation states the isolation level of each mode, including
  the cases with no isolation: kaniko mode inside an unprivileged container
  (no namespaces at all, like kaniko) and the `--parallel-stages` bare-chroot
  path ([ADR-0014](0014-kaniko-mode-sequential-by-default.md)).

### Consequences

- Good, because builds behave like `docker build` and do not fail in
  environments that cannot provide stronger isolation.
- Good, because users with untrusted input have explicit hardening switches.
- Bad, because the host-safety risk remains real for users who build
  untrusted code without hardening; documentation carries that burden.
- Bad, because seccomp profiles must be generated and maintained per CPU
  architecture ([ADR-0009](0009-x86-64-first-aarch64-ready.md)).

## Evidence

- [Experiment 02](../research/experiments/02-run-sandbox.md): root inside `RUN`
  escaped a bare `chroot` (it keeps `CAP_SYS_CHROOT`); `pivot_root` and the
  `MS_MOVE` fallback were not escapable this way. Without dropping the
  bounding set, `RUN` could `umount /etc/resolv.conf` and write the rootfs
  directly; with it, both were blocked.
- [Experiment 06a](../research/experiments/06a-root-rerun.md): as real root on
  the host, the bare-`chroot` escape reached the host's real `/`, even with
  capabilities reduced to Docker's set.
- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md): under a
  bare `chroot`, a malicious `RUN` escaped and read and wrote a sibling
  stage's rootfs.
- [Survey 04](../research/survey/04-risks-and-roadmap.md) risk R1;
  [survey 05](../research/survey/05-decisions.md) Q1.

## More Information

`--seccomp` and `--userns` are designed but were not run end to end in the
survey; they land in phase 3. With `--userns` as real root, every parent
directory of the workdir must be traversable by the mapped uid (experiment
06a). The threat model lives in `docs/security/`.
