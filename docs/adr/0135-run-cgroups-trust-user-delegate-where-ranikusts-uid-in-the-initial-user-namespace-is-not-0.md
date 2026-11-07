---
status: accepted
date: 2026-11-08
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0135: `RUN` cgroups trust `user.delegate` where ranikust's uid in the initial user namespace is not 0

## Context and Problem Statement

[ADR-0110](0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md)
(1), as refined on 2026-11-05, lets a root ranikust trust only
`trusted.delegate` on its cgroup `D`, and counts `user.delegate`, which anyone
who may write `D`'s inode can set, only for a ranikust that is not root and owns
`D`, naming rootless mode
([#79](https://github.com/tran4774/ranikust/issues/79)) as the case. The code
decided "root" on the effective uid where the sandbox starts. Rootless overlay
mode starts it as root of the build's user namespace, so `user.delegate` never
counted there, and `trusted.*` reads as absent outside the initial user
namespace: a rootless build got `RUN` cgroups only at the root of its own cgroup
namespace, against the stated intent. A review of #79 raised it; the question
put to the maintainer in the question window, which has no `question` issue:
decide on which uid, and in which pull request?

## Considered Options

- Decide on ranikust's uid in the initial user namespace, recorded before it
  enters the build's user namespace, in #79 (recommended).
- Keep the effective uid where the sandbox starts, and document that rootless
  overlay mode gets `RUN` cgroups only at its cgroup namespace's root.

## Decision Outcome

ranikust trusts `user.delegate` on its cgroup `D` when its uid in the initial
user namespace is not 0 and it owns `D`; otherwise `trusted.delegate` alone, as
ADR-0110 (1) says for root.

The maintainer's answer (2026-11-08: "Quyết theo uid ngoài, trong #79 (khuyến
nghị)", "Decide on the outer uid, in #79 (recommended)") stood for the first
option, implemented in #79.

- **Which uid.** `rnk_sandbox::enter_build_user_namespace` records, before its
  `unshare`, whether ranikust's effective uid maps to 0 in the parent user
  namespace by `/proc/self/uid_map`. The parent is the initial user namespace on
  a host and in a container's user namespace (rootless Podman). Where ranikust
  never enters the build's user namespace (root's own modes), the same reading is
  made when the sandbox starts. A map ranikust cannot read or parse, or that maps
  the uid nowhere, counts as 0: the rule that trusts fewer marks.
- **Which owner.** `D`'s owner and ranikust's effective uid are compared as the
  sandbox sees them: in the build's user namespace, ranikust's own uid outside
  shows as 0, so a `D` the invoking user owns matches, and one owned by root or
  any unmapped uid shows as the overflow uid and does not.
- **Why it holds.** Where ranikust's uid in the initial user namespace is not
  0, a `RUN` runs with no uid there that ranikust lacks: one that sets
  `user.delegate` on `D` could have written `D` as ranikust does. Real root
  keeps ADR-0110's rule, which the T2 red team
  `red_team_a_run_cannot_forge_a_delegation_mark_root_trusts` checks.
- **Not changed.** The other conditions of ADR-0110 (the mark on `D` itself, a
  read-write cgroupfs, `cgroup.kill`, the start-up probe) and the namespace-root
  rule.

### Consequences

- Good, because a rootless build in a cgroup systemd delegated to the user it
  runs as (a system unit or scope with `Delegate=yes` and `User=`, such as
  `user@<uid>.service`) gets `RUN` cgroups, killed with the `RUN`.
- Good, because root of a container's user namespace (root of rootless Podman)
  follows the same rule as the user it is: its uid in the initial user
  namespace is not 0.
- Bad, because a user's own `Delegate=yes` units carry no mark (systemd 259
  **[measured]**), so a build started from one still gets none there.
- Neutral, because a user namespace nested more than one level below the
  initial one is judged by its parent's view of the uid: a parent that maps a
  uid other than 0 to the initial namespace's root is not seen **[inferred]**;
  no runtime ranikust targets builds such a map.

## Evidence

- On this project's test machine (systemd 259), `user@1000.service` carries
  `user.delegate` `1` and is owned by the user; a `Delegate=yes` service or scope
  of the user's own manager (`systemd-run --user`) carries none **[measured]**.
- `trusted.*` extended attributes need `CAP_SYS_ADMIN` in the initial user
  namespace, so a process in another one reads them as absent **[external]**
  (xattr(7)); `/proc/<pid>/uid_map` read from the process's own namespace maps
  its ids to the parent namespace **[external]** (user_namespaces(7)).
- Unit tests in `rnk-sandbox` (`src/cgroup/tests.rs`): the marks trusted for
  each case, and the map reading on fixture maps **[code]**.

## More Information

Refines how [ADR-0110](0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md)
(1) applies; ADR-0110 itself is unchanged. Implemented in
`crates/rnk-sandbox/src/cgroup.rs` (`trusted_marks`, `maps_to_root`) and
`crates/rnk-sandbox/src/setup.rs`; described in
[`design/sandbox.md`](../design/sandbox.md#run-cgroups) and the book's
[Where to run](../book/src/where-to-run.md).
