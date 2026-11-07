---
status: accepted
date: 2026-11-07
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0130: With `--allow proc.rbind`, the residual is the measured one, the peer scan stays, and the build uses the persistent store

## Context and Problem Statement

[ADR-0126](0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)
made the rbind of the container's `/proc` an opt-in for a rootless `RUN`
(`--allow proc.rbind`, wired in [#79](https://github.com/tran4774/ranikust/issues/79)).
[ADR-0127](0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md)
made the `RUN`s of such a build take turns and accepted the rest of the exposure
as a residual, written as "a `RUN` reaches the environment and files of processes
of its kernel uid".
[ADR-0129](0129-with-allow-proc-rbind-a-shared-pid-namespace-is-refused-and-the-store-is-anonymous.md)
added two measures, both to be built in #79: a peer scan that refuses a PID
namespace shared with a process of another mount namespace, as T21 does
([ADR-0101](0101-a-build-runs-from-a-sealed-copy-of-the-binary.md)), and an
anonymous store in place of the persistent one, as
[ADR-0077](0077-kaniko-mode-on-root-never-uses-a-persistent-store.md) does on
`/`. The second rested on a belief ADR-0129 marked inferred: that a `RUN` could
reach the state directory through `/proc/<pid>/root` or `fd` of a process of its
kernel uid, such as the CI shell that started the container. A VM experiment of
2026-11-07 measured that route, and the kernel closes it (see Evidence). The
question put to the maintainer in the question window, which has no `question`
issue: given the measurement, which of ADR-0129's two measures stay.

## Considered Options

The options are those of the question as put to the maintainer, lettered here by
their order.

- (a) Keep both measures: defence in depth against an unreproduced lead about
  exiting threads, tracked privately, at the cost of no persistent local store for
  every build with the rbind. The registry cache (`--cache`) still works.
- (b) Keep the peer scan, drop the anonymous store (recommended; "Giữ peer scan,
  bỏ anonymous store (khuyến nghị)"): the scan still stops a `RUN` reading the
  `cmdline` and `status` of host processes in a shared PID namespace, and is cheap.
  A build with the rbind uses the persistent store, which the common case
  (rootless in default Docker or Podman) needs for caching.
- (c) Drop both: a shared PID namespace then exposes the `cmdline` and `status` of
  the host's processes to a `RUN`.

## Decision Outcome

A build with `--allow proc.rbind` keeps the peer scan of ADR-0129 and drops its
anonymous store: it uses the persistent store, as a rootless build without the flag
does.

The maintainer's answer (2026-11-07): "Giữ peer scan, bỏ anonymous store (khuyến
nghị)", "Keep the peer scan, drop the anonymous store (recommended)", option (b).

- **The peer scan stays.** It is as ADR-0129 decided it: ADR-0101's scan, run for
  a build whose probe chose the rbind, refusing with guidance a PID namespace
  shared with a process of another mount namespace, with no flag to turn it off.
  It still counts only the processes ranikust can open as it starts.
- **The store.** ADR-0129's anonymous store is dropped, with the warning that the
  state directory is ignored. A build with the rbind opens the store a rootless
  build without the flag opens, and #79 builds the scan only. The point ADR-0129
  left to #79, how the anonymous store of a rootless build is laid out, falls with
  it.
- **The residual (refines ADR-0127).** Under the rbind a `RUN` reaches, with
  ptrace-gated access, only the processes inside the build's own user namespace:
  other `RUN`s, which ADR-0127's turns exclude, and ranikust's own processes, which
  are non-dumpable ([ADR-0051](0051-kaniko-mode-on-root-keeps-state-unreachable-by-run.md)).
  Of every other process of its PID namespace it sees the PIDs and reads `cmdline`,
  `status`, `mountinfo` and `net/*`, and lists the numbers of its descriptors; it
  does not read its environment, root, current directory, descriptors, memory,
  maps, executable or namespace links. ADR-0127's residual wording, that a `RUN`
  reaches the environment and root of the container's other processes of its uid,
  no longer holds. ranikust's own command line (build-argument values, the paths of
  secret files) and mount table stay readable, as ADR-0127 says.
- **The mitigation.** No secrets in other processes' command lines, ranikust's
  included; give the build container a PID namespace of its own. ADR-0127's advice
  to run ranikust as the container's PID 1 and pass secrets through its environment
  answered the reach into other processes' environments and is replaced by these
  two; ADR-0051's advice for kaniko mode on `/` stands, since a `RUN` there shares
  the container's `/proc` with no user namespace.
- **What stays.** ADR-0126's opt-in and ADR-0127's lock.
- **Where it is written.**
  - [#79](https://github.com/tran4774/ranikust/issues/79): the scan, and the
    flag's help and the exposure warnings worded with the measured residual, with
    no anonymous store.
  - The docs that say the rbind brings an anonymous store or that a `RUN` reaches
    other processes of its uid: the threat model's section 6 entry and its T12, T13
    and T21 rows, `docs/design/sandbox.md`,
    [modes-and-detection](../design/modes-and-detection.md), the `rnk-sandbox`
    card and the doc comment of `Fallbacks::allow_proc_rbind`.
- **Not decided here.**
  - Whether `--cache` to an OCI layout (`oci:<dir>`), which kaniko mode on `/`
    refuses ([ADR-0116](0116-the-cache-lives-in-a-registry-or-an-oci-layout-of-one-layout-per-key.md),
    T25), is refused with the rbind too: it stays with #79, as ADR-0129 left it.
  - The severity the threat model gives the section 6 entry once its text is the
    measured residual: ADR-0127 rated it Medium.
  - Whether the exposure closes, and the opt-in's default changes, once a `RUN` runs
    under a uid other than that of the container's processes
    ([#34](https://github.com/tran4774/ranikust/issues/34),
    [#83](https://github.com/tran4774/ranikust/issues/83)); ADR-0126, ADR-0127 and
    ADR-0129 left it open and this ADR does too.

### Consequences

- Good, because a build with the rbind keeps its local layer cache between builds:
  the common case (rootless in default Docker or Podman) keeps its pulled and
  extracted base layers for the next build. ADR-0129's note that ADR-0108's recorded times must say which
  store they measure no longer applies; ADR-0127's note about the lock stands.
- Good, because a shared PID namespace is still refused, which is where a `RUN`
  would read the `cmdline` and `status` of the host's processes, at the cost of a
  scan kaniko mode on `/` already has.
- Good, because the residual is stated as measured and is narrower than the
  inferred one: no environment, root or descriptor of another process is open to a
  `RUN`.
- Bad, because the persistent store's safety under the rbind rests on a kernel rule
  measured on two kernels outside ranikust, not on a test of ranikust's own path;
  kernels newer than 6.8 and SELinux are not measured.
- Bad, because a `RUN` still reads the `cmdline`, `status`, `mountinfo` and `net/*`
  of every process of its PID namespace, and the numbers of their descriptors.
- Bad, because `--allow proc.rbind` is still refused in a PID namespace shared with
  a process of another mount namespace, as ADR-0129 accepted.
- Neutral, because the registry cache (`--cache`) works with or without the store,
  and the lock still costs a build with the flag its parallelism between `RUN`s.

## Evidence

- A VM experiment of 2026-11-07 **[measured]**, on Ubuntu 22.04 (kernel
  5.15.0-203) and Ubuntu 24.04 (kernel 6.8.0-147), Yama `ptrace_scope` 1 and 0,
  AppArmor, with kprobes on both kernels. A process in a child user namespace is
  refused (`EACCES`) every ptrace-gated `/proc/<pid>` file of a target in the
  parent user namespace, even with the same kernel uid and a dumpable target:
  `environ`, `root` and `cwd` (readlink and open through them), `fd/N`, `mem`,
  `maps`, `exe` and `ns/*`. The refusal comes from `cap_ptrace_access_check`,
  which needs `CAP_SYS_PTRACE` in the target's user namespace when the namespaces
  differ.
- The same experiment: the refusal holds for a nested container namespace (the
  build's namespace is a child of the container's) and for a subordinate kernel uid
  (a `RUN` with a non-root `USER` against a `podman exec -u` process of that
  uid) **[measured]**. Still readable, as they are not ptrace-gated: `cmdline`,
  `status`, `mountinfo`, `net/*` and the list of descriptor numbers. Controls
  passed: the same namespace, a parent reading its child, and root in the target's
  namespace each read every file.
- **Not measured:** the real ranikust path end to end (rootless podman, ranikust
  and the rbind), kernels newer than 6.8, SELinux. The residual above is
  **[inferred]** from the measured kernel behaviour for that path.
- An unreproduced lead about exiting threads is tracked privately; it is not
  described here.
- ADR-0129's claim that a `RUN` reaches the state directory through another
  process of its uid (T13, Medium, 6.3) was **[inferred]**; the experiment
  contradicts it for `/proc/<pid>/root` and `fd`. ADR-0051's measurement of a
  non-dumpable ranikust (kaniko mode on `/`) and ADR-0101's peer scan tests, T0
  and T1 `pid-peers`, are unchanged **[measured]**.
- No report file holds the experiment; this section is its record.

## More Information

Refines [ADR-0127](0127-with-allow-proc-rbind-runs-go-one-at-a-time-and-the-rest-of-the-exposure-is-an-accepted-residual.md):
its accepted residual and its mitigation are replaced by the measured ones above;
its lock stands. Partly supersedes
[ADR-0129](0129-with-allow-proc-rbind-a-shared-pid-namespace-is-refused-and-the-store-is-anonymous.md):
its anonymous store, the second measure, is dropped, and with it the Good and Bad
consequences that rest on it; its peer scan stands. Where ADR-0129's "What stays
of ADR-0127" restates the inferred residual, this ADR's residual replaces it.
ADR-0129 is not wholly superseded, so its front matter stays `accepted`. Cites
[ADR-0126](0126-a-rootless-run-gets-an-rbind-of-the-containers-proc-only-with-allow-proc-rbind.md)
(the opt-in), ADR-0101 (the peer scan), ADR-0077 (no persistent store on `/`, which
this ADR does not extend to the rbind), ADR-0051 and T12, T13 and T21 of the
threat model.

Answers a question put to the maintainer in the question window after the
measurement; it has no `question` issue, and #79 is a `task` closed by its own pull
request, so this one closes nothing. Revisit only when a measurement contradicts
this one. The follow-up docs are those listed under Where it is written, and the
pull request's `## Changelog` entry for the flag.
