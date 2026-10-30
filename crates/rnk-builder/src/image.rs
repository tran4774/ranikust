//! Image assembly: the final config, its layers and the manifest.

use std::collections::BTreeMap;

use bytes::Bytes;
use rnk_oci::{
    Descriptor, Digest, History, ImageConfig, ImageFormat, ImageManifest, to_canonical_json,
};
use rnk_registry::RawManifest;
use sha2::Digest as _;
use tracing::debug_span;

use crate::error::{Error, Result};
use crate::layer::BlobLayer;
use crate::time::BuildTime;

/// The `comment` of every history entry a Dockerfile instruction adds, as
/// BuildKit writes it (`buildkit@cd8ee244
/// frontend/dockerfile/dockerfile2llb/convert.go:46`, Q-131, `ADR-0032`).
pub const HISTORY_COMMENT: &str = "buildkit.dockerfile.v0";

/// The history entry of one instruction, as BuildKit's `commitToHistory`
/// (`convert.go:1819-1831`): `created_by` is the instruction's text in
/// BuildKit's form (`RUN`, `COPY` and `ADD` end with ` # buildkit`); no
/// author; `created` is `time`. The caller pushes it with `push_layer` or
/// `push_history`, which set `empty_layer`.
pub fn instruction_history(created_by: &str, time: &BuildTime) -> History {
    History {
        created: Some(time.rfc3339()),
        author: None,
        created_by: Some(created_by.to_owned()),
        comment: Some(HISTORY_COMMENT.to_owned()),
        empty_layer: None,
    }
}

/// The format of the image a build writes: OCI media types, whatever the
/// base image's format and `--reproducible`, as BuildKit at compatibility
/// version 031 (`buildkit@cd8ee244 exporter/containerimage/export.go:576-581`,
/// `exporter/oci/export.go:302-307`; Q-006, `ADR-0032`). Layers are converted
/// to it ([`assemble`]).
pub fn image_format() -> ImageFormat {
    ImageFormat::Oci
}

/// An assembled image: canonical config and manifest bytes, and the layer
/// blobs in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// The format the config, manifest and layer media types are in.
    pub format: ImageFormat,
    /// The config's exact bytes.
    pub config: Bytes,
    /// The config's descriptor.
    pub config_descriptor: Descriptor,
    /// The manifest, with its digest: the image's digest.
    pub manifest: RawManifest,
    /// The layers, base first, with media types in [`Image::format`].
    pub layers: Vec<BlobLayer>,
}

impl Image {
    /// The image's digest: its manifest's.
    pub fn digest(&self) -> &Digest {
        self.manifest.digest()
    }
}

/// Assembles the image of a stage from its final `config` and its `layers`,
/// base first.
///
/// `config.rootfs.diff_ids` must list the layers' diff IDs in order; each
/// executor keeps it so with `ImageConfig::push_layer`. `created` is set to
/// `time`: the build's start, or under `--reproducible` the epoch, which is
/// what BuildKit's clamp of a later `created` to `SOURCE_DATE_EPOCH` gives
/// (`buildkit@cd8ee244 exporter/containerimage/writer.go:830-843`). History
/// entries are left as they are: the base image's keep their times, as
/// BuildKit keeps them (`writer.go:809-821`), and the build's own already
/// carry `time`.
///
/// `format` is the format asked for ([`image_format`]); the image is written
/// in OCI instead when a layer has no Docker media type, such as a zstd base
/// layer: a pulled blob's bytes, and so its compression, do not change
/// (Q-160). [`Image::format`] is the format written.
///
/// # Errors
///
/// [`Error::Inconsistent`] when the diff IDs disagree, [`Error::Oci`] when a
/// layer's media type has no OCI equivalent either or serializing fails.
pub fn assemble(
    config: ImageConfig,
    layers: Vec<BlobLayer>,
    format: ImageFormat,
    time: &BuildTime,
) -> Result<Image> {
    assemble_annotated(config, layers, format, time, &BTreeMap::new())
}

/// [`assemble`], with `annotations` in the manifest: kaniko's
/// `--annotation` (`kaniko@44aa7ddc0 pkg/executor/build.go:2128`). The base
/// image's annotations are not carried over, as in BuildKit and kaniko's
/// default (`FF_KANIKO_NO_PROPAGATE_ANNOTATIONS`). Empty: no `annotations`
/// field. ADR-0036.
///
/// # Errors
///
/// As [`assemble`].
pub fn assemble_annotated(
    mut config: ImageConfig,
    mut layers: Vec<BlobLayer>,
    format: ImageFormat,
    time: &BuildTime,
    annotations: &BTreeMap<String, String>,
) -> Result<Image> {
    let format = match format {
        ImageFormat::Docker
            if layers
                .iter()
                .any(|l| l.blob.media_type.to_format(ImageFormat::Docker).is_err()) =>
        {
            ImageFormat::Oci
        }
        ImageFormat::Docker | ImageFormat::Oci => format,
    };
    let span = debug_span!("rnk_builder.assemble", bytes_out = tracing::field::Empty);
    let _entered = span.enter();
    let diff_ids: Vec<&Digest> = layers.iter().map(|l| &l.diff_id).collect();
    if config.rootfs.diff_ids.iter().collect::<Vec<_>>() != diff_ids {
        return Err(Error::Inconsistent(format!(
            "{} diff IDs in the config, {} layers, or a different order",
            config.rootfs.diff_ids.len(),
            layers.len()
        )));
    }
    config.created = Some(time.rfc3339());
    for layer in &mut layers {
        layer.blob.media_type = layer.blob.media_type.to_format(format)?;
    }

    let config_bytes = Bytes::from(to_canonical_json(&config)?);
    let config_descriptor = Descriptor::new(
        format.config_media_type(),
        sha256(&config_bytes),
        u64::try_from(config_bytes.len()).unwrap_or(u64::MAX),
    );
    let mut manifest = ImageManifest::new(
        format,
        config_descriptor.clone(),
        layers.iter().map(|l| l.blob.clone()).collect(),
    );
    if !annotations.is_empty() {
        manifest.annotations = Some(annotations.clone());
    }
    let manifest = RawManifest::new(format.manifest_media_type(), to_canonical_json(&manifest)?);
    span.record("bytes_out", config_descriptor.size + manifest.size());
    Ok(Image {
        format,
        config: config_bytes,
        config_descriptor,
        manifest,
        layers,
    })
}

/// The sha256 digest of `bytes`.
fn sha256(bytes: &[u8]) -> Digest {
    let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
    Digest::from_sha256_bytes(&hash)
}
