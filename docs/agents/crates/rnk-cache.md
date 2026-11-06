# rnk-cache

> **Status:** in progress · **Phase:** 4 ·
> **Depends on:** `rnk-oci`, `rnk-registry`; `rnk-rootfs` ([ADR-0115](../../adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md)) and
> `rnk-store` ([ADR-0021](../../adr/0021-rnk-store-crate.md)), planned ·
> **Used by:** `rnk-builder` (planned; its lookups started up front by
> `rnk-sched` through `StageRunner`, [ADR-0114](../../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md)) ·
> **Path:** `crates/rnk-cache/`

**TL;DR:** Computes a versioned, chained cache key per step, and looks up
and pushes the layer cached under it in a registry repository
(`RegistryCache`); the OCI layout cache is planned. A key depends on the
step's instruction, its inputs and the previous key only, never on the
snapshot mode, host, the build's time, an mtime outside `--reproducible` or
thread count, and it is not compatible with kaniko's cache. A lookup or push
that fails is a miss or a warning, never a failed build. Nothing calls the
crate yet: the CLI refuses `--cache`, `--cache-repo` and the related flags
until Phase 4 and accepts `--cache=false` (ADR-0036).

## Owns

- The cache key of each step, with a format version, and the tag of its
  entry for one layer encoding.
- Lookup and push of cached layers in a registry (`--cache-repo`, built) or
  an OCI layout (planned), and the form of an entry
  ([cache-and-store](../../design/cache-and-store.md#cache-entries)).

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
- Fail a build or change its image when the cache fails, or vouch for a
  hit: the caller verifies its blob and diff ID and extracts it as a base
  layer's.

## API

Built (keys, tags and the registry cache):

```rust
pub const KEY_VERSION: u32 = 1;      // bump on any change to what a key or a tag hashes, or how
pub struct CacheKey;                 // version + SHA-256; Copy, Eq, Hash; version(), hash()
impl CacheKey {
    pub fn root(base: Option<&Digest>, platform: &Platform) -> Self; // platform manifest digest; None: FROM scratch
    pub fn next(&self, instruction: &str, inputs: KeyInputs) -> Self; // instruction expanded, with its flags
}
pub struct EntryId;                  // new(CacheKey, Encoding); key(), encoding(); Copy, Eq, Hash
impl EntryId { pub fn tag(&self) -> String; } // "rnk-v<version>-<64 hex>"; the annotations and layer type come from it too
pub struct KeyInputs;                // new(&Encoding) opens rnk_cache.key; arg, env, user, workdir, shell, network, mount, copy_from, file
pub enum Network { Default, None, Host }               // the Dockerfile's --network, never what a mode gives
pub enum Mount { Secret { id, target, mode, uid, gid }, Cache { id, sharing, dropped } } // no secret value; given or not
pub enum Sharing { Shared, Private, Locked }
pub enum CopyFrom { Image(Digest), Stage(CacheKey) }   // platform manifest digest; source stage's final key (ADR-0114); either stands for its files
pub struct File { path: Vec<u8>, kind: FileKind, mode, uid, gid, capability: Option<Vec<u8>>, mtime: i64 } // mtime keyed when clamped
pub enum FileKind { Regular(Content), Directory, Symlink(Vec<u8>), Hardlink(Vec<u8>), Fifo, CharDevice { .. }, BlockDevice { .. } }
pub struct Content;                  // sha256(), size(); only from ContentHasher; == compares copied bytes with hashed ones
pub struct ContentHasher;            // update(chunk) or io::Write, then finish() -> Content
pub struct Encoding { compression: rnk_oci::Compression, level: i32, times: TimePolicy } // Display: the encoding annotation
pub enum TimePolicy { Kept, Clamped { epoch: i64 } }   // own mtimes; --reproducible's clamp

pub trait LayerCache {               // futures not Send: block_on on a thread of their own, as rnk-builder's uploads
    fn lookup(&self, entry: &EntryId) -> impl Future<Output = Lookup>;
    fn push(&self, entry: &EntryId, layer: &BuiltLayer) -> impl Future<Output = Pushed>;
}
pub enum Lookup { Hit(CachedLayer), Miss(Miss) }       // #[must_use]
pub enum Miss { Absent, Expired, Failed(Error) }       // Expired: no warning; Failed: a warning, never a failed build
pub struct CachedLayer { pub source: Source, pub blob: Descriptor, pub diff_id: Digest } // blob: the encoding's OCI type, digest and size only
#[non_exhaustive] pub enum Source { Registry(Reference) } // the entry's tagged reference, to pull the blob from
pub struct BuiltLayer { pub blob: Descriptor, pub diff_id: Digest, pub body: BlobBody, pub mount_from: Option<Reference> }
pub enum Pushed { Done(PushOutcome), Failed(Error) }   // #[must_use]; PushOutcome: the layer's, rnk_registry's
pub struct RegistryCache;            // new(Arc<Client>, repository, ttl) -> Result<_, Error>: no tag or digest; Clone
#[non_exhaustive] pub enum Error { Repository, Registry, Refused, Encoding, Document } // messages hold excerpts only
#[non_exhaustive] pub enum Refusal { NotAnImageManifest, Malformed, Layers, LayerType, ConfigType, NotAnImage, Annotation, DiffIds, Created }
pub const KEY_ANNOTATION: &str;      // "io.github.tran4774.ranikust.cache.key": the key's hash, 64 hex
pub const VERSION_ANNOTATION: &str;  // "io.github.tran4774.ranikust.cache.version": the key's version, decimal
pub const ENCODING_ANNOTATION: &str; // "io.github.tran4774.ranikust.cache.encoding": "gzip;level=6;times=kept"
```

Planned (tasks 3 and 11 of #123):

```rust
impl KeyInputs { pub fn heredoc(&mut self, name: &str, body: &[u8]) -> &mut Self; } // task 3 (#126): adds a field to the step hash, so KEY_VERSION is bumped then; its bytes count in bytes_hashed
pub struct LayoutCache;              // task 11: a LayerCache in an OCI layout, through rnk-rootfs (ADR-0116)
```

Never append heredoc bodies to the instruction string, because that would
shift a field boundary.

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
  value, duplicates kept; mounts stay in the `RUN`'s order.

| Hash | Fields in order |
|---|---|
| root | `"rnk-cache root"`; `KEY_VERSION` (`u32`); the base digest string (optional); the platform's architecture, os, `os.version` (optional), `os.features` (optional list), variant (optional), features (optional list) |
| step | `"rnk-cache step"`; `KEY_VERSION`; the previous key's version and hash; the instruction; the build arguments (name, optional value); the environment (name, value); `USER`; `WORKDIR`; `SHELL` (list); the network (optional name); the mounts (`secret`: id, target, mode, uid, gid; `cache`: id, sharing, a dropped byte); the copy source (optional: `image` and the digest string, or `stage` and the key's version and hash); the files (path; the type name, then the content's SHA-256 and size (`u64`), the link target, or the device's major and minor; mode, uid, gid; `security.capability` (optional); under `clamped` a 1 byte and the mtime clamped to the epoch (`i64`), else a 0 byte) |
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
- One `EntryId` gives the tag, the annotations a hit must carry and its
  layer type: a lookup never takes a layer of another encoding than its tag.
- A hit is an entry of [the entry form](../../design/cache-and-store.md#cache-entries)
  naming this key, version and encoding, younger than `--cache-ttl`;
  anything else under the tag is `Miss::Failed`. Every document read is
  bounded by the client's `--limit` values (ADR-0079).
- A push sends the manifest last, so a tag never names an entry whose
  blobs are missing.

## Refs

- **Design:** [`design/cache-and-store.md#cache-keys`](../../design/cache-and-store.md#cache-keys), [`#cache-entries`](../../design/cache-and-store.md#cache-entries), [`#ownership`](../../design/cache-and-store.md#ownership); [`design/scheduler.md#lazy-export`](../../design/scheduler.md#lazy-export) (`--cache` makes a layer needed); [threat model T11, T13](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0005](../../adr/0005-own-versioned-cache-key.md) (own versioned key), [ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md) (the fields, SHA-256, the tag and its encoding), [ADR-0114](../../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md) (`COPY --from=<stage>`; lookahead and skipping cached stages, not built yet), [ADR-0115](../../adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md) (crate edges), [ADR-0116](../../adr/0116-the-cache-lives-in-a-registry-or-an-oci-layout-of-one-layout-per-key.md) (registry or layout, no layout on `/`; the layout cache and the flags not built yet), [ADR-0057](../../adr/0057-direct-run-heredoc-from-memfd-and-refusals.md) (dropped cache mounts), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (cross-mode layers carry differences), [ADR-0006](../../adr/0006-persistent-store.md) and [ADR-0021](../../adr/0021-rnk-store-crate.md) (the local store is `rnk-store`, not this cache)
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
  shift, and that every encoding field changes the tag; it also pins the
  encoding annotation. `tests/registry/` runs `RegistryCache` against
  `rnk-registry`'s fake registry (compiled in through `#[path]`): a push
  then a lookup is a hit with the same digests; an unknown key is a miss
  after one request; a 500, a stall past `registry-idle` and bad credentials
  are `Miss::Failed`; an expired entry is `Miss::Expired`; another encoding
  misses, and an entry copied under its tag is refused; hostile entries (an
  index, a Docker manifest, zero or two layers, another layer or config
  type, an artifact, annotations of another key, version or encoding, two
  diff IDs, a bad `created`) and documents over `--limit` are refused; a
  held blob is not uploaded again and one in a destination is mounted; a
  failed push publishes nothing. `tests/spans.rs` checks the three spans.
- **Planned:** `LayoutCache` round trip in a temp dir; `--cache-repo`
  against a real registry (T1); W2 cold, warm and partial; a layer cached in
  one mode reused by the other, checked against the ADR-0012 list.

## Metrics

- `rnk_cache.key` (`files_hashed`, `bytes_hashed`): one per step, from
  `KeyInputs::new` to `CacheKey::next`; the regular files the key holds and
  their bytes.
- `rnk_cache.lookup` (`hit`): one per lookup, a miss of any kind included.
- `rnk_cache.push` (`bytes_out`): one per push; the layer and config bytes
  uploaded (not held or mounted) plus the manifest's
  ([rows](../../design/metrics.md#rnk-cache)). A push outlives its
  instruction and stage: the caller drives it inside `rnk_sched.run`'s span.

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
  `RUN`, `#!` or not, a `COPY` or an `ADD`) through a `KeyInputs` method
  that task 3 (#126) adds.
- Give `KeyInputs::new` and `EntryId::new` the same `Encoding`, or a
  `--reproducible` hit can carry other mtimes.
- A `RegistryCache` gets the build's `Client`: its registry needs a
  destination's options there (credentials over plain HTTP only if the
  operator named it, verified TLS only on `/`), or it gets none or too
  many.
- Warn on `Miss::Failed` and `Pushed::Failed`, never on `Miss::Expired`
  (kaniko's expired entry is a plain miss too), and never change the exit
  status for either.
- The `LayerCache` futures are not `Send` (the registry client's are not):
  `tokio::spawn` refuses them; drive them with `Handle::block_on`.
- No cache measurement exists: claim no speed-up before W2 runs.
