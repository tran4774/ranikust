---
status: accepted
date: 2026-10-31
decision-makers: Tran Nguyen (maintainer), by the rule to follow BuildKit where kaniko and BuildKit differ
---

# ADR-0045: A kaniko-mode layer holds what changed, not the targets of changed symlinks

## Context and Problem Statement

kaniko's full-filesystem snapshot passes the changed paths through
`ResolvePaths` (`kaniko@44aa7ddc0 pkg/filesystem/resolve.go:39-102`): for a
changed symlink it also adds the link's target, resolved with
`filepath.EvalSymlinks` on the live root, whether the target changed or not
(`:69-98`), and every parent of both (`filesWithParentDirs`, `:100-120`).
A `RUN ln -s /usr/lib/x.so /a` therefore copies the unchanged `x.so` into
the layer. BuildKit's differ writes only what changed
(`buildkit@cd8ee24 util/overlay/overlay_linux.go:186`, `Changes`), and so
does ranikust's overlay mode. The kaniko-mode exporter (Phase 2 task 5,
`WalkDiff::export`) has to pick one.

A smaller difference of the same kind: kaniko writes the parent
directories of a changed socket and then skips the socket
(`pkg/snapshot/snapshot.go:374-387`, `pkg/util/tar_util.go:74-77`); the
exporter, like the overlay exporter and BuildKit, leaves the socket and
with it parents that nothing else needs.

## Considered Options

- Follow kaniko: add the target of every changed symlink, and its parents.
- Follow BuildKit: write what changed.

## Decision Outcome

Chosen: follow BuildKit. A kaniko-mode layer holds the walk's changed
entries, its whiteouts and the parents of both, never an unchanged symlink
target; a socket is left out with no parent written for it alone.

### Consequences

- Good, because overlay mode and kaniko mode give the same layer for the
  same change, and the same flattened image as BuildKit.
- Good, because the exporter never resolves a symlink on the live root,
  which in a host workdir could point anywhere; it reads only what the
  walk saw.
- Neutral, because the flattened image is the same either way: an
  unchanged target is already in a lower layer.
- Bad, because a kaniko-mode layer can hold fewer entries than kaniko's
  for the same step, so a layer-by-layer comparison with kaniko must
  allow for it; the flattened comparison does not.

## Evidence

- kaniko at `44aa7ddc0`: `scanFullFilesystem` resolves the walk's changed
  paths (`pkg/snapshot/snapshot.go:285`), `ResolvePaths` adds link
  targets (`pkg/filesystem/resolve.go:69-98`).
- BuildKit at `cd8ee24`: `Changes` walks the upper directory only
  (`util/overlay/overlay_linux.go:186`).
- `crates/rnk-snapshot/src/kaniko/export/tests.rs`: a new symlink to
  `/etc/passwd` is written as the link alone; the property test rebuilds
  the walked tree from the layers.

## More Information

An accepted difference from kaniko next to those of
[ADR-0032](0032-observable-behaviour-follows-buildkit.md); listed in
[kaniko-map](../agents/kaniko-map.md#behaviour-to-match). Design:
[kaniko-mode](../design/kaniko-mode.md#exporter-requirements-specific-to-the-walk).
