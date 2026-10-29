//! Helpers shared by the `registry` and `spans` test binaries.

#![allow(dead_code, reason = "shared by two test binaries; each uses a part")]

use std::collections::BTreeSet;
use std::time::Duration;

use base64::Engine as _;
use rnk_oci::{Descriptor, Digest, ImageFormat, ImageManifest, MediaType, Reference};
use rnk_registry::{Client, ClientOptions, Credentials, RetryPolicy, TrustStore};

use crate::fake::{FakeRegistry, sha256};

pub(crate) fn fast_retries() -> RetryPolicy {
    RetryPolicy {
        attempts: 3,
        initial_delay: Duration::from_millis(1),
        factor: 2,
    }
}

/// Plain HTTP to the fake registry, fast retries.
pub(crate) fn options(registry: &FakeRegistry) -> ClientOptions {
    let mut options = ClientOptions::default();
    options.insecure = BTreeSet::from([registry.host()]);
    options.retry = fast_retries();
    options
}

pub(crate) fn client(registry: &FakeRegistry, credentials: Credentials) -> Client {
    Client::new(options(registry), TrustStore::load(), credentials).unwrap()
}

/// A client with the credentials of `config_json`.
pub(crate) fn client_with(registry: &FakeRegistry, config_json: &str) -> Client {
    client(registry, credentials(config_json))
}

/// Credentials read from a `config.json` written to a temp dir.
pub(crate) fn credentials(json: &str) -> Credentials {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, json).unwrap();
    Credentials::load(&path).unwrap()
}

/// A `config.json` with a base64 `auth` entry for `host`.
pub(crate) fn basic_auth_entry(host: &str, user: &str, password: &str) -> String {
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!(r#"{{"auths":{{"{host}":{{"auth":"{auth}"}}}}}}"#)
}

pub(crate) fn reference(registry: &FakeRegistry, rest: &str) -> Reference {
    Reference::parse(&format!("{}/{rest}", registry.host())).unwrap()
}

pub(crate) fn descriptor(media_type: MediaType, content: &[u8]) -> Descriptor {
    let size = u64::try_from(content.len()).unwrap();
    Descriptor::new(media_type, Digest::parse(&sha256(content)).unwrap(), size)
}

/// An OCI image (config, one layer, manifest) stored as `repo:tag`.
pub(crate) struct Image {
    pub(crate) layer: Vec<u8>,
    pub(crate) manifest: Vec<u8>,
    pub(crate) manifest_digest: String,
}

pub(crate) fn add_image(registry: &FakeRegistry, repo: &str, tag: &str, layer: Vec<u8>) -> Image {
    let config =
        br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
    registry.add_blob(repo, config);
    registry.add_blob(repo, &layer);
    let manifest = ImageManifest::new(
        ImageFormat::Oci,
        descriptor(MediaType::OCI_CONFIG, config),
        vec![descriptor(MediaType::OCI_LAYER_GZIP, &layer)],
    );
    let manifest = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest =
        registry.add_manifest(repo, tag, MediaType::OCI_MANIFEST.as_str(), &manifest);
    Image {
        layer,
        manifest,
        manifest_digest,
    }
}
