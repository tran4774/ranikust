//! Image assembly: the final config, its layers and the manifest.

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

/// The author kaniko writes into each history entry it adds
/// (`kaniko@44aa7ddc0 pkg/constants/constants.go:25`), as ranikust.
///
/// [open: Q-131] Interim: kaniko's history, one entry per layer with the
/// instruction's text, only the author's name changed.
pub const HISTORY_AUTHOR: &str = "ranikust";

/// The history entry of a layer made by `created_by`, the instruction as
/// written in the Dockerfile, at `time`.
///
/// [open: Q-131] Interim: kaniko's form; metadata instructions get none.
pub fn layer_history(created_by: &str, time: &BuildTime) -> History {
    History {
        created: Some(time.rfc3339()),
        author: Some(HISTORY_AUTHOR.to_owned()),
        created_by: Some(created_by.to_owned()),
        comment: None,
        empty_layer: None,
    }
}

/// The format of the image a build writes.
///
/// [open: Q-006] Interim: kaniko's rule. The base image's format, Docker
/// v2s2 for `FROM scratch` (go-containerregistry's empty image) and under
/// `--reproducible` (`kaniko@44aa7ddc0 pkg/executor/build.go:2105`); OCI
/// whenever layers are zstd, which Docker has no media type for.
pub fn image_format(base: Option<ImageFormat>, zstd: bool, reproducible: bool) -> ImageFormat {
    if zstd {
        return ImageFormat::Oci;
    }
    if reproducible {
        return ImageFormat::Docker;
    }
    base.unwrap_or(ImageFormat::Docker)
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
/// `time`; under `--reproducible` the config is also scrubbed as kaniko's
/// `mutate.Canonical` does: every history entry gets `time` and no author,
/// and `container`, `docker_version` and `config.Hostname` are dropped.
///
/// `format` is the format asked for ([`image_format`]); the image is written
/// in OCI instead when a layer has no Docker media type, such as a zstd layer
/// of an OCI base image under `--reproducible`: a pulled blob's bytes, and
/// so its compression, do not change [open: Q-160]. [`Image::format`] is the
/// format written.
///
/// # Errors
///
/// [`Error::Inconsistent`] when the diff IDs disagree, [`Error::Oci`] when a
/// layer's media type has no OCI equivalent either or serializing fails.
pub fn assemble(
    mut config: ImageConfig,
    mut layers: Vec<BlobLayer>,
    format: ImageFormat,
    time: &BuildTime,
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
    let created = time.rfc3339();
    config.created = Some(created.clone());
    if time.is_reproducible() {
        for entry in config.history.iter_mut().flatten() {
            entry.created = Some(created.clone());
            entry.author = None;
        }
        config.container = None;
        config.docker_version = None;
        if let Some(c) = config.config.as_mut() {
            c.hostname = None;
        }
    }
    for layer in &mut layers {
        layer.blob.media_type = layer.blob.media_type.to_format(format)?;
    }

    let config_bytes = Bytes::from(to_canonical_json(&config)?);
    let config_descriptor = Descriptor::new(
        format.config_media_type(),
        sha256(&config_bytes),
        u64::try_from(config_bytes.len()).unwrap_or(u64::MAX),
    );
    let manifest = ImageManifest::new(
        format,
        config_descriptor.clone(),
        layers.iter().map(|l| l.blob.clone()).collect(),
    );
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
