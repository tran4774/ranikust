---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0129: With `--allow proc.rbind`, a PID namespace shared with another mount namespace is refused as in kaniko mode on `/`, and the build uses an anonymous store of its own

## Context and Problem Statement

[ADR-0126](0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)
made the rbind of the container's `/proc` for a rootless `RUN` an opt-in
(`--allow proc.rbind`, wired in [#79](https://github.com/tran4774/ranikust/issues/79)).
[ADR-0127](0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md)
made the `RUN`s of such a build take turns and accepted the rest of the exposure
as a residual named in the threat model's
[section 6](../security/threat-model.md#6-accepted-risks): what a `RUN` reads of
ranikust and what it reaches of the container's other processes of its uid. A
second review round of the work on [#78](https://github.com/tran4774/ranikust/issues/78)
found two consequences that residual does not weigh. First, the rbind shows a
`RUN` every process of the PID namespace ranikust runs in; where the container
shares that namespace with the host or with other containers it reaches the
environment and files of those that run under the same kernel uid too. Second,
if another process of ranikust's uid runs in the container, a `RUN` can act through
it, which reaches the state directory. Two questions went to the maintainer in the
question window; neither has a `question` issue.

## Considered Options

The options are those of the questions as put to the maintainer, lettered here by
their order.

Question 1, a shared PID namespace. With the rbind a `RUN` sees every process of
the PID namespace ranikust runs in. Where the container shares it (Kubernetes
`hostPID` or `shareProcessNamespace`, Docker or podman `--pid=host` or
`--pid container:<name>`) it sees the processes of the host or of other
containers, and reaches the environment and files of those with the same kernel
uid: the user's own host session, or the host's root in an unconfined set-up.
Running ranikust as PID 1 does not help there. Kaniko mode on `/` already scans
for such peers and refuses the build
([ADR-0101](0101-a-build-runs-from-a-sealed-copy-of-the-binary.md), T21). Shipping
the flag in #79 with neither a refusal nor documentation would rate High.

- (a) Scan peers and refuse as T21 (recommended; "Quét peer và từ chối như T21
  (khuyến nghị)"): in #79 a build with the rbind runs the same peer scan as kaniko
  mode on `/` and refuses, with guidance, a PID namespace shared with a process of
  another mount namespace. Consistent with ADR-0101; no new flag. #78 only corrects
  the docs' scope.
- (b) Only state it in section 6 ("Chỉ ghi rõ trong §6"): accept it into
  ADR-0127's residual, with no refusal.

Question 2, the state directory. With the rbind, if another process of ranikust's
uid runs in the container (for example the CI shell that started it), a `RUN` can
go through that process's root and change the state directory. With a persistent
store on a volume, a hostile base image could poison the extracted layers that
later builds, of other trust domains too, reuse (T13; Medium, 6.3, inferred, not
measured). Kaniko mode on `/` never uses a persistent store for this reason
([ADR-0077](0077-kaniko-mode-on-root-never-uses-a-persistent-store.md)).

- (a) No persistent store with the rbind (recommended; "Không persistent store khi
  rbind (khuyến nghị)"): like ADR-0077, a build with the rbind always uses an
  anonymous store of its own and ignores the long-lived state directory, with a
  warning; done in #79. Cost: the flag loses the local layer cache between builds;
  Phase 4's registry cache, `--cache`, still works.
- (b) Accept it, record the mitigation in section 6 ("Chấp nhận, ghi cách giảm vào
  §6").

## Decision Outcome

A build whose probe chose the `/proc` rbind (`--allow proc.rbind`) runs the peer scan
of kaniko mode on `/` and refuses, with guidance, a PID namespace shared with a
process of another mount namespace, and always uses an anonymous store of its own,
ignoring the long-lived state directory with a warning; #79 builds both.

The maintainer's answers (2026-11-07): to question 1, "Quét peer và từ chối như T21
(khuyến nghị)", "Scan peers and refuse as T21 (recommended)", option (a); to
question 2, "Không persistent store khi rbind (khuyến nghị)", "No persistent store
with the rbind (recommended)", option (a).

- **The refusal.** It is ADR-0101's peer scan, run for a build whose probe chose the
  rbind as it runs for kaniko mode on `/`, and it refuses the same configurations
  with the same guidance: give the build container a PID namespace of its own,
  shared with no other container. It has no flag to turn it off. Like that scan, it
  counts the processes ranikust can open as it starts; the others, and processes
  that appear later, stay the residual ADR-0101 accepted.
- **The store.** A build with the rbind never opens a persistent store and leaves
  the long-lived state directory alone, as kaniko mode on `/` does under ADR-0077.
  It logs a warning that says so. A `RUN` cannot then plant anything that a later
  build, of this or another trust domain, finds under a chain id.
- **What stays of ADR-0127.** The lock (`RUN`s one at a time) and the rest of the
  residual: a `RUN` still reads ranikust's command line and mount table and reaches
  the environment and root of the container's own other processes of its uid. Its
  mitigation (run ranikust as the container's PID 1, pass secrets through its
  environment) still applies to those; it does not help in a shared PID namespace,
  which the build now refuses.
- **Where it is written.**
  - #78: corrects the scope its documentation gives the exposure.
  - [#79](https://github.com/tran4774/ranikust/issues/79): the scan for a build
    that chose the rbind, the anonymous store with its warning, the flag's help
    naming both, and the section 6 entry and the T12, T13 and T21 rows of the
    threat model as they then stand.
- **Not decided here.**
  - How the anonymous store of a rootless build is laid out, and what becomes of
    the long-lived state directory of
    [ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md),
    [ADR-0104](0104-userns-runs-the-whole-build-in-a-user-namespace-with-a-subordinate-range-for-root.md)
    and [ADR-0105](0105-a-minimal-prune-all-empties-a-rootless-state-directory-in-phase-3.md)
    for that build: #79's design, within this decision.
  - Whether `--cache` to an OCI layout (`oci:<dir>`), which kaniko mode on `/`
    refuses ([ADR-0116](0116-the-cache-lives-in-a-registry-or-an-oci-layout-of-one-layout-per-key.md),
    T25), is refused with the rbind too. The maintainer's answer names the registry
    cache only.
  - Whether the exposure closes, and the opt-in's default changes, once a `RUN`
    runs under a uid other than that of the container's processes
    ([#34](https://github.com/tran4774/ranikust/issues/34),
    [#83](https://github.com/tran4774/ranikust/issues/83)); ADR-0126 and ADR-0127
    left it open and this ADR does too.

### Consequences

- Good, because a build with the rbind no longer meets the processes of the host or
  of other containers that ranikust can open at start-up, and the check is the one
  kaniko mode on `/` already has, with no new flag.
- Good, because the extracted layers a hostile base image could poison are never
  the ones a later build reuses, whatever else runs in the container.
- Bad, because `--allow proc.rbind` is refused where the PID namespace is shared
  with a process of another mount namespace (Kubernetes `hostPID` or
  `shareProcessNamespace`, `--pid=host`, `--pid container:`), as kaniko mode on `/`
  is; such a build needs a PID namespace of its own.
- Bad, because a build with the flag keeps no local layer cache between builds:
  every build pulls and extracts its base layers again, so its time is longer than
  the same build with a persistent store, and
  [ADR-0108](0108-rootless-overlay-has-no-performance-target-and-its-time-is-recorded.md)'s
  recorded times for rootless overlay must say which of the two they measure. The
  registry cache (`--cache`) still works.
- Bad, because the scan counts only what ranikust can open as it starts: a process
  it cannot open, or one that appears later, stays an accepted residual.
- Neutral, because ADR-0127's lock and ADR-0126's opt-in stand, and a rootless
  build without the flag keeps its persistent store.

## Evidence

- ADR-0101's peer scan and its tests **[measured]**: T0 tests of the
  scan, T1 `pid-peers` under rootless podman and Docker in the VM's quick suite,
  listed in the T21 row of the [threat model](../security/threat-model.md#5-threats-and-mitigations).
  They measure kaniko mode on `/`; no build with the rbind runs the scan yet.
- ADR-0077's hardening review, 2026-11-02: in kaniko mode on `/` a `RUN` could
  plant a layer under a chain id of a persistent store, which a later build would
  run with its own secrets and push **[inferred]**. The T13 row of the threat model
  states the rule that no lower-trust principal writes a persistent store's state
  directory.
- **[inferred]** That the rbind of a rootless build shows a `RUN` the processes of
  a shared PID namespace and lets it reach those of its kernel uid; that running
  ranikust as PID 1 does not stop that there; and that a `RUN` can reach the state
  directory through another process of the uid (T13, Medium, 6.3): from the second
  review round of #78. No test measures any of them, as for ADR-0127's residual.
- **[code]** The refusal and the anonymous store are built in #79 and are not in
  the tree this ADR was written on.

## More Information

Refines [ADR-0127](0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md):
it narrows the residual that ADR accepted, refusing the shared PID namespace and
removing the persistent store from it, and rewrites none of it. Cites
[ADR-0126](0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)
(the opt-in), ADR-0101 (T21's peer scan), ADR-0077 (no persistent store, T13) and
T12, T13 and T21 of the threat model. The CLI part, the scan, the store and their
warnings, is #79.

Answers two questions put to the maintainer in the question window while the work on
#78 was reviewed; they have no `question` issue, and #78 and #79 are `task`s closed
by their own pull requests, so this one closes nothing. The follow-up docs are those
of #78 (the scope of its documentation of the exposure) and of #79 (the threat
model's section 6 entry and its T12, T13 and T21 rows, `docs/design/sandbox.md` and
[modes-and-detection](../design/modes-and-detection.md), the `--allow proc.rbind`
entry of the command-line page, the warnings of
`crates/ranikust/src/exposure.rs` and the pull request's `## Changelog` entry).
