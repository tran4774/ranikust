//! Kaniko-mode stage roots in per-stage directories
//! (`design/kaniko-mode.md#per-stage-directories`, ADR-0059).
//!
//! Where kaniko mode builds outside a container, root may mount and `RUN`
//! gets the full sandbox (ADR-0050, Q-330), so nothing ties the stages to
//! one root: each stage gets a directory of its own in the build's private
//! area of the state directory, `builds/<id>/stages/<n>/rootfs`, and the
//! stages run as overlay mode's do (the stage graph, `--jobs`), each walked
//! and hashed for its layers as on the shared root.
//!
//! - A stage on an image extracts its base layers into its new directory
//!   (a blob of the store, or streamed and checked at its end, ADR-0055: a
//!   failure deletes the directory). Device nodes and FIFOs are left out,
//!   as for every kaniko-mode extraction (ADR-0041).
//! - A stage built `FROM <stage>` copies the parent's directory into its
//!   own (`rnk_rootfs::copy_filtered`, owners, modes, times, xattrs and
//!   hardlinks inside the tree kept; a reflink where the filesystem has
//!   them, else only the data ranges, so holes stay holes), never hardlinks
//!   to it (ADR-0017): nothing a `RUN` does reaches the parent or a
//!   sibling. Device nodes, FIFOs and sockets are left out, as from every
//!   kaniko-mode extraction (ADR-0041).
//! - Nothing is kept or emptied between stages: a stage's directory stays
//!   until it is released, and a `COPY --from` reads it as it is.
//! - The walk and its record live with the stage and go when it is done
//!   ([`StageRootfs::done`]); a stage whose layers are not needed is never
//!   walked (ADR-0038).
//! - Release deletes the directory through descriptors, crossing no mount.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rnk_layer::LayerDigests;
use rnk_oci::Digest;
use rnk_rootfs::{CopyOpts, IgnoreList, Meta, RootPath, copy_filtered, delete_contents};
use rnk_store::Store;

use super::root::{KanikoLayer, extract_blob};
use super::walk::{WalkDiff, WalkOptions, Walker};
use crate::error::{Error, Result};
use crate::export::DiffCounts;
use crate::stage::{Base, ExportLayer, LayerOpen, Snapshotter, StageRootfs};

/// Mode of a stage directory: its root holds an image's setuid files.
const PRIVATE: u32 = 0o700;

/// The kaniko-mode snapshotter of a build whose stages each get a
/// directory of their own (ADR-0059).
#[derive(Debug)]
pub struct KanikoStages {
    shared: Arc<StagesShared>,
}

#[derive(Debug)]
struct StagesShared {
    /// `builds/<id>/stages`, made and opened once, staying on its mount.
    stages: RootPath,
    /// Its host path, for the sandbox.
    stages_path: PathBuf,
    /// What every stage root leaves out of extraction, the walk and the
    /// copies: the fixed paths and `--ignore-path` (`kaniko::ignore_list`
    /// on the stages directory, where nothing is mounted).
    ignore: IgnoreList,
    walk: WalkOptions,
    next: AtomicU64,
}

impl KanikoStages {
    /// The snapshotter of the build `store` holds (a store with names, in
    /// a state directory checked by the set-up), with the ignore list each
    /// stage root uses (relative to a stage root) and the walk's options.
    ///
    /// # Errors
    ///
    /// [`Error::Rootfs`] when the state directory cannot be opened;
    /// [`Error::State`] for an anonymous store (kaniko mode on `/`, which
    /// has one shared root).
    pub fn new(store: &Store, ignore: IgnoreList, walk: WalkOptions) -> Result<Self> {
        if store.is_anonymous() {
            return Err(Error::State(
                "per-stage directories need a state directory with names; kaniko mode on / \
                 builds on the shared root",
            ));
        }
        let build = Path::new("builds").join(store.build_id());
        let stages = RootPath::open(store.state_dir())?
            .stay_on_mount()
            .sub_root(&build)?
            .create_sub_root("stages", PRIVATE)?;
        Ok(Self {
            shared: Arc::new(StagesShared {
                stages_path: store.state_dir().join(build).join("stages"),
                stages,
                ignore,
                walk,
                next: AtomicU64::new(0),
            }),
        })
    }

    /// The host path of the directory every stage directory is made in,
    /// `<state>/builds/<id>/stages`.
    #[cfg(test)]
    pub(crate) fn stages_dir(&self) -> &Path {
        &self.shared.stages_path
    }
}

/// The owner, mode and times of `root`'s own directory.
fn root_meta(root: &RootPath) -> Result<Meta> {
    let st = root.statx("/")?;
    Ok(Meta {
        uid: Some(st.uid),
        gid: Some(st.gid),
        mode: Some(st.mode & 0o7777),
        atime: Some(st.atime),
        mtime: Some(st.mtime),
        xattrs: Vec::new(),
    })
}

impl Snapshotter for KanikoStages {
    type Root = KanikoStage;
    type Layer = KanikoLayer;

    fn base_layer(
        &self,
        diff_id: &Digest,
        blob: &Path,
        _below: &[KanikoLayer],
    ) -> Result<KanikoLayer> {
        Ok(KanikoLayer::from_blob(diff_id, blob))
    }

    fn streamed_layer(&self, diff_id: &Digest, open: Arc<dyn LayerOpen>) -> Option<KanikoLayer> {
        Some(KanikoLayer::from_stream(diff_id, open))
    }

    fn prepare(&self, base: Base<'_, KanikoStage, KanikoLayer>) -> Result<KanikoStage> {
        if let Base::Stage(parent) = &base
            && parent.open
        {
            return Err(Error::State("the parent stage has a layer open"));
        }
        let shared = &self.shared;
        let layers = match &base {
            Base::Image(layers) => layers.len(),
            Base::Scratch | Base::Stage(_) => 0,
        };
        let span = tracing::info_span!("rnk_snapshot.prepare", layers);
        let _entered = span.enter();
        let name = shared.next.fetch_add(1, Ordering::Relaxed).to_string();
        // Made and opened on the same parent descriptor, checked empty and
        // ours: a name swapped in between is refused, never followed.
        let dir = shared.stages.create_sub_root(&name, PRIVATE)?;
        let root = match dir.create_sub_root("rootfs", 0o755) {
            Ok(root) => root,
            Err(e) => {
                remove_stage_dir(shared, &name, &dir);
                return Err(e.into());
            }
        };
        let stage = KanikoStage {
            shared: Arc::clone(shared),
            path: shared.stages_path.join(&name).join("rootfs"),
            name,
            dir,
            root: Arc::new(root),
            needed: true,
            open: false,
            walked: false,
            walker: None,
        };
        if let Err(e) = stage.fill(base) {
            if let Err(r) = stage.remove() {
                tracing::warn!("removing a stage directory after a failure: {r}");
            }
            return Err(e);
        }
        Ok(stage)
    }
}

/// Deletes a stage directory whose root could not be made.
fn remove_stage_dir(shared: &StagesShared, name: &str, dir: &RootPath) {
    let r = delete_contents(dir, &IgnoreList::new())
        .map_err(Error::from)
        .and_then(|()| shared.stages.remove_dir(name).map_err(Error::from));
    if let Err(r) = r {
        tracing::warn!("removing the stage directory {name}: {r}");
    }
}

/// A stage root in a directory of its own (ADR-0059).
#[derive(Debug)]
pub struct KanikoStage {
    shared: Arc<StagesShared>,
    /// The stage directory's name in `builds/<id>/stages`.
    name: String,
    /// The stage directory, opened once.
    dir: RootPath,
    /// Its `rootfs/`, opened once; shared with the diffs, which read it.
    root: Arc<RootPath>,
    /// The host path of `rootfs/`, for the sandbox.
    path: PathBuf,
    needed: bool,
    open: bool,
    /// Whether the walker's record is the root as it is now.
    walked: bool,
    /// Made at the first walk (it owns a thread pool), dropped when the
    /// stage is done.
    walker: Option<Walker>,
}

impl KanikoStage {
    /// Puts `base` into the new, empty root.
    fn fill(&self, base: Base<'_, KanikoStage, KanikoLayer>) -> Result<()> {
        let root = &*self.root;
        let ignore = &self.shared.ignore;
        match base {
            Base::Scratch => root.set_meta("", &crate::layer::default_root())?,
            Base::Image(layers) => {
                // As overlay mode's stage roots: `0755` and the epoch unless
                // a layer sets `./`.
                root.set_meta("", &crate::layer::default_root())?;
                for layer in layers {
                    extract_blob(root, ignore, layer)?;
                }
            }
            Base::Stage(parent) => {
                // Device nodes, FIFOs and sockets a parent's `RUN` made are
                // left out, as from every kaniko-mode extraction and layer
                // (ADR-0041): nothing is `mknod`ed outside a sandbox.
                let opts = CopyOpts {
                    skip_special: true,
                    ..CopyOpts::default()
                };
                copy_filtered(&parent.root, "", root, "", &opts, &|p, _| !ignore.covers(p))?;
                root.set_meta("", &root_meta(&parent.root)?)?;
            }
        }
        Ok(())
    }

    /// Deletes the stage directory, through descriptors, crossing no mount.
    fn remove(&self) -> Result<()> {
        let span = tracing::info_span!("rnk_snapshot.release");
        let _entered = span.enter();
        delete_contents(&self.dir, &IgnoreList::new())?;
        self.shared.stages.remove_dir(&self.name)?;
        Ok(())
    }

    /// Walks the root as it is now, as the base of the next diff.
    fn baseline(&mut self) -> Result<()> {
        let walker = match self.walker.as_mut() {
            Some(w) => w,
            None => self.walker.insert(Walker::new(self.shared.walk)?),
        };
        walker.forget();
        walker.baseline(&self.root, &self.shared.ignore)?;
        self.walked = true;
        Ok(())
    }
}

impl StageRootfs for KanikoStage {
    type Diff = KanikoStageDiff;

    fn set_layers_needed(&mut self, needed: bool) {
        self.needed = needed;
    }

    fn root(&self) -> Result<&RootPath> {
        if !self.open {
            return Err(Error::State("no layer is open"));
        }
        Ok(&self.root)
    }

    fn mount_point(&self) -> &Path {
        &self.path
    }

    fn begin_layer(&mut self) -> Result<()> {
        if self.open {
            return Err(Error::State("a layer is open already"));
        }
        if self.needed && !self.walked {
            self.baseline()?;
        }
        self.open = true;
        Ok(())
    }

    fn end_layer(&mut self) -> Result<KanikoStageDiff> {
        if !self.open {
            return Err(Error::State("no layer is open"));
        }
        self.open = false;
        if !self.needed {
            self.walked = false;
            return Ok(KanikoStageDiff {
                diff: WalkDiff::default(),
                root: Arc::clone(&self.root),
            });
        }
        let walker = self
            .walker
            .as_mut()
            .ok_or(Error::State("no baseline walk before the layer"))?;
        let diff = walker.diff(&self.root, &self.shared.ignore)?;
        Ok(KanikoStageDiff {
            diff,
            root: Arc::clone(&self.root),
        })
    }

    fn view(&mut self) -> Result<&RootPath> {
        Ok(&self.root)
    }

    fn done(&mut self) {
        // Its record of the root and its thread pool: the root is only read
        // from now on.
        self.walker = None;
        self.walked = false;
    }

    fn release(self) -> Result<()> {
        self.remove()
    }
}

/// A layer of a stage in a directory of its own: the walk's diff with the
/// root it was walked on. Encode it before the root changes again.
#[derive(Debug)]
pub struct KanikoStageDiff {
    diff: WalkDiff,
    root: Arc<RootPath>,
}

impl KanikoStageDiff {
    /// The diff.
    #[must_use]
    pub fn diff(&self) -> &WalkDiff {
        &self.diff
    }
}

impl ExportLayer for KanikoStageDiff {
    fn export(
        self,
        options: rnk_layer::EncodeOptions,
        out: &mut dyn Write,
    ) -> Result<(LayerDigests, DiffCounts)> {
        self.diff.export(&self.root, options, out)
    }
}

#[cfg(test)]
mod tests;
