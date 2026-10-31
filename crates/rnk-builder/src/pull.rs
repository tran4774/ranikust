//! Pulling an image for `FROM` or `COPY --from`: its manifest and config,
//! its blobs into the store, and its layers as the snapshotter keeps them
//! (extracted lower layers in overlay mode, the blobs in kaniko mode). In
//! kaniko mode, when no output needs a base layer's bytes, no blob is
//! pulled at all: each layer is streamed from the registry into the root
//! when a stage is prepared on it (ADR-0055).

use std::path::PathBuf;
use std::sync::Arc;

use rnk_oci::{ImageConfig, ImageFormat, ImageManifest, Platform, Reference};
use rnk_registry::Client;
use rnk_snapshot::{LayerDir, LayerOpen, Snapshotter};
use rnk_store::{EntryKey, Store};
use tokio::io::AsyncWriteExt as _;

use crate::error::{Error, Result};
use crate::layer::BlobLayer;

/// A pulled image: what a stage built on it starts from. `L` is the
/// snapshotter's base layer (`Snapshotter::Layer`).
#[derive(Debug)]
pub struct PulledImage<L = LayerDir> {
    /// The reference as pulled, its default tag added.
    pub reference: Reference,
    /// Its config.
    pub config: ImageConfig,
    /// The format of its manifest.
    pub format: ImageFormat,
    /// Its layers as blobs in the store, image order, for the outputs.
    pub blobs: Vec<BlobLayer>,
    /// Its layers as the snapshotter keeps them, image order.
    pub layers: Vec<L>,
}

/// Pulls `reference` for `platform`: the manifest, the config, every layer
/// blob into the store (`blobs/`, verified while it streams) and every
/// layer made a base layer by the snapshotter (in overlay mode extracted in
/// `layers/`, checked against the config's diff IDs).
///
/// Unless `keep_blobs` (an output written to a file needs the bytes), a
/// snapshotter that streams base layers ([`Snapshotter::streamed_layer`],
/// kaniko mode) gets each layer as a stream from the registry instead, and
/// no blob is pulled or kept: the image's layers are then
/// [`LayerBytes::Registry`](crate::LayerBytes::Registry), which a push mounts or streams
/// from their source.
///
/// Blocking work (the store's locked fills, extraction) runs on blocking
/// threads of the current runtime.
///
/// # Errors
///
/// A registry, store or extraction error; [`Error::Invalid`] when the
/// manifest and the config disagree on the number of layers.
pub async fn pull_image<S>(
    client: &Arc<Client>,
    store: &Arc<Store>,
    snapshotter: &Arc<S>,
    reference: &str,
    platform: &Platform,
    keep_blobs: bool,
) -> Result<PulledImage<S::Layer>>
where
    S: Snapshotter + Send + Sync + 'static,
{
    let reference = Reference::parse(reference)
        .map_err(|e| Error::Invalid(format!("image {reference:?}: {e}")))?
        .with_default_tag();
    let images = client.pull_image_manifest(&reference, platform).await?;
    let format = images
        .manifest
        .media_type()
        .format()
        .unwrap_or(ImageFormat::Docker);
    let manifest = ImageManifest::from_json(images.manifest.bytes())?;
    let mut config_bytes = Vec::new();
    client
        .pull_blob(&reference, &manifest.config, &mut config_bytes)
        .await?;
    let config = ImageConfig::from_json(&config_bytes)?;
    if config.rootfs.diff_ids.len() != manifest.layers.len() {
        return Err(Error::Invalid(format!(
            "image {reference}: the manifest has {} layers, the config {} diff IDs",
            manifest.layers.len(),
            config.rootfs.diff_ids.len()
        )));
    }

    if !keep_blobs
        && let Some(streamed) = streamed(client, snapshotter, &reference, &manifest, &config)
    {
        let (blobs, layers) = streamed.into_iter().unzip();
        return Ok(PulledImage {
            reference,
            config,
            format,
            blobs,
            layers,
        });
    }

    let mut blobs = Vec::with_capacity(manifest.layers.len());
    for (layer, diff_id) in manifest.layers.iter().zip(&config.rootfs.diff_ids) {
        let path = pull_blob(client, store, &reference, layer).await?;
        blobs.push(BlobLayer::base(
            diff_id.clone(),
            layer.clone(),
            path,
            reference.clone(),
        ));
    }

    let mut layers: Vec<S::Layer> = Vec::with_capacity(blobs.len());
    for blob in &blobs {
        let snapshotter = Arc::clone(snapshotter);
        let below = layers.clone();
        let diff_id = blob.diff_id.clone();
        let path = blob
            .path()
            .ok_or_else(|| Error::Invalid("a pulled blob has no file".to_owned()))?
            .to_owned();
        let dir = tokio::task::spawn_blocking(move || -> Result<S::Layer> {
            Ok(snapshotter.base_layer(&diff_id, &path, &below)?)
        })
        .await
        .map_err(|e| Error::Invalid(format!("extracting a layer: {e}")))??;
        layers.push(dir);
    }
    Ok(PulledImage {
        reference,
        config,
        format,
        blobs,
        layers,
    })
}

/// The image's layers as streams, if the snapshotter takes them
/// (ADR-0055): nothing is pulled now, each layer is read from the registry
/// when a stage is prepared on it, through a bounded pipe.
fn streamed<S: Snapshotter>(
    client: &Arc<Client>,
    snapshotter: &Arc<S>,
    reference: &Reference,
    manifest: &ImageManifest,
    config: &ImageConfig,
) -> Option<Vec<(BlobLayer, S::Layer)>> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    manifest
        .layers
        .iter()
        .zip(&config.rootfs.diff_ids)
        .map(|(blob, diff_id)| {
            let open = Arc::new(RegistryLayer {
                handle: handle.clone(),
                client: Arc::clone(client),
                from: reference.clone(),
                blob: blob.clone(),
            });
            let layer = snapshotter.streamed_layer(diff_id, open)?;
            Some((
                BlobLayer::remote(diff_id.clone(), blob.clone(), reference.clone()),
                layer,
            ))
        })
        .collect()
}

/// A base layer read from its registry each time it is extracted.
#[derive(Debug)]
struct RegistryLayer {
    handle: tokio::runtime::Handle,
    client: Arc<Client>,
    from: Reference,
    blob: rnk_oci::Descriptor,
}

impl LayerOpen for RegistryLayer {
    fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        let reader = self
            .client
            .open_blob(&self.handle, &self.from, &self.blob)?;
        Ok(Box::new(reader))
    }
}

/// One blob into the store, unless it is there; its host path.
async fn pull_blob(
    client: &Arc<Client>,
    store: &Arc<Store>,
    reference: &Reference,
    blob: &rnk_oci::Descriptor,
) -> Result<PathBuf> {
    let key = EntryKey::Blob(blob.digest.clone());
    if let Some(path) = store.get(&key)? {
        return Ok(path);
    }
    let store = Arc::clone(store);
    let client = Arc::clone(client);
    let reference = reference.clone();
    let blob = blob.clone();
    let handle = tokio::runtime::Handle::current();
    // The store's fill takes a lock and runs a blocking closure; the
    // download inside it goes back to the runtime.
    tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let path = store.fill(&key, |tmp| -> std::result::Result<u64, Error> {
            let file = tmp
                .file()
                .ok_or_else(|| Error::Invalid("the store gave no file for a blob".to_owned()))?
                .try_clone()
                .map_err(Error::io("reopen", tmp.path()))?;
            handle.block_on(async {
                let mut out = tokio::fs::File::from_std(file);
                client.pull_blob(&reference, &blob, &mut out).await?;
                out.flush().await.map_err(Error::io("write", tmp.path()))?;
                Ok::<_, Error>(())
            })?;
            Ok(blob.size)
        })?;
        Ok(path)
    })
    .await
    .map_err(|e| Error::Invalid(format!("pulling a blob: {e}")))?
}
