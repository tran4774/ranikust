---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0029: zstd layers are written as independent 1 MiB frames

## Context and Problem Statement

Layer bytes, and therefore layer digests, must not depend on the number of
compression threads (`--jobs`, the machine's core count) or on how the tar
stream is chunked; otherwise the same build gives different digests on
different machines and `--reproducible` means nothing. libzstd's own
multithreaded compression is not deterministic across thread counts
(exp-05 Q3). The planned design fed libzstd "fixed 128 KiB chunks" through
its multithreading, which the same experiment showed still varies. How does
`rnk-layer` compress zstd layers in parallel and deterministically?

## Considered Options

- Independent zstd frames over fixed-size blocks of the tar, compressed in
  parallel and written in order.
- libzstd's multithreaded streaming with a fixed job size.
- Single-threaded zstd.

libzstd multithreading gave two different outputs over 20 runs at 32
threads even with fixed write sizes; single-threaded zstd is deterministic
but gives up the parallel speed that makes zstd worth offering.

## Decision Outcome

`rnk-layer` splits the uncompressed tar into 1 MiB blocks by offset,
compresses each block as one complete, independent zstd frame (content size
recorded, no checksum), and writes the frames in order; libzstd's own
multithreading is never used.

- The frame size (1 MiB) is part of the output format: changing it changes
  the digest of every zstd layer, so it needs a new ADR.
- An empty layer is one empty frame.
- The decoder accepts any number of frames, including a single-frame blob
  with a checksum from another tool, and refuses frames whose window needs
  more than 128 MiB.
- gzip keeps its own deterministic pigz-style format (128 KiB blocks, one
  member); both codecs share the block-parallel encoder.

### Consequences

- Good, because zstd layer bytes are identical for any thread count and any
  write pattern, as gzip's already are.
- Good, because every frame is a standard zstd frame: any zstd reader
  (containerd, BuildKit, the `zstd` CLI) decodes the concatenation.
- Bad, because independent frames compress slightly worse than one stream:
  about 0.4% at level 3.
- Neutral, because ranikust's zstd layers differ byte for byte from other
  builders' zstd layers of the same content; digests were never comparable
  across builders anyway.

## Evidence

- [measured] [Experiment 05](../research/experiments/05-rust-ecosystem.md)
  Q3: libzstd multithreading differs between 0 and ≥1 workers, and gave two
  results over 20 runs at 32 workers with 1 GiB or 1 MiB writes.
- [measured] `crates/rnk-layer` tests: identical bytes for 1 to 16 threads
  and random chunkings; a pinned sha256 checked with the zstd 1.5.2 CLI;
  each frame equals a fresh one-shot compression of its block.
- [measured] Size cost on 236 MB of compiled Rust libraries at level 3:
  66.48 MB as 1 MiB frames against 66.20 MB as one single-threaded stream.

## More Information

- Implemented in `crates/rnk-layer/src/zstd_frames.rs` and the shared
  `src/parallel.rs` (branch `feat/zigbuild-zstd`, merged 2026-10-30); the
  C library is built through cargo-zigbuild
  ([ADR-0020](0020-static-musl-via-cargo-zigbuild.md), Q-052).
- Related: [ADR-0012](0012-accept-divergence-between-modes.md) and the
  determinism invariant in `ARCHITECTURE.md`.
