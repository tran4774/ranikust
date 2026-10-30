//! Pulling an image for `FROM` or `COPY --from`: its manifest and config,
//! its blobs into the store, and its layers extracted as lower layers.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use rnk_oci::{ImageConfig, ImageFormat, ImageManifest, Platform, Reference};
use rnk_registry::Client;
use rnk_snapshot::{LayerDir, Snapshotter};
use rnk_store::{EntryKey, Store};
use tokio::io::AsyncWriteExt as _;

use crate::error::{Error, Result};
use crate::layer::BlobLayer;

/// A pulled image: what a stage built on it starts from.
#[derive(Debug)]
pub struct PulledImage {
    /// The reference as pulled, its default tag added.
    pub reference: Reference,
    /// Its config.
    pub config: ImageConfig,
    /// The format of its manifest.
    pub format: ImageFormat,
    /// Its layers as blobs in the store, image order, for the outputs.
    pub blobs: Vec<BlobLayer>,
    /// Its layers extracted in the store, image order, as lower layers.
    pub layers: Vec<LayerDir>,
}

/// Pulls `reference` for `platform`: the manifest, the config, every layer
/// blob into the store (`blobs/`, verified while it streams) and every
/// layer extracted (`layers/`, checked against the config's diff IDs).
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
) -> Result<PulledImage>
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

    let mut layers: Vec<LayerDir> = Vec::with_capacity(blobs.len());
    for blob in &blobs {
        let snapshotter = Arc::clone(snapshotter);
        let below = layers.clone();
        let diff_id = blob.diff_id.clone();
        let path = blob.path.clone();
        let dir = tokio::task::spawn_blocking(move || -> Result<LayerDir> {
            let file = File::open(&path).map_err(Error::io("open", &path))?;
            Ok(snapshotter.base_layer(&diff_id, file, &below)?)
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
