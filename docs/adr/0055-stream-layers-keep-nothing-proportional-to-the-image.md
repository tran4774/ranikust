---
status: accepted
date: 2026-11-01
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0055: Layers are streamed: base layers extracted while downloading, built layers pushed as soon as they are exported, nothing kept in proportion to the image

## Context and Problem Statement

A build held every base layer blob and every built layer until the push
at its end. In kaniko mode on `/` those are anonymous files (ADR-0051),
and on a container's overlayfs root anonymous files fall back to `memfd`
(Q-340): the whole compressed image sat in RAM, against the container's
memory limit, which the `RUN` commands need. The maintainer asked for
ranikust's own memory to be as small as possible, safely, and decided
both remedies put to them on 2026-10-31: "Làm 1 và 2 đi. Làm sao tối ưu
bộ nhớ nhất và an toàn là được" (do 1 and 2; whatever uses the least
memory and is safe). Remedy 2, disk-backed anonymous files created only
while no `RUN` is alive, is a separate change; this ADR is remedy 1:
keep nothing that need not be kept.

## Considered Options

- Keep as today: every blob until the push.
- Stream base layers into their extraction without keeping them, and push
  built layers while the build runs, releasing each once pushed.
- As above, and encode built layers straight into the upload request,
  without a file.

## Decision Outcome

Chosen: the second option ([design/streaming.md](../design/streaming.md)).

- Kaniko mode keeps no base layer blob: `prepare` streams each layer from
  the registry through the decoder into the root, checking the digest at
  the end of the body and the diff ID at the end of the tar. The pushed
  image's base layers are mounted from their source repository on the
  same registry, else streamed from the source registry to the
  destination.
- Overlay mode keeps the store's blob for later builds, on disk through
  fixed buffers; it still downloads, then extracts (extracting while
  teeing into the blob is left for later).
- Built layers of the target's `FROM` chain are pushed to every
  destination as soon as they are exported, at most `--jobs` at once and
  `--jobs` more waiting; the final push sends the config and manifest. A
  failed build cancels the uploads. A layer's file is released once every destination
  holds it and no output needs it.
- `--tar-path` and `--oci-layout-path` keep the blobs they need in the
  build directory until written.
- Every pipeline uses fixed-size buffers; nothing in memory scales with a
  layer's size.
- Destination credentials are needed from the start of the build; a
  refused push fails the stage at its next exported layer. A failed
  build leaves unreferenced blobs, which registries collect; the manifest
  is always pushed last.
- Encoding straight into the upload is left open (Q-370): it saves the
  one-layer file but loses the existence check before sending and the
  retry of a failed request.

### Consequences

- Good, because kaniko mode on `/` holds at most a few layers' worth of
  files at any time, and no base layer, instead of the whole image.
- Good, because uploads overlap the build and download overlaps
  extraction: a build with destinations ends sooner.
- Bad, because a streamed extraction cannot retry a connection dropped
  after the first byte of the body: the build fails where it used to retry
  the download (a request refused before any byte is still retried).
- Bad, because a streamed layer's entries are in the root before its
  digest is checked at the end of the body: a mismatch fails the stage
  with part of the layer extracted, where a kept blob was verified before
  any byte was used. A host root is emptied at the end of the build, but
  on `/` nothing empties the container's root (ADR-0052 D1). Hence the
  amendment below.
- Bad, because a cross-registry push of a base layer downloads it again
  from its source, where it used to be read from the store.
- Neutral, because digests do not change.

## Evidence

- [design/streaming.md#measurements](../design/streaming.md#measurements):
  in kaniko mode on `/` with a node base image, a stage's peak memory fell
  from 157–163 MiB to 49–54 MiB and the container's from 480–492 MiB to
  387–390 MiB; the `--reproducible` digest is unchanged. Measured again
  with the state on disk (ADR-0056) and the spool on `/`: stage peak
  162–170 → 52 MiB, and under a 128 MiB container limit the build passes
  3 times of 3 against 2 of 3 before.
- `oci-client` 0.18 verifies a blob's digest at the end of a pull
  (`src/client.rs:1352-1358`) and offers a streamed push with a known
  digest (`push_blob_stream`).

### Amendment, 2026-11-01: verify before extracting on `/`

The main session decided, under the maintainer's rule of the decision
(memory as small as possible, safely), after a host-safety review: in
kaniko mode on `/` each streamed base layer is spooled to a disk-backed
anonymous file of the store, its compressed digest and diff ID verified,
and only then extracted. Memory stays bounded and the disk holds one
layer at a time; nothing unverified ever reaches a root that nothing
empties. Overlay mode and host roots keep extracting while downloading:
their root is always emptied after a failure. Built with the store's
disk-backed anonymous files (ADR-0056); a `COPY --from=<image>` on `/` is
spooled and verified the same way before it is unpacked. The spool is the
compressed blob, capped by its descriptor's size: one read of the body
writes it and decompresses it to compute the diff ID while the source
checks the compressed digest; extraction decompresses the spool again.
Two decompressions cost CPU; the spool stays the download's size (on disk,
or in memory under the `memfd` fallback), and nothing unverified reaches
`/` even when the published diff ID is wrong.

## More Information

Complements ADR-0051 (state out of `RUN`'s reach) and Q-340; relates to
ADR-0032 and Q-014 (mounts) and to Phase 4's cache (Q-041, Q-162).
