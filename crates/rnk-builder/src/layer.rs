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
use std::path::PathBuf;

use rnk_layer::{
    Compression, EncodeOptions, Entry, EntryPath, EntrySource, LayerDigests, LayerWriter,
};
use rnk_oci::{Descriptor, Digest, ImageFormat, Reference};
use tracing::debug_span;

use crate::error::{Error, Result};
use crate::output::Outputs;
use crate::time::BuildTime;

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
        }
    }

    /// Whether the layer must be encoded.
    ///
    /// The target stage's layers are encoded even without any output: the
    /// image is still assembled and its digest reported, as kaniko does with
    /// `--no-push`.
    pub fn needed(&self) -> bool {
        self.push || self.cache || self.target || self.output
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
    /// The blob's bytes: a build-private file, or a pulled blob in the
    /// store.
    pub path: PathBuf,
    /// For a base-image layer, the repository it came from: a push to the
    /// same registry first asks to mount it from there.
    pub mount_from: Option<Reference>,
}

impl BlobLayer {
    /// A base-image layer already in the store at `path`.
    pub fn base(diff_id: Digest, blob: Descriptor, path: PathBuf, from: Reference) -> Self {
        Self {
            diff_id,
            blob,
            path,
            mount_from: Some(from),
        }
    }
}

/// Encodes the layers of one build into a build-private directory.
///
/// Holds no global state: the directory (`rnk_store::Store::build_dir`, or
/// a directory under it), the options and the build's time come from the
/// caller. Encoded blobs are named by digest, so identical layers share one
/// file. [open: Q-162] Interim: the blobs stay in the build directory and
/// are deleted with it, instead of being published in the store.
#[derive(Debug)]
pub struct LayerExporter {
    dir: PathBuf,
    options: EncodeOptions,
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
        }
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
        let media_type = ImageFormat::Oci.layer_media_type(oci_compression(digests.compression))?;
        Ok(BlobLayer {
            diff_id: Digest::from_sha256_bytes(digests.diff_id.as_bytes()),
            blob: Descriptor::new(media_type, digest, digests.size),
            path,
            mount_from: None,
        })
    }
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
