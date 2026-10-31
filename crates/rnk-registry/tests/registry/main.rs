//! `rnk-registry` against an in-process fake registry (T0), and one
//! ignored test against a real registry named by `RNK_TEST_REGISTRY`.
//! Spans are checked in the `spans` test binary.

mod fake;
mod support;

use std::collections::BTreeSet;

use bytes::Bytes;
use fake::{Auth, FakeRegistry, Fault, sha256};
use hyper::Method;
use rnk_oci::{Digest, ImageFormat, ImageIndex, ImageManifest, MediaType, Platform, Reference};
use rnk_registry::{
    BlobBody, Client, ClientOptions, Credentials, Error, PushOutcome, RawManifest, TrustStore,
};
use support::{
    Image, add_image, basic_auth_entry, client, client_with, descriptor, options, reference,
};

#[tokio::test]
async fn open_registry_pulls() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let image = add_image(&registry, "app", "1.0", b"layer bytes ".repeat(5000));
    let client = client(&registry, Credentials::none());
    let reference = reference(&registry, "app:1.0");

    let manifest = client.pull_manifest(&reference).await.unwrap();
    assert_eq!(manifest.bytes().as_ref(), image.manifest.as_slice());
    assert_eq!(manifest.digest().as_str(), image.manifest_digest);
    assert_eq!(manifest.media_type(), &MediaType::OCI_MANIFEST);

    let mut layer = Vec::new();
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    client
        .pull_blob(&reference, &blob, &mut layer)
        .await
        .unwrap();
    assert_eq!(layer, image.layer);
    // The probe found an open registry: no token requests.
    assert_eq!(registry.count("GET /token"), 0);
}

#[tokio::test]
async fn a_loopback_registry_speaks_plain_http_unasked() {
    // As BuildKit (`util/resolver/resolver.go`, containerd's
    // `MatchLocalhost`): a loopback registry is plain HTTP without a flag,
    // and HTTPS is not even tried: the fake speaks only HTTP, and the
    // registry is not listed as insecure.
    let registry = FakeRegistry::start(Auth::Open).await;
    add_image(&registry, "app", "1.0", b"x".to_vec());
    let mut options = options(&registry);
    options.insecure.clear();
    options.http_credentials.clear();
    let client = Client::new(options, TrustStore::load(), Credentials::none()).unwrap();
    client
        .pull_manifest(&reference(&registry, "app:1.0"))
        .await
        .unwrap();
}

#[tokio::test]
async fn bearer_tokens_from_config_json_credentials() {
    let registry = FakeRegistry::start(Auth::Bearer {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
        refresh_token: "refresh".to_owned(),
        anonymous_pull: false,
    })
    .await;
    let image = add_image(&registry, "team/app", "v1", b"private".repeat(10));
    let reference = reference(&registry, "team/app:v1");

    let anonymous = client(&registry, Credentials::none());
    let err = anonymous.pull_manifest(&reference).await.unwrap_err();
    assert!(matches!(err, Error::Auth { .. }), "{err}");

    let wrong = client_with(
        &registry,
        &basic_auth_entry(&registry.host(), "builder", "nope"),
    );
    let err = wrong.pull_manifest(&reference).await.unwrap_err();
    assert!(!err.to_string().contains("nope"), "{err}");

    let client = client_with(
        &registry,
        &basic_auth_entry(&registry.host(), "builder", "s3cret"),
    );
    let manifest = client.pull_manifest(&reference).await.unwrap();
    assert_eq!(manifest.digest().as_str(), image.manifest_digest);
    let mut layer = Vec::new();
    client
        .pull_blob(
            &reference,
            &descriptor(MediaType::OCI_LAYER_GZIP, &image.layer),
            &mut layer,
        )
        .await
        .unwrap();
    assert_eq!(layer, image.layer);
    // One token for the repository's pull scope, shared by both pulls.
    assert_eq!(registry.count("GET /token"), 3);
}

#[tokio::test]
async fn anonymous_pulls_get_a_token() {
    let registry = FakeRegistry::start(Auth::Bearer {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
        refresh_token: "refresh".to_owned(),
        anonymous_pull: true,
    })
    .await;
    let image = add_image(&registry, "public", "v1", b"public".to_vec());
    let client = client(&registry, Credentials::none());
    let manifest = client
        .pull_manifest(&reference(&registry, "public:v1"))
        .await
        .unwrap();
    assert_eq!(manifest.digest().as_str(), image.manifest_digest);

    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    let body = BlobBody::Bytes(Bytes::from(image.layer.clone()));
    let err = client
        .push_blob(&reference(&registry, "public:v2"), &blob, &body)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Auth { .. }), "{err}");
}

/// A client whose `config.json` holds credentials for the fake, which is
/// not listed as insecure: plain HTTP to loopback, credentials held back.
fn unlisted_client(registry: &FakeRegistry) -> Client {
    let mut options = options(registry);
    options.insecure.clear();
    options.http_credentials.clear();
    Client::new(
        options,
        TrustStore::load(),
        support::credentials(&basic_auth_entry(&registry.host(), "builder", "s3cret")),
    )
    .unwrap()
}

#[tokio::test]
async fn credentials_never_go_in_clear_to_an_unlisted_loopback_registry() {
    // A `RUN` shares the host's network namespace (unlike BuildKit's), so
    // it could sniff `lo` or serve the port: credentials go over plain HTTP
    // only to a registry listed with `--insecure-registry` (T11).
    let registry = FakeRegistry::start(Auth::Bearer {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
        refresh_token: "refresh".to_owned(),
        anonymous_pull: true,
    })
    .await;
    let image = add_image(&registry, "app", "v1", b"x".to_vec());
    let client = unlisted_client(&registry);
    // Anonymous pulls work.
    client
        .pull_manifest(&reference(&registry, "app:v1"))
        .await
        .unwrap();
    // A push needs the credentials: refused, saying how to send them.
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    let body = BlobBody::Bytes(Bytes::from(image.layer.clone()));
    let err = client
        .push_blob(&reference(&registry, "app:v2"), &blob, &body)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Auth { .. }), "{err}");
    assert!(err.to_string().contains("--insecure-registry"), "{err}");
    assert_eq!(registry.count("PUT "), 0, "nothing was pushed");
}

/// `--insecure-pull` lets a registry a Dockerfile names be plain HTTP, but
/// not receive credentials over it: being insecure is not enough
/// (`http_credentials`, T11, ADR-0036).
#[tokio::test]
async fn an_insecure_registry_not_named_for_credentials_gets_none() {
    let registry = FakeRegistry::start(Auth::Basic {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
    })
    .await;
    add_image(&registry, "app", "v1", b"x".to_vec());
    let mut opts = options(&registry);
    opts.http_credentials.clear();
    let client = Client::new(
        opts,
        TrustStore::load(),
        support::credentials(&basic_auth_entry(&registry.host(), "builder", "s3cret")),
    )
    .unwrap();
    let err = client
        .pull_manifest(&reference(&registry, "app:v1"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Auth { .. }), "{err}");
    assert_eq!(registry.count("GET /v2/app"), 0);
    // The control: named for credentials, the same pull works.
    client_with(
        &registry,
        &basic_auth_entry(&registry.host(), "builder", "s3cret"),
    )
    .pull_manifest(&reference(&registry, "app:v1"))
    .await
    .unwrap();
}

/// In kaniko mode on `/` a `RUN` may redirect a registry's name: only
/// verified TLS carries credentials there, even to a registry the operator
/// named for plain HTTP (ADR-0051).
#[tokio::test]
async fn verified_credentials_only_withholds_them_from_plain_http() {
    let registry = FakeRegistry::start(Auth::Basic {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
    })
    .await;
    add_image(&registry, "app", "v1", b"x".to_vec());
    let mut opts = options(&registry);
    assert!(opts.http_credentials.contains(&registry.host()));
    opts.verified_credentials_only = true;
    let client = Client::new(
        opts,
        TrustStore::load(),
        support::credentials(&basic_auth_entry(&registry.host(), "builder", "s3cret")),
    )
    .unwrap();
    let err = client
        .pull_manifest(&reference(&registry, "app:v1"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Auth { .. }), "{err}");
    assert!(err.to_string().contains("verified TLS"), "{err}");
    assert_eq!(registry.count("GET /v2/app"), 0);
}

#[tokio::test]
async fn an_unlisted_loopback_registry_asking_for_basic_auth_is_refused() {
    let registry = FakeRegistry::start(Auth::Basic {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
    })
    .await;
    add_image(&registry, "app", "v1", b"x".to_vec());
    let err = unlisted_client(&registry)
        .pull_manifest(&reference(&registry, "app:v1"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Auth { .. }), "{err}");
    assert!(err.to_string().contains("--insecure-registry"), "{err}");
    // Only the ping reached the registry: no request carried credentials.
    assert_eq!(registry.count("GET /v2/app"), 0);
}

#[tokio::test]
async fn identity_tokens_are_exchanged_per_scope() {
    let registry = FakeRegistry::start(Auth::Bearer {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
        refresh_token: "refresh-xyz".to_owned(),
        anonymous_pull: false,
    })
    .await;
    let image = add_image(&registry, "app", "v1", b"content".repeat(10));
    let host = registry.host();
    let client = client_with(
        &registry,
        &format!(r#"{{"auths":{{"{host}":{{"identitytoken":"refresh-xyz"}}}}}}"#),
    );

    let source = reference(&registry, "app:v1");
    client.pull_manifest(&source).await.unwrap();
    let target = reference(&registry, "copy:v1");
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    let body = BlobBody::Bytes(Bytes::from(image.layer.clone()));
    // `copy` does not hold the blob yet: it is uploaded under the push scope.
    assert_eq!(
        client.push_blob(&target, &blob, &body).await.unwrap(),
        PushOutcome::Uploaded
    );
    client.pull_manifest(&source).await.unwrap();

    let forms = registry.state().token_forms.clone();
    assert_eq!(forms.len(), 2, "{forms:?}");
    assert!(forms.iter().all(|f| f.contains("grant_type=refresh_token")));
    assert!(forms[0].contains("repository%3Aapp%3Apull"), "{}", forms[0]);
    assert!(
        forms[1].contains("repository%3Acopy%3Apull%2Cpush"),
        "{}",
        forms[1]
    );

    let wrong = client_with(
        &registry,
        &format!(r#"{{"auths":{{"{host}":{{"identitytoken":"stale"}}}}}}"#),
    );
    let err = wrong.pull_manifest(&source).await.unwrap_err();
    assert!(matches!(err, Error::Auth { .. }), "{err}");
    assert!(!err.to_string().contains("stale"), "{err}");
}

#[tokio::test]
async fn basic_authentication() {
    let registry = FakeRegistry::start(Auth::Basic {
        user: "u".to_owned(),
        password: "p".to_owned(),
    })
    .await;
    let image = add_image(&registry, "app", "v1", b"basic".to_vec());
    let reference = reference(&registry, "app:v1");

    let err = client(&registry, Credentials::none())
        .pull_manifest(&reference)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");

    let client = client_with(&registry, &basic_auth_entry(&registry.host(), "u", "p"));
    let manifest = client.pull_manifest(&reference).await.unwrap();
    assert_eq!(manifest.digest().as_str(), image.manifest_digest);
}

#[tokio::test]
async fn digest_mismatch_is_rejected_without_retry() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let image = add_image(&registry, "app", "v1", b"genuine".to_vec());
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    registry
        .state()
        .tampered
        .insert(blob.digest.to_string(), Bytes::from_static(b"forgery"));

    let client = client(&registry, Credentials::none());
    let mut out = Vec::new();
    let err = client
        .pull_blob(&reference(&registry, "app"), &blob, &mut out)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::DigestMismatch { .. }), "{err}");
    assert_eq!(registry.count("GET /v2/app/blobs/"), 1);

    // A manifest pulled by digest is checked against that digest.
    let other = sha256(b"something else");
    let manifest = registry.state().manifests[&("app".to_owned(), "v1".to_owned())].clone();
    registry
        .state()
        .manifests
        .insert(("app".to_owned(), other.clone()), manifest);
    let by_digest = reference(&registry, &format!("app@{other}"));
    let err = client.pull_manifest(&by_digest).await.unwrap_err();
    assert!(matches!(err, Error::DigestMismatch { .. }), "{err}");
}

#[tokio::test]
async fn blobs_longer_or_shorter_than_their_descriptor_fail() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let content = vec![1u8; 1000];
    registry.add_blob("app", &content);
    let client = client(&registry, Credentials::none());
    let reference = reference(&registry, "app");

    let mut short = descriptor(MediaType::OCI_LAYER_GZIP, &content);
    short.size = 10;
    let mut out = Vec::new();
    let err = client
        .pull_blob(&reference, &short, &mut out)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::SizeMismatch { expected: 10, .. }),
        "{err}"
    );
    assert!(out.len() <= 10, "{}", out.len());

    let mut long = descriptor(MediaType::OCI_LAYER_GZIP, &content);
    long.size = 2000;
    let err = client
        .pull_blob(&reference, &long, &mut Vec::new())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::SizeMismatch {
                expected: 2000,
                actual: 1000,
                ..
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn transient_failures_are_retried() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let image = add_image(&registry, "app", "v1", b"retry me".repeat(100));
    registry.state().faults.push(Fault {
        method: Method::GET,
        path_contains: "/blobs/".to_owned(),
        status: 503,
        remaining: 2,
    });
    let client = client(&registry, Credentials::none());
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);

    // A file sink is reset between attempts.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    let mut file = tokio::fs::File::create(&path).await.unwrap();
    client
        .pull_blob(&reference(&registry, "app"), &blob, &mut file)
        .await
        .unwrap();
    drop(file);
    assert_eq!(std::fs::read(&path).unwrap(), image.layer);
    assert_eq!(registry.count("GET /v2/app/blobs/"), 3);

    // A permanent failure is not retried.
    registry.state().faults.push(Fault {
        method: Method::GET,
        path_contains: "/manifests/".to_owned(),
        status: 404,
        remaining: 5,
    });
    let before = registry.count("GET /v2/app/manifests/");
    let err = client
        .pull_manifest(&reference(&registry, "app:v1"))
        .await
        .unwrap_err();
    assert!(!err.is_retryable(), "{err}");
    assert_eq!(registry.count("GET /v2/app/manifests/"), before + 1);
}

/// kaniko's `--image-download-retry`: an outer loop around the image
/// manifest fetch, each round with the default policy inside, so never
/// fewer attempts than without it; any error is retried, as kaniko's
/// `Retry` (`remote.go:79,121`, `util.go:221`).
#[tokio::test]
async fn manifest_retries_wrap_the_default_policy() {
    let registry = FakeRegistry::start(Auth::Open).await;
    add_image(&registry, "app", "v1", b"layer".to_vec());
    let mut opts = options(&registry);
    opts.manifest_retries = 1;
    let client = Client::new(opts, TrustStore::load(), Credentials::none()).unwrap();
    let linux = Platform::new("linux", "amd64");

    // Four transient failures: the first round's three inner attempts, then
    // one more in the second round, which succeeds.
    registry.state().faults.push(Fault {
        method: Method::GET,
        path_contains: "/manifests/".to_owned(),
        status: 503,
        remaining: 4,
    });
    client
        .pull_image_manifest(&reference(&registry, "app:v1"), &linux)
        .await
        .unwrap();
    assert_eq!(registry.count("GET /v2/app/manifests/"), 5);

    // A permanent failure is retried by the outer loop only.
    registry.state().faults.push(Fault {
        method: Method::GET,
        path_contains: "/manifests/".to_owned(),
        status: 404,
        remaining: 1,
    });
    let before = registry.count("GET /v2/app/manifests/");
    client
        .pull_image_manifest(&reference(&registry, "app:v1"), &linux)
        .await
        .unwrap();
    assert_eq!(registry.count("GET /v2/app/manifests/"), before + 2);
}

#[tokio::test]
async fn pushes_upload_once_then_find_the_blob() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let client = client(&registry, Credentials::none());
    let target = reference(&registry, "out:latest");
    let dir = tempfile::tempdir().unwrap();
    let layer = vec![9u8; 300 * 1024];
    let path = dir.path().join("layer");
    std::fs::write(&path, &layer).unwrap();
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &layer);
    let body = BlobBody::File(path.clone());

    assert_eq!(
        client.push_blob(&target, &blob, &body).await.unwrap(),
        PushOutcome::Uploaded
    );
    assert_eq!(
        client.push_blob(&target, &blob, &body).await.unwrap(),
        PushOutcome::Existed
    );
    assert_eq!(registry.count("PUT /v2/out/blobs/uploads/"), 1);
    assert_eq!(
        registry.state().blobs[blob.digest.as_str()].as_ref(),
        layer.as_slice()
    );

    // A body that does not match its descriptor is not sent.
    let mut wrong = blob.clone();
    wrong.digest = Digest::parse(&sha256(b"other")).unwrap();
    wrong.size = 5;
    let err = client.push_blob(&target, &wrong, &body).await.unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");

    let manifest = RawManifest::new(MediaType::OCI_MANIFEST, br#"{"schemaVersion":2}"#.to_vec());
    let digest = client.push_manifest(&target, &manifest).await.unwrap();
    assert_eq!(digest, *manifest.digest());
    assert_eq!(
        registry.state().manifests[&("out".to_owned(), "latest".to_owned())].1,
        manifest.bytes()
    );

    let by_digest = target.clone().with_digest(manifest.digest().clone());
    let err = client
        .push_manifest(&by_digest, &manifest)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Reference { .. }), "{err}");
}

#[tokio::test]
async fn mounts_fall_back_to_upload() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let image = add_image(&registry, "base", "v1", b"shared layer".repeat(10));
    let client = client(&registry, Credentials::none());
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    let body = BlobBody::Bytes(Bytes::from(image.layer.clone()));
    let from = reference(&registry, "base:v1");
    // Each repository sees only its own blobs: `a` and `b` miss the blob
    // and try the mount.
    registry.state().refuse_mounts = true;
    let outcome = client
        .mount_or_push(&reference(&registry, "a:v1"), &from, &blob, &body)
        .await;
    assert_eq!(outcome.unwrap(), PushOutcome::Uploaded);
    assert_eq!(registry.count("POST /v2/a/blobs/uploads/?mount="), 1);
    assert_eq!(registry.count("PUT /v2/a/blobs/uploads/"), 1);

    registry.state().refuse_mounts = false;
    let outcome = client
        .mount_or_push(&reference(&registry, "b:v1"), &from, &blob, &body)
        .await;
    assert_eq!(outcome.unwrap(), PushOutcome::Mounted);
    assert_eq!(registry.count("HEAD /v2/b/blobs/"), 1);
    assert_eq!(registry.count("PUT /v2/b/blobs/uploads/"), 0);
}

/// On a token registry the mount carries one token with push access to the
/// destination and pull access to the source, from the realm (basic
/// credentials) or the identity-token exchange.
#[tokio::test]
async fn mounts_on_token_registries_ask_for_the_source_scope() {
    for identity in [false, true] {
        let registry = FakeRegistry::start(Auth::Bearer {
            user: "builder".to_owned(),
            password: "s3cret".to_owned(),
            refresh_token: "refresh-xyz".to_owned(),
            anonymous_pull: false,
        })
        .await;
        let host = registry.host();
        let client = if identity {
            client_with(
                &registry,
                &format!(r#"{{"auths":{{"{host}":{{"identitytoken":"refresh-xyz"}}}}}}"#),
            )
        } else {
            client_with(&registry, &basic_auth_entry(&host, "builder", "s3cret"))
        };
        let image = add_image(&registry, "base", "v1", b"shared layer".repeat(10));
        let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
        let body = BlobBody::Bytes(Bytes::from(image.layer.clone()));
        let from = reference(&registry, "base:v1");

        for target in ["a:v1", "b:v1"] {
            let outcome = client
                .mount_or_push(&reference(&registry, target), &from, &blob, &body)
                .await;
            assert_eq!(
                outcome.unwrap(),
                PushOutcome::Mounted,
                "identity: {identity}"
            );
        }
        assert_eq!(registry.count("PUT /v2/"), 0, "nothing uploaded");
        if identity {
            let forms = registry.state().token_forms.clone();
            assert!(
                forms
                    .iter()
                    .any(|f| f.contains("repository%3Aa%3Apull%2Cpush+repository%3Abase%3Apull")),
                "{forms:?}"
            );
        }
    }
}

#[tokio::test]
async fn index_entries_are_picked_by_platform() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let amd64 = add_image(&registry, "multi", "amd64", b"amd64".to_vec());
    let arm64 = add_image(&registry, "multi", "arm64", b"arm64".to_vec());
    let entry = |image: &Image, arch: &str, variant: Option<&str>| {
        let mut d = descriptor(MediaType::OCI_MANIFEST, &image.manifest);
        let mut platform = Platform::new("linux", arch);
        platform.variant = variant.map(str::to_owned);
        d.platform = Some(platform);
        d
    };
    let index = ImageIndex::new(
        ImageFormat::Oci,
        vec![
            entry(&amd64, "amd64", None),
            entry(&arm64, "arm64", Some("v8")),
        ],
    );
    let index_bytes = serde_json::to_vec(&index).unwrap();
    registry.add_manifest(
        "multi",
        "latest",
        MediaType::OCI_INDEX.as_str(),
        &index_bytes,
    );
    let client = client(&registry, Credentials::none());
    let reference = reference(&registry, "multi:latest");

    let picked = client
        .pull_image_manifest(&reference, &Platform::new("linux", "arm64"))
        .await
        .unwrap();
    assert_eq!(picked.manifest.digest().as_str(), arm64.manifest_digest);
    assert_eq!(
        picked.index.unwrap().bytes().as_ref(),
        index_bytes.as_slice()
    );

    let err = client
        .pull_image_manifest(&reference, &Platform::new("linux", "riscv64"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("linux/riscv64"), "{err}");

    let single = client
        .pull_image_manifest(
            &reference.clone().with_tag("amd64").unwrap(),
            &Platform::new("linux", "arm64"),
        )
        .await
        .unwrap();
    assert!(single.index.is_none());
}

#[tokio::test]
async fn registry_certificates_must_be_readable() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let mut options = options(&registry);
    options
        .registry_certificates
        .push((registry.host(), "/nonexistent/ca.pem".into()));
    let err = Client::new(options, TrustStore::load(), Credentials::none())
        .err()
        .unwrap();
    assert!(matches!(err, Error::Tls(_)), "{err}");
}

/// Pulls `RNK_TEST_IMAGE` (an image on a plain-HTTP registry, such as
/// `127.0.0.1:5001/node:22-bookworm-slim`) for linux/amd64: the manifest,
/// the config and the smallest layer. Read-only.
#[tokio::test]
#[ignore = "needs a registry: RNK_TEST_IMAGE=127.0.0.1:5001/<repo>:<tag>"]
async fn real_registry_pull() {
    let Ok(image) = std::env::var("RNK_TEST_IMAGE") else {
        panic!("set RNK_TEST_IMAGE to an image reference");
    };
    let reference = Reference::parse(&image).unwrap();
    let mut options = ClientOptions::default();
    options.insecure = BTreeSet::from([reference.registry().to_owned()]);
    options.http_credentials = options.insecure.clone();
    let client = Client::new(options, TrustStore::load(), Credentials::none()).unwrap();

    let pulled = client
        .pull_image_manifest(&reference, &Platform::new("linux", "amd64"))
        .await
        .unwrap();
    let manifest = ImageManifest::from_json(pulled.manifest.bytes()).unwrap();
    let mut config = Vec::new();
    client
        .pull_blob(&reference, &manifest.config, &mut config)
        .await
        .unwrap();
    assert_eq!(u64::try_from(config.len()).unwrap(), manifest.config.size);
    let smallest = manifest.layers.iter().min_by_key(|l| l.size).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut file = tokio::fs::File::create(dir.path().join("layer"))
        .await
        .unwrap();
    client
        .pull_blob(&reference, smallest, &mut file)
        .await
        .unwrap();
    assert_eq!(file.metadata().await.unwrap().len(), smallest.size);
}

/// Pushes and pulls a small image in a `k6-*` repository of a real
/// registry, `RNK_TEST_REGISTRY` (`host:port`, plain HTTP, no auth), then
/// deletes the manifest it pushed.
#[tokio::test]
#[ignore = "needs a registry: RNK_TEST_REGISTRY=127.0.0.1:5001"]
async fn real_registry_round_trip() {
    let Ok(host) = std::env::var("RNK_TEST_REGISTRY") else {
        panic!("set RNK_TEST_REGISTRY to host:port");
    };
    let mut options = ClientOptions::default();
    options.insecure = BTreeSet::from([host.clone()]);
    options.http_credentials = options.insecure.clone();
    let client = Client::new(options, TrustStore::load(), Credentials::none()).unwrap();
    let target = Reference::parse(&format!("{host}/k6-rnk-registry/test:round-trip")).unwrap();

    let config =
        br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
    let config_desc = descriptor(MediaType::OCI_CONFIG, config);
    let layer = b"k6 test layer".repeat(1000);
    let layer_desc = descriptor(MediaType::OCI_LAYER_GZIP, &layer);
    for (desc, content) in [
        (&config_desc, config.to_vec()),
        (&layer_desc, layer.clone()),
    ] {
        client
            .push_blob(&target, desc, &BlobBody::Bytes(Bytes::from(content)))
            .await
            .unwrap();
    }
    let manifest = ImageManifest::new(ImageFormat::Oci, config_desc, vec![layer_desc.clone()]);
    let manifest = RawManifest::new(
        MediaType::OCI_MANIFEST,
        serde_json::to_vec(&manifest).unwrap(),
    );
    let digest = client.push_manifest(&target, &manifest).await.unwrap();

    let pulled = client.pull_manifest(&target).await.unwrap();
    assert_eq!(*pulled.digest(), digest);
    let mut out = Vec::new();
    client
        .pull_blob(&target, &layer_desc, &mut out)
        .await
        .unwrap();
    assert_eq!(out, layer);

    // Best-effort clean-up; registries without deletion enabled answer 405.
    let url = format!("http://{host}/v2/k6-rnk-registry/test/manifests/{digest}");
    let status = reqwest::Client::new()
        .delete(url)
        .send()
        .await
        .map(|r| r.status());
    eprintln!("manifest delete: {status:?}");
}

/// Reads a blob through [`Client::open_blob`] on a blocking thread, as a
/// stage's thread does.
async fn read_streamed(
    client: &std::sync::Arc<Client>,
    reference: &Reference,
    blob: &rnk_oci::Descriptor,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let handle = tokio::runtime::Handle::current();
    let (client, reference, blob) = (
        std::sync::Arc::clone(client),
        reference.clone(),
        blob.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let mut reader = client.open_blob(&handle, &reference, &blob)?;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).map(|_| out)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_blob_reads_whole_and_verified() {
    let registry = FakeRegistry::start(Auth::Open).await;
    // Larger than the pipe, so the reader drains it while the pull fills it.
    let content = b"streamed layer ".repeat(60_000);
    let image = add_image(&registry, "app", "v1", content.clone());
    let client = std::sync::Arc::new(client(&registry, Credentials::none()));
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    let got = read_streamed(&client, &reference(&registry, "app"), &blob)
        .await
        .unwrap();
    assert_eq!(got, content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streamed_blob_with_a_wrong_digest_never_reads_to_a_clean_end() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let image = add_image(&registry, "app", "v1", b"genuine".to_vec());
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    // Same length, other bytes: only the digest tells.
    registry
        .state()
        .tampered
        .insert(blob.digest.to_string(), Bytes::from_static(b"forgery"));
    let client = std::sync::Arc::new(client(&registry, Credentials::none()));
    let err = read_streamed(&client, &reference(&registry, "app"), &blob)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("digest"), "{err}");
    // A streamed pull is never retried: what was read cannot be taken back.
    assert_eq!(registry.count("GET /v2/app/blobs/"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_body_is_pulled_from_its_source_while_it_is_pushed() {
    let source = FakeRegistry::start(Auth::Open).await;
    let target = FakeRegistry::start(Auth::Open).await;
    let content = vec![5u8; 700 * 1024];
    let image = add_image(&source, "base", "v1", content.clone());
    let mut opts = options(&source);
    opts.insecure.insert(target.host());
    let client =
        std::sync::Arc::new(Client::new(opts, TrustStore::load(), Credentials::none()).unwrap());
    let blob = descriptor(MediaType::OCI_LAYER_GZIP, &image.layer);
    let body = BlobBody::Remote(Box::new(rnk_registry::RemoteBlob {
        client: std::sync::Arc::clone(&client),
        from: reference(&source, "base:v1"),
        blob: blob.clone(),
    }));
    let outcome = client
        .push_blob(&reference(&target, "out:latest"), &blob, &body)
        .await
        .unwrap();
    assert_eq!(outcome, PushOutcome::Uploaded);
    assert_eq!(
        target.state().blobs[blob.digest.as_str()].as_ref(),
        content.as_slice()
    );

    // A source that serves other bytes fails the push: the destination's
    // upload is never committed.
    let other = add_image(&source, "base", "v2", vec![6u8; 4096]);
    let bad = descriptor(MediaType::OCI_LAYER_GZIP, &other.layer);
    source
        .state()
        .tampered
        .insert(bad.digest.to_string(), Bytes::from(vec![7u8; 4096]));
    let body = BlobBody::Remote(Box::new(rnk_registry::RemoteBlob {
        client: std::sync::Arc::clone(&client),
        from: reference(&source, "base:v2"),
        blob: bad.clone(),
    }));
    let err = client
        .push_blob(&reference(&target, "out:latest"), &bad, &body)
        .await
        .unwrap_err();
    assert!(
        !target.state().blobs.contains_key(bad.digest.as_str()),
        "{err}"
    );
}
