//! Lazy export, image assembly and the three outputs, end to end on fake
//! layers: T0, no root, a loopback fake registry.

#[path = "../../../rnk-registry/tests/registry/fake.rs"]
mod fake;
mod robust;
mod support;

use std::collections::BTreeMap;
use std::io::Read as _;
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Command;

use rnk_builder::{
    BuildTime, Error, LayerDemand, LayerExporter, Outputs, UNSET_DESTINATION, assemble,
    image_format,
};
use rnk_layer::Compression;
use rnk_oci::{
    ImageConfig, ImageFormat, ImageIndex, ImageManifest, MediaType, OciLayout, Reference,
};

use crate::fake::{Auth, FakeRegistry, sha256};
use crate::support::{base, client, client_for, image, layer, options, reference, time};

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn layer_bytes_do_not_depend_on_the_thread_count() {
    let time = BuildTime::reproducible(None).unwrap();
    for compression in [Compression::Gzip, Compression::Zstd, Compression::None] {
        let mut seen = Vec::new();
        for threads in [1, 2, 8] {
            let dir = tempfile::tempdir().unwrap();
            let exporter = LayerExporter::new(dir.path(), options(compression, threads), &time);
            let blob = exporter.export(Box::new(layer(3, 60))).unwrap();
            let bytes = std::fs::read(blob.path().unwrap()).unwrap();
            assert_eq!(sha256(&bytes), blob.blob.digest.as_str());
            assert_eq!(u64::try_from(bytes.len()).unwrap(), blob.blob.size);
            assert_eq!(
                files(dir.path()),
                [blob.blob.digest.encoded()],
                "no temporary file left"
            );
            seen.push((blob.diff_id, blob.blob.digest, bytes));
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "{compression}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exported_layers_are_pushed_during_the_build_then_released() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let client = client(&registry);
    let state = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(rnk_store::Store::open_anonymous(state.path()).unwrap());
    let destinations = vec![
        reference(&registry, "out:v1"),
        reference(&registry, "copy:v1"),
    ];
    let uploader = rnk_builder::Uploader::start(
        client,
        destinations,
        NonZeroUsize::new(2).unwrap(),
        rnk_builder::Release::Free(std::sync::Arc::clone(&store)),
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    let time = BuildTime::reproducible(None).unwrap();
    let kept = std::sync::Arc::clone(&store);
    let exporter = LayerExporter::anonymous(store, options(Compression::Gzip, 2), &time)
        .with_uploader(uploader);
    let push = rnk_builder::LayerDemand {
        push: true,
        target: true,
        ..rnk_builder::LayerDemand::default()
    };
    let (blobs, uploaded) = tokio::task::spawn_blocking(move || {
        let mut blobs = Vec::new();
        for seed in 0..3 {
            let blob = exporter.export(Box::new(layer(seed, 40))).unwrap();
            exporter.exported(&blob, push);
            blobs.push(blob);
        }
        // The same layer again is not sent twice.
        exporter.exported(&blobs[0], push);
        let handle = tokio::runtime::Handle::current();
        let uploaded = handle.block_on(exporter.finish_uploads()).unwrap();
        (blobs, uploaded)
    })
    .await
    .unwrap();

    {
        let state = registry.state();
        for blob in &blobs {
            assert!(uploaded.contains(&blob.blob.digest));
            let repos = &state.blob_repos[blob.blob.digest.as_str()];
            assert!(repos.iter().any(|r| r == "out") && repos.iter().any(|r| r == "copy"));
            // Released once pushed: the store no longer holds it. Never read
            // the old name: its descriptor number may name another file now.
            assert!(!kept.release_anonymous_file(blob.path().unwrap()));
        }
    }
    assert_eq!(registry.count("PUT /v2/out/blobs/uploads/"), 3);
    assert_eq!(
        registry.count("PUT /v2/copy/blobs/uploads/"),
        0,
        "mounted from out"
    );
}

/// An uploader to `destination` with one upload at a time (and one more
/// waiting), releasing into an anonymous store; the exporter that feeds it,
/// and the store to check releases against.
fn uploading(
    registry: &FakeRegistry,
    destination: &str,
) -> (
    LayerExporter,
    std::sync::Arc<rnk_store::Store>,
    tempfile::TempDir,
) {
    let state = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(rnk_store::Store::open_anonymous(state.path()).unwrap());
    let uploader = rnk_builder::Uploader::start(
        client(registry),
        vec![reference(registry, destination)],
        NonZeroUsize::new(1).unwrap(),
        rnk_builder::Release::Free(std::sync::Arc::clone(&store)),
        &tokio::runtime::Handle::current(),
    )
    .unwrap();
    let time = BuildTime::reproducible(None).unwrap();
    let exporter = LayerExporter::anonymous(
        std::sync::Arc::clone(&store),
        options(Compression::Gzip, 2),
        &time,
    )
    .with_uploader(uploader);
    (exporter, store, state)
}

const PUSH: rnk_builder::LayerDemand = rnk_builder::LayerDemand {
    push: true,
    target: true,
    cache: false,
    output: false,
    saved: false,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_upload_releases_every_layer_handed_over() {
    let registry = FakeRegistry::start(Auth::Open).await;
    registry.state().faults.push(crate::fake::Fault {
        method: hyper::Method::POST,
        path_contains: "/blobs/uploads/".to_owned(),
        status: 503,
        remaining: 1000,
    });
    let (exporter, store, _state) = uploading(&registry, "out:v1");
    let exporter = std::sync::Arc::new(exporter);
    let feeding = std::sync::Arc::clone(&exporter);
    let blobs = tokio::task::spawn_blocking(move || {
        (0..3)
            .map(|seed| {
                let blob = feeding.export(Box::new(layer(seed, 40))).unwrap();
                feeding.exported(&blob, PUSH);
                blob
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert!(exporter.finish_uploads().await.is_err());
    for blob in &blobs {
        // Released, whether it was running, waiting or refused. Never read
        // the old name: its descriptor number may name another file now.
        assert!(!store.release_anonymous_file(blob.path().unwrap()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_wait_for_room_and_a_cancel_stops_them_and_releases_their_layers() {
    let registry = FakeRegistry::start(Auth::Open).await;
    // The registry never answers an upload: the first layer's runs forever.
    registry.state().stalled.push("/blobs/uploads/".to_owned());
    let (exporter, store, _state) = uploading(&registry, "out:v1");
    let exporter = std::sync::Arc::new(exporter);
    let feeding = std::sync::Arc::clone(&exporter);
    let (sent, mut fed) = tokio::sync::mpsc::unbounded_channel();
    let feeder = std::thread::spawn(move || {
        for seed in 0..3 {
            let blob = feeding.export(Box::new(layer(seed, 40))).unwrap();
            feeding.exported(&blob, PUSH);
            let _ = sent.send(blob);
        }
    });
    // One runs, one waits: the third waits for room.
    let mut blobs = Vec::new();
    for _ in 0..2 {
        blobs.push(fed.recv().await.unwrap());
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(fed.try_recv().is_err(), "a third layer waits for room");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        exporter.cancel_uploads(),
    )
    .await
    .expect("a cancel does not wait for the registry");
    blobs.push(fed.recv().await.unwrap());
    feeder.join().unwrap();
    for blob in &blobs {
        assert!(!store.release_anonymous_file(blob.path().unwrap()));
    }
}

#[test]
fn an_anonymous_exporter_writes_the_same_bytes_and_names_nothing() {
    let time = BuildTime::reproducible(None).unwrap();
    let named_dir = tempfile::tempdir().unwrap();
    let named = LayerExporter::new(named_dir.path(), options(Compression::Gzip, 2), &time)
        .export(Box::new(layer(4, 30)))
        .unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(rnk_store::Store::open_anonymous(state.path()).unwrap());
    // The blob lives as long as the store.
    let anon = LayerExporter::anonymous(store.clone(), options(Compression::Gzip, 2), &time)
        .export(Box::new(layer(4, 30)))
        .unwrap();
    assert!(
        anon.path().unwrap().starts_with("/proc/self/fd"),
        "{:?}",
        anon.path()
    );
    assert_eq!(
        std::fs::read(anon.path().unwrap()).unwrap(),
        std::fs::read(named.path().unwrap()).unwrap()
    );
    assert_eq!((anon.diff_id, anon.blob), (named.diff_id, named.blob));
    assert!(files(state.path()).is_empty(), "{:?}", files(state.path()));
}

#[test]
fn a_layer_nothing_needs_is_not_encoded() {
    let dir = tempfile::tempdir().unwrap();
    let exporter = LayerExporter::new(dir.path(), options(Compression::Gzip, 2), &time());
    let skipped = exporter
        .export_if_needed(Box::new(layer(1, 5)), LayerDemand::default())
        .unwrap();
    assert_eq!(skipped, None);
    assert!(files(dir.path()).is_empty());

    let demand = LayerDemand {
        output: true,
        ..LayerDemand::default()
    };
    assert!(
        exporter
            .export_if_needed(Box::new(layer(1, 5)), demand)
            .unwrap()
            .is_some()
    );
}

#[test]
fn layer_demand_follows_the_target_and_the_outputs() {
    let none = Outputs::new(&[], false, None, None).unwrap();
    let push = Outputs::new(&["r.example/app".to_owned()], true, None, None).unwrap();
    let tar = Outputs::new(&[], false, Some("/out.tar".into()), None).unwrap();

    // A stage used only through `COPY --from` needs nothing.
    for outputs in [&none, &push, &tar] {
        assert!(!LayerDemand::for_stage(false, outputs, false).needed());
    }
    // ... unless its layers go to the cache.
    assert!(LayerDemand::for_stage(false, &none, true).needed());
    // The target always: its image is assembled for the digest.
    assert!(LayerDemand::for_stage(true, &none, false).needed());
    let d = LayerDemand::for_stage(true, &push, false);
    assert!(d.push && d.target && !d.output && !d.cache);
    let d = LayerDemand::for_stage(true, &tar, false);
    assert!(d.output && !d.push);
}

#[test]
fn outputs_are_checked_like_kaniko() {
    let out = Outputs::new(
        &["alpine".to_owned(), "r.example:5000/a/b:v1".to_owned()],
        true,
        None,
        None,
    )
    .unwrap();
    let names: Vec<String> = out.destinations.iter().map(ToString::to_string).collect();
    assert_eq!(
        names,
        ["docker.io/library/alpine:latest", "r.example:5000/a/b:v1"]
    );
    // RepoTags as go-containerregistry writes them: as typed, `:latest`
    // added when no tag was given.
    assert_eq!(out.tar_tags(), ["alpine:latest", "r.example:5000/a/b:v1"]);

    let none = Outputs::new(&[], false, Some("/x.tar".into()), None).unwrap();
    assert_eq!(none.tar_tags(), [UNSET_DESTINATION]);

    for (dest, push) in [
        (vec!["alpine@sha256:".to_owned() + &"a".repeat(64)], true),
        (vec!["Not A Reference".to_owned()], false),
        (vec![], true),
    ] {
        assert!(
            matches!(
                Outputs::new(&dest, push, None, None),
                Err(Error::Destination { .. })
            ),
            "{dest:?}"
        );
    }
}

#[test]
fn assembly_needs_the_config_to_list_the_layers() {
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let err = assemble(
        ImageConfig::default(),
        vec![base.layer.clone()],
        ImageFormat::Oci,
        &time(),
    )
    .unwrap_err();
    assert!(matches!(err, Error::Inconsistent(_)), "{err}");
}

#[test]
fn assembly_converts_media_types_and_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let time = time();
    let image = image(dir.path(), &base, &time, Compression::Gzip);
    // OCI whatever the base's format, as BuildKit (Q-006); the Docker base
    // layer is converted.
    assert_eq!(image.format, ImageFormat::Oci, "the base is Docker v2s2");
    let manifest = ImageManifest::from_json(image.manifest.bytes()).unwrap();
    assert_eq!(manifest.media_type, Some(MediaType::OCI_MANIFEST));
    assert_eq!(manifest.config.media_type, MediaType::OCI_CONFIG);
    assert!(
        manifest
            .layers
            .iter()
            .all(|l| l.media_type == MediaType::OCI_LAYER_GZIP)
    );
    assert_eq!(manifest.config.digest.as_str(), sha256(&image.config));

    let config = ImageConfig::from_json(&image.config).unwrap();
    assert_eq!(config.created.as_deref(), Some("2026-10-29T22:24:35Z"));
    assert_eq!(config.rootfs.diff_ids.len(), 2);
    let history = config.history.unwrap();
    assert_eq!(
        history[0].author.as_deref(),
        Some("someone"),
        "base history kept"
    );
    assert_eq!(
        history[1].created_by.as_deref(),
        Some("RUN make # buildkit")
    );
    assert_eq!(history[1].author, None);
    assert_eq!(
        history[1].comment.as_deref(),
        Some(rnk_builder::HISTORY_COMMENT)
    );

    // Canonical JSON: keys sorted at every level, no whitespace.
    let text = std::str::from_utf8(&image.config).unwrap();
    assert!(
        text.starts_with(r#"{"architecture":"amd64","config":{"Env":"#),
        "{text}"
    );

    let again = support::image(
        tempfile::tempdir().unwrap().path(),
        &base,
        &time,
        Compression::Gzip,
    );
    assert_eq!(again.digest(), image.digest());

    // `--image-format=docker` (ADR-0036): Docker media types throughout.
    let docker = support::image_in(
        dir.path(),
        &base,
        &time,
        Compression::Gzip,
        ImageFormat::Docker,
    );
    assert_eq!(docker.format, ImageFormat::Docker);
    let m = ImageManifest::from_json(docker.manifest.bytes()).unwrap();
    assert_eq!(m.media_type, Some(MediaType::DOCKER_MANIFEST));
    assert!(
        m.layers
            .iter()
            .all(|l| l.media_type == MediaType::DOCKER_LAYER_GZIP)
    );

    // `--annotation` (ADR-0036): in the manifest, and nowhere when none.
    assert!(manifest.annotations.is_none());
    let annotated = rnk_builder::assemble_annotated(
        ImageConfig::from_json(&image.config).unwrap(),
        image.layers.clone(),
        ImageFormat::Oci,
        &time,
        &BTreeMap::from([("org.example.a".to_owned(), "1".to_owned())]),
    )
    .unwrap();
    let m = ImageManifest::from_json(annotated.manifest.bytes()).unwrap();
    assert_eq!(m.annotations.unwrap()["org.example.a"], "1");
    assert_ne!(annotated.digest(), image.digest());

    // zstd layers force OCI.
    let zstd = support::image(dir.path(), &base, &time, Compression::Zstd);
    assert_eq!(zstd.format, ImageFormat::Oci);
    assert_eq!(zstd.layers[0].blob.media_type, MediaType::OCI_LAYER_GZIP);
    assert_eq!(zstd.layers[1].blob.media_type, MediaType::OCI_LAYER_ZSTD);
}

#[test]
fn reproducible_assembly_keeps_base_history_like_buildkit() {
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let time = BuildTime::reproducible(Some("1700000000")).unwrap();
    assert_eq!(image_format(), ImageFormat::Oci);
    let image = image(dir.path(), &base, &time, Compression::Gzip);
    let config = ImageConfig::from_json(&image.config).unwrap();
    let stamp = Some("2023-11-14T22:13:20Z");
    assert_eq!(config.created.as_deref(), stamp);
    // The base image's entries keep their times and authors, as BuildKit
    // keeps them; the build's own carry the fixed time.
    let history = config.history.unwrap();
    assert_eq!(history[0].created.as_deref(), Some("2024-01-01T00:00:00Z"));
    assert_eq!(history[0].author.as_deref(), Some("someone"));
    assert_eq!(history[1].created.as_deref(), stamp);

    // Two builds at different times give one digest.
    let again = support::image(
        tempfile::tempdir().unwrap().path(),
        &base,
        &time,
        Compression::Gzip,
    );
    assert_eq!(again.digest(), image.digest());
}

#[test]
fn the_oci_layout_holds_every_blob_and_indexes_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let image = image(dir.path(), &base, &time(), Compression::Zstd);
    let out = dir.path().join("layout");
    rnk_builder::write_oci_layout(&image, &out).unwrap();
    // Twice: an existing layout is updated in place.
    rnk_builder::write_oci_layout(&image, &out).unwrap();

    assert_eq!(
        std::fs::read_to_string(out.join(OciLayout::FILE)).unwrap(),
        r#"{"imageLayoutVersion":"1.0.0"}"#
    );
    let index =
        ImageIndex::from_json(&std::fs::read(out.join(OciLayout::INDEX_FILE)).unwrap()).unwrap();
    assert_eq!(index.manifests, [image.manifest.descriptor()]);
    let mut digests = vec![
        image.digest().clone(),
        image.config_descriptor.digest.clone(),
    ];
    digests.extend(image.layers.iter().map(|l| l.blob.digest.clone()));
    for digest in digests {
        let bytes = std::fs::read(out.join(OciLayout::blob_path(&digest))).unwrap();
        assert_eq!(sha256(&bytes), digest.as_str());
    }
    assert!(
        !files(&out.join("blobs/sha256"))
            .iter()
            .any(|n| n.starts_with('.')),
        "no temporary file left"
    );

    if let Some(json) = skopeo_inspect(&format!("oci:{}", out.display())) {
        assert!(json.contains(image.digest().as_str()), "{json}");
    }
}

#[test]
fn the_docker_tar_is_what_docker_save_writes() {
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let image = image(dir.path(), &base, &time(), Compression::Gzip);
    // The same layer twice is stored once.
    let mut doubled = image.clone();
    let mut config = ImageConfig::from_json(&image.config).unwrap();
    let last = image.layers[1].clone();
    config.push_layer(
        last.diff_id.clone(),
        rnk_builder::instruction_history("COPY a b # buildkit", &time()),
    );
    doubled.layers.push(last);
    let doubled = assemble(config, doubled.layers, doubled.format, &time()).unwrap();

    let path = dir.path().join("image.tar");
    let tags = ["r.example/app:v1".to_owned()];
    rnk_builder::write_docker_tar(&doubled, &tags, &path).unwrap();
    let first = std::fs::read(&path).unwrap();
    rnk_builder::write_docker_tar(&doubled, &tags, &path).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        first,
        "the same bytes every time"
    );

    let mut entries = BTreeMap::new();
    for entry in tar::Archive::new(&first[..]).entries().unwrap() {
        let mut entry = entry.unwrap();
        let header = entry.header();
        assert_eq!(
            (
                header.mode().unwrap(),
                header.uid().unwrap(),
                header.mtime().unwrap()
            ),
            (0o644, 0, 0)
        );
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert!(entries.insert(name, bytes).is_none(), "each name once");
    }
    let config_name = doubled.config_descriptor.digest.to_string();
    let layer_names: Vec<String> = doubled
        .layers
        .iter()
        .map(|l| format!("{}.tar.gz", l.blob.digest.encoded()))
        .collect();
    let mut expected: Vec<&str> = vec![
        "manifest.json",
        &config_name,
        &layer_names[0],
        &layer_names[1],
    ];
    expected.sort_unstable();
    assert_eq!(
        entries.keys().map(String::as_str).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(entries[&config_name], doubled.config);
    assert_eq!(entries[&layer_names[0]], base.bytes);
    assert_eq!(
        entries[&layer_names[1]],
        std::fs::read(doubled.layers[1].path().unwrap()).unwrap()
    );

    let manifest: serde_json::Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(
        manifest,
        serde_json::json!([{
            "Config": config_name,
            "RepoTags": ["r.example/app:v1"],
            "Layers": [layer_names[0], layer_names[1], layer_names[1]],
        }])
    );

    if let Some(json) = skopeo_inspect(&format!("docker-archive:{}", path.display())) {
        assert!(json.contains("\"architecture\": \"amd64\""), "{json}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_base_layer_never_kept_is_pushed_from_its_source_registry() {
    // The base layer exists only in another registry: no file, no mount
    // possible (ADR-0055).
    let source = FakeRegistry::start(Auth::Open).await;
    let target = FakeRegistry::start(Auth::Open).await;
    let dir = tempfile::tempdir().unwrap();
    let from = reference(&source, "base:1");
    let base = base(dir.path(), from.clone());
    source.add_blob("base", &base.bytes);
    let mut image = image(dir.path(), &base, &time(), Compression::Gzip);
    let streamed = &mut image.layers[0];
    assert_eq!(streamed.mount_from.as_ref(), Some(&from));
    streamed.bytes = rnk_builder::LayerBytes::Registry(from);
    std::fs::remove_file(dir.path().join("base-blob")).unwrap();

    let client = client_for(&[&source, &target]);
    let out = reference(&target, "out:v1");
    rnk_builder::push(&image, &[out], &client, NonZeroUsize::new(2).unwrap())
        .await
        .unwrap();
    let state = target.state();
    assert_eq!(
        state.blobs[image.layers[0].blob.digest.as_str()].as_ref(),
        base.bytes.as_slice()
    );
    assert_eq!(
        state.manifests[&("out".to_owned(), "v1".to_owned())].1,
        image.manifest.bytes()
    );

    // An output written to a file needs the bytes: refused, not guessed.
    let err = rnk_builder::write_oci_layout(&image, &dir.path().join("layout")).unwrap_err();
    assert!(err.to_string().contains("not kept"), "{err}");
}

#[tokio::test]
async fn push_mounts_base_layers_uploads_the_rest_and_tags_the_manifest() {
    let registry = FakeRegistry::start(Auth::Open).await;
    let client = client(&registry);
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), reference(&registry, "base:1"));
    registry.add_blob("base", &base.bytes);
    let image = image(dir.path(), &base, &time(), Compression::Gzip);
    let one = reference(&registry, "out:v1");
    let two = reference(&registry, "copy:v2");

    let pushed = rnk_builder::push(
        &image,
        &[one.clone(), two.clone()],
        &client,
        NonZeroUsize::new(4).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        pushed,
        [(one, image.digest().clone()), (two, image.digest().clone())]
    );

    // `out`: the base layer mounted from `base`; the config and the built
    // layer uploaded. `copy`: everything mounted from `out`.
    assert_eq!(registry.count("PUT /v2/out/blobs/uploads/"), 2);
    assert_eq!(registry.count("PUT /v2/copy/blobs/uploads/"), 0);
    assert_eq!(registry.count("POST /v2/copy/blobs/uploads/?mount="), 3);
    {
        let state = registry.state();
        for repo in ["out", "copy"] {
            for layer in &image.layers {
                assert!(
                    state.blob_repos[layer.blob.digest.as_str()]
                        .iter()
                        .any(|r| r == repo)
                );
            }
        }
        let tag =
            |repo: &str, tag: &str| state.manifests[&(repo.to_owned(), tag.to_owned())].clone();
        assert_eq!(tag("out", "v1").1, image.manifest.bytes());
        assert_eq!(tag("copy", "v2").0, MediaType::OCI_MANIFEST.as_str());
    }

    // Pushing again sends no blob.
    let before = registry.count("PUT ");
    rnk_builder::push(
        &image,
        &[reference(&registry, "out:v1")],
        &client,
        NonZeroUsize::MIN,
    )
    .await
    .unwrap();
    assert_eq!(registry.count("PUT "), before + 1, "the manifest only");
}

/// `skopeo inspect` of `target`, or `None` without skopeo (rootless, reads
/// only the given file or directory).
fn skopeo_inspect(target: &str) -> Option<String> {
    let output = match Command::new("skopeo")
        .args(["inspect", "--config", target])
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping the skopeo check of {target}: skopeo is not installed");
            return None;
        }
        Err(e) => panic!("running skopeo: {e}"),
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = String::from_utf8(output.stdout).unwrap();
    let raw = Command::new("skopeo")
        .args(["inspect", "--raw", target])
        .output()
        .unwrap();
    assert!(
        raw.status.success(),
        "{}",
        String::from_utf8_lossy(&raw.stderr)
    );
    let manifest = String::from_utf8(raw.stdout).unwrap();
    Some(format!(
        "{config}\n{manifest}\n{}",
        sha256(manifest.as_bytes())
    ))
}
