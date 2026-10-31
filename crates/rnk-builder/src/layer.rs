//! Lazy layer export: a layer is encoded only when something needs it.
//!
//! A stage's snapshotter hands each finished layer over as a
//! [`LayerSource`], which knows how to feed its entries to an
//! `rnk_layer::LayerWriter`. [`LayerExporter::export_if_needed`] encodes it
//! into a build-private file when the [`LayerDemand`] says so and returns a
//! [`BlobLayer`]; otherwise it drops the source unencoded (exp-08: lazy export
//! saved 1.3–6.6 s on W3). Base-image layers are already blobs and become
//! [`BlobLayer`]s directly.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rnk_layer::{
    Compression, EncodeOptions, Entry, EntryPath, EntrySource, LayerDigests, LayerWriter,
};
use rnk_oci::{Descriptor, Digest, ImageFormat, Reference};
use rnk_store::Store;
use tracing::debug_span;

use crate::error::{Error, Result};
use crate::output::Outputs;
use crate::time::BuildTime;
use crate::upload::Uploader;

/// Why a stage's layers are encoded: any reason is enough.
///
/// [open: Q-029] Interim: `rnk-sched`, which holds the stage graph, builds
/// one per stage with [`LayerDemand::for_stage`]; the rule lives here, with
/// the lazy export it drives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayerDemand {
    /// The image is pushed to a registry.
    pub push: bool,
    /// The layers go to the layer cache (`--cache`, Phase 4).
    pub cache: bool,
    /// The stage is the `--target` stage, or an ancestor of it through
    /// `FROM <stage>`, whose layers the target's image inherits.
    pub target: bool,
    /// The image is written as a Docker tar or an OCI layout.
    pub output: bool,
    /// A later stage is built `FROM` this one in kaniko mode, which keeps
    /// the stage's layers (kaniko's `save_image`) though the target does
    /// not inherit them. They are encoded for that stage, never pushed or
    /// written out: only the target's own `FROM` chain reaches a
    /// destination.
    pub saved: bool,
}

impl LayerDemand {
    /// The demand of one stage: `in_target` when the stage is the target or
    /// the target is built `FROM` it (directly or through other stages), and
    /// what the build writes. A stage reached only through `COPY --from`
    /// needs no layer, whatever the outputs.
    pub fn for_stage(in_target: bool, outputs: &Outputs, cache: bool) -> Self {
        Self {
            push: in_target && outputs.push,
            cache,
            target: in_target,
            output: in_target && (outputs.docker_tar.is_some() || outputs.oci_layout.is_some()),
            saved: false,
        }
    }

    /// This demand, with the stage's layers kept for a later stage built
    /// `FROM` it when `saved` (kaniko mode). Pushes and outputs are
    /// unchanged.
    #[must_use]
    pub fn saved(self, saved: bool) -> Self {
        Self {
            saved: self.saved || saved,
            ..self
        }
    }

    /// Whether the layer must be encoded.
    ///
    /// The target stage's layers are encoded even without any output: the
    /// image is still assembled and its digest reported, as kaniko does with
    /// `--no-push`.
    pub fn needed(&self) -> bool {
        self.push || self.cache || self.target || self.output || self.saved
    }
}

/// A finished layer that has not been encoded yet: the snapshot exporter's
/// side of lazy export.
pub trait LayerSource: Send {
    /// Writes the layer to `out` with `options` and returns its digests. The
    /// bytes must depend only on the layer's content and the options other
    /// than `threads`.
    ///
    /// # Errors
    ///
    /// Whatever `rnk-layer` reports while writing.
    fn encode(
        self: Box<Self>,
        options: EncodeOptions,
        out: &mut dyn Write,
    ) -> Result<LayerDigests, rnk_layer::Error>;
}

/// A layer given as its entries and where their content comes from: the
/// simplest [`LayerSource`].
pub struct EntriesLayer<S> {
    entries: Vec<Entry>,
    source: S,
}

impl<S: EntrySource + Send> EntriesLayer<S> {
    /// A layer of `entries`, whose file contents and missing parents
    /// `source` gives.
    pub fn new(entries: Vec<Entry>, source: S) -> Self {
        Self { entries, source }
    }
}

impl<S: EntrySource + Send> LayerSource for EntriesLayer<S> {
    fn encode(
        mut self: Box<Self>,
        options: EncodeOptions,
        out: &mut dyn Write,
    ) -> Result<LayerDigests, rnk_layer::Error> {
        let mut writer = LayerWriter::new(options)?;
        for entry in std::mem::take(&mut self.entries) {
            writer.add(entry)?;
        }
        writer.finish(&mut self.source, out)
    }
}

/// An encoded layer blob on the host, with what the image needs about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobLayer {
    /// sha256 of the uncompressed tar: the config's `diff_id`.
    pub diff_id: Digest,
    /// The blob. Its media type is converted to the image's format when the
    /// image is assembled.
    pub blob: Descriptor,
    /// Where the blob's bytes are.
    pub bytes: LayerBytes,
    /// For a base-image layer, the repository it came from: a push to the
    /// same registry first asks to mount it from there.
    pub mount_from: Option<Reference>,
}

/// Where a layer blob's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayerBytes {
    /// A file: a build-private file, or a pulled blob in the store.
    File(PathBuf),
    /// Nowhere local: a base layer streamed into its extraction and never
    /// kept, still in the repository it came from, which a push mounts it
    /// from or streams it out of (ADR-0055).
    Registry(Reference),
}

impl BlobLayer {
    /// A base-image layer already in the store at `path`.
    pub fn base(diff_id: Digest, blob: Descriptor, path: PathBuf, from: Reference) -> Self {
        Self {
            diff_id,
            blob,
            bytes: LayerBytes::File(path),
            mount_from: Some(from),
        }
    }

    /// A base-image layer that was not kept: only in `from`'s repository.
    pub fn remote(diff_id: Digest, blob: Descriptor, from: Reference) -> Self {
        Self {
            diff_id,
            blob,
            bytes: LayerBytes::Registry(from.clone()),
            mount_from: Some(from),
        }
    }

    /// The blob's file, if it was kept.
    pub fn path(&self) -> Option<&Path> {
        match &self.bytes {
            LayerBytes::File(path) => Some(path),
            LayerBytes::Registry(_) => None,
        }
    }
}

/// Encodes the layers of one build into a build-private directory.
///
/// Holds no global state: the directory (`rnk_store::Store::build_dir`, or
/// a directory under it), the options and the build's time come from the
/// caller. Encoded blobs are named by digest, so identical layers share one
/// file. They stay in the build directory and are deleted with it; Phase 4
/// publishes them in the store (Q-162, ADR-0052).
#[derive(Debug)]
pub struct LayerExporter {
    dir: PathBuf,
    options: EncodeOptions,
    /// [`LayerExporter::anonymous`]: blobs are anonymous files of this
    /// store, never named in a directory (ADR-0051).
    anon: Option<Arc<Store>>,
    /// [`LayerExporter::with_uploader`]: pushed layers go to it as they are
    /// exported (ADR-0055).
    uploader: Option<Uploader>,
}

impl LayerExporter {
    /// An exporter writing into `dir`, which must exist, with `options`
    /// whose `mtime` is replaced by `time`'s: the fixed time under
    /// `--reproducible`, else each file's own. Taking the time here means a
    /// caller cannot encode a reproducible build's layers with file times.
    pub fn new(dir: impl Into<PathBuf>, options: EncodeOptions, time: &BuildTime) -> Self {
        Self {
            dir: dir.into(),
            options: EncodeOptions {
                mtime: time.layer_mtime(),
                ..options
            },
            anon: None,
            uploader: None,
        }
    }

    /// Hands every layer exported for a push to `uploader`, which sends it
    /// to the destinations while the build goes on (ADR-0055).
    #[must_use]
    pub fn with_uploader(mut self, uploader: Uploader) -> Self {
        self.uploader = Some(uploader);
        self
    }

    /// After [`LayerExporter::export`]: hands `layer` to the uploader when
    /// `demand` pushes it. Without an uploader, nothing happens.
    pub fn exported(&self, layer: &BlobLayer, demand: LayerDemand) {
        if demand.push
            && let Some(uploader) = &self.uploader
        {
            uploader.submit(layer);
        }
    }

    /// Waits for the uploads handed over and returns the digests every
    /// destination holds; empty without an uploader.
    ///
    /// # Errors
    ///
    /// The first upload that failed.
    pub async fn finish_uploads(&self) -> Result<std::collections::BTreeSet<Digest>> {
        match &self.uploader {
            Some(uploader) => uploader.finish().await,
            None => Ok(std::collections::BTreeSet::new()),
        }
    }

    /// An exporter whose blobs are anonymous files of `store`
    /// ([`Store::anonymous_file`]): kaniko mode on `/`, where a `RUN` can
    /// read any file with a path (ADR-0051). The blobs live as long as the
    /// store.
    pub fn anonymous(store: Arc<Store>, options: EncodeOptions, time: &BuildTime) -> Self {
        let mut exporter = Self::new(store.state_dir().to_path_buf(), options, time);
        exporter.anon = Some(store);
        exporter
    }

    /// The options layers are encoded with.
    pub fn options(&self) -> EncodeOptions {
        self.options
    }

    /// Encodes `source` if `demand` needs it, else drops it.
    ///
    /// # Errors
    ///
    /// As [`LayerExporter::export`].
    pub fn export_if_needed(
        &self,
        source: Box<dyn LayerSource>,
        demand: LayerDemand,
    ) -> Result<Option<BlobLayer>> {
        if demand.needed() {
            return self.export(source).map(Some);
        }
        let span = debug_span!("rnk_builder.layer", skipped = true);
        let _entered = span.enter();
        drop(source);
        Ok(None)
    }

    /// Encodes `source` into the exporter's directory.
    ///
    /// The blob is written to a uniquely named temporary file, created
    /// exclusively (so a name planted there is never followed), then renamed
    /// to its digest; exporters sharing a directory do not collide.
    ///
    /// # Errors
    ///
    /// [`Error::Layer`] when encoding fails, [`Error::Io`] when the file
    /// cannot be written or renamed; the temporary file is removed.
    pub fn export(&self, source: Box<dyn LayerSource>) -> Result<BlobLayer> {
        let span = debug_span!("rnk_builder.layer", skipped = false);
        let _entered = span.enter();
        if let Some(store) = &self.anon {
            // No name at all: nothing to rename, nothing a RUN can open. A
            // failed encoding drops the file with its descriptor's last
            // user, the store's own copy staying empty and unreferenced.
            let (file, path) = store.anonymous_file()?;
            let mut out = BufWriter::new(file);
            let digests = source.encode(self.options, &mut out)?;
            out.into_inner()
                .map_err(|e| Error::io("write", &path)(e.into_error()))?;
            return blob_layer(&digests, path);
        }
        let tmp = tempfile::Builder::new()
            .prefix(".layer-")
            .tempfile_in(&self.dir)
            .map_err(Error::io("create a temporary file in", &self.dir))?;
        let tmp_path = tmp.path().to_owned();
        let mut out = BufWriter::new(tmp);
        let digests = source.encode(self.options, &mut out)?;
        let tmp = out
            .into_inner()
            .map_err(|e| Error::io("write", &tmp_path)(e.into_error()))?;
        let digest = Digest::from_sha256_bytes(digests.digest.as_bytes());
        let path = self.dir.join(digest.encoded());
        // On failure the temporary file comes back inside the error and is
        // removed when it is dropped.
        tmp.persist(&path)
            .map_err(|e| Error::io("rename", &tmp_path)(e.error))?;
        blob_layer(&digests, path)
    }
}

/// The [`BlobLayer`] of an encoded layer whose bytes are at `path`.
fn blob_layer(digests: &LayerDigests, path: PathBuf) -> Result<BlobLayer> {
    let digest = Digest::from_sha256_bytes(digests.digest.as_bytes());
    let media_type = ImageFormat::Oci.layer_media_type(oci_compression(digests.compression))?;
    Ok(BlobLayer {
        diff_id: Digest::from_sha256_bytes(digests.diff_id.as_bytes()),
        blob: Descriptor::new(media_type, digest, digests.size),
        bytes: LayerBytes::File(path),
        mount_from: None,
    })
}

/// `rnk-layer`'s compression as `rnk-oci` names it.
pub(crate) fn oci_compression(compression: Compression) -> rnk_oci::Compression {
    match compression {
        Compression::None => rnk_oci::Compression::Uncompressed,
        Compression::Gzip => rnk_oci::Compression::Gzip,
        Compression::Zstd => rnk_oci::Compression::Zstd,
    }
}

/// An [`EntrySource`] for files whose content is in memory or in a host
/// file the builder wrote or the store holds; never a path in a root.
pub(crate) struct Contents {
    pub(crate) items: Vec<(EntryPath, Content)>,
}

/// One file's content for [`Contents`].
pub(crate) enum Content {
    Bytes(bytes::Bytes),
    File(PathBuf),
}

impl EntrySource for Contents {
    fn open(&mut self, entry: &Entry) -> io::Result<Box<dyn Read + '_>> {
        let content = self
            .items
            .iter()
            .find(|(path, _)| *path == entry.path)
            .map(|(_, content)| content)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no content for {}", entry.path),
                )
            })?;
        Ok(match content {
            Content::Bytes(bytes) => Box::new(&bytes[..]),
            Content::File(path) => Box::new(File::open(path)?),
        })
    }

    fn parent(&mut self, _path: &EntryPath) -> io::Result<Option<Entry>> {
        Ok(None)
    }
}
