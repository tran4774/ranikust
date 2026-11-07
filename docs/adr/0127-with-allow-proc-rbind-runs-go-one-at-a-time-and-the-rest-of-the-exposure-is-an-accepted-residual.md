---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0127: With `--allow proc.rbind`, `RUN`s go one at a time, and the rest of the exposure is an accepted residual named in the threat model

## Context and Problem Statement

[ADR-0126](0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)
made the rbind of the container's `/proc` for a rootless `RUN` an opt-in
(`--allow proc.rbind`, library `Fallbacks::allow_proc_rbind()`) because it lets a
`RUN` reach the per-process entries of the container's processes of its uid.
The `host-safety` review of the work on
[#78](https://github.com/tran4774/ranikust/issues/78) then found what the opt-in
still exposes, in two parts. First, overlay mode builds stages in parallel and the
`RUN`s of those stages run as root in the build's user namespace, so they share
one kernel uid: through the rbind a `RUN` of one stage (from a hostile base
image, for example) could reach the secrets, the `env=` values and the root of a
`RUN` of another stage. This is the exposure of
[T12](../security/threat-model.md#5-threats-and-mitigations) that
[ADR-0060](0060-copy-leaves-special-files-out-where-run-has-no-nodev.md) removes
in kaniko mode by refusing `--parallel-stages`. Second, whatever the stages do,
the rbind lets a `RUN` read what the container's `/proc` shows of ranikust and
of the container's other processes of its uid. Two questions went to the
maintainer in the question window; neither has a `question` issue.

## Considered Options

The options are those of the questions as put to the maintainer, lettered here by
their order.

Question 1, `RUN`s of parallel stages sharing a kernel uid:

- (a) One `RUN` at a time while the rbind is in use (recommended): while the probe
  chose the `/proc` rbind, `Sandbox::run` holds a lock, so `RUN`s run one at a
  time. A finished `RUN` leaves no process behind, so no `RUN` can reach another's
  `/proc` entries. Stages still schedule in parallel. Cost: a build with the flag
  loses `RUN` parallelism. Built in #78.
- (b) Keep them parallel and write the risk down: name it in the flag's help, in
  the warning and in the threat model's section 6, and open a draft advisory.
- (c) Refuse parallel stages with the flag, as ADR-0060 does in kaniko mode, in
  [#79](https://github.com/tran4774/ranikust/issues/79).

Question 2, what the rbind still exposes to a `RUN`:

- (a) Accept the residual and record it in section 6 (recommended): #78 updates
  the opt-in's documentation, its warning and the sandbox design doc, and adds a
  section 6 entry in general words with the mitigation; #79 extends the warnings
  of `crates/ranikust/src/exposure.rs` to any build whose probe chose the rbind
  and names the exposure in the flag's help; a draft advisory is opened now and
  closed by the maintainer once the section 6 entry is on `main`.
- (b) A draft advisory now, and everything else moved to #79.

## Decision Outcome

While the probe has chosen the `/proc` rbind, `Sandbox::run` holds a lock so that
the `RUN`s of a build run one at a time, and the rest of what the rbind exposes is
an accepted residual of the opt-in, named in the threat model's section 6 with its
mitigation.

The maintainer's answers (2026-11-07): to question 1, "Mỗi lần một RUN khi có
rbind (khuyến nghị)", "One `RUN` at a time while the rbind is in use
(recommended)", option (a); to question 2, "Residual chấp nhận, ghi §6 (khuyến
nghị)", "Residual accepted, recorded in section 6 (recommended)", option (a).

- **The lock.** It is held for as long as a `RUN` runs, only in a build whose
  probe chose the `/proc` rbind; a build that mounted a new `proc` runs its `RUN`s
  in parallel as before. The scheduler still runs stages in parallel; only the
  `RUN`s inside them take turns. A `RUN` finished leaves no process (ADR-0051's
  rule, no surviving process), so a later `RUN` finds nothing of an earlier one.
  This closes `RUN`-to-`RUN` access, not the access to the container's other
  processes. Built in #78.
- **The residual.** With the rbind a `RUN` can read ranikust's command line
  (build-argument values, the paths of secret files) and mount table, and reach
  the per-process entries, the environment and the root, of the container's other
  processes of its uid (for example the shell that started ranikust, whose
  environment may hold CI credentials and which knows the state directory and
  `config.json`). ranikust itself stays non-dumpable ([ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)),
  so its descriptors, environment and memory stay closed. The exposure is
  inherent to the opt-in; its severity is Medium.
- **The mitigation.** Run ranikust as the container's PID 1 and pass secrets
  through its environment, as ADR-0051 already advises for kaniko mode on `/`.
- **Where it is written.**
  - #78: the opt-in's documentation, the warning when the rbind is chosen,
    `docs/design/sandbox.md`, and the section 6 entry of
    `docs/security/threat-model.md`, in general words and with the mitigation.
  - [#79](https://github.com/tran4774/ranikust/issues/79): the warnings of
    `crates/ranikust/src/exposure.rs` (a build argument on the command line, a
    `--secret src=` file, the environment of the CI shell), which warn in kaniko
    mode on `/` today, apply to any build whose probe chose the rbind; the help of
    `--allow proc.rbind` names the exposure.
  - A draft security advisory, as [ADR-0111](0111-a-low-or-medium-security-finding-is-a-draft-advisory-that-does-not-block-a-release.md)
    provides for a Medium finding, until the section 6 entry is on `main`; the
    maintainer closes it then.
- **Not decided here.** Whether the exposure closes, and the default of the
  opt-in changes, once a `RUN` runs under a uid other than that of the container's
  processes ([#34](https://github.com/tran4774/ranikust/issues/34),
  [#83](https://github.com/tran4774/ranikust/issues/83)); ADR-0126 left it open
  and this ADR does too.

### Consequences

- Good, because no `RUN` of one stage can reach a `RUN` of another through the
  rbind, with no refusal of parallel stages: stages still schedule in parallel and
  a build that has a new `proc` is unchanged.
- Good, because the remaining exposure is stated once, with its mitigation, in the
  place the threat model keeps accepted risks, and the operator who passes the flag
  is told what it costs.
- Bad, because a build run with `--allow proc.rbind` loses parallelism between
  `RUN`s; its wall time is longer than the same build with a new `proc`, and
  [ADR-0108](0108-rootless-overlay-has-no-performance-target-and-its-time-is-recorded.md)'s
  recorded times for rootless overlay must say which of the two they measure.
- Bad, because a `RUN` of a build with the flag still reads ranikust's command
  line and mount table and reaches the environment and root of the container's
  other processes of its uid; the mitigation is the operator's to apply and
  ranikust can only warn.
- Neutral, because T12 and ADR-0060 stand: kaniko mode on `/` still refuses
  `--parallel-stages`, and the lock concerns only the rbind.

## Evidence

- [Experiment 12](../research/experiments/12-kaniko-mode-parallel.md) and the
  [threat model](../security/threat-model.md) (T12): a malicious `RUN` escaping a
  bare `chroot` reaches a sibling stage **[measured]**; ADR-0060 refuses
  `--parallel-stages` in kaniko mode for that reason. The route here is another
  one (the shared uid and `/proc`) to the same exposure, one stage reading
  another's root and secrets.
- ADR-0051's proof of concept and VM suite, 2026-10-31 **[measured]**: with a
  non-dumpable ranikust a root `RUN` read neither its descriptors, environment
  nor memory; `/proc/<pid>/cmdline` stayed readable; the environment of a parent
  shell leaked a credential unless ranikust was the container's PID 1. That was
  measured for kaniko mode on `/`, where the `RUN` shares the container's `/proc`.
- **[inferred]** That the same entries are open to a `RUN` through the rbind of a
  rootless build, and that two `RUN`s in the build's user namespace share a kernel
  uid: from the `host-safety` review of #78 and from the namespace's id map. No
  test measures either yet; #78's tests cover the lock.
- **[code]** The lock is built in #78 and is not yet in the tree this ADR was
  written on.

## More Information

Refines [ADR-0126](0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md):
what the opt-in costs and exposes, which it left to the flag's statement of its
risk. It rewrites none of it. Cites ADR-0060 (T12, `--parallel-stages` refused in
kaniko mode), ADR-0051 (non-dumpable ranikust, no surviving process, run as PID 1)
and ADR-0111 (a Medium finding is a draft advisory). The CLI part, the flag and the
extended warnings, is #79.

Answers two questions put to the maintainer while building #78, which has no
`question` issue; #78 is a `task`, closed by its own pull request, so this one
closes nothing. The follow-up docs are those of #78 (the opt-in's documentation,
`docs/design/sandbox.md`, the `rnk-sandbox` card, the threat model's section 6) and
of #79 (the command-line page, the warnings and the pull request's `## Changelog`
entry).
