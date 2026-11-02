//! A registry that stalls a blob's body holds the store's per-digest lock
//! no longer than the `registry-idle` limit (ADR-0079): the pull fails, the
//! fill ends through its error path, and another build on the same state
//! directory takes the lock and fills the entry (T0).

#[path = "../../rnk-registry/tests/registry/fake.rs"]
mod fake;

use std::collections::BTreeSet;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use rnk_oci::{Descriptor, Digest, MediaType, Reference};
use rnk_registry::{Client, ClientOptions, Credentials, RetryPolicy, TrustStore};
use rnk_store::{BuildPaths, EntryKey, Store};

use crate::fake::{Auth, FakeRegistry, sha256};

/// Pulls `blob` into the store entry `key` as `rnk-builder` does: inside
/// the store's locked fill.
fn fill(
    store: &Store,
    handle: &tokio::runtime::Handle,
    client: &Client,
    reference: &Reference,
    blob: &Descriptor,
) -> Result<(), String> {
    let key = EntryKey::Blob(blob.digest.clone());
    store
        .fill(&key, |tmp| {
            let file = tmp.file().ok_or("no file")?.try_clone()?;
            handle.block_on(async {
                let mut out = tokio::fs::File::from_std(file);
                client.pull_blob(reference, blob, &mut out).await
            })?;
            Ok::<u64, Box<dyn std::error::Error + Send + Sync>>(blob.size)
        })
        .map(drop)
        .map_err(|e| e.to_string())
}

#[test]
fn a_stalled_blob_pull_releases_the_store_lock() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime.block_on(FakeRegistry::start(Auth::Open));
    registry.state().stalled_bodies.push("/blobs/".to_owned());
    // The fake announces 1 MiB and sends its first KiB, zeros.
    let content = vec![0u8; 1 << 20];
    let blob = Descriptor::new(
        MediaType::OCI_LAYER_GZIP,
        Digest::parse(&sha256(&content)).unwrap(),
        1 << 20,
    );
    let reference = Reference::parse(&format!("{}/app", registry.host())).unwrap();
    let mut options = ClientOptions::default();
    options.insecure = BTreeSet::from([registry.host()]);
    options.read_timeout = Some(Duration::from_millis(500));
    options.retry = RetryPolicy {
        attempts: 2,
        initial_delay: Duration::from_millis(1),
        factor: 2,
    };
    let client = Client::new(options, TrustStore::load(), Credentials::none()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let first = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    // Another build on the same state directory: its own lock descriptors.
    let second = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let handle = runtime.handle().clone();

    let started = Instant::now();
    let err = fill(&first, &handle, &client, &reference, &blob).unwrap_err();
    assert!(err.contains("registry-idle"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the stalled pull took {:?}",
        started.elapsed()
    );

    // The lock is free: the other build fills the entry from a registry
    // that now answers.
    registry.state().stalled_bodies.clear();
    registry.add_blob("app", &content);
    let (done, finished) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _ = done.send(fill(&second, &handle, &client, &reference, &blob));
        });
        let outcome = finished
            .recv_timeout(Duration::from_secs(30))
            .expect("the other build waits for no lock");
        outcome.unwrap();
    });
    assert!(
        second
            .get(&EntryKey::Blob(blob.digest.clone()))
            .unwrap()
            .is_some()
    );
}

/// A registry asking for basic authentication answers ranikust's own
/// `GET /v2/` and then no other. A pull under basic credentials asks
/// nothing more of `/v2/` inside the store's locked fill, where an
/// unbounded wait would hold the per-digest lock (ADR-0079): `oci-client`'s
/// own `GET /v2/` is not sent, and the blob comes with the credential.
#[test]
fn a_basic_auth_pull_asks_no_second_ping_inside_the_store_lock() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime.block_on(FakeRegistry::start(Auth::Basic {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
    }));
    let content = b"layer".repeat(100);
    registry.add_blob("app", &content);
    registry.state().pings_answered = Some(1);
    let blob = Descriptor::new(
        MediaType::OCI_LAYER_GZIP,
        Digest::parse(&sha256(&content)).unwrap(),
        u64::try_from(content.len()).unwrap(),
    );
    let host = registry.host();
    let reference = Reference::parse(&format!("{host}/app")).unwrap();
    let mut options = ClientOptions::default();
    options.insecure = BTreeSet::from([host.clone()]);
    options.http_credentials = options.insecure.clone();
    options.read_timeout = Some(Duration::from_millis(500));
    options.retry = RetryPolicy {
        attempts: 2,
        initial_delay: Duration::from_millis(1),
        factor: 2,
    };
    let auth = base64::engine::general_purpose::STANDARD.encode("builder:s3cret");
    let config = format!(r#"{{"auths":{{"{host}":{{"auth":"{auth}"}}}}}}"#);
    let credentials = Credentials::from_env_value(config.as_bytes()).unwrap();
    let client = Client::new(options, TrustStore::load(), credentials).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let handle = runtime.handle().clone();
    let (done, finished) = mpsc::channel();
    // Not a scoped thread: a fill that never returns must fail this test,
    // not hang it.
    let worker = {
        let blob = blob.clone();
        std::thread::spawn(move || {
            let outcome = fill(&store, &handle, &client, &reference, &blob);
            let _ = done.send(outcome);
            store
        })
    };
    let outcome = finished
        .recv_timeout(Duration::from_secs(20))
        .expect("the fill still waits on the registry, holding the store lock");
    outcome.unwrap();
    let store = worker.join().unwrap();
    assert!(
        store
            .get(&EntryKey::Blob(blob.digest.clone()))
            .unwrap()
            .is_some()
    );
    assert_eq!(registry.count("GET /v2/?"), 1, "{:?}", registry.state().log);
}
