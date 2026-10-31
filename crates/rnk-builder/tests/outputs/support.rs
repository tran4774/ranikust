//! Layers, images and registries for the output tests.

#![allow(dead_code, reason = "each test file uses a part")]

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rnk_builder::{
    BlobLayer, BuildTime, EntriesLayer, LayerExporter, assemble, image_format, instruction_history,
};
use rnk_layer::{Compression, EncodeOptions, Entry, EntryKind, EntryPath, EntrySource};
use rnk_oci::{ContainerConfig, Descriptor, Digest, History, ImageConfig, MediaType, Reference};
use rnk_registry::{Client, ClientOptions, Credentials, RetryPolicy, TrustStore};

use crate::fake::{FakeRegistry, sha256};

/// File contents by path, for [`EntriesLayer`].
pub(crate) struct Files(pub(crate) BTreeMap<Vec<u8>, Vec<u8>>);

impl EntrySource for Files {
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>> {
        let content = self
            .0
            .get(entry.path.as_bytes())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, entry.path.to_string()))?;
        Ok(Box::new(&content[..]))
    }

    fn parent(&mut self, _path: &EntryPath) -> io::Result<Option<Entry>> {
        Ok(None)
    }
}

fn entry(path: &str, kind: EntryKind, mtime: i64) -> Entry {
    Entry {
        path: EntryPath::new(path).unwrap(),
        kind,
        mode: 0o755,
        uid: 0,
        gid: 0,
        mtime,
        xattrs: BTreeMap::new(),
        inode: None,
    }
}

/// A layer of a directory `app` with `files` files of a few KiB each, given
/// in reverse order: the writer sorts them.
pub(crate) fn layer(seed: u8, files: usize) -> EntriesLayer<Files> {
    let mut entries = vec![entry("app", EntryKind::Dir, 1_700_000_000)];
    let mut contents = BTreeMap::new();
    for i in (0..files).rev() {
        let path = format!("app/f{i:03}");
        let content: Vec<u8> = (0..4096 + i * 97)
            .map(|n| u8::try_from((n * 31 + i + usize::from(seed)) % 251).unwrap())
            .collect();
        entries.push(entry(
            &path,
            EntryKind::File {
                size: u64::try_from(content.len()).unwrap(),
            },
            1_700_000_000 + i64::try_from(i).unwrap(),
        ));
        contents.insert(path.into_bytes(), content);
    }
    EntriesLayer::new(entries, Files(contents))
}

/// Encoding options; the exporter sets `mtime` from the build's time.
pub(crate) fn options(compression: Compression, threads: usize) -> EncodeOptions {
    EncodeOptions {
        compression,
        level: match compression {
            Compression::Zstd => 3,
            Compression::Gzip | Compression::None => 1,
        },
        threads: NonZeroUsize::new(threads).unwrap(),
        mtime: None,
    }
}

/// A base image: one layer blob in `dir`, a config listing it, as a
/// registry pull would leave it.
pub(crate) struct Base {
    pub(crate) config: ImageConfig,
    pub(crate) layer: BlobLayer,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) fn base(dir: &Path, from: Reference) -> Base {
    let bytes = b"base layer blob, gzip in a real image".to_vec();
    let path = dir.join("base-blob");
    std::fs::write(&path, &bytes).unwrap();
    let digest = Digest::parse(&sha256(&bytes)).unwrap();
    let diff_id = Digest::from_sha256_bytes(&[7; 32]);
    let blob = Descriptor::new(
        MediaType::DOCKER_LAYER_GZIP,
        digest,
        u64::try_from(bytes.len()).unwrap(),
    );
    let mut config = ImageConfig {
        architecture: "amd64".to_owned(),
        os: "linux".to_owned(),
        config: Some(ContainerConfig {
            env: Some(vec!["PATH=/usr/bin".to_owned()]),
            ..ContainerConfig::default()
        }),
        ..ImageConfig::default()
    };
    config.push_layer(
        diff_id.clone(),
        History {
            created: Some("2024-01-01T00:00:00Z".to_owned()),
            author: Some("someone".to_owned()),
            created_by: Some("ADD rootfs /".to_owned()),
            ..History::default()
        },
    );
    Base {
        config,
        layer: BlobLayer::base(diff_id, blob, path, from),
        bytes,
    }
}

/// An image of `base` plus one built layer, encoded in `dir`.
pub(crate) fn image(
    dir: &Path,
    base: &Base,
    time: &BuildTime,
    compression: Compression,
) -> rnk_builder::Image {
    image_in(dir, base, time, compression, image_format())
}

/// [`image`], asking for `format`.
pub(crate) fn image_in(
    dir: &Path,
    base: &Base,
    time: &BuildTime,
    compression: Compression,
    format: rnk_oci::ImageFormat,
) -> rnk_builder::Image {
    let exporter = LayerExporter::new(dir, options(compression, 4), time);
    let built = exporter.export(Box::new(layer(1, 20))).unwrap();
    let mut config = base.config.clone();
    config.push_layer(
        built.diff_id.clone(),
        instruction_history("RUN make # buildkit", time),
    );
    assemble(config, vec![base.layer.clone(), built], format, time).unwrap()
}

pub(crate) fn time() -> BuildTime {
    BuildTime::at(std::time::UNIX_EPOCH + Duration::from_secs(1_793_312_675))
}

/// A client for the plain-HTTP fake registry, with fast retries.
pub(crate) fn client(registry: &FakeRegistry) -> Arc<Client> {
    client_with(registry, Credentials::none())
}

/// A client with basic credentials for `registry`, from a `config.json`.
pub(crate) fn client_with_basic(
    registry: &FakeRegistry,
    user: &str,
    password: &str,
) -> Arc<Client> {
    use base64::Engine as _;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"auths":{{"{}":{{"auth":"{auth}"}}}}}}"#,
            registry.host()
        ),
    )
    .unwrap();
    client_with(registry, Credentials::load(&path).unwrap())
}

fn client_with(registry: &FakeRegistry, credentials: Credentials) -> Arc<Client> {
    client_over(&[registry], credentials)
}

/// A client for several plain-HTTP fake registries, without credentials.
pub(crate) fn client_for(registries: &[&FakeRegistry]) -> Arc<Client> {
    client_over(registries, Credentials::none())
}

fn client_over(registries: &[&FakeRegistry], credentials: Credentials) -> Arc<Client> {
    let mut options = ClientOptions::default();
    options.insecure = registries.iter().map(|r| r.host()).collect();
    options.http_credentials = options.insecure.clone();
    options.retry = RetryPolicy {
        attempts: 3,
        initial_delay: Duration::from_millis(1),
        factor: 2,
    };
    Arc::new(Client::new(options, TrustStore::load(), credentials).unwrap())
}

pub(crate) fn reference(registry: &FakeRegistry, rest: &str) -> Reference {
    Reference::parse(&format!("{}/{rest}", registry.host())).unwrap()
}
