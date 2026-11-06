//! Keys, layers and caches for the registry cache's tests, and access to
//! the entries the fake registry holds, to rewrite them as a hostile
//! repository would.

#![allow(dead_code, reason = "shared by two test binaries; each uses a part")]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rnk_cache::{
    BuiltLayer, CacheKey, Encoding, EntryId, KeyInputs, Lookup, RegistryCache, TimePolicy,
};
use rnk_oci::{Compression, Descriptor, Digest, MediaType, Platform, Reference};
use rnk_registry::{BlobBody, Client, ClientOptions, Credentials, RetryPolicy, TrustStore};
use serde_json::Value;

use crate::fake::{FakeRegistry, sha256};

/// The cache repository of every test.
pub(crate) const REPO: &str = "cache";

/// gzip at level 6, file times kept.
pub(crate) const GZIP: Encoding = Encoding {
    compression: Compression::Gzip,
    level: 6,
    times: TimePolicy::Kept,
};

/// A week, the TTL of the tests that want hits.
pub(crate) const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

/// Plain HTTP to the fake registry, fast retries.
pub(crate) fn options(registry: &FakeRegistry) -> ClientOptions {
    let mut options = ClientOptions::default();
    options.insecure = BTreeSet::from([registry.host()]);
    options.http_credentials = options.insecure.clone();
    options.retry = RetryPolicy {
        attempts: 3,
        initial_delay: Duration::from_millis(1),
        factor: 2,
    };
    options
}

pub(crate) fn client(options: ClientOptions, credentials: Credentials) -> Arc<Client> {
    Arc::new(Client::new(options, TrustStore::load(), credentials).unwrap())
}

pub(crate) fn repository(registry: &FakeRegistry) -> Reference {
    Reference::parse(&format!("{}/{REPO}", registry.host())).unwrap()
}

/// The cache in [`REPO`], anonymous, with `ttl`.
pub(crate) fn cache(registry: &FakeRegistry, ttl: Duration) -> RegistryCache {
    let client = client(options(registry), Credentials::none());
    RegistryCache::new(client, repository(registry), ttl).unwrap()
}

/// A step's key, told apart by `instruction`.
pub(crate) fn key(instruction: &str) -> CacheKey {
    let root = CacheKey::root(None, &Platform::new("linux", "amd64"));
    root.next(instruction, KeyInputs::new(&GZIP))
}

pub(crate) fn entry(instruction: &str) -> EntryId {
    EntryId::new(key(instruction), GZIP)
}

/// A gzip layer of `content` (the cache never decodes it), its diff ID
/// made up from `content`.
pub(crate) fn layer(content: &[u8]) -> BuiltLayer {
    let size = u64::try_from(content.len()).unwrap();
    let digest = Digest::parse(&sha256(content)).unwrap();
    let mut tar = content.to_vec();
    tar.extend_from_slice(b" as a tar");
    BuiltLayer {
        blob: Descriptor::new(MediaType::OCI_LAYER_GZIP, digest, size),
        diff_id: Digest::parse(&sha256(&tar)).unwrap(),
        platform: Platform::new("linux", "amd64"),
        body: BlobBody::Bytes(Bytes::copy_from_slice(content)),
        mount_from: None,
    }
}

/// The manifest and config stored under `tag` in [`REPO`].
pub(crate) fn stored(registry: &FakeRegistry, tag: &str) -> (Value, Value) {
    let state = registry.state();
    let (_, manifest) = &state.manifests[&(REPO.to_owned(), tag.to_owned())];
    let manifest: Value = serde_json::from_slice(manifest).unwrap();
    let digest = manifest["config"]["digest"].as_str().unwrap();
    let config = serde_json::from_slice(&state.blobs[digest]).unwrap();
    (manifest, config)
}

/// Stores `config` as a blob of [`REPO`], points `manifest` at it, and
/// stores `manifest` under `tag`.
pub(crate) fn store(registry: &FakeRegistry, tag: &str, mut manifest: Value, config: &Value) {
    let config = serde_json::to_vec(config).unwrap();
    let digest = registry.add_blob(REPO, &config);
    manifest["config"]["digest"] = Value::from(digest);
    manifest["config"]["size"] = Value::from(config.len());
    store_manifest(registry, tag, &serde_json::to_vec(&manifest).unwrap());
}

/// Stores `bytes` under `tag` as they are.
pub(crate) fn store_manifest(registry: &FakeRegistry, tag: &str, bytes: &[u8]) {
    registry.add_manifest(REPO, tag, MediaType::OCI_MANIFEST.as_str(), bytes);
}

/// The hit `lookup` found, or a panic naming what it found instead.
#[track_caller]
pub(crate) fn hit(lookup: Lookup) -> rnk_cache::CachedLayer {
    match lookup {
        Lookup::Hit(layer) => layer,
        Lookup::Miss(miss) => panic!("a miss: {miss:?}"),
    }
}
