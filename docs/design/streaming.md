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
  pass, through a bounded channel (a few chunks of at most 64 KiB). The
  compressed digest is checked by the registry client at the end of the
  body, the diff ID by the decoder at the end of the tar.
- The pushed image takes its base layers from where they came from: a
  cross-repository mount on the same registry (ADR-0032, Q-014), else the
  layer streamed from the source registry to the destination, again
  through a bounded channel. Nothing of it is ever written locally.
- When an output needs the bytes (`--tar-path`, `--oci-layout-path`), the
  image's blobs are pulled into the build directory as before: those
  outputs are files the operator asked for, of the image's size anyway.

Overlay mode keeps extracted layers in the store for later builds, so the
blob is only needed by the push. It extracts while downloading as well,
the bytes teed into the blob file the store keeps: one pass instead of
download-then-extract, the same disk as before, no memory proportional to
the layer. [inferred: overlay's persistent store makes keeping the blob
cheap and saves the next build a download; dropping it is Phase 4's
choice with the cache, Q-162.]

### Built layers: pushed as soon as they are exported

A built layer that the target image needs is handed to an uploader when
its export finishes. The uploader pushes it to every destination (mount
from the first destination for the later ones on the same registry, as
before) while the next instructions run, with at most `--jobs` transfers
at once. The final push sends only the config and the manifest, after
waiting for the layer uploads. Once every destination holds a layer and
no output needs its bytes, its file is released: removed from the build
directory, or, for an anonymous file, its descriptor closed.

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
frames (zstd per 1 MiB frame, gzip members), 64 KiB to 256 KiB transfer
chunks, at most a few of them queued. Nothing scales with a layer's size;
the number of layers in flight is bounded by `--jobs`.

## Both modes

| Part | Overlay mode | Kaniko mode |
|---|---|---|
| Base layer extracted while downloading | yes, teed into the store's blob | yes, into the root, no blob kept |
| Base layer pushed from its source registry | no: the store has the blob | yes (mount, else streamed) |
| Built layer pushed while the build runs | yes | yes |
| Built layer released after its upload | yes | yes |

## Credentials and failures

- **Credentials are needed from the start.** The uploader pushes during
  the build, so destination credentials are resolved and checked at
  set-up, before the first stage, as the pulls' are. A destination that
  refuses the push fails the build at its first layer instead of at the
  end.
- **A failed build leaves blobs in the destination repository** that no
  manifest references: the manifest is pushed last, only after every
  blob. Registries collect such blobs (garbage collection of unreferenced
  blobs; upload sessions expire). No tag ever points at a partial image.
- **A base layer whose digest or diff ID does not match** fails the build.
  In kaniko mode its entries are already in the root when the check fails
  at the end of the stream; nothing runs on that root again in this build
  (the stage fails before its first instruction). The root is emptied
  before its next use: on a host at the end of the build, on `/` by the
  next build (ADR-0052 D1). A partial upload is an upload session the
  registry discards.
- **A dropped connection during a streamed extraction** cannot be retried
  in place (part of the layer is extracted): the build fails. The pull of
  manifests and configs, and the uploads from files, keep their retries.
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

## Open questions

- [Q-370](../open-questions.md#q-370-should-a-built-layer-be-encoded-straight-into-its-upload)
  Should a built layer be encoded straight into its upload?
