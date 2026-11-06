//! The `rnk_snapshot.*` spans the metrics report reads (ADR-0025), on what
//! needs no mount: preparing and releasing a stage, extracting a base
//! layer, applying a cached layer, exporting a diff. `mount`, `umount` and `probe` run in the T2
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
        let base = [o.base_layer_from(&id, tar.as_slice(), &[]).unwrap()];
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

#[test]
fn walks_are_recorded_once_each_with_their_counts() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("d")).unwrap();
    fs::write(dir.path().join("d/f"), b"12345").unwrap();
    let root = rnk_rootfs::RootPath::open(fs::canonicalize(dir.path()).unwrap()).unwrap();
    let ignore = rnk_rootfs::IgnoreList::new();
    capture.run(|| {
        let mut walker = rnk_snapshot::Walker::new(rnk_snapshot::WalkOptions::default()).unwrap();
        walker.baseline(&root, &ignore).unwrap();
        fs::write(dir.path().join("d/g"), b"678").unwrap();
        walker.diff(&root, &ignore).unwrap();
    });
    let report = capture.report();
    let phases = &report["phases"];
    assert_eq!(phases["rnk_snapshot.baseline"]["count"], 1);
    assert_eq!(phases["rnk_snapshot.baseline"]["entries"], 2);
    assert_eq!(phases["rnk_snapshot.walk"]["count"], 2);
    assert_eq!(phases["rnk_snapshot.walk"]["entries"], 5);
    // The racy window keeps both fresh files from the cache.
    assert_eq!(phases["rnk_snapshot.walk"]["files_hashed"], 3);
    assert_eq!(phases["rnk_snapshot.walk"]["bytes_hashed"], 13);
}

#[test]
fn a_walk_diff_export_and_its_diff_are_recorded() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("d")).unwrap();
    fs::write(dir.path().join("d/old"), b"x").unwrap();
    let root = rnk_rootfs::RootPath::open(fs::canonicalize(dir.path()).unwrap()).unwrap();
    let ignore = rnk_rootfs::IgnoreList::new();
    let mut walker = rnk_snapshot::Walker::new(rnk_snapshot::WalkOptions::default()).unwrap();
    walker.baseline(&root, &ignore).unwrap();
    fs::write(dir.path().join("d/new"), b"12345").unwrap();
    fs::remove_file(dir.path().join("d/old")).unwrap();
    let diff = walker.diff(&root, &ignore).unwrap();
    capture.run(|| {
        let options = EncodeOptions {
            compression: Compression::Gzip,
            level: 1,
            threads: NonZeroUsize::MIN,
            mtime: Some(0),
        };
        diff.export(&root, options, std::io::sink()).unwrap();
    });
    let report = capture.report();
    let phases = &report["phases"];
    assert_eq!(phases["rnk_snapshot.export"]["count"], 1);
    // `d` (its mtime changed), `d/new` and the whiteout of `d/old`.
    assert_eq!(phases["rnk_snapshot.export"]["entries"], 3);
    assert_eq!(phases["rnk_snapshot.export"]["bytes_read"], 5);
    assert_eq!(phases["rnk_snapshot.diff"]["files_added"], 2);
    assert_eq!(phases["rnk_snapshot.diff"]["whiteouts"], 1);
    assert_eq!(phases["rnk_snapshot.diff"]["bytes"], 5);
}

#[test]
fn a_kaniko_stage_directory_records_prepare_and_release() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let state = fs::canonicalize(dir.path()).unwrap().join("state");
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let (tar, id) = empty_layer();
    let blob = dir.path().join("layer.tar");
    fs::write(&blob, &tar).unwrap();
    capture.run(|| {
        let stages = rnk_snapshot::KanikoStages::new(
            &store,
            rnk_rootfs::IgnoreList::new(),
            rnk_snapshot::WalkOptions::default(),
        )
        .unwrap();
        let base = [stages.base_layer(&id, &blob, &[]).unwrap()];
        let stage = stages.prepare(Base::Image(&base)).unwrap();
        stage.release().unwrap();
    });
    let report = capture.report();
    let phases = &report["phases"];
    assert_eq!(phases["rnk_snapshot.prepare"]["count"], 1);
    assert_eq!(phases["rnk_snapshot.prepare"]["layers"], 1);
    assert_eq!(phases["rnk_rootfs.extract"]["count"], 1);
    // The baseline walk waits for the first layer (lazy snapshots, ADR-0038).
    assert!(phases["rnk_snapshot.baseline"].is_null(), "{report}");
    // Releasing deletes the stage directory through descriptors.
    assert_eq!(phases["rnk_snapshot.release"]["count"], 1);
    assert_eq!(phases["rnk_rootfs.delete"]["count"], 1);
}

/// A step walked, a cached layer whose blob is gone, then the step run in
/// its place.
fn step_after_a_layer_that_fails_to_open<R: StageRootfs>(
    root: &mut R,
    apply: impl FnOnce(&mut R) -> rnk_snapshot::Result<()>,
) {
    root.set_layers_needed(true);
    root.begin_layer().unwrap();
    drop(root.end_layer().unwrap());
    assert!(apply(root).is_err());
    root.begin_layer().unwrap();
    drop(root.end_layer().unwrap());
}

/// A cached layer that fails to open changed nothing: the walk's record
/// still holds, so the step run in its place takes no second baseline.
#[test]
fn a_cached_layer_that_fails_to_open_takes_no_new_baseline() {
    use std::os::unix::fs::PermissionsExt as _;
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let top = fs::canonicalize(dir.path()).unwrap();
    let store = Store::open(&BuildPaths::new(&top, top.join("state"))).unwrap();
    let (_, id) = empty_layer();
    let missing = top.join("missing.tar");
    let root = top.join("root");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(top.join("kept")).unwrap();
    capture.run(|| {
        let stages = rnk_snapshot::KanikoStages::new(
            &store,
            rnk_rootfs::IgnoreList::new(),
            rnk_snapshot::WalkOptions::default(),
        )
        .unwrap();
        let gone = stages.base_layer(&id, &missing, &[]).unwrap();
        let mut stage = stages.prepare(Base::Scratch).unwrap();
        step_after_a_layer_that_fails_to_open(&mut stage, |s| s.apply_layer(&gone));
        stage.release().unwrap();

        let shared = rnk_snapshot::Kaniko::new(
            rnk_rootfs::RootPath::open(&root).unwrap(),
            root.clone(),
            rnk_rootfs::IgnoreList::new(),
            rnk_snapshot::WalkOptions::default(),
            rnk_rootfs::RootPath::open(top.join("kept")).unwrap(),
        );
        let gone = shared.base_layer(&id, &missing, &[]).unwrap();
        let mut held = shared.prepare(Base::Scratch).unwrap();
        step_after_a_layer_that_fails_to_open(&mut held, |r| r.apply_layer(&gone));
        held.release().unwrap();
    });
    let report = capture.report();
    let phases = &report["phases"];
    assert_eq!(phases["rnk_snapshot.apply"]["count"], 2);
    assert_eq!(phases["rnk_snapshot.baseline"]["count"], 2, "one per root");
}

#[test]
fn a_cached_layer_applied_to_a_kaniko_stage_is_recorded() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let state = fs::canonicalize(dir.path()).unwrap().join("state");
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let (tar, id) = empty_layer();
    let blob = dir.path().join("layer.tar");
    fs::write(&blob, &tar).unwrap();
    capture.run(|| {
        let stages = rnk_snapshot::KanikoStages::new(
            &store,
            rnk_rootfs::IgnoreList::new(),
            rnk_snapshot::WalkOptions::default(),
        )
        .unwrap();
        let layer = stages.base_layer(&id, &blob, &[]).unwrap();
        let mut stage = stages.prepare(Base::Scratch).unwrap();
        stage.apply_layer(&layer).unwrap();
        stage.release().unwrap();
    });
    let report = capture.report();
    let phases = &report["phases"];
    assert_eq!(phases["rnk_snapshot.apply"]["count"], 1);
    // The extraction is a base layer's, nested in it.
    assert_eq!(phases["rnk_layer.decode"]["count"], 1);
    assert_eq!(phases["rnk_rootfs.extract"]["count"], 1);
    // No walk: the next layer opened takes the baseline.
    assert!(phases["rnk_snapshot.baseline"].is_null(), "{report}");
}
