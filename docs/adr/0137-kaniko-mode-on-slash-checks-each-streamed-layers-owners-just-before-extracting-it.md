---
status: accepted
date: 2026-11-08
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0137: Kaniko mode on `/` checks each streamed layer's owners just before extracting it

## Context and Problem Statement

[ADR-0098](0098-rootless-without-subuid-ranges-refuses-images-with-several-owners.md)
refuses an image with an owner the build's user namespace does not map, and
reads the owners before any layer is extracted where the check allows it.
In kaniko mode on a container's `/` no blob is kept: each base layer is
streamed from its registry
([ADR-0055](0055-stream-layers-keep-nothing-proportional-to-the-image.md)),
spooled to an anonymous file of the store and verified before it reaches
`/`, once per build
([ADR-0081](0081-a-streamed-layer-on-slash-is-verified-once-per-build.md)).
In a user namespace (a rootless container), the owner check runs in that
spool pass, one layer at a time, so when a later layer has an owner the
namespace lacks, the layers before it may already be on `/`. The
host-safety review of #80 raised it. The question put to the maintainer in
the question window, which has no `question` issue: keep the per-layer
check, or check every layer of an image before extracting any?

## Considered Options

The options are those of the question as put to the maintainer.

- (a) Keep the per-layer check (recommended): the container is disposable,
  the build fails naming the owners, and it costs nothing.
- (b) Read every layer's headers before extracting any: each layer is
  downloaded twice.
- (c) Spool every layer into the state directory first: one download, but
  disk equal to the image.

## Decision Outcome

Kaniko mode on `/` keeps checking each streamed layer's owners in the pass
that spools and verifies it, just before that layer is extracted, so the
layers before a refused one may already be on `/`.

The maintainer's answer (2026-11-08: "Giữ như hiện tại (khuyến nghị)",
"Keep it as it is (recommended)") stood for option (a).

- **What a refusal leaves.** The layers of the image before the refused one,
  on `/`. The build fails, naming the owners and the ways out
  (ADR-0098). `/` already carries the marker of a build on it, made before
  the first extraction, so the next build in the container empties `/` at
  its first base
  ([ADR-0052](0052-push-input-aliases-and-issue-mining-fixes.md),
  D1) **[code]**.
- **`COPY --from=<image>` on `/`** spools and checks every layer of the
  image before any is unpacked, as it unpacks them only when the
  instruction reads them **[code]**.
- **Scope.** Only `/`. Off `/` (a directory per stage, or another root) in
  a user namespace, no base layer is streamed: the blobs are pulled and
  every owner of every layer is read before any is extracted, as in overlay
  mode (the fix of the same review). Outside a user namespace every owner
  maps and nothing is checked.
- **Not decided here.** Owners of entries on ignored paths, which the
  extraction leaves out
  ([#209](https://github.com/tran4774/ranikust/issues/209)).

### Consequences

- Good, because each layer is downloaded once and spooled as before
  (ADR-0081): no second download and no disk beyond the one spool.
- Good, because the build still fails before the layer with the owner is
  extracted, naming the owners and the ways out.
- Bad, because a refused build leaves part of the image on the container's
  `/` until the container ends or the next build's first base empties it.
- Neutral, because overlay mode and kaniko mode off `/` refuse before any
  layer is extracted.

## Evidence

- **[code]** The spool pass (`Spooled::of`, `crates/rnk-snapshot/src/kaniko/root.rs`)
  checks a layer's owners against the namespace's map once its digest and
  diff ID are verified; the T0 test
  `on_slash_a_streamed_layer_with_an_owner_the_namespace_lacks_is_refused_by_the_spool_pass`
  shows the refusal and, as its control, the same layer passing outside a
  user namespace and in one that maps every id.
- **[inferred]** The costs of (b) and (c), from how the spool works: (b)
  downloads each layer twice, (c) holds the image's compressed size in the
  state directory. Neither was measured.

## More Information

- Refines ADR-0098 for kaniko mode on `/`; builds on ADR-0055 and ADR-0081.
- Design: [modes and detection](../design/modes-and-detection.md#owners-the-user-namespace-does-not-map),
  [kaniko mode](../design/kaniko-mode.md).
- Revisit if kaniko mode on `/` keeps blobs again, or if a build on `/` stops
  being in a disposable container.
