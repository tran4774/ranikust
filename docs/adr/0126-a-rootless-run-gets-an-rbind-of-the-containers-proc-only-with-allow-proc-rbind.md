---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0126: A rootless `RUN` gets an rbind of the container's `/proc` only with `--allow proc.rbind`, and is refused without it

## Context and Problem Statement

In the build's user namespace a new `proc` cannot be mounted where the
container's `/proc` is masked, which is the case in default rootless Podman
(and in Docker with seccomp and AppArmor unconfined): the kernel answers
`EPERM`. The design so far fell back to an rbind of the container's `/proc` by
itself, so that rootless overlay mode would not be refused in nearly every
container. [ADR-0004](0004-support-every-privilege-level.md) says the probe
falls back "step by step" and leaves its "Order and fallbacks" to
[modes-and-detection](../design/modes-and-detection.md#probe-order---modeauto),
whose probe order listed that rbind; [experiment 03](../research/experiments/03-privilege-matrix.md)
found the fallback necessary; no ADR decides it, and ADR-0100 (row 5) and
ADR-0108 take it as given. With the container's `/proc`
bound in, a `RUN` shares the uid of the container's other processes and could
reach their per-process entries, so the rbind exposes the container's other
processes to the build's commands. The question came up while building
[#78](https://github.com/tran4774/ranikust/issues/78) (a `task`, not a `question`
issue): does the rbind stay automatic?

## Considered Options

The numbers are those of the question as put to the maintainer.

- (1) Opt in with a flag (recommended): by default refuse with clear guidance
  when no new `proc` can be mounted; a flag, like BuildKit's
  `--oci-worker-no-process-sandbox`, enables the rbind and states its risk. Kept
  until a `RUN` runs under a uid other than that of the container's processes
  ([#34](https://github.com/tran4774/ranikust/issues/34),
  [#83](https://github.com/tran4774/ranikust/issues/83)). Cost: default rootless
  Podman must add the flag or configure a new `proc`.
- (2) Rbind automatically and accept the risk: keep the automatic rbind, log
  once, record the exposure as an accepted residual.
- (3) Never rbind until #34: drop the `/proc` rbind fallback entirely until
  #34 and #83.

## Decision Outcome

A rootless `RUN` gets an rbind of the container's `/proc` only when the operator
passes `--allow proc.rbind`; without it, a build that cannot mount a new `proc`
is refused with guidance.

The maintainer's answer (2026-11-07: "Opt-in bằng cờ (khuyến nghị)", "Opt in with
a flag (recommended)") stood for option (1).

- **The entitlement.** `proc.rbind`, passed as `--allow proc.rbind` as buildctl's
  `--allow`, listed next to `network.host` in `crates/ranikust/src/cli.rs`. The
  flag is wired by [#79](https://github.com/tran4774/ranikust/issues/79); it is
  not built yet.
- **The library opt-in.** `Fallbacks::allow_proc_rbind()` in `rnk-sandbox`,
  built by #78. A `Fallbacks` value allows no `/proc` rbind unless it is called.
- **Without it.** The probe fails at the step that mounts a new `proc`
  (`Step::MountProc`), and the guidance names three ways on: a host that allows
  user namespaces, a container where ranikust may mount, or `--allow proc.rbind`
  with what it exposes.
- **With it.** The rbind of `/proc` is what the design described, chosen once and
  logged once per build; the flag states the exposure above. How the rbind is
  made is in [sandbox](../design/sandbox.md#per-run-sequence), step 3.2.
- **Until when.** The opt-in holds until a `RUN` runs under a uid other than that
  of the container's other processes (#34, #83). What the default is then is not
  decided here.
- **`/sys` is not covered.** The read-only rbind of `/sys` where a new `sysfs` is
  refused stays automatic.
- **Security.** No release carries the automatic rbind, which was designed and
  never shipped; this decision replaces it before one does.

### Consequences

- Good, because a `RUN` never sees the container's other processes unless the
  operator asked for it, and the refusal says how to get a new `proc` instead.
- Good, because the exposure is a named choice of the operator, as BuildKit's
  `--oci-worker-no-process-sandbox` makes it.
- Bad, because default rootless Podman (and Docker with seccomp and AppArmor
  unconfined) gets no rootless overlay build until the operator adds the flag or
  configures a new `proc`; the T1 smoke run, the book's rows and any CI recipe
  for that container must say so.
- Neutral, because the `/proc` rbind code stays; only its default changes.

## Evidence

- [Experiment 01](../research/experiments/01-overlay-semantics.md): in a default
  container overlay works but the new `proc` fails; an rbind of the container's
  `/proc` works, "but `RUN` then sees every process in the container: no PID
  isolation".
- [Experiment 03](../research/experiments/03-privilege-matrix.md): in a default
  `podman run`, `unshare -Urmpf --mount-proc` fails (`mount /proc failed:
  Operation not permitted`), also with `seccomp=unconfined` and with
  `--cap-add SYS_ADMIN`; it succeeds with `--security-opt unmask=ALL` or
  `systempaths=unconfined`, and `unshare -Urm` with `mount --rbind /proc` succeeds
  **[measured]**. The experiment calls the fallback required, as what BuildKit's
  `--oci-worker-no-process-sandbox` does **[external]**.
- **[inferred]** That a `RUN` could reach the per-process entries of the
  container's processes of its uid. The [sandbox](../design/sandbox.md#per-run-sequence)
  design and the [threat model](../security/threat-model.md) cite BuildKit's
  documentation of the same flag for the exposure; no test of ranikust measures
  it.

## More Information

Partly supersedes [ADR-0004](0004-support-every-privilege-level.md): only its
step-by-step fall back, for the `/proc` step (an rbind of `/proc` after a
refused new `proc`, which it left to the design doc); the rest of it stands.
Refines [ADR-0100](0100-probe-test-environments-are-todays-harnesses-plus-runner-vm-and-k3s-legs-and-alma-9.md):
the expected outcome of its row 5 (rootless Podman, default options) becomes
"overlay, rootless, `/proc` rbind" only with `--allow proc.rbind`, a refusal at
the new `proc` without it. [ADR-0108](0108-rootless-overlay-has-no-performance-target-and-its-time-is-recorded.md)
says rootless overlay differs from overlay as root by an rbind of `/proc` where a
new `proc` is refused; that stays true of a build run with the flag. None of the
three is rewritten.

Answers a question put to the maintainer while building #78, which has no
`question` issue; #78 is a `task`, closed by its own pull request, so this one
closes nothing. The design docs, the `rnk-sandbox` card, the threat model and
the smoke tests say "on request" already (#78). The `--allow proc.rbind` entry
of the command-line page and of the pull request's `## Changelog` come with #79,
which wires the flag.
