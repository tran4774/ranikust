---
status: accepted
date: 2026-11-08
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0141: An owner of 4294967295 over an earlier layer's directory is an accepted difference between the modes

## Context and Problem Statement

An owner id of 4294967295 is `chown`'s -1, "leave unchanged". kaniko and
BuildKit pass it to `chown` as it is; neither treats it specially.
[#217](https://github.com/tran4774/ranikust/pull/217)
([#212](https://github.com/tran4774/ranikust/issues/212)) made it mean
"unchanged" in every ranikust build, as release builds already treated it, and
documented the result in [rootfs](../design/rootfs.md#metadata). The two modes
then differ for a directory entry with that owner over a directory an earlier
layer made: kaniko mode extracts onto the existing directory and keeps the
earlier layer's owner, while overlay mode makes the directory fresh in the
upperdir and gives it the build process's own id (root). Layer digests of such
an image can therefore differ between the modes. Should the modes be made to
agree? Asked in [#216](https://github.com/tran4774/ranikust/issues/216).

## Considered Options

- (a) Record it as an accepted mode difference: a short ADR, plus a line in the
  book's list of mode differences. No security cost, since both owners are ones
  the build may set, and no performance cost.
- (b) Make the modes agree, for example overlay mode copying the lower
  directory's owner up when the entry's id is -1. It costs a lookup per such
  entry, and kaniko-mode parity is lost if kaniko mode is changed instead.
- (c) Refuse an owner of 4294967295 in a layer, as an id with no meaning. This
  breaks parity with kaniko and BuildKit for images that carry it, which are
  rare.

(a) was the recommendation: the id is an edge case no real image relies on,
each mode already matches its upstream, and no owner outside what the build may
set results.

## Decision Outcome

A layer's directory entry with owner id 4294967295 over a directory an earlier
layer made keeps that layer's owner in kaniko mode and gets the build process's
own id in overlay mode, and this is an accepted difference between the modes,
with no code change.

The maintainer's answer, 2026-11-08: "Chấp nhận khác biệt (khuyến nghị)"
("Accept the difference (recommended)"), for option (a) as the question's body
gave it at the time.

- **The difference.** Over a directory an earlier layer made, kaniko mode
  extracts every layer into one root, so the entry changes nothing of the
  owner; overlay mode extracts each layer into a directory of its own, so the
  directory is new there and takes the extracting process's id. A new entry
  gets the process's id in both modes, and the owner scans read the id as no
  owner in both.
- **Each mode matches its upstream.** kaniko mode does what kaniko does, and
  overlay mode what BuildKit's overlayfs snapshotter does; both pass the id to
  `chown` as it is.
- **Stated** in [rootfs](../design/rootfs.md#metadata) and in the book's
  [differences between the modes](../book/src/how-it-builds.md#differences-between-the-modes).
- **Not decided.** Any other owner id, and the refusals of owners a user
  namespace does not map, which are unchanged.

### Consequences

- Good, because nothing changes for any build: both modes keep the behaviour
  release builds already had, so no lookup is added per entry in overlay mode.
- Good, because each mode keeps its parity with its upstream tool, and neither
  gets an owner outside what the build may set.
- Bad, because layer digests of an image that carries the id over an earlier
  layer's directory differ between the modes, and the layer cache the modes
  share can mix the two behaviours, as ADR-0012 says of its own differences.
- Neutral, because it is reversible: a later (b) or (c) is a new ADR, and the
  question's premise, that no real image relies on the id, is **[inferred]**,
  not measured.

## Evidence

- ranikust passes the id through one helper, `rnk_rootfs::chown_id`, which
  returns no owner for `u32::MAX`, and the extraction forms, the owner scans,
  the cache owner and the secret owner use it **[code]**
  (`crates/rnk-rootfs/src/meta.rs`). The test
  `an_owner_of_u32_max_leaves_the_processs_own_and_the_scans_agree`
  (`crates/rnk-rootfs/src/extract/tests.rs`) covers a new entry in every
  extraction form; it does not cover an entry over an earlier layer's
  directory.
- The two modes' result over an earlier layer's directory is the question's
  reading of the two extractors and the statement in
  [rootfs](../design/rootfs.md#metadata), not a test of the layered case
  **[inferred]**.
- kaniko (`pkg/util/fs_util.go:1323,1336`) and BuildKit
  (`vendor/github.com/containerd/containerd/v2/pkg/archive/tar.go:409-417`) pass
  the id to `chown` as it is, and BuildKit refuses it only when an id map is
  configured, which ranikust has none of **[external]**, as cited in
  [rootfs](../design/rootfs.md#metadata) and in the question; not re-read for
  this record.

## More Information

Answers [#216](https://github.com/tran4774/ranikust/issues/216). Adds one
difference to those of
[ADR-0012](0012-accept-divergence-between-modes.md), which stays whole, and
rests on [ADR-0002](0002-two-snapshot-modes.md) (two modes, one interface).
The docs it changes: [rootfs](../design/rootfs.md#metadata) and the book's
differences between the modes. Revisit if an image that carries the id turns up
in practice, or if a later change makes the modes agree on it; either is a new
ADR.
