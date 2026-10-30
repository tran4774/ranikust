# rnk-oci

> **Status:** done for Phase 1 (open: Q-007, Q-130 to Q-132) · **Phase:** 1 ·
> **Depends on:** — (`serde`, `serde_json`, `thiserror`) · **Used by:** `rnk-registry`, `rnk-layer`, `rnk-cache`, `rnk-builder`, `rnk-store` (`Digest`) · **Path:** `crates/rnk-oci/`

**TL;DR:** OCI and Docker manifests, indexes, image configs and media types,
owned here rather than taken from `oci-spec`. A digest covers exact bytes:
keep pulled manifests as raw bytes, and serialize what ranikust writes with
sorted keys.

## Owns

- Digests, media types, descriptors, manifests, indexes and image configs of
  the OCI and Docker v2s2 formats, and their (de)serialization.
- Image references, normalized as docker/distribution does.
- Canonical JSON, with sorted keys, of every config and manifest ranikust
  writes.

## Must not

- Access the network or the filesystem: types and (de)serialization only.
- Encode or compress layers (`rnk-layer`), move blobs (`rnk-registry`), or
  write a Docker tar or OCI layout to disk (`rnk-builder`).
- Read the clock: `created` and history timestamps are inputs.

## API

Implemented (optional properties are `Option`s: absent or `null` is `None`,
a present value is written back even if empty; maps are `BTreeMap`s):

```rust
pub struct Digest;  // "sha256:<64 hex>" | "sha512:<128 hex>", validated; Ord = string order; serde as string
    // parse, from_parts, from_sha256_bytes(&[u8; 32]), from_sha512_bytes, algorithm, encoded, as_str
pub enum DigestAlgorithm { Sha256, Sha512 }
pub struct MediaType; // any string; consts OCI_* / DOCKER_*: manifest, index, config, layers, foreign layers, schema 1
    // format() by vendor prefix, is_manifest/is_index/is_config/is_layer/is_foreign_layer, layer_compression, to_format
pub enum ImageFormat { Oci, Docker } // no Default (Q-006, ADR-0032): kaniko follows the base image's format; or OCI
    // manifest_media_type, index_media_type, config_media_type, layer_media_type(Compression) (Docker + zstd: Err)
pub enum Compression { Uncompressed, Gzip, Zstd }
pub struct Descriptor; pub struct Platform;     // Platform keeps "os.version" / "os.features"
pub struct ImageManifest; pub struct ImageIndex; // one type each for both formats
    // new(ImageFormat, ..), from_json (schemaVersion 2, own media type), format() (Err if OCI and Docker mix)
pub struct ImageConfig;     // created, author, architecture, os, os.version, os.features, variant, config, rootfs, history,
                            // container, docker_version; from_json, platform()
pub struct ContainerConfig; // User, ExposedPorts, Env, Entrypoint, Cmd, Volumes, WorkingDir, Labels, StopSignal,
                            // Healthcheck, OnBuild, Shell, ArgsEscaped, legacy Docker fields; ports and volumes as BTreeSets
pub struct HealthConfig; pub struct RootFs; pub struct History;
pub enum Error;             // one enum for the crate; untrusted input quoted as an 80-char excerpt
pub fn to_canonical_json<T: Serialize + ?Sized>(v: &T) -> Result<Vec<u8>, Error>; // bytes to digest and write
pub struct Reference; // parse (docker/distribution ParseNormalizedNamed), registry, repository, tag, digest, name,
                      // with_default_tag ("latest"), with_tag, with_digest; Display "registry/repo[:tag][@digest]"
```

Canonical JSON is OLPC's, as OCI [considerations.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/considerations.md#json)
asks: every object's keys sorted by UTF-8 bytes, struct fields included; no
whitespace; integers only; `serde_json`'s string escaping.

Config edits and output index files (pure; (Q-007) here rather than
in `rnk-builder`):

```rust
pub enum CommandLine { Exec(Vec<String>), Shell(String) }   // CMD/ENTRYPOINT; shell form uses config Shell or DEFAULT_SHELL
pub enum ConfigEdit { Env(Vec<(String, String)>), Label(..), User(String), Workdir(String), Expose(Vec<String>),
    Volume(Vec<String>), Entrypoint(CommandLine), Cmd(CommandLine), Shell(Vec<String>), StopSignal(String),
    Healthcheck(HealthConfig), OnBuild(String) }       // values already expanded
impl ConfigEdit { pub fn apply(&self, c: &mut ImageConfig) -> Result<(), Error>; pub fn instruction(&self) -> &'static str }
impl ImageConfig {
    pub fn push_layer(&mut self, diff_id: Digest, h: History);  // diff_ids + history, empty_layer unset
    pub fn push_history(&mut self, h: History);                 // empty_layer: true
}
pub struct DockerTarImage { config, repo_tags, layers, layer_sources } // one entry of manifest.json (a JSON array)
    // config_path(&Digest) = "sha256:<hex>", layer_path(&Digest, Compression) = "<hex>.tar.gz|.tar.zst|.tar"
pub struct OciLayout { image_layout_version }          // `oci-layout`; FILE, INDEX_FILE, VERSION, REF_NAME_ANNOTATION, blob_path
```

`apply` follows kaniko's `ExecuteCommand`s (`pkg/commands/*.go`): an
empty `ENV`, `LABEL`, `EXPOSE`, `VOLUME` or `SHELL` list is refused; `ENV`
replaces a key in place or appends, rewriting every entry as `key=value`;
`USER` keeps two `:`-separated parts; `HEALTHCHECK` zeros become `None`;
`WORKDIR` keeps an absolute path as written and joins and cleans a relative
one; `EXPOSE` adds `/tcp` and accepts only `tcp` and `udp`; `STOPSIGNAL` is
checked as moby's `ParseSignal` and stored as written; `CMD` drops
`ArgsEscaped`; an empty exec form clears the field. Where BuildKit differs
(`ENTRYPOINT` resetting an inherited `CMD`, `sctp` and port ranges,
`--start-interval`): (Q-130, ADR-0032). Which instructions get a history entry:
(Q-131, ADR-0032). On error the config is unchanged. Creating the `WORKDIR`
and `VOLUME` directories is `rnk-builder`'s, through `RootPath`.

`RawManifest` and `select_platform` live in `rnk-registry` (`client.rs`,
`platform.rs`): platform matching is kaniko's pull rule, not a property of
the format.

## Invariants

- Pure: no I/O, no clock, no randomness.
- Same value, same bytes: map keys sorted; no `HashMap` order in any output.
- A manifest never mixes OCI and Docker media types.
- Parsing untrusted manifests, configs, digests and references never panics.

## Refs

- **Design:** [ARCHITECTURE.md#data-flow-of-a-build](../../../ARCHITECTURE.md#data-flow-of-a-build) (step 8), [#determinism](../../../ARCHITECTURE.md#determinism)
- **Decisions:** [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (`--compression`, `--reproducible`, outputs), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (digests may differ between modes), [ADR-0009](../../adr/0009-x86-64-first-aarch64-ready.md) (platforms)
- **Evidence:** [exp-05](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180): `push_manifest_raw` keeps the
  bytes, so the digest is stable.
- **kaniko:** `kaniko@44aa7ddc0 cmd/executor/cmd/root.go:177-188` (zstd needs
  OCI), `:358` (`--image-format`), `:113-118` (default platform);
  `pkg/executor/build.go:1225-1268` (format conversion), `:2099-2124`
  (`created`, `--reproducible`); `pkg/image/transform.go:131-148` (no mixing)
- **External:** OCI image-spec v1.1.1 [manifest.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md), [config.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/config.md), [media-types.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/media-types.md), [descriptor.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/descriptor.md#digests), [image-layout.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/image-layout.md); [Docker schema 2](https://github.com/distribution/distribution/blob/v2.8.3/docs/spec/manifest-v2-2.md); oci-spec-rs v0.10.0 [config.rs](https://github.com/youki-dev/oci-spec-rs/blob/v0.10.0/src/image/config.rs#L394)

## Tests

- T0 next to the code: hostile digests and references (no panic, short
  messages); media types per format and compression, OCI matrix conversion;
  malformed descriptors, manifests and configs; distribution v0.6.0's
  reference test vectors, adapted to normalized parsing.
- T0 fixtures in `testdata/` (Docker v2s2 manifest, list, config; OCI
  manifest, artifact manifest, index, config) serialize back to the same
  JSON value. Canonical JSON: golden bytes, `HashMap` order never shows over
  100 runs, proptest on insertion order, any JSON value, random documents.
- T0: every `ConfigEdit` against kaniko's rules (ENV order, WORKDIR joins,
  EXPOSE and STOPSIGNAL validation, shell form), history and `diff_ids`
  appends in canonical bytes; `manifest.json` and layout files as
  go-containerregistry and the image-spec write them.
- Planned: config fields compared with kaniko's and BuildKit's (T1/T2, with
  `rnk-builder`).

## Metrics

- None: pure data. Serializing configs and manifests is charged to
  `rnk_builder.assemble` ([rows](../../design/metrics.md#rnk-oci)).
- No `tracing` dependency needed.

## Pitfalls

- `oci-spec` 0.10.0 writes labels, annotations, `ExposedPorts` and
  `Volumes` from `HashMap`s ([config.rs](https://github.com/youki-dev/oci-spec-rs/blob/v0.10.0/src/image/config.rs#L394)),
  drops `Healthcheck`, `OnBuild` and `Shell`, and writes `os_version` for
  `os.version`: hence the types are owned here.
- Only `sha256` and `sha512` digests parse; `sha384` (which go-digest
  accepts) is rejected: ranikust cannot verify it.
- `Reference` normalizes Docker Hub to `docker.io` (distribution);
  go-containerregistry, and so kaniko, says `index.docker.io`. It is not
  `oci_client::Reference`: `rnk-registry` converts through the string.
- Re-serializing a pulled manifest changes its digest; push the raw bytes
  ([exp-05](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180)).
  Parsing drops unknown properties (Docker's `container_config` too, as
  go-containerregistry does).
- `from_json` accepts mixed media types so a base image can still be read;
  check `format()` before writing.
- Docker schema 2 has no zstd layer type: zstd needs OCI; kaniko rejects
  zstd with `--image-format=docker` or `--tar-path` (`root.go:177-188`).
- kaniko's `--image-format` defaults to the base's format (`root.go:358`)
  (Q-006, ADR-0032), converts layer media types and asserts no mixing
  (`build.go:1237-1268`, `transform.go:141-148`); its table maps OCI zstd to
  Docker gzip and Docker foreign gzip to uncompressed OCI, mislabeling the
  blob. `MediaType::to_format` follows the OCI matrix instead.
- Config `diff_ids` hash uncompressed tars; manifest layer digests hash the
  compressed blobs. Swapping them gives a valid-looking, broken image.
- Digest only `to_canonical_json` output, never `serde_json::to_vec` (Rust
  field order). Go escapes `<`, `>`, `&` as `\u003c`…; canonical output
  keeps them raw, so such a config differs from kaniko's byte for byte.
- kaniko stamps the pushed stage's `created` with the wall clock
  (`build.go:2100`); under `--reproducible` every timestamp must be fixed.
- A base reference may be an index: pick the platform (kaniko
  `--custom-platform`, default host, `root.go:113-118`).

## Phase 1

- All of it: the MVP pushes to a registry and writes Docker tars and OCI
  layouts with gzip or zstd and `--reproducible` ([survey 04 §2](../../research/survey/04-risks-and-roadmap.md#2-mvp-and-10-scope)).
- Order: `Digest` and media types → raw manifest, index and config parsing,
  platform selection → canonical serialization and config edits → tar and
  layout index types. All built (`RawManifest` and platform selection in
  `rnk-registry`); left: the comparison with kaniko's and BuildKit's configs,
  which needs `rnk-builder`, and Q-130 to Q-132.
- Nothing deferred; multi-platform output is out of scope ([ADR-0008](../../adr/0008-out-of-scope-bake-warmer-cloud-contexts.md)).
