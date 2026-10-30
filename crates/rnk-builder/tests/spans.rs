//! The `rnk_builder.*` spans the metrics report reads (ADR-0025).
//!
//! A test binary of its own, and every test creates its [`Capture`] before
//! it reaches a span: tracing caches whether a span is wanted once per
//! process, and a test reaching it with no subscriber alive could race a
//! `Capture` into caching "never".

#[path = "../../rnk-registry/tests/registry/fake.rs"]
mod fake;
#[path = "outputs/support.rs"]
mod support;

use std::num::NonZeroUsize;

use rnk_builder::{BuildTime, LayerDemand, LayerExporter};
use rnk_layer::Compression;
use rnk_metrics::Capture;
use rnk_oci::Reference;

use crate::fake::{Auth, FakeRegistry};
use crate::support::{base, client, image, layer, options, reference};

#[test]
fn layers_count_skipped_exports() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let exporter = LayerExporter::new(dir.path(), options(Compression::Gzip, 2, &support::time()));
    capture.run(|| {
        exporter
            .export_if_needed(Box::new(layer(1, 3)), LayerDemand::default())
            .unwrap();
        exporter
            .export_if_needed(
                Box::new(layer(2, 3)),
                LayerDemand {
                    target: true,
                    ..LayerDemand::default()
                },
            )
            .unwrap();
    });
    let report = capture.report();
    let layer = &report["phases"]["rnk_builder.layer"];
    assert_eq!(layer["count"], 2);
    assert_eq!(layer["skipped"], 1);
    assert_eq!(
        report["phases"]["rnk_layer.encode"]["count"], 1,
        "encoding nested in it"
    );
}

#[test]
fn assemble_and_outputs_count_bytes_and_layers() {
    let capture = Capture::new();
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let image = capture.run(|| image(dir.path(), &base, &support::time(), Compression::Gzip));
    let tar = dir.path().join("image.tar");
    capture.run(|| {
        rnk_builder::write_docker_tar(&image, &["r.example/a:1".to_owned()], &tar).unwrap();
        rnk_builder::write_oci_layout(&image, &dir.path().join("layout")).unwrap();
    });
    let report = capture.report();
    let assemble = &report["phases"]["rnk_builder.assemble"];
    assert_eq!(assemble["count"], 1);
    assert_eq!(
        assemble["bytes_out"],
        image.config_descriptor.size + image.manifest.size()
    );
    let output = &report["phases"]["rnk_builder.output"];
    assert_eq!(output["count"], 2);
    assert_eq!(output["layers"], 4);
    let tar_size = std::fs::metadata(&tar).unwrap().len();
    let blobs: u64 = image.layers.iter().map(|l| l.blob.size).sum();
    let layout = 30 + image.config_descriptor.size + image.manifest.size() + blobs;
    let index = std::fs::metadata(dir.path().join("layout/index.json"))
        .unwrap()
        .len();
    assert_eq!(output["bytes_out"], tar_size + layout + index);
}

#[test]
fn a_push_counts_uploaded_bytes() {
    let capture = Capture::new();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = runtime.block_on(FakeRegistry::start(Auth::Open));
    let client = client(&registry);
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), reference(&registry, "base:1"));
    registry.add_blob("base", &base.bytes);
    let image = image(dir.path(), &base, &support::time(), Compression::Gzip);
    capture.run(|| {
        runtime
            .block_on(rnk_builder::push(
                &image,
                &[reference(&registry, "out:v1")],
                &client,
                NonZeroUsize::new(2).unwrap(),
            ))
            .unwrap();
    });
    let report = capture.report();
    let output = &report["phases"]["rnk_builder.output"];
    assert_eq!(output["count"], 1);
    assert_eq!(output["layers"], 2);
    // The config and the built layer; the base layer was mounted.
    assert_eq!(
        output["bytes_out"],
        image.config_descriptor.size + image.layers[1].blob.size + image.manifest.size()
    );
}

#[test]
fn metrics_do_not_change_reproducible_digests() {
    let time = BuildTime::reproducible(None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let base = base(dir.path(), Reference::parse("r.example/base:1").unwrap());
    let without = image(dir.path(), &base, &time, Compression::Zstd);
    let capture = Capture::new();
    let with = capture.run(|| {
        image(
            tempfile::tempdir().unwrap().path(),
            &base,
            &time,
            Compression::Zstd,
        )
    });
    assert_eq!(with.digest(), without.digest());
}
