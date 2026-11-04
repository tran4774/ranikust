---
status: accepted
date: 2026-11-04
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0106: Extraction in a user namespace skips device nodes and counts them as `nodes_skipped`

## Context and Problem Statement

Overlay as root creates a base layer's block and character device nodes in the
store ([ADR-0041](0041-live-root-extraction-skips-device-nodes.md) left
`extract_overlay` unchanged). In a user namespace `mknod` of a real device needs
`CAP_MKNOD` in the initial user namespace, so it fails, while the whiteout node
(character device 0/0) works **[measured]**
([experiment 01](../research/experiments/01-overlay-semantics.md),
[experiment 02](../research/experiments/02-run-sandbox.md)). Rootless overlay
mode, planned for Phase 3 ([#41](https://github.com/tran4774/ranikust/issues/41)),
extracts base layers in such a namespace
([ADR-0099](0099-rootless-publishes-extracted-layers-by-rename-from-inside-the-user-namespace.md)).
What does extraction do with device nodes there (question N5 of the Phase 3
plan)?

## Considered Options

- (a) Skip them and count them as `nodes_skipped`, as ADR-0041's live extraction
  does.
- (b) Refuse images holding device nodes in rootless mode.
- (c) Put empty regular files in their place.

## Decision Outcome

Extraction in a user namespace skips block and character device nodes and counts
them as `nodes_skipped`, as ADR-0041's live extraction does: option (a) (the
maintainer, 2026-11-04: "tất cả theo khuyến nghị", all as recommended).

- **Planned, Phase 3**, task 6 of #41 (extract and publish layers inside the
  namespace). Nothing is built by this ADR; overlay as root keeps creating the
  nodes.
- **Whiteouts are not affected:** `mknod` of the 0/0 node of a whiteout works in
  a namespace **[measured]**, so whiteouts are still created.
- **Reason:** the output reuses the base blobs unchanged, so the image keeps the
  nodes; and the root of a full-sandbox `RUN` is `nodev`, so no `RUN` could open
  one anyway ([sandbox](../design/sandbox.md#per-run-sequence)). containerd skips
  them in a user namespace too **[external]**.

### Consequences

- Good, because extraction never fails on an image holding device nodes, and the
  image is exported with them, unchanged.
- Good, because it matches ADR-0041 and containerd's behaviour in a user
  namespace.
- Bad, because a `COPY --from` of a path holding a device node differs from
  overlay as root: the copy lacks the node.

## Evidence

- [Experiment 01](../research/experiments/01-overlay-semantics.md): in a user
  namespace `mknod c 0 0` works **[measured]**.
- [Experiment 02](../research/experiments/02-run-sandbox.md): `mknod` is refused
  at every rootless level because `CAP_MKNOD` must be held in the initial user
  namespace **[measured]**.
- **[external]** BuildKit `cd8ee2441a4c` with the containerd v2.4.2 it vendors,
  read by `upstream-oracle`: the archive apply skips block and character device
  entries in a user namespace (`pkg/archive/tar_unix.go:100-108`).
- **[code]** [sandbox](../design/sandbox.md#per-run-sequence): the `RUN` root is
  remounted `nodev`, so no node in it opens.

## More Information

Answers question N5 of the Phase 3 plan on
[#41](https://github.com/tran4774/ranikust/issues/41), put to the maintainer in
the plan draft. Related:
[ADR-0041](0041-live-root-extraction-skips-device-nodes.md) (the same rule for
live roots), [ADR-0032](0032-observable-behaviour-follows-buildkit.md).

Follow-ups, with task 6 of #41: `rnk-rootfs` (`extract_overlay` in a namespace)
and `rnk-snapshot`, with a T1 or T2 test of a base layer holding a device node.
When it is built, change [overlay-snapshot](../design/overlay-snapshot.md), the
`rnk-rootfs` and `rnk-snapshot` cards, and the book if a difference from overlay
as root is user-visible (the `## Changelog` entry).
