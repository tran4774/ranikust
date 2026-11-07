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

A later review of #79 found that the first way of telling that uid, the map of
ranikust's own user namespace, reads one level only: below a namespace that
maps another uid to the initial namespace's root, it misses root. The second
question put to the maintainer: fix it in #79, or leave it to an advisory?

## Considered Options

- Decide on ranikust's uid in the initial user namespace, recorded before it
  enters the build's user namespace, in #79 (recommended, chosen).
- Keep the effective uid where the sandbox starts, and document that rootless
  overlay mode gets `RUN` cgroups only at its cgroup namespace's root.

How to tell that uid:

- Compare the effective uid with the owner a sysctl that the initial
  namespace's root owns shows, `/proc/sys/kernel/ostype`; the map one level up
  where that file is not the sysctl (recommended, chosen).
- Keep the map one level up alone (the first implementation): no runtime
  ranikust targets nests a namespace that maps another uid to the initial
  namespace's root **[inferred]**.

## Decision Outcome

ranikust trusts `user.delegate` on its cgroup `D` when its uid in the initial
user namespace is not 0 and it owns `D`; otherwise `trusted.delegate` alone, as
ADR-0110 (1) says for root.

The maintainer's answers stood for the chosen options, both implemented in #79:
2026-11-08, "Quyết theo uid ngoài, trong #79 (khuyến nghị)" ("Decide on the
outer uid, in #79 (recommended)"); 2026-11-08, "Sửa luôn trong #79 (khuyến
nghị)" ("Fix it in #79 now (recommended)").

- **Which uid.** `rnk_sandbox::enter_build_user_namespace` records, before its
  `unshare`, whether ranikust's effective uid is the initial namespace's root:
  - It is where it equals the owner that `/proc/sys/kernel/ostype` shows. The
    initial namespace's root owns that sysctl, and `stat` shows that owner
    through every level of user namespaces, or as the overflow uid where a
    level maps it to nothing.
  - The file counts only as a regular file of a procfs (`statfs`
    `PROC_SUPER_MAGIC`) that reads `Linux`, which excludes a tmpfs or another
    procfs file over it.
  - Elsewhere `/proc/self/uid_map` decides: whether it maps the uid to 0 one
    level up. A map ranikust cannot read or parse, or that maps the uid
    nowhere, counts as 0.
  - Where ranikust never enters the build's user namespace (root's own modes),
    the same reading is made when the sandbox starts.
  - Where it cannot tell, the uid counts as 0, the rule that trusts fewer
    marks: an effective uid that is the overflow uid where root shows as it
    too.
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
  namespace is not 0, at any depth below it.
- Good, because real root is told at any depth, through namespaces that remap
  it.
- Bad, because a user's own `Delegate=yes` units carry no mark (systemd 259
  **[measured]**), so a build started from one still gets none there.
- Neutral, because a uid other than root's that shows as the overflow uid,
  where root does too, counts as root and trusts `trusted.delegate` alone
  (Podman or Docker `--user 65534` in a user namespace **[measured]**).
- Neutral, because where `/proc/sys` is not procfs the map one level up
  decides, as the first implementation did.

## Evidence

- On this project's test machine (systemd 259), `user@1000.service` carries
  `user.delegate` `1` and is owned by the user; a `Delegate=yes` service or scope
  of the user's own manager (`systemd-run --user`) carries none **[measured]**.
- `trusted.*` extended attributes need `CAP_SYS_ADMIN` in the initial user
  namespace, so a process in another one reads them as absent **[external]**
  (xattr(7)); `/proc/<pid>/uid_map` read from the process's own namespace maps
  its ids to the parent namespace **[external]** (user_namespaces(7)).
- The owner of `/proc/sys/kernel/ostype`, compared with the effective uid, was
  measured in Ubuntu 22.04 (5.15) and 24.04 (6.8) VMs and on this project's
  test machine (7.0, rootless Podman 5.7) **[measured]**:
  - It is root for real root on the host, one level down mapped 0 to 0, and
    two and three levels down through other uids. The map one level up reads
    `0 5 1` there and misses it.
  - It is root in rootful Docker, default and `--privileged`.
  - It is not root for a user, `nobody`, root of rootless Podman (also
    `--userns=keep-id` and nested `unshare` below it), Docker `userns-remap`,
    and a user's own namespaces.
  - The overflow uid on both sides gives a false "root" for a non-root process
    of uid 65534 (Podman `--user 65534`, `userns-remap` with `--user 65534`, a
    user mapped to 65534).
  - A tmpfs over `/proc/sys` or `/proc/sys/kernel`, or a tmpfs or ext4 file
    bound over the sysctl, shows a type other than procfs.
  - On 24.04 with `kernel.apparmor_restrict_unprivileged_userns` 1, a second
    level below root's own namespace gets no map.
- Unit tests in `rnk-sandbox` **[code]**:
  - `src/cgroup/tests.rs`: the marks trusted for each case, the map reading on
    fixture maps, and the sysctl decision.
  - `src/userns/build/tests.rs`: through nested namespaces, real root and the
    sudo user (T2), and root of rootless Podman (T1).

## More Information

Refines how [ADR-0110](0110-a-run-cgroup-only-in-a-marked-or-namespaced-tree-nested-with-cpu-where-ranikust-is-alone.md)
(1) applies; ADR-0110 itself is unchanged. Implemented in
`crates/rnk-sandbox/src/cgroup.rs` (`trusted_marks`, `root_shown_as`,
`is_kernel_root`, `maps_to_root`) and `crates/rnk-sandbox/src/setup.rs`;
described in [`design/sandbox.md`](../design/sandbox.md#run-cgroups) and the
book's [Where to run](../book/src/where-to-run.md).
