---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0105: A minimal `ranikust prune --all` in Phase 3 empties a rootless state directory inside the user namespace and refuses when root runs it

## Context and Problem Statement

In rootless mode (planned, Phase 3), extracted layers and upper directories hold
files of subordinate ids, which the user cannot rename, `chmod` or unlink from
outside the user namespace
([ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md#evidence)).
The start-up clean-up removes the directories of dead builds only (inside the
namespace, once task 5 of
[#41](https://github.com/tran4774/ranikust/issues/41) builds that), and
`ranikust prune` is Phase 5
([cache-and-store](../design/cache-and-store.md#prune)). A user who drops
ranikust then has no tool left to empty `~/.local/share/ranikust`. How does a
rootless user remove a state directory holding files of subordinate ids
(raised while planning Phase 3,
[#41](https://github.com/tran4774/ranikust/issues/41))? And what does the
subcommand do when root runs it, where the state directory is the shared
`/var/lib/ranikust` (or `--state-dir`) and no user namespace is involved (#93)?

## Considered Options

- (a) A minimal `ranikust prune --all` in Phase 3 that empties the state
  directory inside the namespace, extended in Phase 5.
- (b) Document `podman unshare rm -rf <dir>` until Phase 5.
- (c) A general `ranikust unshare <command>`, as buildah and podman have.

(b) needs podman installed. (c) hands out a shell in the namespace, a wider tool
than the need.

When root runs it (#93):

- (d) Refuse, naming Phase 5's `prune`; root can remove its own state directory
  with ordinary tools.
- (e) Empty root's state directory too: the same `rnk-store` clean-up without
  the namespace, with the same marker check and the same protection of running
  builds (#76).
- (f) Empty it as root only with `--state-dir` naming a directory other than the
  default shared one.

(d) because root does not need the subcommand to leave no state behind, which is
the only reason this ADR builds it ahead of Phase 5; the shared root store is
where concurrent builds and in-use tracking matter most, and that is Phase 5's
design. (e) is the same code and would be the choice if a uniform subcommand
mattered more than a narrow one (the recommendation on #93).

## Decision Outcome

Phase 3 ships a minimal `ranikust prune --all` that empties the state directory
inside the build's user namespace, and Phase 5 extends it into the full `prune`:
option (a) (the maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as
recommended). When root runs it, it refuses, naming Phase 5's `prune`: option
(d) (the maintainer, 2026-11-04, on #93: "Từ chối khi là root (khuyến nghị)").

- **Planned, Phase 3**, [#76](https://github.com/tran4774/ranikust/issues/76)
  (it serves task 8's acceptance criterion that the state directory is
  removable). Nothing is built by this ADR; `prune` does not exist today.
- **It reuses `rnk-store`'s clean-up**, which task 5 of #41 makes run inside the
  namespace, and Phase 5's `prune` grows from it
  ([ADR-0021](0021-rnk-store-crate.md): `ranikust prune` only calls
  `rnk-store`).
- **Scope.** It is for a rootless state directory. When root runs it, it
  refuses and names Phase 5's `prune`; root can remove its own state directory
  with ordinary tools, and the shared `/var/lib/ranikust` is Phase 5's design.
- **Safety.** `prune --all` opens the store as a build does (the
  state-directory checks and the `.ranikust-state` marker), so it refuses a
  directory ranikust did not make. It never removes what a running build uses,
  the mechanism being #76's. It deletes through `rnk-rootfs` by descriptor, as
  the start-up clean-up does.
- **Phase 5's defaults are still** those of [ADR-0032](0032-observable-behaviour-follows-buildkit.md)
  (BuildKit's tiers, size caps, age by last use); this ADR adds only the flag
  that removes everything.

### Consequences

- Good, because it reuses `rnk-store`'s clean-up, so no second way to delete
  exists to audit.
- Good, because a user needs no other tool (podman) to leave no state behind.
- Good, because it cannot empty a directory ranikust did not make, and never
  removes what a running build uses, since it opens the store as a build does.
- Good, because the shared root store, where concurrent builds and in-use
  tracking matter most, is emptied by no narrow flag ahead of Phase 5's design.
- Bad, because the `prune` subcommand appears in Phase 3 with one flag, ahead
  of the Phase 5 design (size and age tiers, in-use tracking), which then
  extends it.
- Bad, because root has no ranikust command to empty a state directory until
  Phase 5, and removes it with ordinary tools.
- Neutral, because a general `unshare` command stays unbuilt: nothing here hands
  out a shell in the namespace.

## Evidence

- **[measured]** `poc-runner` in the test VM, 2026-11-04: removing a populated
  `0555` tree of subordinate ids from outside the namespace fails (unlink
  `EACCES`, `chmod` `EPERM`)
  ([ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md#evidence)).
- [cache-and-store](../design/cache-and-store.md#prune): `prune` is
  not built; start-up removes the directories of builds no longer
  running, and refuses a non-empty directory without the marker
  ([rnk-store](../agents/crates/rnk-store.md)).
- **[external]** buildah and podman have `unshare`, which is option (c).

## More Information

Raised while planning Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Related:
[ADR-0021](0021-rnk-store-crate.md) (the store and `prune`),
[ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md).

Follow-ups: `ranikust` and `rnk-store`, with a T1 test that empties a state
directory holding files of subordinate ids. When it is built, change
[cache-and-store](../design/cache-and-store.md#prune), the `ranikust` and
`rnk-store` cards, `ARCHITECTURE.md` and the book, and add the `## Changelog`
entry.
