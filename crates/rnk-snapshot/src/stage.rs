//! Stage roots: the `Snapshotter` and `StageRootfs` traits and their overlay
//! implementation (`design/overlay-snapshot.md#one-layer-per-instruction`).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rnk_layer::{EncodeOptions, LayerDigests};
use rnk_oci::Digest;
use rnk_rootfs::{IgnoreList, RootPath, delete_contents};
use rnk_store::Store;

use crate::error::{Error, Result};
use crate::export::{DiffCounts, LayerDiff};
use crate::layer::{self, LayerDir};
use crate::mount::{self, MountApi, Spec};
use crate::probe::OverlayCaps;

/// Mode of the stage directory and its work directories.
const PRIVATE: u32 = 0o700;

/// What a stage starts from.
#[derive(Debug)]
pub enum Base<'a, R, L> {
    /// `FROM scratch`: nothing.
    Scratch,
    /// `FROM <image>`: its layers as the snapshotter keeps them
    /// ([`Snapshotter::Layer`]), in image order (the config's `diff_ids`
    /// order, bottom first).
    Image(&'a [L]),
    /// `FROM <stage>`: the root another stage ended with. That stage must
    /// not be released while this one runs.
    Stage(&'a R),
}

/// What a finished stage keeps of its root for the stages that read it
/// later (`COPY --from`, `RUN --mount from=`) or build `FROM` it, when its
/// root is about to be reused ([`StageRootfs::keep`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keep {
    /// The whole root, as the stage ended with it.
    All,
    /// These paths of the root: a symlink is kept as the link, anything
    /// else with everything below it; the directories above each are made.
    Paths(Vec<PathBuf>),
}

/// A closed layer, ready to be encoded: an overlay upper directory
/// ([`LayerDiff`]) or a kaniko-mode walk diff (`KanikoDiff`).
pub trait ExportLayer: Send {
    /// Writes the layer to `out` and returns its digests and counts. The
    /// bytes depend only on the layer's content and `options` other than
    /// its thread count.
    ///
    /// # Errors
    ///
    /// As the mode's exporter: [`Error::Layer`] when encoding fails,
    /// [`Error::Rootfs`] or [`Error::Export`] when an entry cannot be read.
    fn export(
        self,
        options: EncodeOptions,
        out: &mut dyn Write,
    ) -> Result<(LayerDigests, DiffCounts)>;
}

impl ExportLayer for LayerDiff {
    fn export(
        self,
        options: EncodeOptions,
        out: &mut dyn Write,
    ) -> Result<(LayerDigests, DiffCounts)> {
        LayerDiff::export(&self, options, out)
    }
}

/// One stage's root filesystem, changed one layer at a time (ADR-0046): a
/// layer is opened before an instruction that changes files and closed
/// after it, which gives its diff; between layers the root is only read.
/// A needed diff is encoded before the next layer opens.
pub trait StageRootfs {
    /// A closed layer.
    type Diff: ExportLayer + 'static;

    /// Whether the stage's layers are needed (lazy snapshots, ADR-0038):
    /// called once after `prepare`, before the first layer. Kaniko mode
    /// walks only when they are; overlay mode ignores it. Default: needed.
    fn set_layers_needed(&mut self, _needed: bool) {}

    /// The writable root while a layer is open, for `COPY`, `ADD` and
    /// `WORKDIR`.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when no layer is open.
    fn root(&self) -> Result<&RootPath>;

    /// The host path of the root, for `rnk-sandbox`, which enters it for a
    /// `RUN` while a layer is open.
    fn mount_point(&self) -> &Path;

    /// Opens the next layer: a fresh upper and work directory whose root
    /// takes the root metadata of the layers below, mounted over them.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when a layer is open already; [`Error::Mount`] and
    /// the like when mounting fails.
    fn begin_layer(&mut self) -> Result<()>;

    /// Closes the open layer and returns its diff; the diff becomes the
    /// topmost lower layer of the next one.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when no layer is open; [`Error::Unmount`].
    fn end_layer(&mut self) -> Result<Self::Diff>;

    /// The root as the next layer would see it, read-only, for reading
    /// between layers (`USER` lookups, `COPY --from` sources). A layer
    /// open: its writable root.
    ///
    /// # Errors
    ///
    /// [`Error::Mount`] and the like when the read-only view cannot be
    /// mounted.
    fn view(&mut self) -> Result<&RootPath>;

    /// The stage is done and later stages read it, but its root is about to
    /// be reused: keep `what` of it, and from then [`StageRootfs::view`]
    /// and `Base::Stage` read the kept copy. May be called again, with more,
    /// until the root is reused. Overlay mode keeps every stage root until
    /// it is released, so its default does nothing.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when the root was reused since; [`Error::Rootfs`]
    /// when copying fails.
    fn keep(&mut self, _what: &Keep) -> Result<()> {
        Ok(())
    }

    /// The stage ran its last instruction: from now its root is only read
    /// ([`StageRootfs::view`], `Base::Stage`) or released. What served
    /// only its own layers may go; kaniko mode's per-stage directories
    /// drop the walker and its record of the root (ADR-0059). Default:
    /// nothing.
    fn done(&mut self) {}

    /// Unmounts and deletes the stage's directories, diffs included: every
    /// [`LayerDiff`] of the stage must be exported, and every stage built
    /// on it done, before.
    ///
    /// # Errors
    ///
    /// [`Error::Unmount`], or [`Error::Rootfs`] when deleting fails.
    fn release(self) -> Result<()>;
}

/// Makes stage roots and keeps base layers as its stage roots take them
/// (ADR-0046).
pub trait Snapshotter {
    /// The stage roots it makes.
    type Root: StageRootfs;

    /// A base layer as this snapshotter keeps it: extracted in the store
    /// ([`LayerDir`]) or its blob (`KanikoLayer`).
    type Layer: Clone + std::fmt::Debug + Send + Sync;

    /// Makes the layer blob at `blob` (in the store), whose uncompressed
    /// tar is `diff_id`, a base layer above `below` (image order).
    ///
    /// # Errors
    ///
    /// A store, decoding, diff ID or extraction error.
    fn base_layer(
        &self,
        diff_id: &Digest,
        blob: &Path,
        below: &[Self::Layer],
    ) -> Result<Self::Layer>;

    /// A base layer whose compressed bytes `open` yields each time a stage
    /// is prepared on it, its blob never kept (ADR-0055); `None` when this
    /// snapshotter keeps base layers itself (overlay mode). Default: `None`.
    fn streamed_layer(&self, _diff_id: &Digest, _open: Arc<dyn LayerOpen>) -> Option<Self::Layer> {
        None
    }

    /// A new stage root on `base`. No layer is open yet.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when the parent stage has a layer open or, in
    /// kaniko mode, no longer holds the root; [`Error::Rootfs`] when the
    /// stage directory cannot be made.
    fn prepare(&self, base: Base<'_, Self::Root, Self::Layer>) -> Result<Self::Root>;

    /// A new stage root on the root `parent` ended with (`FROM <stage>`),
    /// made by the only stage that reads `parent` (decided from the stage
    /// graph, ADR-0062): nothing reads that root after this, so the
    /// snapshotter may take it over instead of copying it (ADR-0061). What the new root holds is what
    /// [`Snapshotter::prepare`] with `Base::Stage(parent)` gives. Default:
    /// that.
    ///
    /// # Errors
    ///
    /// As [`Snapshotter::prepare`].
    fn prepare_last(&self, parent: &Self::Root) -> Result<Self::Root> {
        self.prepare(Base::Stage(parent))
    }

    /// A root on an image's layers that a stage only reads (`COPY
    /// --from=<image>`), made while that stage holds its own root. Default:
    /// [`Snapshotter::prepare`]; kaniko mode unpacks it beside the shared
    /// root instead of into it.
    ///
    /// # Errors
    ///
    /// As [`Snapshotter::prepare`].
    fn prepare_source(&self, layers: &[Self::Layer]) -> Result<Self::Root> {
        self.prepare(Base::Image(layers))
    }
}

/// A base layer's compressed bytes, opened when a stage is prepared on it
/// instead of kept: kaniko mode streams them from the registry into the
/// root (ADR-0055).
pub trait LayerOpen: std::fmt::Debug + Send + Sync {
    /// The blob from its first byte, compressed as in the registry. A read
    /// error, a digest that does not match included, ends the extraction.
    ///
    /// # Errors
    ///
    /// When the source cannot be reached.
    fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>>;
}

/// Overlay mode: one overlay mount per layer over the store's extracted
/// layers and the stage's earlier diffs (`ADR-0002`, `ADR-0016`).
#[derive(Debug)]
pub struct Overlay {
    store: Arc<Store>,
    state: Arc<RootPath>,
    api: MountApi,
    next_stage: AtomicU64,
}

impl Overlay {
    /// The overlay snapshotter of a build, with what the probe found.
    ///
    /// # Errors
    ///
    /// [`Error::UnsafePath`] when the state directory's path holds overlay
    /// option syntax; [`Error::Rootfs`] when it cannot be opened.
    pub fn new(store: Arc<Store>, caps: OverlayCaps) -> Result<Self> {
        mount::check_path(store.state_dir())?;
        let state = Arc::new(RootPath::open(store.state_dir())?);
        Ok(Self {
            store,
            state,
            api: caps.api,
            next_stage: AtomicU64::new(0),
        })
    }

    /// The store the snapshotter fills.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// [`Snapshotter::base_layer`] from a reader instead of a blob in the
    /// store.
    ///
    /// # Errors
    ///
    /// As [`Snapshotter::base_layer`].
    pub fn base_layer_from(
        &self,
        diff_id: &Digest,
        blob: impl std::io::Read,
        below: &[LayerDir],
    ) -> Result<LayerDir> {
        layer::extract_layer(&self.store, diff_id, blob, below)
    }
}

impl Snapshotter for Overlay {
    type Root = OverlayStage;
    type Layer = LayerDir;

    fn base_layer(&self, diff_id: &Digest, blob: &Path, below: &[LayerDir]) -> Result<LayerDir> {
        let file = File::open(blob).map_err(|source| Error::Io {
            op: "open",
            path: blob.to_owned(),
            source,
        })?;
        layer::extract_layer(&self.store, diff_id, file, below)
    }

    fn prepare(&self, base: Base<'_, OverlayStage, LayerDir>) -> Result<OverlayStage> {
        let (lowers, built, parent): (Vec<PathBuf>, _, _) = match base {
            Base::Scratch => (Vec::new(), 0, None),
            Base::Image(layers) => (
                layers.iter().rev().map(|l| l.path.clone()).collect(),
                0,
                None,
            ),
            Base::Stage(parent) => {
                if parent.mounted == Mounted::Layer {
                    return Err(Error::State("the parent stage has a layer open"));
                }
                (
                    parent.lowers.clone(),
                    parent.built,
                    Some(Arc::clone(&parent.children)),
                )
            }
        };
        let span = tracing::info_span!("rnk_snapshot.prepare", layers = lowers.len());
        let _entered = span.enter();
        let n = self.next_stage.fetch_add(1, Ordering::Relaxed);
        let rel = Path::new("builds")
            .join(self.store.build_id())
            .join("stages")
            .join(n.to_string());
        self.state.create_dir_all(&rel, PRIVATE)?;
        let dir = self.store.state_dir().join(&rel);
        let root = RootPath::open(&dir)?;
        root.create_dir("rootfs", 0o755)?;
        Ok(OverlayStage {
            state: Arc::clone(&self.state),
            rel,
            rootfs: dir.join("rootfs"),
            dir,
            root,
            api: self.api,
            lowers,
            built,
            next: 0,
            mounted: Mounted::Nothing,
            open: None,
            view: None,
            diffs: Vec::new(),
            children: Arc::new(()),
            _parent: parent,
        })
    }
}

/// What is mounted on a stage's `rootfs/`, tracked apart from the
/// `RootPath` opened on it, so that a mount whose `RootPath` could not be
/// opened is still known, and unmounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mounted {
    Nothing,
    /// The open layer, writable.
    Layer,
    /// The read-only view between layers.
    View,
}

/// An overlay stage root: `builds/<id>/stages/<n>/` with `rootfs/` (the
/// mount point), `diff-<k>/` and `work-<k>/`.
#[derive(Debug)]
pub struct OverlayStage {
    state: Arc<RootPath>,
    /// The stage directory relative to the state directory.
    rel: PathBuf,
    /// Its host path.
    dir: PathBuf,
    /// `rootfs/`, the mount point.
    rootfs: PathBuf,
    /// The stage directory as a root.
    root: RootPath,
    api: MountApi,
    /// Lower layers, topmost first: earlier diffs, then the base.
    lowers: Vec<PathBuf>,
    /// How many of the topmost `lowers` are diffs made by this build (this
    /// stage's or a parent stage's), as opposed to extracted image layers.
    built: usize,
    /// The number of layers begun.
    next: u64,
    /// What is mounted on `rootfs/`.
    mounted: Mounted,
    /// The writable root of the open layer.
    open: Option<RootPath>,
    /// The read-only view, mounted on `rootfs/` between layers.
    view: Option<RootPath>,
    /// The diffs of this stage, counted at release.
    diffs: Vec<PathBuf>,
    /// Shared with every stage built on this one ([`Base::Stage`]), whose
    /// lower stack holds this stage's diffs: release refuses while a clone
    /// is alive.
    children: Arc<()>,
    /// The parent's `children`, while this stage lives.
    _parent: Option<Arc<()>>,
}

impl OverlayStage {
    fn rootfs(&self) -> PathBuf {
        self.rootfs.clone()
    }

    /// Mounts `spec`; the `mount(2)` fallback makes its links in the
    /// stage's directory.
    fn mount(&self, spec: &Spec<'_>) -> Result<()> {
        mount::mount(spec, self.api, &self.root)
    }

    /// An empty directory of the stage, for mounts that need a lower layer
    /// more than the stack has.
    fn empty(&self, i: u8) -> Result<PathBuf> {
        let name = format!("empty-{i}");
        match self.root.create_dir(&name, 0o755) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::EXIST.raw_os_error()) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(self.dir.join(name))
    }

    /// Mounts `spec` on `rootfs/` as `what`, then opens it as a root. If
    /// that open fails, the mount is undone, so no error leaves it behind.
    fn mount_root(&mut self, spec: &Spec<'_>, what: Mounted) -> Result<RootPath> {
        self.mount(spec)?;
        self.mounted = what;
        match RootPath::open(&self.rootfs) {
            Ok(root) => Ok(root),
            Err(e) => {
                if mount::unmount(&self.rootfs).is_ok() {
                    self.mounted = Mounted::Nothing;
                }
                Err(e.into())
            }
        }
    }

    /// Unmounts whatever is mounted on `rootfs/`; the state changes only
    /// once the unmount succeeded.
    fn unmount_all(&mut self) -> Result<()> {
        if self.mounted != Mounted::Nothing {
            mount::unmount(&self.rootfs)?;
            self.mounted = Mounted::Nothing;
        }
        self.open = None;
        self.view = None;
        Ok(())
    }

    fn drop_view(&mut self) -> Result<()> {
        if self.mounted == Mounted::View {
            self.unmount_all()?;
        }
        Ok(())
    }

    /// The lower layers opened as roots, topmost first.
    fn lower_roots(&self) -> Result<Vec<RootPath>> {
        Ok(self
            .lowers
            .iter()
            .map(RootPath::open)
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

impl StageRootfs for OverlayStage {
    type Diff = LayerDiff;

    fn root(&self) -> Result<&RootPath> {
        self.open.as_ref().ok_or(Error::State("no layer is open"))
    }

    fn mount_point(&self) -> &Path {
        &self.rootfs
    }

    fn begin_layer(&mut self) -> Result<()> {
        if self.mounted == Mounted::Layer {
            return Err(Error::State("a layer is open already"));
        }
        self.drop_view()?;
        self.next += 1;
        let k = self.next;
        let (diff, work) = (format!("diff-{k}"), format!("work-{k}"));
        self.root.create_dir(&diff, PRIVATE)?;
        self.root.create_dir(&work, PRIVATE)?;
        // Overlay takes `/`'s metadata from the upper directory: give it the
        // lower root's, or a plain mkdir turns an image's 555 into 755.
        let meta = layer::root_meta(&self.lower_roots()?)?;
        self.root.set_meta(&diff, &meta)?;
        let lowers = if self.lowers.is_empty() {
            vec![self.empty(0)?]
        } else {
            self.lowers.clone()
        };
        let (upper, work) = (self.dir.join(diff), self.dir.join(work));
        let target = self.rootfs();
        let root = self.mount_root(
            &Spec {
                lowers: &lowers,
                upper: Some((&upper, &work)),
                target: &target,
            },
            Mounted::Layer,
        )?;
        self.open = Some(root);
        self.diffs.push(upper);
        Ok(())
    }

    fn end_layer(&mut self) -> Result<LayerDiff> {
        if self.mounted != Mounted::Layer {
            return Err(Error::State("no layer is open"));
        }
        // On failure the layer stays open, and so does its root.
        self.unmount_all()?;
        let upper = self.dir.join(format!("diff-{}", self.next));
        let diff = LayerDiff {
            upper: upper.clone(),
            lowers: self.lowers.clone(),
            built: self.built,
        };
        self.lowers.insert(0, upper);
        self.built += 1;
        Ok(diff)
    }

    fn view(&mut self) -> Result<&RootPath> {
        if self.mounted == Mounted::Layer {
            return self.root();
        }
        if self.mounted == Mounted::Nothing {
            // A read-only overlay needs two lower layers.
            let mut lowers = self.lowers.clone();
            for i in 0..2_u8 {
                if lowers.len() < 2 {
                    lowers.push(self.empty(i)?);
                }
            }
            let target = self.rootfs();
            let root = self.mount_root(
                &Spec {
                    lowers: &lowers,
                    upper: None,
                    target: &target,
                },
                Mounted::View,
            )?;
            self.view = Some(root);
        }
        self.view.as_ref().ok_or(Error::State("no view"))
    }

    /// Also refuses while a stage built on this one ([`Base::Stage`]) is
    /// alive: its lower stack holds this stage's diffs. A refused or failed
    /// release drops the stage (unmounted); its directories stay until the
    /// store closes.
    fn release(mut self) -> Result<()> {
        let span = tracing::info_span!("rnk_snapshot.release", entries = self.diffs.len());
        let _entered = span.enter();
        if Arc::strong_count(&self.children) > 1 {
            return Err(Error::State(
                "a stage built on this one is still alive; release it first",
            ));
        }
        self.unmount_all()?;
        delete_contents(&self.root, &IgnoreList::new())?;
        self.state.remove_dir(&self.rel)?;
        Ok(())
    }
}

impl Drop for OverlayStage {
    /// A stage dropped without [`StageRootfs::release`] still unmounts, so
    /// the store can delete `builds/<id>` at close; its directories stay
    /// for that.
    fn drop(&mut self) {
        let _ = self.unmount_all();
    }
}
