# rnk-cache

> **Status:** skeleton · **Phase:** 4 ·
> **Depends on:** `rnk-oci`, `rnk-store`, `rnk-registry` (planned) ·
> **Used by:** `rnk-builder`, `rnk-sched` (up-front lookups) ·
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
    pub fn root(base: &Digest, platform: &Platform) -> Self;             // [design] resolved base digest [inferred]
    pub fn next(&self, instr: &ExpandedInstr, inputs: &KeyInputs) -> Self; // [design]
    pub fn tag(&self) -> String;                                          // [open: #13] form in --cache-repo
}
pub struct KeyInputs { /* sorted file digests, mount specs, args, env */ }   // [open: #13] fixed in Phase 4 with W2
pub trait LayerCache {                               // [design] RegistryCache, LayoutCache
    async fn lookup(&self, key: &CacheKey) -> Result<Option<CachedLayer>>;
    async fn push(&self, key: &CacheKey, layer: &EncodedLayer) -> Result<()>;  // asynchronous
}
```

`H` is a cryptographic hash; SHA-256 is likely but not decided [open: #13].

## Invariants

The key formula and its fields: [`design/cache-and-store.md#cache-keys`](../../design/cache-and-store.md#cache-keys).

- A new algorithm bumps `KEY_VERSION`; old entries are never matched again
  and never rewritten.
- `COPY`/`ADD` inputs are the digests of the files actually read, after
  `.dockerignore`; the instruction is hashed after variable expansion, the
  environment in a fixed order [inferred].
- Same inputs, same key, in either mode and with any `--jobs`; a layer cached
  by one mode may serve the other (ADR-0005).
- A `type=secret` mount adds its specification, never its content (T11);
  cached layers are digest-checked when read (T13).
- One compressed blob serves both the image and the cache; pushes run
  asynchronously. Waiting for them before exit, as kaniko does, is [inferred].

## Refs

- **Design:** [`design/cache-and-store.md#cache-keys`](../../design/cache-and-store.md#cache-keys), [`#ownership`](../../design/cache-and-store.md#ownership); [`design/scheduler.md#lazy-export`](../../design/scheduler.md#lazy-export) (`--cache` makes a layer needed); [threat model T11, T13](../../security/threat-model.md#5-threats-and-mitigations)
- **Decisions:** [ADR-0005](../../adr/0005-own-versioned-cache-key.md) (own versioned key), [ADR-0012](../../adr/0012-accept-divergence-between-modes.md) (cross-mode layers carry differences), [ADR-0006](../../adr/0006-persistent-store.md) and [ADR-0021](../../adr/0021-rnk-store-crate.md) (the local store is `rnk-store`, not this cache)
- **Open:** [#13](https://github.com/tran4774/ranikust/issues/13) (key fields and tag), [#14](https://github.com/tran4774/ranikust/issues/14) (lookahead, skipping cached stages), [#37](https://github.com/tran4774/ranikust/issues/37) (signing cache entries)
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
- The chain root and instruction fields are [inferred] and no cache
  measurement exists: decide nothing and claim no speed-up before Phase 4.
