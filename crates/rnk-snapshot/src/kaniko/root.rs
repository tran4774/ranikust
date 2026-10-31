//! Kaniko-mode stage roots on the one shared root
//! (`design/kaniko-mode.md#stage-roots-on-the-shared-root`, ADR-0046).
//!
//! The root (`/` in a container, else a directory `ranikust` made in its
//! state directory, ADR-0050) is opened
//! once by the caller and held by at most one [`KanikoRoot`] at a time.
//! Base layers stay blobs in the store; a stage on an image extracts them
//! into the root, which is emptied first unless nothing has changed it yet
//! and it is `/`. Layers come from the walk, only for a stage whose layers
//! are needed.
//!
//! When the root is emptied follows kaniko on `/` (`kaniko@44aa7ddc0
//! pkg/executor/build.go`): the first base is extracted over what is there
//! (`:632-641`, no deletion without `--pre-cleanup`, `:1944-1947`), the root
//! is emptied between stages (`:2179`), and the end of the build leaves it
//! (only `--cleanup` empties it, `:1955-1957`). A root in the state
//! directory, which ranikust made for the build, is emptied before the
//! first base too,
//! and at the end of a build that changed it ([`Kaniko::finish`]).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rnk_layer::{EncodeOptions, LayerDigests};
use rnk_oci::Digest;
use rnk_rootfs::{IgnoreList, RootPath, delete_contents, extract_live};

use super::walk::{WalkDiff, WalkOptions, Walker};
use crate::error::{Error, Result};
use crate::export::DiffCounts;
use crate::stage::{Base, ExportLayer, Snapshotter, StageRootfs};

/// A base layer in kaniko mode: its blob in the store, extracted into the
/// root only when a stage is prepared on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KanikoLayer {
    diff_id: Digest,
    blob: PathBuf,
}

impl KanikoLayer {
    /// The diff ID of the layer.
    #[must_use]
    pub fn diff_id(&self) -> &Digest {
        &self.diff_id
    }

    /// The blob, compressed as pulled, in the store.
    #[must_use]
    pub fn blob(&self) -> &Path {
        &self.blob
    }
}

/// Which stage holds the root, and what the walker knows of it.
#[derive(Debug)]
struct Hold {
    /// The [`KanikoRoot`] holding the root, by id; `None` when released.
    holder: Option<u64>,
    /// Whether the walker's record is the root as it is now: false after
    /// an instruction that was not walked, or a new base.
    walked: bool,
    /// Created at the first walk, after the process set-up: it owns a
    /// thread pool.
    walker: Option<Walker>,
    /// Whether a stage of this build changed the root (a base extracted
    /// into it): only then is there anything of the build's to empty.
    touched: bool,
}

#[derive(Debug)]
struct Shared {
    root: RootPath,
    path: PathBuf,
    /// The root's owner and mode when the build began, given back each
    /// time a root other than `/` is emptied: a base's `./` may change them.
    original: Option<rnk_rootfs::Meta>,
    ignore: IgnoreList,
    walk: WalkOptions,
    hold: Mutex<Hold>,
    next_id: AtomicU64,
}

impl Shared {
    fn hold(&self) -> MutexGuard<'_, Hold> {
        self.hold.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The kaniko-mode snapshotter of a build: stage roots on one shared root.
#[derive(Debug, Clone)]
pub struct Kaniko {
    shared: Arc<Shared>,
}

impl Kaniko {
    /// The snapshotter on `root`, opened by the caller at `path` (`/` or the
    /// directory made in the state directory; `path` is only handed to the
    /// sandbox), with the
    /// build's ignore list (`kaniko::ignore_list`) and walk options.
    #[must_use]
    pub fn new(root: RootPath, path: PathBuf, ignore: IgnoreList, walk: WalkOptions) -> Self {
        let original = root.statx("/").ok().map(|st| rnk_rootfs::Meta {
            uid: Some(st.uid),
            gid: Some(st.gid),
            mode: Some(st.mode & 0o7777),
            ..rnk_rootfs::Meta::default()
        });
        Self {
            shared: Arc::new(Shared {
                root,
                path,
                original,
                ignore,
                walk,
                hold: Mutex::new(Hold {
                    holder: None,
                    walked: false,
                    walker: None,
                    touched: false,
                }),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// The end of the build: a root other than `/` that a stage changed is
    /// emptied, keeping what the ignore list holds; `/` is left as it is,
    /// as kaniko leaves it without `--cleanup`; a root no stage changed
    /// (the build failed before its first base) is never touched. Returns
    /// whether it emptied the root.
    ///
    /// # Errors
    ///
    /// [`Error::State`] while a stage holds the root; [`Error::Rootfs`].
    pub fn finish(&self) -> Result<bool> {
        let mut hold = self.shared.hold();
        if hold.holder.is_some() {
            return Err(Error::State("a stage holds the shared root"));
        }
        if !hold.touched || self.shared.is_slash() {
            return Ok(false);
        }
        empty_root(&self.shared, &mut hold)?;
        hold.touched = false;
        Ok(true)
    }
}

impl Shared {
    fn is_slash(&self) -> bool {
        self.path == Path::new("/")
    }
}

fn empty_root(shared: &Shared, hold: &mut Hold) -> Result<()> {
    let span = tracing::info_span!("rnk_snapshot.empty");
    let _entered = span.enter();
    delete_contents(&shared.root, &shared.ignore)?;
    // A root other than `/` gets its owner and mode back; `/` is never
    // emptied whole.
    if !shared.is_slash()
        && let Some(meta) = &shared.original
    {
        shared.root.set_meta("", meta)?;
    }
    hold.walked = false;
    if let Some(w) = hold.walker.as_mut() {
        w.forget();
    }
    Ok(())
}

impl Snapshotter for Kaniko {
    type Root = KanikoRoot;
    type Layer = KanikoLayer;

    fn base_layer(
        &self,
        diff_id: &Digest,
        blob: &Path,
        _below: &[KanikoLayer],
    ) -> Result<KanikoLayer> {
        Ok(KanikoLayer {
            diff_id: diff_id.clone(),
            blob: blob.to_owned(),
        })
    }

    fn prepare(&self, base: Base<'_, KanikoRoot, KanikoLayer>) -> Result<KanikoRoot> {
        let shared = &self.shared;
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut hold = shared.hold();
        match base {
            Base::Stage(parent) => {
                // A squashed chain: the child goes on from the parent's root
                // as it is. Anything else needs the parent's image, which
                // the root no longer holds.
                if !Arc::ptr_eq(&parent.shared, shared) || hold.holder != Some(parent.id) {
                    return Err(Error::State(
                        "the parent stage no longer holds the shared root",
                    ));
                }
                if parent.open {
                    return Err(Error::State("the parent stage has a layer open"));
                }
            }
            Base::Scratch | Base::Image(_) => {
                if hold.holder.is_some() {
                    return Err(Error::State("another stage holds the shared root"));
                }
                let layers = match base {
                    Base::Image(layers) => layers,
                    Base::Scratch | Base::Stage(_) => &[],
                };
                let span = tracing::info_span!("rnk_snapshot.prepare", layers = layers.len());
                let _entered = span.enter();
                // On `/` the first base goes over what is there, as kaniko
                // (see the module doc); later ones, and every base on a
                // root other than `/`, start from an empty root.
                if hold.touched || !shared.is_slash() {
                    empty_root(shared, &mut hold)?;
                } else {
                    hold.walked = false;
                }
                hold.touched = true;
                // Another root starts as overlay mode's stage roots do:
                // `0755` and the epoch, unless a layer sets `./`. On `/`
                // the container's root stays as it is, as under kaniko.
                if !shared.is_slash() {
                    shared.root.set_meta("", &crate::layer::default_root())?;
                }
                for layer in layers {
                    extract_blob(shared, layer)?;
                }
            }
        }
        hold.holder = Some(id);
        Ok(KanikoRoot {
            shared: Arc::clone(shared),
            id,
            needed: false,
            open: false,
        })
    }
}

/// Extracts one base layer into the root, checking its diff ID.
fn extract_blob(shared: &Shared, layer: &KanikoLayer) -> Result<()> {
    let file = File::open(&layer.blob).map_err(|source| Error::Io {
        op: "open",
        path: layer.blob.clone(),
        source,
    })?;
    let mut decoder = rnk_layer::Decoder::new(file)?;
    extract_live(&shared.root, &mut decoder, &shared.ignore)?;
    let actual = Digest::from_sha256_bytes(decoder.finish()?.as_bytes());
    if actual != layer.diff_id {
        return Err(Error::DiffIdMismatch {
            expected: layer.diff_id.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// A stage on the shared root.
#[derive(Debug)]
pub struct KanikoRoot {
    shared: Arc<Shared>,
    id: u64,
    needed: bool,
    open: bool,
}

impl KanikoRoot {
    fn check_held(&self, hold: &Hold) -> Result<()> {
        if hold.holder == Some(self.id) {
            Ok(())
        } else {
            Err(Error::State("the stage no longer holds the shared root"))
        }
    }

    /// Walks the root as it is now, as the base of the next diff.
    fn baseline(&self, hold: &mut Hold) -> Result<()> {
        let walker = match hold.walker.as_mut() {
            Some(w) => w,
            None => hold.walker.insert(Walker::new(self.shared.walk)?),
        };
        walker.forget();
        walker.baseline(&self.shared.root, &self.shared.ignore)?;
        hold.walked = true;
        Ok(())
    }
}

impl StageRootfs for KanikoRoot {
    type Diff = KanikoDiff;

    fn set_layers_needed(&mut self, needed: bool) {
        self.needed = needed;
    }

    fn root(&self) -> Result<&RootPath> {
        if !self.open {
            return Err(Error::State("no layer is open"));
        }
        Ok(&self.shared.root)
    }

    fn mount_point(&self) -> &Path {
        &self.shared.path
    }

    fn begin_layer(&mut self) -> Result<()> {
        if self.open {
            return Err(Error::State("a layer is open already"));
        }
        let mut hold = self.shared.hold();
        self.check_held(&hold)?;
        if self.needed && !hold.walked {
            self.baseline(&mut hold)?;
        }
        self.open = true;
        Ok(())
    }

    fn end_layer(&mut self) -> Result<KanikoDiff> {
        if !self.open {
            return Err(Error::State("no layer is open"));
        }
        self.open = false;
        let mut hold = self.shared.hold();
        self.check_held(&hold)?;
        if !self.needed {
            hold.walked = false;
            return Ok(KanikoDiff {
                diff: WalkDiff::default(),
                shared: Arc::clone(&self.shared),
            });
        }
        let walker = hold
            .walker
            .as_mut()
            .ok_or(Error::State("no baseline walk before the layer"))?;
        let diff = walker.diff(&self.shared.root, &self.shared.ignore)?;
        Ok(KanikoDiff {
            diff,
            shared: Arc::clone(&self.shared),
        })
    }

    fn view(&mut self) -> Result<&RootPath> {
        self.check_held(&self.shared.hold())?;
        Ok(&self.shared.root)
    }

    /// Gives the root up; the next stage on a base empties it, and the end
    /// of the build ([`Kaniko::finish`]) empties a root other than `/`.
    /// Nothing when the
    /// root was handed over to a child stage built on this one.
    fn release(self) -> Result<()> {
        let mut hold = self.shared.hold();
        if hold.holder == Some(self.id) {
            hold.holder = None;
        }
        Ok(())
    }
}

/// A kaniko-mode layer: the walk's diff with the root it was walked on.
/// Encode it before the root changes again.
#[derive(Debug)]
pub struct KanikoDiff {
    diff: WalkDiff,
    shared: Arc<Shared>,
}

impl KanikoDiff {
    /// The diff.
    #[must_use]
    pub fn diff(&self) -> &WalkDiff {
        &self.diff
    }
}

impl ExportLayer for KanikoDiff {
    fn export(
        self,
        options: EncodeOptions,
        out: &mut dyn Write,
    ) -> Result<(LayerDigests, DiffCounts)> {
        self.diff.export(&self.shared.root, options, out)
    }
}

#[cfg(test)]
mod tests;
