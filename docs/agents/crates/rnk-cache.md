# rnk-cache

> **Status:** skeleton · **Phase:** 4 ·
> **Depends on:** `rnk-registry`, `rnk-oci`, `rnk-rootfs` (ADR-0115) and
> `rnk-store` (ADR-0021), all planned: the crate declares none yet ·
> **Used by:** `rnk-builder` (its lookups started up front by `rnk-sched`
> through `StageRunner`) ·
> **Path:** `crates/rnk-cache/`

**TL;DR:** Computes a versioned, chained cache key per instruction and looks
up and pushes cached layers in a registry repository or an OCI layout. A key
depends on the instruction, the context it reads and the previous key only,
never on the snapshot mode, host, time or thread count, and it is not
compatible with kaniko's cache. The crate is a skeleton (docs, no code): the
CLI refuses `--cache`, `--cache-repo` and the related flags until Phase 4 and
accepts `--cache=false` (ADR-0036).

## Owns

- The cache key of each instruction, with a format version.
- Lookup and push of cached layers in a registry (`--cache-repo`) or an OCI
  layout.

## Must not

Boundaries: `crates/rnk-cache/src/lib.rs`. In short:

- Read or write kaniko's cache or mimic its keys (ADR-0005).
- Let a key depend on the snapshot mode, host, users, timestamps, thread
  count or the order of an unordered collection.
- Transfer blobs (`rnk-registry`), encode layers (`rnk-layer`), own the local
  store (`rnk-store`) or choose what to export (`rnk-builder`).

## API (planned)

Nothing exists yet; this is the design sketch.

```rust
pub const KEY_VERSION: u32 = 1;                      // [decided] a version field, ADR-0005; bump on any change
pub struct CacheKey { version: u32, hash: [u8; 32] } // [design] key(k) = H(version, key(k-1), instruction, inputs(k))
impl CacheKey {
    pub fn root(base: &Digest, platform: &Platform) -> Self;             // ADR-0113: the base's platform manifest digest plus the platform
    pub fn next(&self, instr: &ExpandedInstr, inputs: &KeyInputs) -> Self; // [design]
    pub fn tag(&self, encoding: &Encoding) -> String;                     // form: ADR-0113; derivation: #125
}
pub struct KeyInputs { /* per file: path, content digest, attributes, clamped mtime under --reproducible; mount specs; args; env; USER, WORKDIR, SHELL */ }   // fields: ADR-0113
pub trait LayerCache {                               // [design] RegistryCache, LayoutCache
    async fn lookup(&self, key: &CacheKey, encoding: &Encoding) -> Result<Option<CachedLayer>>;  // takes the encoding (ADR-0113): a value of rnk-cache's own, as it does not depend on rnk-layer (ADR-0115)
    async fn push(&self, key: &CacheKey, encoding: &Encoding, layer: &EncodedLayer) -> Result<()>;  // asynchronous; the tag and the entry's annotations need the encoding (ADR-0113)
}
```

`H` is SHA-256, and the fields, the entry and the form of the tag are decided by
[ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md);
how the tag's `<hex>` is derived is decided before
[#125](https://github.com/tran4774/ranikust/issues/125) is built. None of it is
built yet.

## Invariants

The key formula and its fields: [`design/cache-and-store.md#cache-keys`](../../design/cache-and-store.md#cache-keys).

- A new algorithm bumps `KEY_VERSION`; old entries are never matched again
  and never rewritten.
- `COPY`/`ADD` inputs are the files actually read, after `.dockerignore`: each
  one's path relative to the source, content digest and attributes, and under
  `--reproducible` its clamped mtime (ADR-0113). The instruction is hashed after
  variable expansion, with the environment in a fixed order [inferred]: the
  order itself is not decided.
- Same inputs, same key, in either mode and with any `--jobs`; a layer cached
  by one mode may serve the other (ADR-0005).
- A `type=secret` mount adds its specification, never its content (T11);
  cached layers are digest-checked when read (T13).
- One compressed blob serves both the image and the cache; pushes run
  asynchronously. Waiting for them before exit, as kaniko does, is [inferred].

## Refs

- **Design:** [`design/cache-and-store.md#cache-keys`](../../design/cache-and-store.md#cache-keys), [`#ownership`](../../design/cache-and-store.md#ownership); [`design/scheduler.md#lazy-export`](../../design/scheduler.md#lazy-export) (`--cache` makes a layer needed); [threat model T11, T13](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0005](../../adr/0005-own-versioned-cache-key.md) (own versioned key), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (cross-mode layers carry differences), [ADR-0006](../../adr/0006-persistent-store.md) and [ADR-0021](../../adr/0021-rnk-store-crate.md) (the local store is `rnk-store`, not this cache); decided, not built yet: [ADR-0113](../../adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md) (key fields, tag, encoding), [ADR-0114](../../adr/0114-every-cache-key-is-computed-before-the-build-and-fully-cached-stages-are-skipped.md) (lookahead, skipping cached stages), [ADR-0115](../../adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md) (edges), [ADR-0116](../../adr/0116-the-cache-lives-in-a-registry-or-an-oci-layout-of-one-layout-per-key.md) (registry or layout, none on `/`)
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

Planned.

- **T0:** golden keys pin the algorithm (a changed golden means: bump
  `KEY_VERSION`); property tests: a key ignores environment order, file
  enumeration order and read chunking, and changes when any input changes;
  the API cannot take a snapshot mode; `LayoutCache` round trip in a temp dir.
- **Integration:** `--cache-repo` against a local registry; W2 cold, warm and
  partial; a layer cached in one mode reused by the other, checked against
  the ADR-0012 list.

## Metrics

- Phase 4: `rnk_cache.key` (`files_hashed`, `bytes_hashed`),
  `rnk_cache.lookup` (`hit`), `rnk_cache.push` (`bytes_out`)
  ([rows](../../design/metrics.md#rnk-cache)).
- A push outlives its instruction and stage: parent it to `rnk_sched.run`.

## Pitfalls

- Environment order is undefined, so kaniko sorts it before hashing
  (`kaniko@44aa7ddc0 pkg/executor/build.go:328-330`); one unsorted input
  makes keys differ between runs.
- A mode in the key would block cross-mode reuse; with reuse, layers carry
  the other mode's hardlink and copy-up differences (ADR-0012).
- Users moving from kaniko start with a cold cache: decided, not a bug
  (ADR-0005).
- A secret in a key or a cache tag leaks through the registry (T11).
- The key's fields and the chain root are decided by ADR-0113 and not built,
  and no cache measurement exists yet: claim no speed-up before Phase 4
  measures one.
