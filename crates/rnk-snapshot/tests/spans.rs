//! The `rnk_snapshot.*` spans the metrics report reads (ADR-0025), on what
//! needs no mount: preparing and releasing a stage, extracting a base
//! layer, exporting a diff. `mount`, `umount` and `probe` run in the T2
//! tests only.
//!
//! A test binary of its own: every test creates its [`Capture`] before it
//! reaches a span, as tracing caches per process whether a span is wanted.

use std::fs;
use std::num::NonZeroUsize;
use std::sync::Arc;

use rnk_layer::{Compression, EncodeOptions};
use rnk_metrics::Capture;
use rnk_oci::Digest;
use rnk_snapshot::{Base, MountApi, Overlay, OverlayCaps, Snapshotter, StageRootfs};
use rnk_store::{BuildPaths, Store};
use sha2::{Digest as _, Sha256};

fn overlay(dir: &std::path::Path) -> Overlay {
    let store = Store::open(&BuildPaths::new(dir, dir.join("state"))).unwrap();
    Overlay::new(
        Arc::new(store),
        OverlayCaps {
            api: MountApi::Mount2,
        },
    )
    .unwrap()
}

/// An empty tar stream and its diff ID.
fn empty_layer() -> (Vec<u8>, Digest) {
    let tar = vec![0; 1024];
    let hash: [u8; 32] = Sha256::digest(&tar).into();
    (tar, Digest::from_sha256_bytes(&hash))
}

#[test]
fn prepare_extract_and_release_are_recorded() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    capture.run(|| {
        let o = overlay(dir.path());
        let (tar, id) = empty_layer();
        let base = [o.base_layer(&id, tar.as_slice(), &[]).unwrap()];
        let stage = o.prepare(Base::Image(&base)).unwrap();
        stage.release().unwrap();
    });
    let report = capture.report();
    let phases = &report["phases"];
    assert_eq!(phases["rnk_snapshot.prepare"]["count"], 1);
    assert_eq!(phases["rnk_snapshot.prepare"]["layers"], 1);
    assert_eq!(phases["rnk_snapshot.release"]["count"], 1);
    // Extraction nests the decoder's and the extractor's spans.
    assert_eq!(phases["rnk_layer.decode"]["count"], 1);
    assert_eq!(phases["rnk_rootfs.extract"]["count"], 1);
}

#[test]
fn export_and_its_diff_are_recorded() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    // A diff by hand: no mount needed to export one.
    let upper = dir.path().join("upper");
    fs::create_dir(&upper).unwrap();
    fs::write(upper.join("f"), b"12345").unwrap();
    let diff = rnk_snapshot::LayerDiff::new(upper.clone(), Vec::new());
    capture.run(|| {
        let options = EncodeOptions {
            compression: Compression::Gzip,
            level: 1,
            threads: NonZeroUsize::MIN,
            mtime: Some(0),
        };
        diff.export(options, std::io::sink()).unwrap();
    });
    let report = capture.report();
    let phases = &report["phases"];
    assert_eq!(phases["rnk_snapshot.export"]["count"], 1);
    assert_eq!(phases["rnk_snapshot.export"]["entries"], 1);
    assert_eq!(phases["rnk_snapshot.export"]["bytes_read"], 5);
    assert_eq!(phases["rnk_snapshot.diff"]["files_added"], 1);
    assert_eq!(phases["rnk_snapshot.diff"]["bytes"], 5);
}
