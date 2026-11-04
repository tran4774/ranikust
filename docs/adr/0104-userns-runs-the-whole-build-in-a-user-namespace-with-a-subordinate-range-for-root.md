---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0104: `--userns` runs the whole build in a user namespace with a range for root from a `ranikust` entry of `/etc/subuid`, extracted layers keyed by their id map, and rootless mode also leaves the init's uid unmapped

## Context and Problem Statement

[ADR-0003](0003-dockerfile-trust-and-default-isolation.md) says `--userns` runs
"the build" in a user namespace, [ADR-0083](0083-the-command-gets-its-own-cgroup-where-delegated-and-a-user-namespace-in-phase-3.md)
(b) says "the command" runs in one that leaves the init's uid unmapped, and
[sandbox](../design/sandbox.md#per-run-sequence) had rootless mode add
`CLONE_NEWUSER` to the `RUN`'s clone. A `RUN`'s root owns the image's root
files only if they are owned by mapped ids, while root's builds share `layers/`
in `/var/lib/ranikust`, extracted with real ids. How does `--userns` map ids as
real root, and does it share the store (raised while planning Phase 3,
[#41](https://github.com/tran4774/ranikust/issues/41))? How are its extracted
layers kept apart from root's (#90), and where does root's range come from,
which many hosts do not list (#91)? And does rootless mode also leave the init's
uid unmapped, which ADR-0083 names only for `--userns`?

## Considered Options

- (a) The whole build runs in a user namespace, as in rootless mode, with a
  subordinate range for root (podman's `--userns=auto` allocates one from the
  `containers` entry of `/etc/subuid` **[external]**) and extracted layers kept
  apart from root's (keyed by the map, or in their own state directory).
- (b) Only the command runs in a user namespace, given the image's ids through
  an idmapped mount of the stage root (`mount_setattr(MOUNT_ATTR_IDMAP)`), the
  store unchanged, with a kernel floor for overlayfs on idmapped layers
  **[inferred]**.
- (c) A shifted copy of the lowers per build.

How the layers are kept apart, given (a) (#90, where these three are (a), (b)
and (c)):

- (d) Key extracted layers by their id map in the same state directory (the
  map's digest is part of the layer's key): one store, one lock protocol and a
  prune that sees everything; one extracted copy per distinct map, and #1's
  naming must include the map.
- (e) A separate state directory (or subtree) for `--userns` builds: keys
  unchanged; a second store to lock and prune, and another one for each further
  map.
- (f) One extraction with real ids, shifted for the namespace by idmapped mounts
  of the lowers: no copy, but the kernel floor and the choice against idmapped
  mounts of (b).

(d) because the map is part of what produced the extracted content, so it
belongs in its key; it also covers a rootless user whose ranges change in
`/etc/subuid`, whose old layers would otherwise carry stale ids (the
recommendation on #90).

Where root's range comes from, given (a) (#91, where these three are (a), (b)
and (c)):

- (g) Refuse before anything is written where no range is listed, naming the
  lines to add.
- (h) A fixed default range where none is listed: works without set-up, but the
  range may overlap real users' ids or another tool's ranges, so a `RUN` could
  own files of an unrelated account on shared filesystems.
- (i) Map one id only (the build's root to one unprivileged uid), refusing
  images with several owners as ADR-0098 does for a one-id map.

(g) is security first (ADR-0049): an overlapping range silently shares ids
across an isolation boundary, which `--userns` exists to prevent, and the flag
is opt-in, so a line of set-up is a fair cost; (i) would refuse most images with
service users, a surprise for a flag meant to add safety (the recommendation on
#91).

Which entry of `/etc/subuid` and `/etc/subgid` a root build reads, given (g):

- `ranikust`, a dedicated one.
- `containers`, from which podman allocates its containers' ranges
  **[external]** (see Evidence): ranikust would not see those allocations.
- `root`, which other tools such as LXD may use **[inferred]**.

## Decision Outcome

`--userns` runs the whole build in a user namespace, as rootless mode does, with
a subordinate range for root and extracted layers kept apart from root's, and
rootless mode also leaves the init's uid unmapped: option (a) (the maintainer,
2026-11-04: "tất cả theo khuyến nghị", all as recommended). It also answers #90
and #91: the layers are keyed by their id map in the same state directory,
option (d), (a) in #90 ("Khóa theo id map (khuyến nghị)"), and root's range
comes from a dedicated `ranikust` entry, refused where it is missing, option
(g), (a) in #91 ("Từ chối, chỉ dòng cần thêm (khuyến nghị)", then "Mục riêng
`ranikust` (khuyến nghị)" when asked which entry).

- **Planned, Phase 3**, the `--userns` task
  ([#83](https://github.com/tran4774/ranikust/issues/83), task 12 of #41) and
  the init's uid (task 13, #34). Nothing is built by this ADR.
- **One user-namespace path** for rootless mode and `--userns`: no kernel floor
  beyond rootless mode's.
- **Extracted layers, in rootless mode and under `--userns`, are keyed by their
  id map**, in the same state directory: the map's digest is part of the
  layer's key, so a layer extracted with real ids is never used under a map. It
  is one store, one lock protocol and one prune, with one extracted copy per
  distinct map; it also covers a rootless user whose ranges change. How [#1](https://github.com/tran4774/ranikust/issues/1)
  (Q-015, still open) names an extracted layer must include the map.
- **The init's uid stays unmapped in rootless mode too**, not only under
  `--userns`, since the code is the same. ADR-0083 (b) follows from running the
  init under a uid outside the command's map **[inferred]**.
- **Root's range** is read from a dedicated `ranikust` entry of `/etc/subuid`
  and `/etc/subgid`, not from `containers` (podman allocates its containers'
  ranges from it, without ranikust seeing them) and not from `root` (other tools
  such as LXD may use it). Where the entry is missing, the build is refused
  before anything is written, naming the lines to add, `ranikust:<start>:65536`
  in both files; the message leaves `<start>` to the admin.
- **An owner outside the `ranikust` range is refused**, as
  [ADR-0098](0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)
  says for every user namespace a build's files are written in: the scan of the
  base layers and the refusal at the first failing `chown` take the range as
  input (the maintainer, 2026-11-04: "Từ chối như ADR-0098 (khuyến nghị)").

### Consequences

- Good, because there is one user-namespace path to build, test and audit for
  rootless mode and `--userns`.
- Good, because no kernel floor is added beyond rootless mode's, unlike (b),
  which needs overlayfs on idmapped layers and a kernel floor for it
  **[inferred]**.
- Good, because the init's uid is unmapped in both modes, so ADR-0083's
  measurement (8 and 32 spinners delaying the init's death) can be repeated
  under both.
- Good, because the layers share one store, one lock protocol and one prune, and
  a rootless user's changed ranges cannot leave stale ids behind.
- Good, because the `ranikust` entry keeps ranikust's range apart from podman's
  and from root's other tools, and a missing one is refused before anything is
  written.
- Bad, because a root that uses `--userns` needs a `ranikust` line in both files
  (the set-up is one line each), and each distinct map has its own extracted
  copy of the layers it uses, a cost in disk and time for the first build under
  each map **[inferred]**.

## Evidence

- Nothing is measured here: `--userns` does not exist.
- **[external]** podman's `--userns=auto`: rootful, it needs the user
  `containers` in `/etc/subuid` and `/etc/subgid` with an unused range, from
  which it allocates unique ranges; rootless, it uses the user's own range from
  those files ([podman-run](https://docs.podman.io/en/latest/markdown/podman-run.1.html),
  the `--userns` option, `auto` mode). That is why ranikust does not share the
  `containers` entry.
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
and [ADR-0003](0003-dockerfile-trust-and-default-isolation.md). Answers #90 and
#91. [#1](https://github.com/tran4774/ranikust/issues/1)
(Q-015, which digest names an extracted layer) stays open, and its answer must
include the id map.

Follow-ups, with [#83](https://github.com/tran4774/ranikust/issues/83) (after
tasks 5 and 7 of #41): `rnk-sandbox`, `ranikust` and `rnk-store`, with T2
tests (a `RUN`'s uid 0 is an unprivileged host uid, its `uid_map` read from
outside; the flattened content equals the build without `--userns`; a state
directory whose parents the mapped uid cannot traverse is refused before
anything is written; a root build with no `ranikust` entry is refused, naming
the lines to add; an image with an owner outside the `ranikust` range is refused
(ADR-0098); two maps give two extracted copies of a layer, and a layer
extracted under one map is not used under another). When it is built, change
[sandbox](../design/sandbox.md), [cache-and-store](../design/cache-and-store.md)
(the map in a layer's key), the threat model, the cards, the book's flag list
and the `## Changelog`.
