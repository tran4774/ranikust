---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0054: Kaniko mode on `/` is refused where root may mount or `/proc/sys` is writable; `dir://` and a local `--context-sub-path` are refused

## Context and Problem Statement

Two questions were left for the maintainer after the host-safety review of
[ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)'s
branch and the decision of
[ADR-0053](0053-local-context-is-a-plain-path-no-dir-scheme.md).

**Q-341.** In a `--privileged` container (or one with `CAP_SYS_ADMIN`),
`--mode=kaniko` built on `/` with `Isolation::Direct`, which mounts
nothing, so a `RUN` as uid 0 saw the host's devices (`/dev/sda`) and a
writable `/proc/sys` (`kernel/core_pattern` runs a binary as host root).
Overlay mode, which that container can run, gives `RUN` the full sandbox.
The same hole opens, without any capability, where `/proc/sys` is not
read-only (Docker's `systempaths=unconfined`, Kubernetes' `procMount:
Unmasked`).

**Q-081.** ADR-0053 dropped kaniko's `dir://` context syntax and made
`--context-sub-path` apply only to fetched contexts, and left two points
open: how `--context=dir://...` is treated (refused, with a message that
names the plain path) and what a `--context-sub-path` given with a local
context does (ignored with a warning, as the code did, or refused, as
[ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md)
requires of a flag that changes nothing).

## Considered Options

Q-341:

- (a) Refuse kaniko mode on `/` where root may mount, and point to overlay
  mode.
- (b) In that case, enter a private mount namespace and apply Docker's
  read-only and masked paths (`/proc/sys`, `sysrq-trigger`, `kcore`...) and
  a `nodev` remount before the first `RUN`.

Q-081 (the main session's proposal, answered "A"):

- (A) `--context=dir://...` is refused with a message to use a plain path,
  and `--context-sub-path` with a local context is refused (exit 2) with a
  message to pass the sub-directory itself as `--context`.
- Keep ignoring `--context-sub-path` for a local context, with a warning
  (the code's behaviour until now): not taken, ADR-0049 refuses a flag with
  no effect instead of ignoring it.

## Decision Outcome

Chosen: Q-341 option (a) and the Q-081 proposal (A). The maintainer, on
2026-10-31: "Q341 chọn A" and "Q81 A".

- **Q-341.** Kaniko mode on `/` is refused where root may mount, and
  also where `/proc/sys` is not a read-only mount (`statvfs` does not
  report it read-only). The message points to overlay mode
  (`--mode=overlay` or `--mode=auto`, with a volume at the state
  directory). `--mode=auto` already picks overlay mode there. Host devices
  given to the container with `--device` stay readable and writable by a
  `RUN`: ranikust cannot tell them from the container's own, so they stay
  the operator's responsibility and the documentation says so. Option (b)
  is not built.
- **Q-081, point 1.** `--context=dir://<path>` is refused (exit 2), with a
  message that says to pass the plain path.
- **Q-081, point 2.** `--context-sub-path` with a local context is refused
  (exit 2), with a message that says to pass the sub-directory itself as
  `--context`. The flag then has a context it applies to only with fetched
  contexts (Phase 5).
- The Q-341 refusal is in the code already (`crates/ranikust/src/main.rs`,
  `exposure::proc_sys_read_only`). The Q-081 code change (`config.rs`,
  `cli.rs`, the `main.rs` warning, `rnk-context` `parse_spec` and the
  `dir://` tests, `tests/vm/guest/e2e.sh`) is not part of this ADR's
  commit; it lands with the Phase 2 task 7 integration branch
  ([phase-2.md](../agents/phase-2.md)), as ADR-0053 said. Until then the
  code still accepts `dir://` and warns on a plain path with a
  sub-path.

### Consequences

- Good, because a privileged container never runs a `RUN` on `/` with the
  host's `kernel/core_pattern` writable; the better build (overlay mode,
  full sandbox) is the one it gets.
- Good, because no second sandbox (private mount namespace, masked paths,
  `nodev` remount) has to be built and kept right for a case that overlay
  mode already serves.
- Good, because a flag that changes nothing no longer fails silently: a
  pipeline that passes `--context-sub-path` with a local directory learns
  at once, and how to fix it.
- Bad, because a pipeline that runs kaniko mode in a privileged container,
  or with an unmasked `/proc/sys`, stops until it switches to overlay mode
  (with a volume at the state directory) or removes the setting.
- Bad, because a kaniko pipeline that passes `--context=dir://...` or
  `--context-sub-path` with a local directory needs both changed (ADR-0049:
  compatibility with kaniko is not a goal).
- Neutral, because host devices given with `--device` remain outside what
  ranikust can see.

## Evidence

- `docs/open-questions.md` Q-341 (the options, the recommendation (a), the
  `statvfs` check and the `--device` limit) and its host-safety review
  origin [code].
- `crates/ranikust/src/main.rs` (the refusal where root may mount and the
  `exposure::proc_sys_read_only` check) and
  `crates/ranikust/src/exposure.rs` [code]: the behaviour this ADR makes
  the decision.
- ADR-0053 More Information, "Not decided here": the sub-path question;
  ADR-0049: a flag that changes nothing is refused [code].
- BuildKit, `frontend/dockerui/context.go:169-173`, `:348-353`
  [external]: no `dir://` form, and the sub-path applies to fetched
  contexts only.
- No experiment: both questions are of interface and policy, not of
  measured cost.

## More Information

Completes the answer to Q-081 recorded under
[ADR-0032](0032-observable-behaviour-follows-buildkit.md) and
[ADR-0053](0053-local-context-is-a-plain-path-no-dir-scheme.md), which
stand; where ADR-0053's "Not decided here" paragraph and its note that the
sub-path flag is ignored say otherwise, this ADR decides: refused. Settles
the Q-341 refusal that [ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)
recorded as pending the maintainer. Priority rule: ADR-0049.

Threat model: T17 ([threat-model.md](../security/threat-model.md)).
