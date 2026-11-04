---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0103: A default seccomp profile applies wherever the full sandbox runs, `--seccomp=unconfined` turns it off, and the flag is refused in kaniko mode on `/`

## Context and Problem Statement

[ADR-0003](0003-dockerfile-trust-and-default-isolation.md) gives `RUN` isolation
"roughly equivalent to `docker build`" by default and adds
`--seccomp=default|unconfined|<file>`, but no document says whether `default`
applies when the flag is not given: [sandbox](../design/sandbox.md#differences-from-docker)
and the [threat model](../security/threat-model.md) say only "until
`--seccomp`", and today the one filter a `RUN` gets is
[ADR-0033](0033-network-none-loopback-through-a-user-namespace.md)'s, under
`--network=none`. BuildKit applies its default profile to every `RUN` unless
the insecure entitlement is granted **[external, not yet confirmed with
`upstream-oracle`]**. In kaniko mode on `/` the `RUN` child lacks
`CAP_SYS_ADMIN`, so it can load a filter only after `no_new_privs`, which stops
setuid programs (`sudo`, `su`) in the `RUN` **[external]** (`seccomp(2)`). Is a
seccomp profile applied when `--seccomp` is not given, and what does the flag do
in kaniko mode on `/` (raised while planning Phase 3,
[#41](https://github.com/tran4774/ranikust/issues/41))?

## Considered Options

- (a) The default profile applies wherever the full sandbox runs;
  `--seccomp=unconfined` turns it off; in kaniko mode on `/` the flag is refused
  (exit 2), the container's own profile applying.
- (b) No filter unless `--seccomp` asks.
- (c) As (a), with `no_new_privs` in kaniko mode on `/`.

(b) leaves every `RUN` without a filter unless the user knows the flag, which is
today's state. (c) breaks setuid programs in a `RUN` silently, which
[ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md) refuses
for a flag that cannot work as asked.

## Decision Outcome

The default seccomp profile applies to every `RUN` wherever the full sandbox
runs, `--seccomp=unconfined` turns it off, and in kaniko mode on `/` the flag is
refused (exit 2) while the container's own profile applies: option (a) (the
maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as recommended).

- **Planned, Phase 3**, the `--seccomp` task
  ([#82](https://github.com/tran4774/ranikust/issues/82), task 11 of #41). Nothing is built
  by this ADR: a default `RUN` has no seccomp filter today.
- **Where the default applies:** every mode that gives `RUN` the full sandbox.
  `--seccomp=<file>` loads a profile file, as ADR-0003 says.
- **Kaniko mode on `/`** (`Isolation::Direct`): ranikust loads no filter and
  sets no `no_new_privs`; the container's own profile is what applies, and the
  flag is refused rather than accepted and ignored.
- **Reason:** [ADR-0032](0032-observable-behaviour-follows-buildkit.md) follows
  BuildKit for observable behaviour, and ADR-0049 puts security first.

### Consequences

- Good, because `RUN` isolation by default is then what ADR-0003 says, roughly
  `docker build`'s, and not only for users who pass the flag.
- Good, because a flag that cannot work as asked is refused, not ignored
  (ADR-0049), and no setuid program breaks silently.
- Bad, because a `RUN` that needs a call the default profile denies fails until
  the user passes `--seccomp=unconfined`, as under BuildKit's default
  **[external, unconfirmed]**.
- Bad, because kaniko mode on `/` keeps the isolation the container gives, with
  no filter of ranikust's own: ADR-0003 already names it as a case with no
  isolation.
- Neutral, because the profiles are generated per architecture, x86_64 and
  aarch64 ([ADR-0009](0009-x86-64-first-aarch64-ready.md)).

## Evidence

- Nothing is measured here: the profile and the flag do not exist.
- **[external]** BuildKit applies its default seccomp profile unless the insecure
  entitlement is granted; to confirm with `upstream-oracle` (ADR-0032).
- **[external]** `seccomp(2)`: loading a filter without `CAP_SYS_ADMIN` needs
  `no_new_privs`, which stops setuid programs from gaining privileges.
- **[docs]** [sandbox](../design/sandbox.md#differences-from-docker) and
  [threat model](../security/threat-model.md) section 6: a default `RUN` has no
  seccomp filter until `--seccomp`.

## More Information

Raised while planning Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Refines
[ADR-0003](0003-dockerfile-trust-and-default-isolation.md), which stays
`accepted` with its body unchanged.

Follow-ups, with [#82](https://github.com/tran4774/ranikust/issues/82): `rnk-sandbox` and `ranikust`, with T2 tests
(a `RUN` refused what the profile denies and allowed it under `unconfined`; a
profile file taking effect; a malformed one refused before the build), ADR-0033's
filter still applying on top, and the native aarch64 T0. When it is built,
change [sandbox](../design/sandbox.md), the threat model (T11 and section 6),
the `rnk-sandbox` and `ranikust` cards, the book's flag list and the
`## Changelog`, since a default changes.
