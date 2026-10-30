//! The three outputs of an assembled image: registry push, Docker tar and
//! OCI image layout.
//!
//! kaniko writes them in the order OCI layout, tar, push
//! (`kaniko@44aa7ddc0 pkg/executor/push.go:182-330`); the caller does the
//! same. Output paths come from the command line and are trusted host paths;
//! nothing here reads an image root.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::num::NonZeroUsize;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use rnk_layer::{Compression, EncodeOptions, Entry, EntryKind, EntryPath, LayerWriter};
use rnk_oci::{
    Descriptor, Digest, DockerTarImage, ImageFormat, ImageIndex, OciLayout, Reference,
    to_canonical_json,
};
use rnk_registry::{BlobBody, Client, PushOutcome};
use tracing::{Instrument as _, debug_span};

use crate::error::{Error, Result};
use crate::image::Image;
use crate::layer::{Content, Contents};

/// The tag kaniko gives a `--tar-path` image built without `--destination`
/// (`kaniko@44aa7ddc0 pkg/executor/push.go:67`).
pub const UNSET_DESTINATION: &str = "docker.io/unset-repo/unset-image-name:latest";

/// Where a build's image goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outputs {
    /// Images to push to, tagged (`latest` when no tag was given).
    pub destinations: Vec<Reference>,
    /// Whether to push: false under `--no-push`.
    pub push: bool,
    /// A Docker `save` tarball to write (`--tar-path`).
    pub docker_tar: Option<PathBuf>,
    /// An OCI image layout to write (`--oci-layout-path`).
    pub oci_layout: Option<PathBuf>,
}

impl Outputs {
    /// Checks and normalizes kaniko's output flags: each destination gets
    /// the tag `latest` if it has none.
    ///
    /// # Errors
    ///
    /// [`Error::Destination`] for a destination that does not parse or names
    /// a digest (a manifest is pushed by tag), or when pushing with no
    /// destination, as kaniko refuses it (`push.go:186`).
    pub fn new(
        destinations: &[String],
        push: bool,
        docker_tar: Option<PathBuf>,
        oci_layout: Option<PathBuf>,
    ) -> Result<Self> {
        let destinations = destinations
            .iter()
            .map(|d| {
                let reference = Reference::parse(d).map_err(|_| Error::Destination {
                    destination: d.clone(),
                    reason: "not an image reference",
                })?;
                if reference.digest().is_some() {
                    return Err(Error::Destination {
                        destination: d.clone(),
                        reason: "a destination is pushed by tag and cannot name a digest",
                    });
                }
                Ok(reference.with_default_tag())
            })
            .collect::<Result<Vec<_>>>()?;
        if push && destinations.is_empty() {
            return Err(Error::Destination {
                destination: String::new(),
                reason: "pushing needs at least one --destination, or --no-push",
            });
        }
        Ok(Self {
            destinations,
            push,
            docker_tar,
            oci_layout,
        })
    }

    /// The tags a Docker tar loads as: the destinations, or kaniko's
    /// placeholder when there are none.
    pub fn tar_tags(&self) -> Vec<String> {
        if self.destinations.is_empty() {
            return vec![UNSET_DESTINATION.to_owned()];
        }
        self.destinations.iter().map(ToString::to_string).collect()
    }
}

/// Writes `image` as an OCI image layout in `dir`, created if missing.
///
/// As go-containerregistry's `layout.Write` with an empty index followed by
/// `AppendImage`, which kaniko uses: blobs already present are kept, and
/// `index.json` lists this image alone, without a name annotation.
///
/// # Errors
///
/// [`Error::Io`] when a file cannot be written or a layer blob read.
pub fn write_oci_layout(image: &Image, dir: &Path) -> Result<()> {
    let span = debug_span!(
        "rnk_builder.output",
        layers = image.layers.len(),
        bytes_out = tracing::field::Empty
    );
    let _entered = span.enter();
    fs::create_dir_all(dir).map_err(Error::io("create", dir))?;
    let mut bytes_out = 0;
    bytes_out += write_new(
        dir,
        OciLayout::FILE,
        &serde_json::to_vec(&OciLayout::new()).map_err(rnk_oci::Error::from)?,
    )?;
    bytes_out += put_blob(
        dir,
        &image.config_descriptor.digest,
        BlobFrom::Bytes(&image.config),
    )?;
    bytes_out += put_blob(
        dir,
        image.manifest.digest(),
        BlobFrom::Bytes(image.manifest.bytes()),
    )?;
    for layer in &image.layers {
        bytes_out += put_blob(dir, &layer.blob.digest, BlobFrom::File(&layer.path))?;
    }
    let index = ImageIndex::new(ImageFormat::Oci, vec![image.manifest.descriptor()]);
    bytes_out += write_new(dir, OciLayout::INDEX_FILE, &to_canonical_json(&index)?)?;
    span.record("bytes_out", bytes_out);
    Ok(())
}

/// Writes `image` as a Docker `save` tarball at `path`, loadable as `tags`.
///
/// The layout is go-containerregistry's `tarball.Write`, which kaniko's
/// `--tar-path` uses: the config at `sha256:<hex>`, each distinct layer
/// blob at `<hex>.tar.gz` (or `.tar.zst`, `.tar`) and `manifest.json`. The
/// tar itself is written by `rnk-layer`, uncompressed, every entry `0644`,
/// owned by 0:0, with mtime 0, so the file depends only on the image and the
/// tags; its entries are sorted by name, not in go-containerregistry's
/// order, which loaders do not depend on.
///
/// # Errors
///
/// [`Error::Io`] when the file cannot be written or a layer blob read,
/// [`Error::Layer`] when the tar cannot be written.
pub fn write_docker_tar(image: &Image, tags: &[String], path: &Path) -> Result<()> {
    let span = debug_span!(
        "rnk_builder.output",
        layers = image.layers.len(),
        bytes_out = tracing::field::Empty
    );
    let _entered = span.enter();
    let mut writer = LayerWriter::new(EncodeOptions {
        compression: Compression::None,
        level: 0,
        threads: NonZeroUsize::MIN,
        mtime: Some(0),
    })?;
    let mut items = Vec::new();
    let mut add = |name: String, size: u64, content: Content| -> Result<()> {
        let path = EntryPath::new(name)?;
        writer.add(file_entry(path.clone(), size))?;
        items.push((path, content));
        Ok(())
    };

    let config_path = DockerTarImage::config_path(&image.config_descriptor.digest);
    add(
        config_path.clone(),
        image.config_descriptor.size,
        Content::Bytes(image.config.clone()),
    )?;
    let mut layer_paths = Vec::with_capacity(image.layers.len());
    let mut seen = BTreeSet::new();
    for layer in &image.layers {
        let compression = layer.blob.media_type.layer_compression().ok_or_else(|| {
            Error::Inconsistent(format!(
                "{} is not a layer media type",
                layer.blob.media_type
            ))
        })?;
        let name = DockerTarImage::layer_path(&layer.blob.digest, compression);
        if seen.insert(name.clone()) {
            add(
                name.clone(),
                layer.blob.size,
                Content::File(layer.path.clone()),
            )?;
        }
        layer_paths.push(name);
    }
    let manifest = vec![DockerTarImage {
        config: config_path,
        repo_tags: tags.to_vec(),
        layers: layer_paths,
        layer_sources: None,
    }];
    let manifest = bytes::Bytes::from(serde_json::to_vec(&manifest).map_err(rnk_oci::Error::from)?);
    add(
        "manifest.json".to_owned(),
        u64::try_from(manifest.len()).unwrap_or(u64::MAX),
        Content::Bytes(manifest),
    )?;

    let size = write_atomically(path, |out| {
        Ok(writer.finish(&mut Contents { items }, out)?.size)
    })?;
    span.record("bytes_out", size);
    Ok(())
}

/// Pushes `image` to every destination, one after another as kaniko does:
/// up to `jobs` blobs at once, then the manifest by tag. Returns each
/// destination with the image's digest.
///
/// A base layer is first mounted from the repository it came from, and
/// every blob from the first destination for later ones on the same
/// registry; a refused mount falls back to an upload (Q-014: mounts are in
/// Phase 1, interim). Blobs the registry has are not sent again.
///
/// # Errors
///
/// [`Error::Registry`] for the first failed request after its retries.
pub async fn push(
    image: &Image,
    destinations: &[Reference],
    client: &Client,
    jobs: NonZeroUsize,
) -> Result<Vec<(Reference, Digest)>> {
    let mut pushed = Vec::with_capacity(destinations.len());
    let first = destinations.first();
    for destination in destinations {
        let span = debug_span!(
            "rnk_builder.output",
            layers = image.layers.len(),
            bytes_out = tracing::field::Empty
        );
        let bytes_out = push_one(
            image,
            destination,
            first.filter(|f| *f != destination),
            client,
            jobs,
        )
        .instrument(span.clone())
        .await?;
        span.record("bytes_out", bytes_out);
        pushed.push((destination.clone(), image.digest().clone()));
    }
    Ok(pushed)
}

async fn push_one(
    image: &Image,
    destination: &Reference,
    earlier: Option<&Reference>,
    client: &Client,
    jobs: NonZeroUsize,
) -> Result<u64> {
    let config = BlobBody::Bytes(image.config.clone());
    let mut blobs: Vec<(&Descriptor, BlobBody, Option<&Reference>)> =
        vec![(&image.config_descriptor, config, earlier)];
    let mut seen = BTreeSet::new();
    for layer in &image.layers {
        if seen.insert(&layer.blob.digest) {
            let from = layer.mount_from.as_ref().or(earlier);
            blobs.push((&layer.blob, BlobBody::File(layer.path.clone()), from));
        }
    }
    let uploaded: Vec<u64> = stream::iter(blobs)
        .map(|(blob, body, from)| async move {
            let outcome = match from {
                Some(from) => client.mount_or_push(destination, from, blob, &body).await?,
                None => client.push_blob(destination, blob, &body).await?,
            };
            Ok::<_, Error>(match outcome {
                PushOutcome::Uploaded => blob.size,
                PushOutcome::Existed | PushOutcome::Mounted => 0,
            })
        })
        .buffer_unordered(jobs.get())
        .try_collect()
        .await?;
    client.push_manifest(destination, &image.manifest).await?;
    Ok(uploaded.iter().sum::<u64>() + image.manifest.size())
}

/// A regular file entry of an output tar.
fn file_entry(path: EntryPath, size: u64) -> Entry {
    Entry {
        path,
        kind: EntryKind::File { size },
        mode: 0o644,
        uid: 0,
        gid: 0,
        mtime: 0,
        xattrs: std::collections::BTreeMap::new(),
        inode: None,
    }
}

enum BlobFrom<'a> {
    Bytes(&'a [u8]),
    File(&'a Path),
}

/// Puts a blob into a layout's `blobs/` unless a file of the same size is
/// there; returns the bytes written.
fn put_blob(dir: &Path, digest: &Digest, from: BlobFrom<'_>) -> Result<u64> {
    let path = dir.join(OciLayout::blob_path(digest));
    let size = match &from {
        BlobFrom::Bytes(b) => u64::try_from(b.len()).unwrap_or(u64::MAX),
        BlobFrom::File(f) => fs::metadata(f).map_err(Error::io("stat", *f))?.len(),
    };
    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == size) {
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(Error::io("create", parent))?;
    }
    write_atomically(&path, |out| match from {
        BlobFrom::Bytes(bytes) => out
            .write_all(bytes)
            .map_err(Error::io("write", &path))
            .map(|()| size),
        BlobFrom::File(src) => {
            let mut file = File::open(src).map_err(Error::io("open", src))?;
            std::io::copy(&mut file, out).map_err(Error::io("copy", src))
        }
    })
}

/// Writes a small file of a layout, replacing what is there.
fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<u64> {
    let path = dir.join(name);
    write_atomically(&path, |out| {
        out.write_all(bytes).map_err(Error::io("write", &path))
    })?;
    Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
}

/// Writes `path` through a temporary sibling renamed into place, so a
/// failed or interrupted write never leaves a partial output under the
/// name; on error the temporary file is removed.
fn write_atomically<T>(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<T>,
) -> Result<T> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.ranikust-tmp"));
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&tmp)
        .map_err(Error::io("create", &tmp))?;
    let mut out = BufWriter::new(file);
    let result = write(&mut out).and_then(|value| {
        let file = out
            .into_inner()
            .map_err(|e| Error::io("write", &tmp)(e.into_error()))?;
        file.sync_all().map_err(Error::io("sync", &tmp))?;
        fs::rename(&tmp, path).map_err(Error::io("rename", &tmp))?;
        Ok(value)
    });
    if result.is_err() {
        // The write error is what matters; a leftover temporary file is
        // only clutter next to the output.
        let _ = fs::remove_file(&tmp);
    }
    result
}
