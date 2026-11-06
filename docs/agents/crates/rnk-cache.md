# rnk-cache

> **Status:** in progress · **Phase:** 4 ·
> **Depends on:** `rnk-oci`; `rnk-registry`, `rnk-rootfs` ([ADR-0115](../../adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md)) and
> `rnk-store` ([ADR-0021](../../adr/0021-rnk-store-crate.md)), planned ·
> **Used by:** `rnk-builder` (planned; its lookups started up front by
> `rnk-sched` through `StageRunner`, [ADR-0114](../../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)) ·
> **Path:** `crates/rnk-cache/`

**TL;DR:** Computes a versioned, chained cache key per step and the tag its
cached layer is looked up by; lookup and push in a registry repository or an
OCI layout are not built yet. A key depends on the step's instruction, its
inputs and the previous key only, never on the snapshot mode, host, the
build's time, an mtime outside `--reproducible` or thread count, and it is
not compatible with kaniko's cache. Nothing calls the crate yet: the CLI
refuses `--cache`, `--cache-repo` and the related flags until Phase 4 and
accepts `--cache=false` (ADR-0036).

## Owns

- The cache key of each step, with a format version, and the tag of its
  entry for one layer encoding.
- Lookup and push of cached layers in a registry (`--cache-repo`) or an OCI
  layout (planned).

## Must not

Boundaries: `crates/rnk-cache/src/lib.rs`. In short:

- Read or write kaniko's cache or mimic its keys (ADR-0005).
- Let a key depend on the snapshot mode, host, users, the build's time,
  an mtime outside `--reproducible` ([ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)), thread count or the
  order of an unordered collection, or hold a secret's value.
- Open files to compute a key: content arrives as bytes the caller read
  through `rnk-rootfs`. The layout cache reaches its directory only through
  `rnk-rootfs`, the registry only through `rnk-registry` ([ADR-0115](../../adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md)).
- Depend on `rnk-dockerfile`, `rnk-ignore` or `rnk-layer`: `rnk-builder`
  builds `KeyInputs` ([ADR-0115](../../adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md)).
- Transfer blobs (`rnk-registry`), encode layers (`rnk-layer`), own the local
  store (`rnk-store`) or choose what to export (`rnk-builder`).

## API

Built (keys and tags):

```rust
pub const KEY_VERSION: u32 = 2;      // bump on any change to what a key or a tag hashes, or how, or to the layer format
pub struct CacheKey;                 // version + SHA-256; Copy, Eq, Hash; version(), hash()
impl CacheKey {
    pub fn root(base: Option<&Digest>, platform: &Platform) -> Self; // platform manifest digest; None: FROM scratch
    pub fn next(&self, instruction: &str, inputs: KeyInputs) -> Self; // instruction expanded, with its flags
    pub fn tag(&self, encoding: &Encoding) -> String;                 // "rnk-v<version>-<64 hex>"
}
pub struct KeyInputs;                // new(&Encoding) opens rnk_cache.key; arg, env, user, workdir, shell, network, mount, copy_from, file, heredoc(name, body)
pub enum Network { Default, None, Host }               // the Dockerfile's --network, never what a mode gives
pub enum Mount { Secret { id, target, mode, uid, gid }, Cache { id, sharing, dropped } } // no secret value; given or not
pub enum Sharing { Shared, Private, Locked }
pub enum CopyFrom { Image(Digest), Stage(CacheKey) }   // platform manifest digest; source stage's final key (ADR-0114); either stands for its files
pub struct File { path: Vec<u8>, kind: FileKind, mode, uid, gid, capability: Option<Vec<u8>>, mtime: i64 } // mtime keyed when clamped; an archive ADD is one File
pub enum FileKind { Regular(Content), Directory, Symlink(Vec<u8>), Hardlink(Vec<u8>), Fifo, CharDevice { .. }, BlockDevice { .. } }
pub struct Content;                  // sha256(), size(); only from ContentHasher; == compares copied bytes with hashed ones
pub struct ContentHasher;            // update(chunk) or io::Write, then finish() -> Content
pub struct Encoding { compression: rnk_oci::Compression, level: i32, times: TimePolicy }
pub enum TimePolicy { Kept, Clamped { epoch: i64 } }   // own mtimes; --reproducible's clamp
```

Planned (tasks 4 and 11 of #123):

```rust
pub trait LayerCache {                               // [design] RegistryCache, LayoutCache
    async fn lookup(&self, key: &CacheKey, encoding: &Encoding) -> Result<Option<CachedLayer>>; // by tag, ADR-0113
    async fn push(&self, key: &CacheKey, encoding: &Encoding, layer: &Descriptor) -> Result<()>; // rnk_oci; the new layer's blob, read from the store; asynchronous
}
```

Never append heredoc bodies to the instruction string, because that would
shift a field boundary: pass each to `KeyInputs::heredoc`, after the
expansion BuildKit does (none for a `RUN`'s; the build variables for a
`COPY`'s or an `ADD`'s whose delimiter is not quoted).

An `ADD` of a local archive, which extracts it, is one `File`: the archive
and its bytes, never its members, as BuildKit keys the source's checksum and
decides at run time whether to extract it; the instruction says `ADD`.

### Byte form

The fields and the form of the tag are
[ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)'s,
and what each hash covers is
[cache-and-store](../../design/cache-and-store.md#cache-keys)'s; this is how
it is written. Each hash is SHA-256 over self-delimiting fields, so two
different inputs never feed the same bytes:

- a string or byte field: its length as a big-endian `u64`, then its bytes;
- a list: its count as a `u64`, then its items;
- an optional field: a byte, 0 when absent, else 1 and the field;
- integers big-endian (`u32`, `i32`, `i64`), a 32-byte hash as it is, an
  enum as its lower-case name (`gzip`, `secret`, `regular`);
- unordered collections (build arguments, environment, files) sorted by
  value, duplicates kept; mounts and heredocs stay in the instruction's
  order.

| Hash | Fields in order |
|---|---|
| root | `"rnk-cache root"`; `KEY_VERSION` (`u32`); the base digest string (optional); the platform's architecture, os, `os.version` (optional), `os.features` (optional list), variant (optional), features (optional list) |
| step | `"rnk-cache step"`; `KEY_VERSION`; the previous key's version and hash; the instruction; the build arguments (name, optional value); the environment (name, value); `USER`; `WORKDIR`; `SHELL` (list); the network (optional name); the mounts (`secret`: id, target, mode, uid, gid; `cache`: id, sharing, a dropped byte); the copy source (optional: `image` and the digest string, or `stage` and the key's version and hash); the files (path; the type name, then the content's SHA-256 and size (`u64`), the link target, or the device's major and minor; mode, uid, gid; `security.capability` (optional); under `clamped` a 1 byte and the mtime clamped to the epoch (`i64`), else a 0 byte); the heredocs (name, body) |
| tag | `"rnk-cache tag"`; the key's version and hash; the compression name, then for `gzip` and `zstd` the level (`i32`); `kept`, or `clamped` and the epoch (`i64`) |

The tag is `rnk-v<version>-` followed by the tag hash in lower-case hex.

## Invariants

- A new algorithm bumps `KEY_VERSION`; old entries are never matched again
  and never rewritten. The goldens fail on any change to the byte form.
- A change to the layer format bumps `KEY_VERSION` too (ADR-0005): an
  `Encoding` has no encoder version.
- Same inputs, same key, in either mode, with any `--jobs`, whatever the
  order of the build arguments, environment and files, and however file
  content is chunked; a layer cached by one mode may serve the other
  (ADR-0005, ADR-0012).
- A `type=secret` mount adds its id, target, mode and owner, never its
  content (T11), whether or not the build is given the secret, as
  BuildKit; cached layers are digest-checked when read (T13).
- `COPY`/`ADD` inputs are the files actually read, after `.dockerignore`;
  under `--reproducible` each one adds its mtime clamped to the epoch, as
  the layer's header carries it, so only the mtime of a file older than the
  epoch counts ([ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)); without it, no mtime.
- A tag carries the encoding, so a hit was encoded with this build's
  settings ([ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md)). One compressed blob serves both the image and the
  cache; pushes run asynchronously. Waiting for them before exit, as kaniko
  does, is [inferred].

## Refs

- **Design:** [`design/cache-and-store.md#cache-keys`](../../design/cache-and-store.md#cache-keys), [`#ownership`](../../design/cache-and-store.md#ownership); [`design/scheduler.md#lazy-export`](../../design/scheduler.md#lazy-export) (`--cache` makes a layer needed); [threat model T11, T13](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0005](../../adr/0005-own-versioned-cache-key.md) (own versioned key), [ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md) (the fields, SHA-256, the tag and its encoding), [ADR-0114](../../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md) (`COPY --from=<stage>`; lookahead and skipping cached stages, not built yet), [ADR-0115](../../adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md) (crate edges), [ADR-0116](../../adr/0116-the-cache-lives-in-a-registry-or-an-oci-layout-of-one-layout-per-key.md) (registry or layout, none on `/`; not built yet), [ADR-0057](../../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md) (dropped cache mounts), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (cross-mode layers carry differences), [ADR-0006](../../adr/0006-persistent-store.md) and [ADR-0021](../../adr/0021-rnk-store-crate.md) (the local store is `rnk-store`, not this cache)
- **Open:** [#37](https://github.com/tran4774/ranikust/issues/37) (signing cache entries)
- **Evidence:** none measured: workload W2 (cold, warm, partial cache) was never run; mode independence is argued in [survey 06](../../research/survey/06-overlay-and-parallelism.md) §6.
- **kaniko** (compare, do not copy): `kaniko@44aa7ddc0`
  `pkg/executor/build.go:322` `populateCompositeKey` (sorts the environment
  at `:328-330`); `pkg/executor/composite_cache.go:91` `Hash`, `:101`
  `AddPath`; `pkg/cache/cache.go:51` and `:141` `RetrieveLayer` (registry,
  layout); `pkg/executor/push.go:404` `pushLayerToCache`;
  `pkg/executor/build.go:959` `WaitCachePushes`.
- **External:** [distribution-spec](https://github.com/opencontainers/distribution-spec), [image layout](https://github.com/opencontainers/image-spec/blob/main/image-layout.md)

## Tests

- **T0** (`cargo test -p rnk-cache`): `tests/keys/golden.rs` pins keys and
  tags (a changed golden means: bump `KEY_VERSION`); `tests/keys/props.rs`
  shows that a key ignores the order of build arguments, environment and
  files, the chunking of content and any mtime the header does not carry,
  that changing any one field changes it, that field boundaries cannot
  shift, that every encoding field changes the tag, and that a key ignores
  the compression and its level (`a_key_ignores_the_compression_and_its_level`);
  `tests/spans.rs` checks `rnk_cache.key`. That an archive `ADD` is keyed
  as one file is `rnk-builder`'s test (`tests/key_plan.rs`).
- **Planned:** `LayoutCache` round trip in a temp dir; `--cache-repo`
  against a local registry; W2 cold, warm and partial; a layer cached in one
  mode reused by the other, checked against the ADR-0012 list.

## Metrics

- `rnk_cache.key` (`files_hashed`, `bytes_hashed`): one per step, from
  `KeyInputs::new` to `CacheKey::next`; the regular files the key holds and
  their bytes, plus the bytes of its heredoc bodies.
- Phase 4, planned: `rnk_cache.lookup` (`hit`), `rnk_cache.push`
  (`bytes_out`) ([rows](../../design/metrics.md#rnk-cache)). A push
  outlives its instruction and stage: parent it to `rnk_sched.run`.

## Pitfalls

- Environment order is undefined, so kaniko sorts it before hashing
  (`kaniko@44aa7ddc0 pkg/executor/build.go:328-330`); `KeyInputs` sorts
  build arguments, environment and files itself, but mounts keep their
  order.
- An entry given twice is hashed twice: pass what is in effect, once.
- Create the `KeyInputs` before hashing a step's files, or `rnk_cache.key`
  misses the hashing time.
- A mode in the key would block cross-mode reuse; with reuse, layers carry
  the other mode's hardlink and copy-up differences (ADR-0012).
- Users moving from kaniko start with a cold cache: decided, not a bug
  (ADR-0005).
- A secret in a key or a cache tag leaks through the registry (T11): give
  `KeyInputs::env` the stage's environment, never the variables a secret
  mount's `env=` adds to a `RUN`.
- A `File` path with a host or mode-specific prefix ties the key to the
  host or the mode, and one relative to each source operand lets two files
  share a path: paths are relative to the build context.
- The instruction holds no heredoc body: `rnk-builder` adds each one (of a
  `RUN`, `#!` or not, a `COPY` or an `ADD`) through `KeyInputs::heredoc`.
- Give `KeyInputs::new` and `CacheKey::tag` the same `Encoding`, or a
  `--reproducible` hit can carry other mtimes.
- No cache measurement exists: claim no speed-up before W2 runs.
