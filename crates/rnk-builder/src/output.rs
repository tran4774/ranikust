//! The three outputs of an assembled image: registry push, Docker tar and
//! OCI image layout.
//!
//! kaniko writes them in the order OCI layout, tar, push
//! (`kaniko@44aa7ddc0 pkg/executor/push.go:182-330`); the caller does the
//! same. Output paths come from the command line and are trusted host paths;
//! nothing here reads an image root.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Read as _, Write};
use std::num::NonZeroUsize;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use rnk_layer::{Compression, EncodeOptions, Entry, EntryKind, EntryPath, LayerWriter};
use rnk_oci::{
    Descriptor, Digest, DockerTarImage, ImageFormat, ImageIndex, OciLayout, Reference,
    to_canonical_json,
};
use rnk_registry::{BlobBody, Client, PushOutcome};
use sha2::Digest as _;
use tempfile::NamedTempFile;
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
    /// The destinations as typed, for the tar's `RepoTags`.
    tar_tags: Vec<String>,
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
        let mut references = Vec::with_capacity(destinations.len());
        let mut tar_tags = Vec::with_capacity(destinations.len());
        for d in destinations {
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
            // go-containerregistry's `tag.String()` is the reference as
            // typed; `tarball.Write` adds `:latest` when no tag was given
            // (`kaniko@44aa7ddc0 vendor/github.com/google/go-containerregistry/pkg/v1/tarball/write.go:347-350`).
            tar_tags.push(match reference.tag() {
                Some(_) => d.clone(),
                None => format!("{d}:{}", Reference::DEFAULT_TAG),
            });
            references.push(reference.with_default_tag());
        }
        if push && references.is_empty() {
            return Err(Error::Destination {
                destination: String::new(),
                reason: "pushing needs at least one --destination, or --no-push",
            });
        }
        if tar_tags.is_empty() {
            tar_tags.push(UNSET_DESTINATION.to_owned());
        }
        Ok(Self {
            destinations: references,
            push,
            docker_tar,
            oci_layout,
            tar_tags,
        })
    }

    /// The tags a Docker tar loads as: the destinations as typed, `:latest`
    /// added when they name no tag, or kaniko's placeholder when there are
    /// none.
    pub fn tar_tags(&self) -> &[String] {
        &self.tar_tags
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
        let compression =
            layer
                .blob
                .media_type
                .layer_compression()
                .ok_or_else(|| Error::UnknownLayerType {
                    media_type: layer.blob.media_type.clone(),
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
/// registry; a refused mount falls back to an upload. Blobs the registry
/// has are not sent again. Mounts are part of Phase 1 (ADR-0032).
///
/// # Errors
///
/// [`Error::Push`] for the first destination that fails, wrapping the
/// failed request after its retries. The first failed blob cancels the
/// other transfers to that destination, and its manifest is not pushed.
/// The destinations before it stay pushed and are listed in the error; the
/// ones after it are not tried.
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
        let result = push_one(
            image,
            destination,
            first.filter(|f| *f != destination),
            client,
            jobs,
        )
        .instrument(span.clone())
        .await;
        match result {
            Ok(bytes_out) => {
                span.record("bytes_out", bytes_out);
                pushed.push((destination.clone(), image.digest().clone()));
            }
            Err(source) => {
                return Err(Error::Push {
                    destination: Box::new(destination.clone()),
                    pushed,
                    source: Box::new(source),
                });
            }
        }
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
    // `try_collect` stops at the first error and drops the transfers still
    // running, which cancels them.
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

/// Puts a blob into a layout's `blobs/` unless a regular file with exactly
/// its bytes is there (hashed, not trusted by size); returns the bytes
/// written.
fn put_blob(dir: &Path, digest: &Digest, from: BlobFrom<'_>) -> Result<u64> {
    let path = dir.join(OciLayout::blob_path(digest));
    if holds(&path, digest)? {
        return Ok(0);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(Error::io("create", parent))?;
    }
    write_atomically(&path, |out| match from {
        BlobFrom::Bytes(bytes) => out
            .write_all(bytes)
            .map_err(Error::io("write", &path))
            .map(|()| u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
        BlobFrom::File(src) => {
            let mut file = File::open(src).map_err(Error::io("open", src))?;
            std::io::copy(&mut file, out).map_err(Error::io("copy", src))
        }
    })
}

/// Whether `path` is a regular file (not a symlink) whose sha256 is
/// `digest`.
fn holds(path: &Path, digest: &Digest) -> Result<bool> {
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
        return Ok(false);
    }
    let mut file = File::open(path).map_err(Error::io("open", path))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0; 128 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(Error::io("read", path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash: [u8; 32] = hasher.finalize().into();
    Ok(Digest::from_sha256_bytes(&hash) == *digest)
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
/// name.
///
/// The temporary file has a random name and is created exclusively
/// (`O_CREAT | O_EXCL`), so nothing planted beside the output is followed or
/// overwritten. On every error it is removed when dropped; only a killed
/// process leaves one behind.
fn write_atomically<T>(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<NamedTempFile>) -> Result<T>,
) -> Result<T> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        Some(_) | None => Path::new("."),
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".ranikust-tmp")
        .permissions(fs::Permissions::from_mode(0o644))
        .tempfile_in(parent)
        .map_err(Error::io("create a temporary file in", parent))?;
    let tmp_path = tmp.path().to_owned();
    let mut out = BufWriter::new(tmp);
    let value = write(&mut out)?;
    let tmp = out
        .into_inner()
        .map_err(|e| Error::io("write", &tmp_path)(e.into_error()))?;
    tmp.as_file()
        .sync_all()
        .map_err(Error::io("sync", &tmp_path))?;
    tmp.persist(path)
        .map_err(|e| Error::io("rename", &tmp_path)(e.error))?;
    Ok(value)
}
