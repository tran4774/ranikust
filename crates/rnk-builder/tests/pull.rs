//! A pulled image's owners are checked, every layer's, before the first
//! is extracted (ADR-0098).

#[path = "../../rnk-registry/tests/registry/fake.rs"]
mod fake;
#[path = "outputs/support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rnk_builder::Error;
use rnk_oci::{
    Descriptor, Digest, History, ImageConfig, ImageFormat, ImageManifest, MediaType, Platform,
};
use rnk_rootfs::Unmapped;
use rnk_snapshot::{Base, OverlayStage, Snapshotter};
use rnk_store::{BuildPaths, Store};

use crate::fake::{Auth, FakeRegistry, sha256};
use crate::support::{client, reference};

/// Records the calls `pull_image` makes, and refuses the owners when
/// `refuse` is set.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
    refuse: bool,
}

impl Recorder {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Snapshotter for Recorder {
    type Root = OverlayStage;
    type Layer = Digest;

    fn base_layer(
        &self,
        diff_id: &Digest,
        _: &Path,
        below: &[Digest],
    ) -> rnk_snapshot::Result<Digest> {
        let call = format!("extract {} above {}", short(diff_id), below.len());
        self.calls.lock().unwrap().push(call);
        Ok(diff_id.clone())
    }

    fn check_owners(&self, layers: &[(Digest, PathBuf)]) -> rnk_snapshot::Result<()> {
        let mut names = Vec::new();
        for (diff_id, blob) in layers {
            assert!(blob.is_file(), "{} is pulled", blob.display());
            names.push(short(diff_id));
        }
        self.calls
            .lock()
            .unwrap()
            .push(format!("check {}", names.join(" ")));
        if self.refuse {
            let found = Unmapped::chown(Path::new("/etc/shadow"), Some(3_000_000_000), None);
            return Err(rnk_rootfs::Error::Unmapped(found).into());
        }
        Ok(())
    }

    fn prepare(&self, _: Base<'_, OverlayStage, Digest>) -> rnk_snapshot::Result<OverlayStage> {
        Err(rnk_snapshot::Error::State("pull_image prepares no stage"))
    }
}

fn short(d: &Digest) -> String {
    d.encoded()[..4].to_owned()
}

/// An image of two layers in `registry` as `repo:1`; their diff IDs.
fn add_image(registry: &FakeRegistry, repo: &str) -> [Digest; 2] {
    let diff_ids = [
        Digest::from_sha256_bytes(&[0xaa; 32]),
        Digest::from_sha256_bytes(&[0xbb; 32]),
    ];
    let mut config = ImageConfig {
        architecture: "amd64".to_owned(),
        os: "linux".to_owned(),
        ..ImageConfig::default()
    };
    let mut layers = Vec::new();
    for (n, diff_id) in diff_ids.iter().enumerate() {
        config.push_layer(diff_id.clone(), History::default());
        let blob = format!("layer {n}, gzip in a real image").into_bytes();
        registry.add_blob(repo, &blob);
        layers.push(descriptor(MediaType::OCI_LAYER_GZIP, &blob));
    }
    let config = serde_json::to_vec(&config).unwrap();
    registry.add_blob(repo, &config);
    let manifest = ImageManifest::new(
        ImageFormat::Oci,
        descriptor(MediaType::OCI_CONFIG, &config),
        layers,
    );
    let manifest = serde_json::to_vec(&manifest).unwrap();
    registry.add_manifest(repo, "1", MediaType::OCI_MANIFEST.as_str(), &manifest);
    diff_ids
}

fn descriptor(media_type: MediaType, content: &[u8]) -> Descriptor {
    let size = u64::try_from(content.len()).unwrap();
    Descriptor::new(media_type, Digest::parse(&sha256(content)).unwrap(), size)
}

/// Pulls `repo:1` from `registry` into a new store with `snapshotter`.
fn pull(
    registry: &FakeRegistry,
    runtime: &tokio::runtime::Runtime,
    snapshotter: &Arc<Recorder>,
) -> rnk_builder::Result<rnk_builder::PulledImage<Digest>> {
    let dir = tempfile::tempdir().unwrap();
    let store =
        Arc::new(Store::open(&BuildPaths::new(dir.path(), dir.path().join("state"))).unwrap());
    let image = reference(registry, "base:1").to_string();
    runtime.block_on(rnk_builder::pull_image(
        &client(registry),
        &store,
        snapshotter,
        &image,
        &Platform::new("linux", "amd64"),
        false,
    ))
}

/// Every layer's owners are checked once, all layers together, before the
/// first is extracted; then each is extracted in image order.
#[test]
fn every_layer_s_owners_are_checked_before_the_first_is_extracted() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime.block_on(FakeRegistry::start(Auth::Open));
    let [a, b] = add_image(&registry, "base");
    let recorder = Arc::new(Recorder::default());
    let pulled = pull(&registry, &runtime, &recorder).unwrap();
    assert_eq!(pulled.layers, [a.clone(), b.clone()]);
    assert_eq!(
        recorder.calls(),
        [
            format!("check {} {}", short(&a), short(&b)),
            format!("extract {} above 0", short(&a)),
            format!("extract {} above 1", short(&b)),
        ]
    );
}

/// A refusal of the owners ends the pull before any layer is extracted,
/// naming the image, with the refusal among its sources.
#[test]
fn a_refused_owner_ends_the_pull_before_any_extraction() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime.block_on(FakeRegistry::start(Auth::Open));
    let [a, b] = add_image(&registry, "base");
    let recorder = Arc::new(Recorder {
        refuse: true,
        ..Recorder::default()
    });
    let e = pull(&registry, &runtime, &recorder).unwrap_err();
    assert!(matches!(&e, Error::Image { .. }), "{e:?}");
    let found = std::iter::successors(Some(&e as &dyn std::error::Error), |e| e.source())
        .find_map(|e| e.downcast_ref::<Unmapped>());
    assert!(found.is_some(), "{e:?}");
    assert_eq!(
        recorder.calls(),
        [format!("check {} {}", short(&a), short(&b))]
    );
}
