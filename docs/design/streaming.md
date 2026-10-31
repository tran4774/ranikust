# Streaming layers: what ranikust keeps, and for how long

**TL;DR:** ranikust's own memory and disk stay small and independent of
the image's size. A base layer that nothing but its extraction needs is
extracted while it downloads and never kept. A built layer goes to its
registry destinations as soon as it is exported, while the build goes on,
and its bytes are released once every destination has it. Every pipeline
works through fixed-size buffers. Decided in
[ADR-0055](../adr/0055-stream-layers-keep-nothing-proportional-to-the-image.md).

## Where the bytes were

Before this design, per build:

| What | Kept until | Where |
|---|---|---|
| Base layer blobs | the end of the build (the push reads them) | the store (`blobs/`); in kaniko mode on `/`, anonymous files, which fall back to `memfd` on a container's overlayfs root (Q-340): RAM |
| Extracted base layers | the store's prune (overlay mode) | `layers/` |
| Built layer blobs | the end of the build | the build directory; on `/`, anonymous files (RAM under `memfd`) |
| Push | — | streamed from those files, 256 KiB chunks |

So a kaniko-mode build on `/` held the whole compressed base image plus
every built layer in memory until the push.

## What changes

### Base layers: extract while downloading, keep nothing

Kaniko mode extracts a base image into the root when a stage is prepared
on it, and needs its blob for nothing else unless an output asks for the
bytes. So in kaniko mode:

- The pull of an image reads the manifest and config only. Each base layer
  becomes a reference to the registry (repository, descriptor), not a file.
- `prepare(Image)` streams each layer from the registry through the
  decoder into `extract_live`: download → decompress → extract in one
  pass, through an in-memory pipe of 256 KiB (`rnk_registry::PIPE_BYTES`);
  on `/` the compressed blob goes to a disk-backed anonymous file first,
  and is extracted once verified (see Credentials and failures).
  The compressed digest is checked by the registry client at the end of
  the body, the diff ID by the decoder at the end of the tar. The pull
  runs on a thread owned by the reader: a reader dropped early stops the
  pull and waits for its thread, and a reader that failed fails on every
  later read.
- The pushed image takes its base layers from where they came from: a
  cross-repository mount on the same registry (ADR-0032, Q-014), else the
  layer streamed from the source registry to the destination, again
  through a bounded channel. Nothing of it is ever written locally.
- When an output needs the bytes (`--tar-path`, `--oci-layout-path`), the
  image's blobs are pulled into the build directory as before: those
  outputs are files the operator asked for, of the image's size anyway.

Overlay mode keeps extracted layers in the store for later builds, and
keeps the blob beside them: on disk, through fixed buffers, never in
memory. It still downloads and then extracts; extracting while
downloading, teeing into the blob file, would save the second read of the
blob and is left for later. Dropping the blob of a persistent store is
Phase 4's choice with the cache (Q-162).

### Built layers: pushed as soon as they are exported

A built layer that the target image needs is handed to an uploader when
its export finishes. The uploader pushes it to every destination (mount
from the first destination for the later ones on the same registry, as
before) while the next instructions run, with at most `--jobs` transfers
at once, and at most `--jobs` more waiting: a stage that exports faster
than the registry takes the layers waits for room instead of piling up
files. The final push sends only the config and the manifest, after
waiting for the layer uploads. Once every destination holds a layer and
no output needs its bytes, its file is released: removed from the build
directory, or, for an anonymous file, released through its handle
(`rnk_builder::AnonBlob`), shared by every copy of the layer and the only
way to open it, so a released file's descriptor number, reused by a later
file, is never read as the layer. Only the
target's `FROM` chain is pushed: a stage that a later stage is built
`FROM` in kaniko mode keeps its layers for that stage, and a stage the
target only copies from never reaches a destination. A failed build
cancels the uploads (running ones are dropped, waiting ones never start)
and releases their files.

The layer is still encoded to a file before it is sent, rather than
encoded straight into the request: its digest must be known for the
existence check (a layer the registry has is not sent again), for a retry
after a failed request (a stream cannot be replayed), and for the
manifest. The file lives one layer's upload long, not the build's.
Encoding into the request itself (`push_blob_stream_chunked`, the digest
computed while sending) is [open: Q-370].

### Bounded memory

Decompression, extraction, encoding and the transfers each work through
fixed buffers: the decoder's window, the tar writer's blocks, the encoder's
frames (zstd per 1 MiB frame, gzip members), 256 KiB transfer chunks and
pipes, at most a few of them queued. Nothing scales with a layer's size;
the number of layers uploading or waiting to is bounded by twice
`--jobs`.

## Both modes

| Part | Overlay mode | Kaniko mode |
|---|---|---|
| Base layer extracted while downloading | no: downloaded to the store, then extracted | yes, into the root, no blob kept |
| Base layer pushed from its source registry | no: the store has the blob | yes (mount, else streamed) |
| Built layer pushed while the build runs | yes | yes |
| Built layer released after its upload | yes | yes |

## Credentials and failures

- **Credentials are needed from the start.** The uploader pushes during
  the build, so destination credentials are resolved at set-up, with the
  pulls'. ranikust makes no write check before the build (kaniko's
  `--skip-push-permission-check` is accepted and changes nothing): once
  a destination has refused a layer, the next layer the stage exports
  fails the stage with the registry's answer, and a build with no more
  layers fails at its final push; never only at the end of a long build.
- **A failed build leaves blobs in the destination repository** that no
  manifest references: the manifest is pushed last, only after every
  blob. Registries collect such blobs (garbage collection of unreferenced
  blobs; upload sessions expire). No tag ever points at a partial image.
- **A base layer whose digest or diff ID does not match** fails the build.
  On `/`, which nothing empties (ADR-0052 D1), none of its entries ever
  reaches the root: each streamed layer's compressed blob is first spooled
  into a disk-backed anonymous file of the store (ADR-0056), capped by
  its descriptor's size, while one decompression of the same bytes
  computes the diff ID; the size and compressed digest are checked at the
  end of the body. Only then is the spool decompressed a second time into
  the root (the diff ID checked again). Decompressing twice costs CPU but
  keeps the spool, on disk or under the `memfd` fallback, to the
  download's size, and keeps the rule that nothing unverified reaches `/`
  even when the publisher's diff ID is wrong. The spool goes with its
  file, so the disk holds one compressed layer at a time. A
  `COPY --from=<image>` on `/` is spooled and verified the same way before
  any directory is unpacked for it, and its verified compressed blobs kept
  for the stage's later readers. On a host root, and in
  a fresh keep directory, a layer is extracted as it arrives: a mismatch
  fails the stage with part of the layer there, nothing runs on it again,
  and the root is emptied at the end of the build (a later stage's root
  holds none of those entries). A partial upload is an upload session the
  registry discards.
- **A dropped connection during a streamed extraction** cannot be retried
  in place (part of the layer is extracted): the build fails at once,
  with the connection's error, without waiting for a retry. A request
  refused or broken before the first byte of the body (a 503, a reset
  connection) is retried as before, since nothing was read yet. The pull
  of manifests and configs, and the uploads from files, keep their
  retries.
- **Nothing falls back** to keeping a whole layer in memory.

## Outputs and reproducibility

- `--no-push` without outputs: built layers are not even encoded (the
  layer demand, unchanged); base layers are streamed into the root only.
- `--tar-path` and `--oci-layout-path`: the image's blobs are kept in the
  build directory until the outputs are written, as before.
- Digests do not change: the same layers are encoded with the same
  options, and pushed bytes are the encoded file's. `--reproducible` builds
  give the same digests as before this design.

## Cache (Phase 4)

The cache reuses layers by cache key (Q-041) and publishes built blobs in
the persistent store (Q-162). With this design a cache hit needs no local
blob either: the cached layer is mounted or streamed from the cache
repository, as a base layer is. Publishing built blobs in the store for
later builds is a store-backed choice of overlay mode and host builds; on
`/` the state never outlives the process (ADR-0051).

## Measurements

2026-11-01, rootless podman on the development machine (kernel 7.0), a
default container (`alpine:3.20`), kaniko mode on `/` with the state in
`memfd` (the container's overlayfs root refuses `O_TMPFILE`, Q-340),
`FROM node:22-bookworm-slim` (via `mirror.gcr.io`) plus one `RUN`,
`--no-push`; three rounds each, main at `76bd634` against this design, both
binaries patched alike for the missing build directory of an anonymous
store (see the branch report). Figures from `--metrics-file`:

| | before | after |
|---|---:|---:|
| Stage peak memory (`rnk_sched.stage.max_memory_bytes`) | 157–163 MiB | 49–54 MiB |
| Container peak (`memory.peak`: ranikust, `RUN`, page cache, `memfd`) | 480–492 MiB | 387–390 MiB |
| ranikust's own max RSS (`rusage_self`) | 143–150 MiB | 122–126 MiB |

The stage peak falls by the compressed base image (about 100 MiB): its
blobs no longer sit in `memfd`. The `--reproducible` digest is the same
before and after.

Again after ADR-0056 put the state on disk (named files unlinked at once
on the container's overlayfs), main at `38f1dfd` against this design with
the spool on `/`, same build, no patch:

| | before | after |
|---|---:|---:|
| Stage peak memory | 157–170 MiB | 49–52 MiB |
| Container peak (`memory.peak`, no limit) | 481–485 MiB | 385–389 MiB |
| ranikust's own max RSS | 140–147 MiB | 123–124 MiB |
| `RUN`'s max RSS | 114–122 MiB | 98–100 MiB |
| Under `--memory 128m --memory-swap 128m` | 3 builds of 6 | 6 of 6 |
| Under `--memory 112m` or `96m` | killed | killed |

The spool is the compressed blob (a first version spooled the
decompressed tar: its container peak was 466–472 MiB). With the state on
disk, much of the container's peak is page cache (the downloaded blobs
before, the compressed spool and the extracted root after), which the
kernel reclaims under a limit; under a limit the build now passes where
it failed half the time. Below about 120 MiB the floor is ranikust's own heap and the `RUN`
(node), neither of which grows with the image. The `--reproducible`
digest is again the same.

## Open questions

- [Q-370](../open-questions.md#q-370-should-a-built-layer-be-encoded-straight-into-its-upload)
  Should a built layer be encoded straight into its upload?
