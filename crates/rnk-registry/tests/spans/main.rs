//! `rnk-registry`'s spans, checked with `rnk_metrics::Capture` (T0).
//!
//! A test binary of its own with a single test: tracing caches each
//! callsite's interest per process, so a span first hit by a test without
//! a collector can stay unseen by a later one in the same binary.

#[path = "../registry/fake.rs"]
mod fake;
#[path = "../registry/support.rs"]
mod support;

use bytes::Bytes;
use fake::{Auth, FakeRegistry, Fault};
use hyper::Method;
use rnk_oci::MediaType;
use rnk_registry::{BlobBody, Credentials, PushOutcome, RawManifest};
use serde_json::Value;
use support::{add_image, client, descriptor, reference};
use tracing::instrument::WithSubscriber as _;

/// Runs `future` with a fresh collector; returns its output and the
/// report's span sections.
async fn measured<T>(future: impl Future<Output = T>) -> (T, Value) {
    let capture = rnk_metrics::Capture::new();
    let output = future.with_subscriber(capture.dispatch().clone()).await;
    (output, capture.report())
}

/// How many `push_blob` spans had `flag` true; absent when none.
fn flag_count(report: &Value, flag: &str) -> u64 {
    report["phases"]["rnk_registry.push_blob"][flag]
        .as_u64()
        .unwrap_or(0)
}

#[tokio::test]
async fn registry_spans() {
    pulls().await;
    retries().await;
    pushes().await;
    mounts().await;
}

/// Pull spans and the per-host totals.
async fn pulls() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let image = add_image(&registry, "app", "1.0", b"layer bytes ".repeat(5000));
    let reference = reference(&registry, "app:1.0");
    let (_, report) = measured(async {
        let client = client(&registry, Credentials::none());
        client.pull_manifest(&reference).await.unwrap();
        let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
        client
            .pull_blob(&reference, &blob, &mut Vec::new())
            .await
            .unwrap();
    })
    .await;

    let phases = &report["phases"];
    let blob = &phases["rnk_registry.pull_blob"];
    assert_eq!(blob["count"], 1);
    assert_eq!(blob["bytes_in"], image.layer.len());
    assert_eq!(blob["requests"], 1);
    assert_eq!(blob["retries"].as_u64().unwrap_or(0), 0);
    let manifest = &phases["rnk_registry.pull_manifest"];
    assert_eq!(manifest["count"], 1);
    assert_eq!(manifest["bytes_in"], image.manifest.len());
    // The scheme probe: HTTPS refused, then plain HTTP.
    assert_eq!(phases["rnk_registry.auth"]["count"], 1);
    assert_eq!(phases["rnk_registry.auth"]["requests"], 2);
    let host = &report["network"]["hosts"][registry.host()];
    assert_eq!(host["bytes_in"], image.layer.len() + image.manifest.len());
    assert_eq!(host["requests"], 4);
}

/// `retries` and `requests` count the re-sent requests.
async fn retries() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let image = add_image(&registry, "app", "v1", b"retry me".repeat(100));
    registry.state().faults.push(Fault {
        method: Method::GET,
        path_contains: "/blobs/".to_owned(),
        status: 503,
        remaining: 2,
    });
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    let (_, report) = measured(async {
        let client = client(&registry, Credentials::none());
        client
            .pull_blob(&reference(&registry, "app"), &blob, &mut Vec::new())
            .await
            .unwrap();
    })
    .await;
    let phase = &report["phases"]["rnk_registry.pull_blob"];
    assert_eq!(phase["retries"], 2);
    assert_eq!(phase["requests"], 3);
    // Bytes of the attempts that failed before any body are not counted.
    assert_eq!(phase["bytes_in"], image.layer.len());
}

/// `push_blob` with `existed`, and `push_manifest`.
async fn pushes() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let target = reference(&registry, "out:latest");
    let layer = vec![9u8; 300 * 1024];
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &layer);
    let body = BlobBody::Bytes(Bytes::from(layer.clone()));
    let manifest = RawManifest::new(MediaType::OCI_MANIFEST, br#"{"schemaVersion":2}"#.to_vec());
    let (outcomes, report) = measured(async {
        let client = client(&registry, Credentials::none());
        let first = client.push_blob(&target, &blob, &body).await.unwrap();
        let second = client.push_blob(&target, &blob, &body).await.unwrap();
        client.push_manifest(&target, &manifest).await.unwrap();
        (first, second)
    })
    .await;
    assert_eq!(outcomes, (PushOutcome::Uploaded, PushOutcome::Existed));
    let phase = &report["phases"]["rnk_registry.push_blob"];
    assert_eq!(phase["count"], 2);
    assert_eq!(phase["bytes_out"], layer.len());
    // HEAD, POST and PUT, then HEAD.
    assert_eq!(phase["requests"], 4);
    assert_eq!(flag_count(&report, "existed"), 1);
    assert_eq!(flag_count(&report, "mounted"), 0);
    let push_manifest = &report["phases"]["rnk_registry.push_manifest"];
    assert_eq!(push_manifest["bytes_out"], manifest.size());
    assert_eq!(push_manifest["requests"], 1);
}

/// `mounted`, and no bytes sent for a mount.
async fn mounts() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let image = add_image(&registry, "base", "v1", b"shared layer".repeat(10));
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    let body = BlobBody::Bytes(Bytes::from(image.layer.clone()));
    // The fake shares one blob store across repositories: make the
    // existence check miss so the mount is tried.
    registry.state().faults.push(Fault {
        method: Method::HEAD,
        path_contains: "/v2/b/blobs/".to_owned(),
        status: 404,
        remaining: 1,
    });
    let (outcome, report) = measured(async {
        let client = client(&registry, Credentials::none());
        let from = reference(&registry, "base:v1");
        client
            .mount_or_push(&reference(&registry, "b:v1"), &from, &blob, &body)
            .await
            .unwrap()
    })
    .await;
    assert_eq!(outcome, PushOutcome::Mounted);
    assert_eq!(flag_count(&report, "mounted"), 1);
    assert_eq!(report["phases"]["rnk_registry.push_blob"]["bytes_out"], 0);
}
