//! The `--limit` limits of `rnk-registry` (ADR-0079), against a hostile
//! fake registry (T0): each refuses its input while it arrives, and memory
//! stays bounded, measured by a counting allocator.
//!
//! A test binary of its own, with one test running the cases in turn: the
//! allocator counts the whole process, so cases must not overlap.

#[path = "../registry/fake.rs"]
mod fake;
#[path = "../registry/support.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fake::{Auth, FakeRegistry};
use rnk_oci::{Descriptor, Digest, MediaType};
use rnk_registry::{Client, ClientOptions, Credentials, Error, TrustStore};
use support::{add_image, descriptor, options, reference};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged; the counters only
// observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` is passed on as is.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's contract for `dealloc` is passed on as is.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc` is passed on as is.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// How much more memory than at the start the process held at its peak
/// while `future` ran.
async fn peak_of<T>(future: impl Future<Output = T>) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = future.await;
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// What a hostile registry serves: far more than any limit.
const HOSTILE: usize = 64 << 20;
/// What the client may hold for one: a limit plus socket and frame buffers.
const BOUND: usize = 16 << 20;

fn client_with(registry: &FakeRegistry, edit: impl FnOnce(&mut ClientOptions)) -> Client {
    let mut options = options(registry);
    edit(&mut options);
    Client::new(options, TrustStore::load(), Credentials::none()).unwrap()
}

fn sent(registry: &FakeRegistry) -> usize {
    usize::try_from(registry.state().oversized_sent.load(Ordering::Relaxed)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limits_refuse_hostile_bodies_with_bounded_memory() {
    a_manifest_over_the_limit_is_refused_while_it_arrives().await;
    a_raised_manifest_limit_lets_a_larger_manifest_through().await;
    a_token_response_over_the_limit_is_refused().await;
    error_bodies_are_read_only_up_to_the_limit().await;
    a_config_over_the_limit_is_refused_before_any_request().await;
    a_stalled_blob_fails_after_the_idle_limit().await;
}

async fn a_manifest_over_the_limit_is_refused_while_it_arrives() {
    let registry = FakeRegistry::start(Auth::Open).await;
    registry
        .state()
        .oversized
        .insert("/manifests/".to_owned(), (200, HOSTILE));
    let client = client_with(&registry, |_| {});
    let (result, peak) = peak_of(client.pull_manifest(&reference(&registry, "app:1"))).await;
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            Error::TooLarge {
                limit: "manifest",
                ..
            }
        ),
        "{err}"
    );
    let text = err.to_string();
    assert!(
        text.contains("4 MiB") && text.contains("--limit manifest=<size>"),
        "{text}"
    );
    assert!(
        peak < BOUND,
        "held {peak} bytes for a {HOSTILE}-byte manifest"
    );
    assert!(sent(&registry) < BOUND, "read {} bytes", sent(&registry));
    // Not retried: the same answer would come back.
    assert_eq!(registry.count("GET /v2/app/manifests/"), 1);
}

async fn a_raised_manifest_limit_lets_a_larger_manifest_through() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let mut big = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#.to_vec();
    big.resize(6 << 20, b' ');
    registry.add_manifest(
        "app",
        "1",
        "application/vnd.oci.image.manifest.v1+json",
        &big,
    );
    let refused = client_with(&registry, |_| {})
        .pull_manifest(&reference(&registry, "app:1"))
        .await
        .unwrap_err();
    assert!(matches!(
        refused,
        Error::TooLarge {
            limit: "manifest",
            ..
        }
    ));
    let raised = client_with(&registry, |o| o.max_manifest_bytes = 8 << 20)
        .pull_manifest(&reference(&registry, "app:1"))
        .await
        .unwrap();
    assert_eq!(raised.bytes().len(), big.len());
}

async fn a_token_response_over_the_limit_is_refused() {
    let registry = FakeRegistry::start(Auth::Bearer {
        user: "u".to_owned(),
        password: "p".to_owned(),
        refresh_token: "r".to_owned(),
        anonymous_pull: true,
    })
    .await;
    add_image(&registry, "app", "1", b"layer".to_vec());
    registry
        .state()
        .oversized
        .insert("/token".to_owned(), (200, HOSTILE));
    let client = client_with(&registry, |_| {});
    let (result, peak) = peak_of(client.pull_manifest(&reference(&registry, "app:1"))).await;
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            Error::TooLarge {
                limit: "registry-body",
                ..
            }
        ),
        "{err}"
    );
    assert!(peak < BOUND, "held {peak} bytes for a {HOSTILE}-byte token");
    assert!(sent(&registry) < BOUND, "read {} bytes", sent(&registry));
}

async fn error_bodies_are_read_only_up_to_the_limit() {
    for path in ["/manifests/", "/blobs/"] {
        let registry = FakeRegistry::start(Auth::Open).await;
        registry
            .state()
            .oversized
            .insert(path.to_owned(), (404, HOSTILE));
        let client = client_with(&registry, |_| {});
        let blob = descriptor(MediaType::OCI_LAYER_GZIP, b"anything");
        let (result, peak) = peak_of(async {
            if path == "/blobs/" {
                client
                    .pull_blob(&reference(&registry, "app"), &blob, &mut Vec::new())
                    .await
            } else {
                client
                    .pull_manifest(&reference(&registry, "app:1"))
                    .await
                    .map(drop)
            }
        })
        .await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("404"), "{path}: {err}");
        assert!(peak < BOUND, "{path}: held {peak} bytes of an error body");
        assert!(
            sent(&registry) < BOUND,
            "{path}: read {} bytes",
            sent(&registry)
        );
    }
}

async fn a_config_over_the_limit_is_refused_before_any_request() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let config = Descriptor::new(
        MediaType::OCI_CONFIG,
        Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap(),
        (8 << 20) + 1,
    );
    let client = client_with(&registry, |_| {});
    let err = client
        .pull_config(&reference(&registry, "app"), &config, &mut Vec::new())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::TooLarge {
                limit: "config",
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("8 MiB"), "{err}");
    assert_eq!(registry.count("GET"), 0, "nothing was requested");

    // At the limit, it is pulled.
    let bytes = vec![b' '; 8 << 20];
    registry.add_blob("app", &bytes);
    let mut out = Vec::new();
    client
        .pull_config(
            &reference(&registry, "app"),
            &descriptor(MediaType::OCI_CONFIG, &bytes),
            &mut out,
        )
        .await
        .unwrap();
    assert_eq!(out.len(), bytes.len());
}

async fn a_stalled_blob_fails_after_the_idle_limit() {
    let registry = FakeRegistry::start(Auth::Open).await;
    registry.state().stalled_bodies.push("/blobs/".to_owned());
    let client = client_with(&registry, |o| {
        o.read_timeout = Some(Duration::from_millis(500));
    });
    let blob = Descriptor::new(
        MediaType::OCI_LAYER_GZIP,
        Digest::parse(&format!("sha256:{}", "b".repeat(64))).unwrap(),
        1 << 20,
    );
    let started = Instant::now();
    let err = tokio::time::timeout(
        Duration::from_secs(30),
        client.pull_blob(&reference(&registry, "app"), &blob, &mut Vec::new()),
    )
    .await
    .expect("a stalled body ends with an error, not a wait")
    .unwrap_err();
    assert!(err.to_string().contains("registry-idle"), "{err}");
    // Each of the three attempts waits for the idle limit once.
    assert!(started.elapsed() >= Duration::from_millis(1500));
    assert_eq!(registry.count("GET /v2/app/blobs/"), 3);
}
