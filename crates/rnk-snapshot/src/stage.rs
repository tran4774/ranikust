//! Stage roots: the `Snapshotter` and `StageRootfs` traits and their overlay
//! implementation (`design/overlay-snapshot.md#one-layer-per-instruction`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rnk_oci::Digest;
use rnk_rootfs::{IgnoreList, RootPath, delete_contents};
use rnk_store::Store;

use crate::error::{Error, Result};
use crate::export::LayerDiff;
use crate::layer::{self, LayerDir};
use crate::mount::{self, MountApi, ShortNames, Spec};
use crate::probe::OverlayCaps;

/// Mode of the stage directory and its work directories.
const PRIVATE: u32 = 0o700;

/// What a stage starts from.
#[derive(Debug)]
pub enum Base<'a, R> {
    /// `FROM scratch`: nothing.
    Scratch,
    /// `FROM <image>`: its layers, extracted, in image order (the
    /// config's `diff_ids` order, bottom first).
    Image(&'a [LayerDir]),
    /// `FROM <stage>`: the root another stage ended with. That stage must
    /// not be released while this one runs.
    Stage(&'a R),
}

/// One stage's root filesystem, changed one layer at a time.
///
/// [open: Q-024] Interim signature, designed at Phase 1 task 9: a layer is
/// opened before an instruction that changes files and closed after it,
/// which gives its diff; between layers the root is only read.
pub trait StageRootfs {
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
    fn end_layer(&mut self) -> Result<LayerDiff>;

    /// The root as the next layer would see it, read-only, for reading
    /// between layers (`USER` lookups, `COPY --from` sources). A layer
    /// open: its writable root.
    ///
    /// # Errors
    ///
    /// [`Error::Mount`] and the like when the read-only view cannot be
    /// mounted.
    fn view(&mut self) -> Result<&RootPath>;

    /// Unmounts and deletes the stage's directories, diffs included: every
    /// [`LayerDiff`] of the stage must be exported, and every stage built
    /// on it done, before.
    ///
    /// # Errors
    ///
    /// [`Error::Unmount`], or [`Error::Rootfs`] when deleting fails.
    fn release(self) -> Result<()>;
}

/// Makes stage roots and fills the store with base layers.
///
/// [open: Q-024] Interim signature (see [`StageRootfs`]); kaniko mode
/// implements it in Phase 2.
pub trait Snapshotter {
    /// The stage roots it makes.
    type Root: StageRootfs;

    /// Extracts the layer blob `blob` as `diff_id` above `below` (image
    /// order), unless the store has it, and returns it.
    ///
    /// # Errors
    ///
    /// A store, decoding, diff ID or extraction error.
    fn base_layer<R: Read>(
        &self,
        diff_id: &Digest,
        blob: R,
        below: &[LayerDir],
    ) -> Result<LayerDir>;

    /// A new stage root on `base`. No layer is open yet.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when the parent stage has a layer open;
    /// [`Error::Rootfs`] when the stage directory cannot be made.
    fn prepare(&self, base: Base<'_, Self::Root>) -> Result<Self::Root>;
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
    /// [`Error::Rootfs`] when the state directory cannot be opened.
    pub fn new(store: Arc<Store>, caps: OverlayCaps) -> Result<Self> {
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
}

impl Snapshotter for Overlay {
    type Root = OverlayStage;

    fn base_layer<R: Read>(
        &self,
        diff_id: &Digest,
        blob: R,
        below: &[LayerDir],
    ) -> Result<LayerDir> {
        layer::extract_layer(&self.store, diff_id, blob, below)
    }

    fn prepare(&self, base: Base<'_, OverlayStage>) -> Result<OverlayStage> {
        let lowers: Vec<PathBuf> = match base {
            Base::Scratch => Vec::new(),
            Base::Image(layers) => layers.iter().rev().map(|l| l.path.clone()).collect(),
            Base::Stage(parent) => {
                if parent.open.is_some() {
                    return Err(Error::State("the parent stage has a layer open"));
                }
                parent.lowers.clone()
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
            state_path: self.store.state_dir().to_owned(),
            rel,
            rootfs: dir.join("rootfs"),
            dir,
            root,
            api: self.api,
            lowers,
            next: 0,
            open: None,
            view: None,
            diffs: Vec::new(),
        })
    }
}

/// An overlay stage root: `builds/<id>/stages/<n>/` with `rootfs/` (the
/// mount point), `diff-<k>/` and `work-<k>/`.
#[derive(Debug)]
pub struct OverlayStage {
    state: Arc<RootPath>,
    state_path: PathBuf,
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
    /// The number of layers begun.
    next: u64,
    /// The writable root of the open layer.
    open: Option<RootPath>,
    /// The read-only view, mounted on `rootfs/` between layers.
    view: Option<RootPath>,
    /// The diffs of this stage, for their short names.
    diffs: Vec<PathBuf>,
}

impl OverlayStage {
    fn rootfs(&self) -> PathBuf {
        self.rootfs.clone()
    }

    fn mount(&self, spec: &Spec<'_>) -> Result<()> {
        let names = ShortNames {
            state: &self.state,
            state_path: &self.state_path,
        };
        mount::mount(spec, self.api, &mut |p| names.get(p))
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

    fn drop_view(&mut self) -> Result<()> {
        if self.view.take().is_some() {
            mount::unmount(&self.rootfs())?;
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
    fn root(&self) -> Result<&RootPath> {
        self.open.as_ref().ok_or(Error::State("no layer is open"))
    }

    fn mount_point(&self) -> &Path {
        &self.rootfs
    }

    fn begin_layer(&mut self) -> Result<()> {
        if self.open.is_some() {
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
        self.mount(&Spec {
            lowers: &lowers,
            upper: Some((&upper, &work)),
            target: &target,
        })?;
        self.open = Some(RootPath::open(&target)?);
        self.diffs.push(upper);
        Ok(())
    }

    fn end_layer(&mut self) -> Result<LayerDiff> {
        let Some(root) = self.open.take() else {
            return Err(Error::State("no layer is open"));
        };
        drop(root);
        mount::unmount(&self.rootfs())?;
        let upper = self.dir.join(format!("diff-{}", self.next));
        let diff = LayerDiff {
            upper: upper.clone(),
            lowers: self.lowers.clone(),
        };
        self.lowers.insert(0, upper);
        Ok(diff)
    }

    fn view(&mut self) -> Result<&RootPath> {
        if self.open.is_some() {
            return self.root();
        }
        if self.view.is_none() {
            // A read-only overlay needs two lower layers.
            let mut lowers = self.lowers.clone();
            for i in 0..2_u8 {
                if lowers.len() < 2 {
                    lowers.push(self.empty(i)?);
                }
            }
            let target = self.rootfs();
            self.mount(&Spec {
                lowers: &lowers,
                upper: None,
                target: &target,
            })?;
            self.view = Some(RootPath::open(&target)?);
        }
        self.view.as_ref().ok_or(Error::State("no view"))
    }

    fn release(mut self) -> Result<()> {
        let span = tracing::info_span!("rnk_snapshot.release", entries = self.diffs.len());
        let _entered = span.enter();
        self.unmount_all()?;
        delete_contents(&self.root, &IgnoreList::new())?;
        self.state.remove_dir(&self.rel)?;
        let names = ShortNames {
            state: &self.state,
            state_path: &self.state_path,
        };
        for d in std::mem::take(&mut self.diffs) {
            names.remove(&d);
        }
        Ok(())
    }
}

impl OverlayStage {
    fn unmount_all(&mut self) -> Result<()> {
        if self.open.take().is_some() {
            mount::unmount(&self.rootfs())?;
        }
        self.drop_view()
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
