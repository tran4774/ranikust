//! Outputs under adverse conditions: planted files, shared directories,
//! token registries, refused mounts and failing destinations.

use std::num::NonZeroUsize;
use std::os::unix::fs::symlink;
use std::sync::Arc;
use std::thread;

use hyper::Method;
use rnk_builder::{BuildTime, Error, LayerExporter};
use rnk_layer::Compression;
use rnk_oci::{ImageFormat, MediaType, OciLayout, Reference};

use crate::fake::{Auth, FakeRegistry, Fault, sha256};
use crate::support::{
    base, client, client_with_basic, image, image_in, layer, options, reference, time,
};

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn an_output_never_follows_a_planted_name_and_leaves_nothing_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let base = base(blobs.path(), Reference::parse("r.example/base:1").unwrap());
    let image = image(blobs.path(), &base, &time(), Compression::Gzip);
    let out = dir.path().join("image.tar");
    // A victim behind the name an earlier version used for its temporary
    // file, and behind the output's own name pattern.
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"keep me").unwrap();
    symlink(&victim, dir.path().join(".image.tar.ranikust-tmp")).unwrap();

    rnk_builder::write_docker_tar(&image, &["r.example/a:1".to_owned()], &out).unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
    assert!(std::fs::symlink_metadata(&out).unwrap().is_file());

    // A layer blob that is gone: the write fails, and neither the output nor
    // a temporary file is left.
    let mut broken = image.clone();
    broken.layers[1].bytes = rnk_builder::LayerBytes::File(blobs.path().join("missing"));
    std::fs::remove_file(&out).unwrap();
    let err = rnk_builder::write_docker_tar(&broken, &[], &out).unwrap_err();
    assert!(matches!(err, Error::Layer(_)), "{err}");
    assert_eq!(names(dir.path()), [".image.tar.ranikust-tmp", "victim"]);

    let layout = dir.path().join("layout");
    let err = rnk_builder::write_oci_layout(&broken, &layout).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    let sha = layout.join("blobs/sha256");
    assert!(
        names(&sha).iter().all(|n| !n.starts_with('.')),
        "{:?}",
        names(&sha)
    );
}

#[test]
fn exporters_sharing_a_directory_do_not_collide() {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let workers: Vec<_> = (0..4u8)
        .map(|w| {
            let dir = Arc::clone(&dir);
            thread::spawn(move || {
                let exporter =
                    LayerExporter::new(dir.path(), options(Compression::Gzip, 1), &time());
                (0..5u8)
                    .map(|i| exporter.export(Box::new(layer(w * 10 + i, 3))).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let blobs: Vec<_> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    for blob in &blobs {
        let bytes = std::fs::read(blob.path().unwrap()).unwrap();
        assert_eq!(sha256(&bytes), blob.blob.digest.as_str());
    }
    assert_eq!(names(dir.path()).len(), 20, "20 blobs, no temporary file");
}

/// ADR-0058: a layer without entries is dropped before it gets a name;
/// nothing is left in the directory.
#[test]
fn an_empty_layer_dropped_as_empty_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let exporter = LayerExporter::new(dir.path(), options(Compression::Gzip, 1), &time());
    let empty = rnk_builder::EntriesLayer::new(
        Vec::new(),
        crate::support::Files(std::collections::BTreeMap::new()),
    );
    assert!(
        exporter
            .export_unless_empty(Box::new(empty))
            .unwrap()
            .is_none()
    );
    assert!(names(dir.path()).is_empty(), "{:?}", names(dir.path()));
    let blob = exporter
        .export_unless_empty(Box::new(layer(1, 1)))
        .unwrap()
        .unwrap();
    assert_ne!(blob.diff_id.encoded(), rnk_builder::EMPTY_LAYER_DIFF_ID);
    assert_eq!(names(dir.path()).len(), 1);
}

#[test]
fn a_reproducible_exporter_clamps_mtimes_to_the_fixed_time() {
    let dir = tempfile::tempdir().unwrap();
    // The fake layer's entries are dated 1_700_000_000 to 1_700_000_003: an
    // epoch before them replaces them, one after keeps them, as BuildKit
    // clamps (Q-161).
    for (epoch, low, high) in [
        ("1600000000", 1_600_000_000, 1_600_000_000),
        ("1700000123", 1_700_000_000, 1_700_000_003),
    ] {
        let time = BuildTime::reproducible(Some(epoch)).unwrap();
        // `options` leaves `mtime` unset: the exporter takes it from the time.
        let exporter = LayerExporter::new(dir.path(), options(Compression::None, 2), &time);
        assert_eq!(exporter.options().mtime, time.layer_mtime());
        let blob = exporter.export(Box::new(layer(1, 4))).unwrap();
        let tar = std::fs::read(blob.path().unwrap()).unwrap();
        let mut archive = tar::Archive::new(&tar[..]);
        for entry in archive.entries().unwrap() {
            let mtime = entry.unwrap().header().mtime().unwrap();
            assert!((low..=high).contains(&mtime), "{epoch}: {mtime}");
        }
    }
}

#[test]
fn a_layer_without_a_docker_type_makes_the_image_oci() {
    let dir = tempfile::tempdir().unwrap();
    let mut base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    base.layer.blob.media_type = MediaType::OCI_LAYER_ZSTD;
    let time = BuildTime::reproducible(None).unwrap();
    // Docker v2s2 asked for, but the base's zstd layer has no Docker media
    // type.
    let image = image_in(
        dir.path(),
        &base,
        &time,
        Compression::Gzip,
        ImageFormat::Docker,
    );
    assert_eq!(image.format, ImageFormat::Oci);
    assert_eq!(image.manifest.media_type(), &MediaType::OCI_MANIFEST);
    assert_eq!(image.layers[0].blob.media_type, MediaType::OCI_LAYER_ZSTD);
    assert_eq!(image.layers[1].blob.media_type, MediaType::OCI_LAYER_GZIP);
}

#[test]
fn the_oci_layout_rewrites_a_blob_that_only_has_the_right_size() {
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let image = image(dir.path(), &base, &time(), Compression::Gzip);
    let out = dir.path().join("layout");
    rnk_builder::write_oci_layout(&image, &out).unwrap();
    let path = out.join(OciLayout::blob_path(&base.layer.blob.digest));
    let wrong = vec![b'x'; base.bytes.len()];
    std::fs::write(&path, &wrong).unwrap();

    rnk_builder::write_oci_layout(&image, &out).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), base.bytes);
}

#[test]
fn a_docker_tar_refuses_a_layer_type_it_cannot_name() {
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let mut image = image(dir.path(), &base, &time(), Compression::Gzip);
    image.layers[0].blob.media_type = MediaType::new("application/x-unknown");
    let err = rnk_builder::write_docker_tar(&image, &[], &dir.path().join("x.tar")).unwrap_err();
    assert!(matches!(err, Error::UnknownLayerType { .. }), "{err}");
}

/// A token registry with basic credentials: the mount of the base layer
/// needs a token for both repositories.
#[tokio::test]
async fn push_mounts_on_a_token_registry() {
    let registry = FakeRegistry::start(Auth::Bearer {
        user: "builder".to_owned(),
        password: "s3cret".to_owned(),
        refresh_token: "unused".to_owned(),
        anonymous_pull: false,
    })
    .await;
    let client = client_with_basic(&registry, "builder", "s3cret");
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), reference(&registry, "base:1"));
    registry.add_blob("base", &base.bytes);
    let image = image(dir.path(), &base, &time(), Compression::Gzip);

    rnk_builder::push(
        &image,
        &[reference(&registry, "out:v1")],
        &client,
        NonZeroUsize::new(4).unwrap(),
    )
    .await
    .unwrap();
    // The config and the built layer are uploaded; the base layer mounted.
    assert_eq!(registry.count("PUT /v2/out/blobs/uploads/"), 2);
    assert_eq!(registry.count("POST /v2/out/blobs/uploads/?mount="), 1);
    assert!(
        registry.state().blob_repos[base.layer.blob.digest.as_str()]
            .iter()
            .any(|r| r == "out")
    );
}

#[tokio::test]
async fn a_refused_mount_falls_back_to_an_upload() {
    let registry = FakeRegistry::start(Auth::Open).await;
    registry.state().refuse_mounts = true;
    let client = client(&registry);
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), reference(&registry, "base:1"));
    registry.add_blob("base", &base.bytes);
    let image = image(dir.path(), &base, &time(), Compression::Gzip);

    rnk_builder::push(
        &image,
        &[reference(&registry, "out:v1")],
        &client,
        NonZeroUsize::new(2).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(registry.count("POST /v2/out/blobs/uploads/?mount="), 1);
    assert_eq!(
        registry.count("PUT /v2/out/blobs/uploads/"),
        3,
        "all three uploaded"
    );
}

#[tokio::test]
async fn a_failing_destination_names_itself_and_what_was_pushed() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let client = client(&registry);
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), reference(&registry, "base:1"));
    registry.add_blob("base", &base.bytes);
    let image = image(dir.path(), &base, &time(), Compression::Gzip);
    registry.state().faults.push(Fault {
        method: Method::PUT,
        path_contains: "/v2/two/manifests/".to_owned(),
        status: 400,
        remaining: 100,
    });
    let one = reference(&registry, "one:v1");
    let two = reference(&registry, "two:v1");

    let err = rnk_builder::push(
        &image,
        &[one.clone(), two.clone(), reference(&registry, "three:v1")],
        &client,
        NonZeroUsize::new(2).unwrap(),
    )
    .await
    .unwrap_err();
    let Error::Push {
        destination,
        pushed,
        source,
    } = err
    else {
        panic!("{err}");
    };
    assert_eq!(*destination, two);
    assert_eq!(pushed, [(one, image.digest().clone())]);
    assert!(matches!(*source, Error::Registry(_)), "{source}");
    assert_eq!(registry.count("HEAD /v2/three/"), 0, "later ones not tried");
}
