# rnk-oci

> **Status:** skeleton · **Phase:** 1 ·
> **Depends on:** — (`oci-spec`) · **Used by:** `rnk-registry`, `rnk-layer`, `rnk-cache`, `rnk-builder`, `rnk-store` (`Digest`) · **Path:** `crates/rnk-oci/`

**TL;DR:** OCI and Docker manifests, indexes, image configs and media types,
on `oci-spec`. A digest covers exact bytes: keep pulled manifests as raw
bytes, and serialize what ranikust writes with sorted keys.

## Owns

- Manifests, image configs and media types of the OCI and Docker image
  formats, built on `oci-spec`, and their (de)serialization.
- Canonical JSON, with sorted keys, of every config and manifest ranikust
  writes: `oci-spec` 0.10 writes some maps from `HashMap`s.

## Must not

- Access the network or the filesystem: types and (de)serialization only.
- Encode or compress layers (`rnk-layer`), move blobs (`rnk-registry`), or
  write a Docker tar or OCI layout to disk (`rnk-builder`).
- Read the clock: `created` and history timestamps are inputs.

## API (planned)

```rust
pub use oci_spec::image::{Descriptor, ImageConfiguration, ImageIndex, ImageManifest, MediaType, Platform}; // [design]
pub use oci_spec::distribution::Reference;     // [design] the type oci-client re-exports
pub struct Digest(String);                      // [design] "sha256:<64 hex>", validated on parse
pub struct RawManifest { pub bytes: Vec<u8>, pub digest: Digest, pub media_type: MediaType } // [design] never re-serialized
pub enum ImageFormat { Oci, Docker }            // [open: Q-006] default: base image's format (kaniko) or OCI
pub enum Compression { Gzip, Zstd, None }
pub fn layer_media_type(f: ImageFormat, c: Compression) -> Result<MediaType, Error>; // Docker + zstd is an error
pub fn to_canonical_json<T: Serialize>(v: &T) -> Result<Vec<u8>, Error>; // [design] owned here; sorted keys, as Go writes them
pub fn select_platform<'a>(i: &'a ImageIndex, p: &Platform) -> Option<&'a Descriptor>; // [design]
pub struct ConfigEdit;   // [open: Q-007] ENV, LABEL, USER, … and history: here or in rnk-builder
// [open: Q-007] docker-save `manifest.json` and OCI layout `index.json`/`oci-layout` types: here or in rnk-builder
```

## Invariants

- Pure: no I/O, no clock, no randomness.
- Same value, same bytes: map keys sorted; no `HashMap` order in any output.
- A manifest never mixes OCI and Docker media types.
- Parsing untrusted manifests and configs never panics.

## Refs

- **Design:** [ARCHITECTURE.md#data-flow-of-a-build](../../../ARCHITECTURE.md#data-flow-of-a-build) (step 8), [#determinism](../../../ARCHITECTURE.md#determinism)
- **Decisions:** [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (`--compression`, `--reproducible`, outputs), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (digests may differ between modes), [ADR-0009](../../adr/0009-x86-64-first-aarch64-ready.md) (platforms)
- **Evidence:** [exp-05](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180): `push_manifest_raw` keeps the
  bytes, so the digest is stable; `oci-spec` 0.10 is actively maintained (Q7).
- **kaniko:** `kaniko@44aa7ddc0 cmd/executor/cmd/root.go:177-188` (zstd needs
  OCI), `:358` (`--image-format`), `:113-118` (default platform);
  `pkg/executor/build.go:1225-1268` (format conversion), `:2099-2124`
  (`created`, `--reproducible`); `pkg/image/transform.go:131-148` (no mixing)
- **External:** OCI image-spec v1.1.1 [manifest.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md), [config.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/config.md), [media-types.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/media-types.md), [image-layout.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-layout.md); [Docker schema 2](https://github.com/distribution/distribution/blob/v2.8.3/docs/spec/manifest-v2-2.md); oci-spec-rs v0.10.0 [config.rs](https://github.com/youki-dev/oci-spec-rs/blob/v0.10.0/src/image/config.rs#L394)

## Tests

- T0 round trip: manifests, indexes and configs of real images (both
  formats) parse and serialize back to identical bytes; canonical output is
  byte-identical over 100 runs (catches `HashMap` order).
- T0 tables: media type per format and compression (Docker + zstd fails),
  hostile digest strings, platform selection; proptest on canonical output.
- Config fields (`Env`, `Entrypoint`, `Cmd`, `WorkingDir`, `User`, `Labels`)
  compared with kaniko's in the image-content suite (T1/T2).

## Metrics

- None: pure data. Serializing configs and manifests is charged to
  `rnk_builder.assemble` ([rows](../../design/metrics.md#rnk-oci)).
- No `tracing` dependency needed.

## Pitfalls

- `oci-spec` 0.10.0 serializes labels, annotations, `ExposedPorts` and
  `Volumes` from `HashMap`s outside its own tests ([config.rs](https://github.com/youki-dev/oci-spec-rs/blob/v0.10.0/src/image/config.rs#L394)):
  key order, and with it the config digest, changes between runs.
- Re-serializing a pulled manifest changes its digest; push the raw bytes
  ([exp-05](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180)).
- Docker schema 2 has no zstd layer type: zstd needs OCI; kaniko rejects
  zstd with `--image-format=docker` or `--tar-path` (`root.go:177-188`).
- kaniko's `--image-format` defaults to the base's format (`root.go:358`)
  [open: Q-006]; it converts layer media types when formats differ and
  asserts no mixing (`build.go:1237-1268`, `transform.go:141-148`).
- Config `diff_ids` hash uncompressed tars; manifest layer digests hash the
  compressed blobs. Swapping them gives a valid-looking, broken image.
- kaniko stamps the pushed stage's `created` with the wall clock
  (`build.go:2100`); under `--reproducible` every timestamp must be fixed.
- A base reference may be an index: pick the platform (kaniko
  `--custom-platform`, default host, `root.go:113-118`).

## Phase 1

- All of it: the MVP pushes to a registry and writes Docker tars and OCI
  layouts with gzip or zstd and `--reproducible` ([survey 04 §2](../../research/survey/04-risks-and-roadmap.md#2-mvp-and-10-scope)).
- Order: `Digest` and media types → raw manifest, index and config parsing,
  platform selection → canonical serialization and config edits → tar and
  layout index types.
- Nothing deferred; multi-platform output is out of scope ([ADR-0008](../../adr/0008-out-of-scope-bake-warmer-cloud-contexts.md)).
