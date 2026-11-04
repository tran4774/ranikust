---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0105: A minimal `ranikust prune --all` in Phase 3 empties a rootless state directory inside the user namespace

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
(question N4 of the Phase 3 plan,
[#41](https://github.com/tran4774/ranikust/issues/41))?

## Considered Options

- (a) A minimal `ranikust prune --all` in Phase 3 that empties the state
  directory inside the namespace, extended in Phase 5.
- (b) Document `podman unshare rm -rf <dir>` until Phase 5.
- (c) A general `ranikust unshare <command>`, as buildah and podman have.

(b) needs podman installed. (c) hands out a shell in the namespace, a wider tool
than the need.

## Decision Outcome

Phase 3 ships a minimal `ranikust prune --all` that empties the state directory
inside the build's user namespace, and Phase 5 extends it into the full `prune`:
option (a) (the maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as
recommended).

- **Planned, Phase 3**, within the rootless mode tasks of #41 (it serves task 8's
  acceptance criterion that the state directory is removable). Nothing is built
  by this ADR; `prune` does not exist today.
- **It reuses `rnk-store`'s clean-up**, which task 5 of #41 makes run inside the
  namespace, and Phase 5's `prune` grows from it
  ([ADR-0021](0021-rnk-store-crate.md): `ranikust prune` only calls
  `rnk-store`).
- **Phase 5's defaults are still** those of [ADR-0032](0032-observable-behaviour-follows-buildkit.md)
  (BuildKit's tiers, size caps, age by last use); this ADR adds only the flag
  that removes everything.

### Consequences

- Good, because it reuses `rnk-store`'s clean-up, so no second way to delete
  exists to audit.
- Good, because a user needs no other tool (podman) to leave no state behind.
- Bad, because the `prune` subcommand appears in Phase 3 with one flag, ahead
  of the Phase 5 design (size and age tiers, in-use tracking), which then
  extends it.
- Neutral, because a general `unshare` command stays unbuilt: nothing here hands
  out a shell in the namespace.

## Evidence

- **[measured]** `poc-runner` in the test VM, 2026-11-04: removing a populated
  `0555` tree of subordinate ids from outside the namespace fails (unlink
  `EACCES`, `chmod` `EPERM`)
  ([ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md#evidence)).
- **[code]** [cache-and-store](../design/cache-and-store.md#prune): `prune` is
  not built (Phase 5); start-up removes the directories of builds no longer
  running.
- **[external]** buildah and podman have `unshare`, which is option (c).

## More Information

Answers question N4 of the Phase 3 plan on
[#41](https://github.com/tran4774/ranikust/issues/41), put to the maintainer in
the plan draft. Related:
[ADR-0021](0021-rnk-store-crate.md) (the store and `prune`),
[ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md).

Follow-ups: `ranikust` and `rnk-store`, with a T1 test that empties a state
directory holding files of subordinate ids. When it is built, change
[cache-and-store](../design/cache-and-store.md#prune), the `ranikust` and
`rnk-store` cards, `ARCHITECTURE.md` and the book (`prune` is listed as not
existing until Phase 5), and add the `## Changelog` entry.
