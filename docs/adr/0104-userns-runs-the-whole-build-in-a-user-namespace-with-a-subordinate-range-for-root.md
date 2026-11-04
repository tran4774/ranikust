---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0104: `--userns` runs the whole build in a user namespace with a subordinate range for root, extracted layers kept apart, and rootless mode also leaves the init's uid unmapped

## Context and Problem Statement

[ADR-0003](0003-dockerfile-trust-and-default-isolation.md) says `--userns` runs
"the build" in a user namespace, [ADR-0083](0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md)
(b) says "the command" runs in one that leaves the init's uid unmapped, and
[sandbox](../design/sandbox.md#per-run-sequence) had rootless mode add
`CLONE_NEWUSER` to the `RUN`'s clone (rewritten with this ADR). A `RUN`'s root owns the image's root files
only if they are owned by mapped ids, while root's builds share `layers/` in
`/var/lib/ranikust`, extracted with real ids. How does `--userns` map ids as
real root, and does it share the store (raised while planning Phase 3,
[#41](https://github.com/tran4774/ranikust/issues/41))? And does rootless mode
also leave the init's uid unmapped, which ADR-0083 names only for `--userns`?

## Considered Options

- (a) The whole build runs in a user namespace, as in rootless mode, with a
  subordinate range for root (podman's `--userns=auto` reads one from
  `/etc/subuid` **[unverified]**) and extracted layers kept apart from root's
  (keyed by the map, or in their own state directory).
- (b) Only the command runs in a user namespace, given the image's ids through
  an idmapped mount of the stage root (`mount_setattr(MOUNT_ATTR_IDMAP)`), the
  store unchanged, with a kernel floor for overlayfs on idmapped layers
  **[unverified]**.
- (c) A shifted copy of the lowers per build.

## Decision Outcome

`--userns` runs the whole build in a user namespace, as rootless mode does, with
a subordinate range for root and extracted layers kept apart from root's, and
rootless mode also leaves the init's uid unmapped: option (a) (the maintainer,
2026-11-04: "tất cả theo khuyến nghị", all as recommended).

- **Planned, Phase 3**, the `--userns` task
  ([#83](https://github.com/tran4774/ranikust/issues/83), task 12 of #41) and the
  init's uid (task 13, #34). Nothing is built by this ADR.
- **One user-namespace path** for rootless mode and `--userns`: no kernel floor
  beyond rootless mode's.
- **Extracted layers are kept apart** from the layers root's builds without the
  flag share, so a layer extracted with real ids is never used under a map. How
  (keyed by the map, or in their own state directory) is [open: #90].
- **The init's uid stays unmapped in rootless mode too**, not only under
  `--userns`, since the code is the same. ADR-0083 (b) follows from running the
  init under a uid outside the command's map **[inferred]**.
- **Not settled here:** what `--userns` does where `/etc/subuid` holds no range
  for root, [open: #91].

### Consequences

- Good, because there is one user-namespace path to build, test and audit for
  rootless mode and `--userns`.
- Good, because no kernel floor is added beyond rootless mode's, unlike (b),
  whose overlayfs on idmapped layers is **[unverified]**.
- Good, because the init's uid is unmapped in both modes, so ADR-0083's
  measurement (8 and 32 spinners delaying the init's death) can be repeated
  under both.
- Bad, because a root that uses `--userns` needs a subordinate range, and keeps
  a second set of extracted layers, a cost in disk and time for the first
  `--userns` build **[inferred]**.

## Evidence

- Nothing is measured here: `--userns` does not exist.
- **[unverified]** podman's `--userns=auto` takes a range from `/etc/subuid` for
  the container; not checked against podman's documentation or source.
- **[inferred]** ADR-0083 (b) follows from running the init under a uid outside
  the command's map; ADR-0083's own evidence is a measurement of the delay
  without it (0.9 to 1.8 ms, 7.6 s with 8 spinners, 30.6 s with 32).
- The rootless user-namespace path itself is measured in
  [ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md#evidence)
  (maps, renames and the Ubuntu 24.04 restriction).

## More Information

Raised while planning Phase 3
([#41](https://github.com/tran4774/ranikust/issues/41)). Refines
[ADR-0083](0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md)
(b), which named only `--userns` and stays `accepted` with its body unchanged,
and [ADR-0003](0003-dockerfile-trust-and-default-isolation.md). Two points are
left to question issues: #90 (how extracted layers are kept apart) and #91 (no
range for root in `/etc/subuid`). If layers are keyed by their id map,
[#1](https://github.com/tran4774/ranikust/issues/1) (Q-015, which digest names an
extracted layer) is affected.

Follow-ups, with [#83](https://github.com/tran4774/ranikust/issues/83) (after tasks 5 and 7 of #41): `rnk-sandbox`, `ranikust`
and `rnk-store`, with T2 tests (a `RUN`'s uid 0 is an unprivileged host uid,
its `uid_map` read from outside; the flattened content equals the build without
`--userns`; a state directory whose parents the mapped uid cannot traverse is
refused before anything is written). When it is built, change
[sandbox](../design/sandbox.md), [cache-and-store](../design/cache-and-store.md)
(if layers are kept apart), the threat model, the cards, the book's flag list
and the `## Changelog`.
