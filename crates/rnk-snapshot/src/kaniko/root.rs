//! Kaniko-mode stage roots on the one shared root
//! (`design/kaniko-mode.md#stage-roots-on-the-shared-root`, ADR-0046).
//!
//! The root (`/` in a container, else a directory `ranikust` made in its
//! state directory, ADR-0050) is opened
//! once by the caller and held by at most one [`KanikoRoot`] at a time.
//! Base layers stay blobs in the store, or are streamed from their
//! registry each time (ADR-0055); a stage on an image extracts them into
//! the root, which is emptied first unless nothing has changed it yet
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
//!
//! What a later stage reads of an earlier one (`COPY --from`, `RUN --mount
//! from=`, `FROM <stage>`) is kept before the root is reused
//! ([`StageRootfs::keep`]):
//!
//! - On a root other than `/`, copied into the build's keep area beside the
//!   root, `<state>/builds/<id>/kept`, one directory per stage; an image
//!   read by `COPY --from=<image>` is unpacked there
//!   ([`Snapshotter::prepare_source`]).
//! - On `/`, where a `RUN` reads every file with a name (ADR-0051), as a
//!   plain tar in an anonymous file of the store, and an image as its
//!   layers' blobs (ADR-0056, Q-350). A `COPY --from` reads either from a
//!   directory unpacked in the state directory, under a name the store
//!   gives, inside a window of the store's fence: no `RUN` runs while it
//!   exists, and the end of the instruction that read it deletes it
//!   ([`StageRootfs::end_layer`]). A stage built `FROM` a kept one gets the
//!   tar extracted into the emptied root, with device nodes and FIFOs left
//!   out as for any extraction into a live root (ADR-0041).

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use rnk_layer::{Compression, EncodeOptions, LayerDigests};
use rnk_oci::Digest;
use rnk_rootfs::{
    CopyOpts, FileType, IgnoreList, MappedIds, RootPath, Unmapped, Whiteouts, copy_filtered,
    delete_contents, extract, extract_live, extract_live_literal,
};
use rnk_store::{NamedGuard, Store};
use rustix::io::Errno;

use super::walk::{SnapshotMode, WalkDiff, WalkOptions, Walker, baseline_in, diff_in};
use crate::error::{Error, Result};
use crate::export::DiffCounts;
use crate::owners;
use crate::stage::{Base, ExportLayer, Keep, LayerOpen, Snapshotter, StageRootfs};
use crate::userns::UserNamespace;

/// A base layer in kaniko mode, extracted into the root only when a stage
/// is prepared on it: its blob in the store, or streamed from its source
/// each time, never kept (ADR-0055).
#[derive(Clone, Debug)]
pub struct KanikoLayer {
    diff_id: Digest,
    source: LayerSource,
}

/// Where a [`KanikoLayer`]'s compressed bytes come from.
#[derive(Clone, Debug)]
enum LayerSource {
    /// A blob file in the store.
    Blob(PathBuf),
    /// Opened anew for each extraction.
    Stream(Arc<dyn LayerOpen>),
}

impl PartialEq for KanikoLayer {
    fn eq(&self, other: &Self) -> bool {
        self.diff_id == other.diff_id
            && match (&self.source, &other.source) {
                (LayerSource::Blob(a), LayerSource::Blob(b)) => a == b,
                (LayerSource::Stream(a), LayerSource::Stream(b)) => Arc::ptr_eq(a, b),
                (LayerSource::Blob(_), LayerSource::Stream(_))
                | (LayerSource::Stream(_), LayerSource::Blob(_)) => false,
            }
    }
}

impl Eq for KanikoLayer {}

impl KanikoLayer {
    /// A layer whose blob is `blob` in the store.
    pub(super) fn from_blob(diff_id: &Digest, blob: &Path) -> Self {
        Self {
            diff_id: diff_id.clone(),
            source: LayerSource::Blob(blob.to_owned()),
        }
    }

    /// A layer opened anew from its source for each extraction.
    pub(super) fn from_stream(diff_id: &Digest, open: Arc<dyn LayerOpen>) -> Self {
        Self {
            diff_id: diff_id.clone(),
            source: LayerSource::Stream(open),
        }
    }

    /// The diff ID of the layer.
    #[must_use]
    pub fn diff_id(&self) -> &Digest {
        &self.diff_id
    }

    /// The blob, compressed as pulled, in the store; `None` for a layer
    /// streamed from its source.
    #[must_use]
    pub fn blob(&self) -> Option<&Path> {
        match &self.source {
            LayerSource::Blob(path) => Some(path),
            LayerSource::Stream(_) => None,
        }
    }

    /// The layer's compressed bytes from the first one.
    pub(super) fn open(&self) -> Result<Box<dyn std::io::Read + Send>> {
        match &self.source {
            LayerSource::Blob(path) => {
                let file = File::open(path).map_err(|source| Error::Io {
                    op: "open",
                    path: path.clone(),
                    source,
                })?;
                Ok(Box::new(file))
            }
            LayerSource::Stream(open) => open.open().map_err(|source| Error::Io {
                op: "open the stream of layer",
                path: PathBuf::from(self.diff_id.as_str()),
                source,
            }),
        }
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
    /// Counts the times a stage started on a new base: a stage can keep
    /// its root only while the epoch it ran in lasts.
    epoch: u64,
    /// A cached layer failed after part of it may have reached the root:
    /// no stage goes on from it until it is emptied.
    broken: bool,
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
    /// The keep area: kept stage roots and unpacked `COPY --from` images,
    /// one directory each, outside the root; on `/`, the state directory,
    /// where only short-lived unpacked directories go.
    kept: RootPath,
    /// Set on `/` ([`Kaniko::keep_in_anonymous_files`]): kept stages are
    /// tars in this store's anonymous files. Weak: the build closes the
    /// store after the snapshotter, and only a closed store removes its
    /// marker of a build in progress.
    tars: OnceLock<Weak<Store>>,
    /// Directories unpacked for a `COPY --from` on `/`, each holding the
    /// store's fence open, deleted at the end of the instruction.
    views: Mutex<Vec<View>>,
    /// Bumped each time the views are deleted: a stage's cached view of an
    /// older epoch is gone.
    view_epoch: AtomicU64,
    /// On `/`: the verified spool of each streamed layer, by diff ID, kept
    /// while stages made on the layer are still to extract it (ADR-0081).
    spools: Mutex<HashMap<Digest, SpoolUse>>,
    /// This process runs in the initial user namespace
    /// ([`in_initial_user_namespace`]): a root other than `/` takes
    /// streamed base layers only then.
    initial_ns: bool,
    /// The user namespace this process runs in, read at its first use
    /// ([`Shared::user_namespace`]).
    userns: OnceLock<Option<UserNamespace>>,
}

/// The uses of one streamed layer on `/` ([`Shared::spool`]).
#[derive(Debug, Default)]
struct SpoolUse {
    /// Stage roots and `COPY --from` images made on the layer: one per
    /// [`Snapshotter::streamed_layer`] call.
    expected: usize,
    /// Extractions served so far.
    used: usize,
    /// The verified spool, kept while `used < expected`.
    spool: Option<Arc<Spooled>>,
    /// Whether the kept spool's owners were checked on the ignored paths
    /// too, as a `COPY --from` image unpacked whole needs.
    whole: bool,
}

/// A directory unpacked in the state directory for a `COPY --from` on `/`.
#[derive(Debug)]
struct View {
    name: String,
    /// Device, inode and mount of the directory made: only that directory
    /// is ever emptied under the name.
    id: (u32, u32, u64, u64),
    /// No `RUN` starts while it is held: dropped only once the directory
    /// is gone.
    _window: NamedGuard,
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
    /// build's ignore list (`kaniko::ignore_list`), walk options, and the
    /// keep area `kept`: an empty private directory of the build outside
    /// the root, or on the ignore list when the root is `/`.
    #[must_use]
    pub fn new(
        root: RootPath,
        path: PathBuf,
        ignore: IgnoreList,
        walk: WalkOptions,
        kept: RootPath,
    ) -> Self {
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
                    epoch: 0,
                    broken: false,
                }),
                next_id: AtomicU64::new(0),
                kept,
                tars: OnceLock::new(),
                views: Mutex::new(Vec::new()),
                view_epoch: AtomicU64::new(0),
                spools: Mutex::new(HashMap::new()),
                initial_ns: in_initial_user_namespace(),
                userns: OnceLock::new(),
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
        clear_views(&self.shared)?;
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

/// The directory of the marker of a build on `/`, on the ignore list
/// (`kaniko::ignore`'s fixed entries): no deletion or extraction reaches it.
const MARKER_DIR: &str = "ranikust";

/// The marker's name in `/ranikust`.
const MARKER_NAME: &str = ".built";

/// The marker a build on `/` leaves in `/ranikust` (ADR-0052, D1), relative
/// to the root.
pub const BUILT_MARKER: &str = "ranikust/.built";

/// On `/`, at the first base, under the hold and `/`'s lock: marks `/` as
/// built on and returns whether an earlier build had marked it already
/// (ADR-0052, D1); then the first base empties `/` (the ignore list kept),
/// as every later base does, so the image never holds what an earlier build
/// in the same container left. Made only once a base is about to change
/// `/`, so a build that stops before (a typo, a failed pull) leaves no
/// marker, and before the first extraction, so one killed half-way does.
///
/// `/ranikust` is made (`0700`) when missing and set to `0700` otherwise.
/// It must be a directory on `/`'s own mount: a symlink or another mount (a
/// volume that could outlive the container and empty a later one's `/`) is
/// refused. The directory is opened once and checked against the name; the
/// marker is looked up and made relative to that descriptor, without
/// following a symlink and without leaving its mount. Anything under the
/// marker's name counts as a marker, and so does one that appears between
/// the look-up and the creation.
fn mark_slash(root: &RootPath) -> Result<bool> {
    let missing = |e: &rnk_rootfs::Error| e.raw_os_error() == Some(Errno::NOENT.raw_os_error());
    let made = match root.statx(MARKER_DIR) {
        Err(e) if missing(&e) => {
            root.create_dir(MARKER_DIR, 0o700)?;
            root.statx(MARKER_DIR)?
        }
        r => r?,
    };
    check_marker_dir_name(&made, root.mnt_id())?;
    let dir = root.sub_root(MARKER_DIR)?.stay_on_mount();
    check_marker_dir_opened(&made, &dir.statx("/")?, root.mnt_id(), dir.mnt_id())?;
    dir.set_meta(
        "",
        &rnk_rootfs::Meta {
            mode: Some(0o700),
            ..rnk_rootfs::Meta::default()
        },
    )?;
    match dir.statx(MARKER_NAME) {
        Ok(_) => Ok(true),
        Err(e) if missing(&e) => {
            let how = rnk_rootfs::OpenHow::write().create_new(0o600).no_follow();
            match dir.open_at(MARKER_NAME, &how) {
                Ok(fd) => {
                    drop(fd);
                    Ok(false)
                }
                Err(e) if e.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => Ok(true),
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// `/ranikust` as its name shows it (`statx`, not following it): a
/// directory on `/`'s own mount, not a mount point.
fn check_marker_dir_name(made: &rnk_rootfs::Stat, root_mnt: u64) -> Result<()> {
    if made.file_type() != FileType::Directory {
        return Err(Error::State(
            "/ranikust is not a directory (a symlink included): the marker of a build on / \
             cannot go there",
        ));
    }
    if made.mount_root || made.mnt_id != root_mnt {
        return Err(Error::State(
            "/ranikust is another mount (a volume): the marker of a build on / must stay with \
             the container's own /; mount volumes below /ranikust, not on it",
        ));
    }
    Ok(())
}

/// The directory opened is the one the name showed, on `/`'s own mount.
fn check_marker_dir_opened(
    made: &rnk_rootfs::Stat,
    opened: &rnk_rootfs::Stat,
    root_mnt: u64,
    opened_mnt: u64,
) -> Result<()> {
    if opened_mnt != root_mnt || !same_inode(made, opened) {
        return Err(Error::State(
            "/ranikust changed between its look-up and its opening",
        ));
    }
    Ok(())
}

impl Kaniko {
    /// Keeps stages as tars in `store`'s anonymous files instead of
    /// directories (kaniko mode on `/`, ADR-0056); the keep area given to
    /// [`Kaniko::new`] then holds only short-lived unpacked directories and
    /// must be the state directory, on the ignore list.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when called twice.
    pub fn keep_in_anonymous_files(&self, store: Arc<Store>) -> Result<()> {
        self.shared
            .tars
            .set(Arc::downgrade(&store))
            .map_err(|_| Error::State("the keep store is set already"))
    }
}

impl Shared {
    /// The store kept stages go to as tars, when set; an error once the
    /// store is gone.
    fn tar_store(&self) -> Result<Option<Arc<Store>>> {
        self.tars
            .get()
            .map(|w| {
                w.upgrade()
                    .ok_or(Error::State("the store of kept tars is closed"))
            })
            .transpose()
    }

    fn is_slash(&self) -> bool {
        self.path == Path::new("/")
    }

    /// Whether the owner check of pulled blobs skips the ignored paths: on
    /// `/` with the stages kept as tars only, where a base layer is
    /// extracted with the ignore list and a `COPY --from` image is checked
    /// again whole before it is unpacked. Without them
    /// [`Kaniko::prepare_source`] unpacks the blobs whole into a directory
    /// with no check of its own; off `/` every path is checked anyway
    /// (ADR-0144).
    fn pulled_owners_skip_ignored(&self) -> bool {
        self.is_slash() && self.tars.get().is_some()
    }

    fn views(&self) -> MutexGuard<'_, Vec<View>> {
        self.views.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn spools(&self) -> MutexGuard<'_, HashMap<Digest, SpoolUse>> {
        self.spools.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The user namespace this process runs in, if not the initial one,
    /// read once: whose ids the owner checks hold the layers to (ADR-0098).
    fn user_namespace(&self) -> Result<Option<&UserNamespace>> {
        UserNamespace::once(&self.userns)
    }

    /// `layer` verified in `store` ([`Spooled::of`]). A streamed layer that
    /// a later stage or image of the build was also made on keeps its
    /// spool for it: that stage extracts it again without downloading and
    /// decompressing it a second time to verify it (ADR-0081). The spool
    /// goes with the last use the build announced; one used more often
    /// than announced is spooled anew. While the store's files are in
    /// memory (`memfd`), charged to the container's memory, no new spool is
    /// kept: each use spools the layer anew, one layer at a time. A blob of
    /// the store is read as it is. The owners are checked as the extraction
    /// that follows writes them: a base layer's skip the ignore list, a
    /// `COPY --from` image's (`whole`) do not, so a spool a base's check
    /// kept, or a blob [`Snapshotter::check_owners`] checked, is checked
    /// again before an image uses it.
    fn spool(&self, store: &Store, layer: &KanikoLayer, whole: bool) -> Result<Arc<Spooled>> {
        let none = IgnoreList::new();
        let keep = if whole { &none } else { &self.ignore };
        let ids = self.user_namespace()?.map(UserNamespace::ids);
        if !matches!(layer.source, LayerSource::Stream(_)) {
            let blob = Spooled::of(store, layer, None, keep)?;
            if let Some(ids) = ids.filter(|_| whole) {
                blob.check_owners(ids, keep)?;
            }
            return Ok(Arc::new(blob));
        }
        let cached = {
            let mut spools = self.spools();
            let entry = spools.entry(layer.diff_id.clone()).or_default();
            entry.used += 1;
            entry.spool.clone().map(|spool| (spool, entry.whole))
        };
        let (spooled, checked_whole) = match cached {
            Some((spooled, checked_whole)) => {
                if let Some(ids) = ids.filter(|_| whole && !checked_whole) {
                    spooled.check_owners(ids, keep)?;
                }
                (spooled, checked_whole || whole)
            }
            None => (Arc::new(Spooled::of(store, layer, ids, keep)?), whole),
        };
        let mut spools = self.spools();
        if let Some(entry) = spools.get_mut(&layer.diff_id) {
            if entry.used >= entry.expected {
                spools.remove(&layer.diff_id);
            } else if !store.files_in_memory() {
                entry.spool = Some(Arc::clone(&spooled));
                entry.whole = checked_whole;
            }
        }
        Ok(spooled)
    }
}

/// Deletes every directory unpacked for a `COPY --from`. One that cannot be
/// deleted keeps its fence window open, so no `RUN` can start while it is
/// on disk, and fails the build.
fn clear_views(shared: &Shared) -> Result<()> {
    let mut views = shared.views();
    if views.is_empty() {
        return Ok(());
    }
    shared.view_epoch.fetch_add(1, Ordering::Relaxed);
    while let Some(view) = views.pop() {
        if let Err(e) = remove_view(shared, &view) {
            views.push(view);
            return Err(e);
        }
    }
    Ok(())
}

/// Device, inode and mount of `st`.
fn identity(st: &rnk_rootfs::Stat) -> (u32, u32, u64, u64) {
    (st.dev_major, st.dev_minor, st.ino, st.mnt_id)
}

/// Whether `a` and `b` are the same inode (device and inode number): what a
/// name showed is what was opened. The mount is checked apart.
fn same_inode(a: &rnk_rootfs::Stat, b: &rnk_rootfs::Stat) -> bool {
    (a.dev_major, a.dev_minor, a.ino) == (b.dev_major, b.dev_minor, b.ino)
}

/// Deletes `view`'s directory, only if the name still leads, without
/// following a link, to the directory made, and that directory is the one
/// opened.
fn remove_view(shared: &Shared, view: &View) -> Result<()> {
    let changed = Error::State("an unpacked view changed before its deletion");
    let st = shared.kept.statx(&view.name)?;
    if st.file_type() != FileType::Directory || st.mount_root || identity(&st) != view.id {
        return Err(changed);
    }
    let dir = shared.kept.sub_root(&view.name)?;
    if identity(&dir.statx("/")?) != view.id {
        return Err(changed);
    }
    delete_contents(&dir, &IgnoreList::new())?;
    drop(dir);
    shared.kept.remove_dir(&view.name)?;
    Ok(())
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
    hold.broken = false;
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
            source: LayerSource::Blob(blob.to_owned()),
        })
    }

    fn check_owners(&self, layers: &[(Digest, PathBuf)]) -> Result<()> {
        let none = IgnoreList::new();
        let keep = if self.shared.pulled_owners_skip_ignored() {
            &self.shared.ignore
        } else {
            &none
        };
        check_live_owners(layers, self.shared.user_namespace()?, keep)
    }

    fn streamed_layer(&self, diff_id: &Digest, open: Arc<dyn LayerOpen>) -> Option<KanikoLayer> {
        // Off `/` a layer streams into the root as it arrives, so in a user
        // namespace it comes as a blob, its owners checked first (ADR-0098);
        // on `/` the spool pass checks each one (ADR-0137).
        if !self.shared.is_slash() && !self.shared.initial_ns {
            return None;
        }
        if self.shared.is_slash() {
            // One more stage or image of the build to extract it: its spool
            // waits for that use ([`Shared::spool`]).
            self.shared
                .spools()
                .entry(diff_id.clone())
                .or_default()
                .expected += 1;
        }
        Some(KanikoLayer {
            diff_id: diff_id.clone(),
            source: LayerSource::Stream(open),
        })
    }

    fn prepare(&self, base: Base<'_, KanikoRoot, KanikoLayer>) -> Result<KanikoRoot> {
        let shared = &self.shared;
        clear_views(shared)?;
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut hold = shared.hold();
        match base {
            Base::Stage(parent) if parent.kept.is_some() => {
                // A stage built `FROM` a kept one: the kept root is copied
                // back into the emptied root.
                if !Arc::ptr_eq(&parent.shared, shared) {
                    return Err(Error::State("the parent stage is of another build"));
                }
                if hold.holder.is_some() {
                    return Err(Error::State("another stage holds the shared root"));
                }
                let span = tracing::info_span!("rnk_snapshot.prepare", layers = 0_u64);
                let _entered = span.enter();
                new_base(shared, &mut hold)?;
                match &parent.kept {
                    Some(Kept::Dir {
                        root, root_meta, ..
                    }) => {
                        copy_filtered(
                            root,
                            "",
                            &shared.root,
                            "",
                            &CopyOpts::default(),
                            &|p, _| !shared.ignore.covers_walked(p),
                        )?;
                        if !shared.is_slash()
                            && let Some(meta) = root_meta
                        {
                            shared.root.set_meta("", meta)?;
                        }
                    }
                    Some(Kept::Tar { file }) => {
                        // Our own snapshot of a tree: `.wh.` names are files.
                        extract_live_literal(
                            &shared.root,
                            ReadAt::from_start(file)?,
                            &shared.ignore,
                        )?;
                    }
                    Some(Kept::Image { .. }) | None => {
                        return Err(Error::State("a stage built on an unpacked image"));
                    }
                }
            }
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
                if hold.broken {
                    return Err(BROKEN);
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
                new_base(shared, &mut hold)?;
                for layer in layers {
                    extract_base_layer(shared, layer)?;
                }
            }
        }
        hold.holder = Some(id);
        Ok(KanikoRoot {
            shared: Arc::clone(shared),
            id,
            needed: false,
            open: false,
            epoch: hold.epoch,
            kept: None,
            view: None,
        })
    }

    fn prepare_source(&self, layers: &[KanikoLayer]) -> Result<KanikoRoot> {
        let shared = &self.shared;
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let span = tracing::info_span!("rnk_snapshot.prepare", layers = layers.len());
        let _entered = span.enter();
        let kept = if shared.tars.get().is_some() {
            // On `/`: the layers as they are (a streamed one is spooled and
            // verified on first read, before any directory is made),
            // unpacked only when a `COPY --from` reads them.
            Kept::Image {
                layers: layers.to_vec(),
                tars: None,
            }
        } else {
            let (name, root, _) = make_dir(shared, &format!("k{id}"))?;
            // A fresh directory: nothing in it is ignored or a mount;
            // device nodes and FIFOs are left out as on the root
            // (ADR-0041).
            let none = IgnoreList::new();
            for layer in layers {
                if let Err(e) = extract_blob(&root, &none, layer) {
                    Kept::Dir {
                        name,
                        root,
                        root_meta: None,
                    }
                    .remove(shared);
                    return Err(e);
                }
            }
            Kept::Dir {
                name,
                root,
                root_meta: None,
            }
        };
        Ok(KanikoRoot {
            shared: Arc::clone(shared),
            id,
            needed: false,
            open: false,
            epoch: u64::MAX,
            kept: Some(kept),
            view: None,
        })
    }
}

/// Starts a new base on the root. On `/` the first base goes over what is
/// there, as kaniko (see the module doc); later ones, and every base on a
/// root other than `/`, start from an empty root. Another root starts as
/// overlay mode's stage roots do: `0755` and the epoch, unless a layer sets
/// `./`; on `/` the container's root stays as it is, as under kaniko.
fn new_base(shared: &Shared, hold: &mut Hold) -> Result<()> {
    // The first base on `/`: an earlier build's marker makes it empty `/`
    // first (ADR-0052, D1).
    if shared.is_slash() && hold.epoch == 0 && !hold.touched && mark_slash(&shared.root)? {
        tracing::info!(
            "an earlier build ran on / in this container: / is emptied (the ignore list \
             kept) before the first base, so nothing of it reaches the image"
        );
        hold.touched = true;
    }
    if hold.touched || !shared.is_slash() {
        empty_root(shared, hold)?;
    } else {
        hold.walked = false;
    }
    hold.touched = true;
    hold.epoch += 1;
    if !shared.is_slash() {
        shared.root.set_meta("", &crate::layer::default_root())?;
    }
    Ok(())
}

/// What a stage keeps for later stages.
#[derive(Debug)]
enum Kept {
    /// A directory of the keep area (a root other than `/`).
    Dir {
        name: String,
        root: RootPath,
        /// The owner and mode of the stage's root, given back when a stage
        /// is built on the kept copy on a root other than `/`.
        root_meta: Option<rnk_rootfs::Meta>,
    },
    /// A plain tar of the stage in an anonymous file (`/`, ADR-0056).
    Tar { file: File },
    /// The layers of a `COPY --from` image, unpacked when read (`/`). A
    /// streamed layer is spooled and verified on first use, before any
    /// directory is made, and its plain tar kept in `tars` for later reads.
    Image {
        layers: Vec<KanikoLayer>,
        tars: Option<Vec<Arc<Spooled>>>,
    },
}

impl Kept {
    /// Deletes a kept directory; a tar goes with its file. A failure is
    /// logged: the build directory's removal at the end of the build
    /// deletes what is left.
    fn remove(self, shared: &Shared) {
        if let Self::Dir { name, root, .. } = self {
            let r = delete_contents(&root, &IgnoreList::new())
                .and_then(|()| shared.kept.remove_dir(&name));
            if let Err(e) = r {
                tracing::warn!("removing the kept directory {name}: {e}");
            }
        }
    }
}

/// The directory's name, the directory opened, and its identity
/// ([`identity`]).
type Made = (String, RootPath, (u32, u32, u64, u64));

/// A new, empty directory `name` of the keep area. It is opened by name,
/// which follows a symlink, so the directory opened must be the one just
/// made (type, device, inode and mount, not following a link): a name
/// swapped for a link to `.` would otherwise make its removal empty the
/// whole keep area.
fn make_dir(shared: &Shared, name: &str) -> Result<Made> {
    shared.kept.create_dir(name, 0o700)?;
    let opened = shared.kept.statx(name).and_then(|made| {
        let root = shared.kept.sub_root(name)?;
        let st = root.statx("/")?;
        Ok((made, st, root))
    });
    match opened {
        Ok((made, st, root))
            if made.file_type() == FileType::Directory
                && same_inode(&made, &st)
                && root.mnt_id() == shared.kept.mnt_id() =>
        {
            Ok((name.to_owned(), root, identity(&st)))
        }
        Ok(_) => Err(Error::State(
            "the kept directory changed between its creation and its opening",
        )),
        Err(e) => {
            if let Err(r) = shared.kept.remove_dir(name) {
                tracing::warn!("removing the kept directory {name}: {r}");
            }
            Err(e.into())
        }
    }
}

/// An anonymous file of the store read from its start through the
/// descriptor held, at offsets of its own (`pread`): no path is looked up,
/// after `RUN`s either, and readers of one file never move each other's
/// offset, nor the writer's.
struct ReadAt {
    /// A duplicate of the held descriptor (`F_DUPFD_CLOEXEC`).
    file: File,
    at: u64,
}

impl ReadAt {
    fn from_start(file: &File) -> Result<Self> {
        let file = file.try_clone().map_err(|source| Error::Io {
            op: "duplicate the descriptor of",
            path: PathBuf::from("an anonymous file of the store"),
            source,
        })?;
        Ok(Self { file, at: 0 })
    }
}

impl std::io::Read for ReadAt {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt as _;
        let n = self.file.read_at(buf, self.at)?;
        self.at += u64::try_from(n).map_err(std::io::Error::other)?;
        Ok(n)
    }
}

/// Writes `what` of the root as a plain tar into a new anonymous file of
/// `store`: a walk of the root (metadata only; the export reads the content
/// through descriptors it checks against the walk), the ignore list leaving
/// out what it holds, then the export of every entry, or of those at or
/// below the [`Keep::Paths`] (physical paths, as for [`copy_out`]).
fn tar_out(shared: &Shared, what: &Keep, store: &Store) -> Result<File> {
    let span = tracing::info_span!("rnk_snapshot.keep", paths = tracing::field::Empty);
    let _entered = span.enter();
    let paths = match what {
        Keep::All => None,
        Keep::Paths(paths) => {
            let clean = paths
                .iter()
                .map(|p| physical(&shared.root, p))
                .collect::<Result<Vec<_>>>()?;
            (!clean.iter().any(|p| p.as_os_str().is_empty())).then_some(clean)
        }
    };
    let mut walker = Walker::new(WalkOptions {
        workers: shared.walk.workers,
        mode: SnapshotMode::Time,
    })?;
    // Only below the kept paths: a tree a `RUN` left elsewhere, past
    // `PATH_MAX`, which the walk refuses, is never entered for them.
    let mut diff = match &paths {
        None => walker.diff(&shared.root, &shared.ignore)?,
        Some(paths) => walker.diff_below(&shared.root, &shared.ignore, paths)?,
    };
    match &paths {
        None => {
            span.record("paths", 0_u64);
        }
        Some(paths) => {
            span.record("paths", paths.len() as u64);
            diff.changed
                .retain(|c| paths.iter().any(|p| c.path.starts_with(p)));
        }
    }
    let mut file = store.scratch_file()?;
    let options = EncodeOptions {
        compression: Compression::None,
        level: 0,
        threads: std::num::NonZeroUsize::MIN,
        mtime: None,
    };
    diff.export(&shared.root, options, &mut file)?;
    Ok(file)
}

/// Copies `what` of the root into `to`, leaving out what the ignore list
/// holds (mounts, `/proc`, the state directory and the rest).
///
/// The paths of [`Keep::Paths`] must be physical: no symlink before their
/// last name, which the ignore list then judges as it judges the walk's
/// paths; a path through a link is refused. The root itself (an empty
/// path) is the whole root.
fn copy_out(shared: &Shared, what: &Keep, to: &RootPath) -> Result<()> {
    let span = tracing::info_span!("rnk_snapshot.keep", paths = tracing::field::Empty);
    let _entered = span.enter();
    let opts = CopyOpts::default();
    // The copies start at the root or at a path `physical` gave: no `..`.
    let filter = |p: &Path, _: bool| !shared.ignore.covers_walked(p);
    let paths = match what {
        Keep::All => None,
        Keep::Paths(paths) => {
            let clean = paths
                .iter()
                .map(|p| physical(&shared.root, p))
                .collect::<Result<Vec<_>>>()?;
            (!clean.iter().any(|p| p.as_os_str().is_empty())).then_some(clean)
        }
    };
    match paths {
        None => {
            span.record("paths", 0_u64);
            copy_filtered(&shared.root, "", to, "", &opts, &filter)?;
        }
        Some(paths) => {
            span.record("paths", paths.len() as u64);
            for p in &paths {
                if shared.ignore.covers(p) {
                    continue;
                }
                if let Some(parent) = p.parent()
                    && !parent.as_os_str().is_empty()
                {
                    to.create_dir_all(parent, 0o755)?;
                }
                let st = shared.root.statx(p)?;
                // A mount the ignore list misses is never kept: the copy
                // stays on the root's own mount.
                if st.mnt_id != shared.root.mnt_id() || st.mount_root {
                    return Err(Error::State("a path to keep lies on another mount"));
                }
                if st.file_type() == FileType::Symlink {
                    // Kept as the link; its target is one of `paths` too.
                    if to.statx(p).is_err() {
                        to.symlink(shared.root.read_link(p)?, p)?;
                    }
                } else {
                    copy_filtered(&shared.root, p, to, p, &opts, &filter)?;
                }
            }
        }
    }
    Ok(())
}

/// `p` relative to the root, with no symlink before its last name and no
/// `..`; refused otherwise.
fn physical(root: &RootPath, p: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::Normal(name) => {
                if !out.as_os_str().is_empty() && root.statx(&out)?.file_type() == FileType::Symlink
                {
                    return Err(Error::State("a path to keep goes through a symlink"));
                }
                out.push(name);
            }
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(Error::State("a path to keep climbs with .."));
            }
        }
    }
    Ok(out)
}

/// Whether this process runs in the initial user namespace, where every
/// owner maps. Only there may a base layer be streamed into a root other
/// than `/`, where it is extracted as it arrives (ADR-0055): in another,
/// the builder pulls the blobs so that [`check_live_owners`] reads every
/// owner of every layer before any is extracted (ADR-0098). Where the
/// namespace cannot be told, the blobs are pulled too, and the check says
/// why.
pub(crate) fn in_initial_user_namespace() -> bool {
    matches!(rnk_store::user_namespace_proc(), Ok(None))
}

/// [`Snapshotter::check_owners`] of the kaniko-mode snapshotters, in the
/// user namespace `ns` this process was started in, if any (a rootless
/// container's), for an extraction with the ignore list `keep`: every blob
/// is read, as nothing of a live root is kept.
pub(crate) fn check_live_owners(
    layers: &[(Digest, PathBuf)],
    ns: Option<&UserNamespace>,
    keep: &IgnoreList,
) -> Result<()> {
    match ns {
        None => Ok(()),
        Some(ns) => owners::check(layers, ns.ids(), owners::Form::Live(keep), |_| Ok(false)),
    }
}

/// Refuses the cached `layer` an `apply_layer` is about to extract when its
/// owners are not all mapped in `ns`, the paths `keep` covers skipped: a
/// base layer's blobs went through [`Snapshotter::check_owners`] when they
/// were pulled, a cached layer's did not (ADR-0098, ADR-0144). A streamed
/// layer is not read here: on `/` its spool pass checks it, and elsewhere a
/// user namespace takes no streamed layer ([`Snapshotter::streamed_layer`]).
pub(crate) fn check_cached_owners(
    layer: &KanikoLayer,
    ns: Option<&UserNamespace>,
    keep: &IgnoreList,
) -> Result<()> {
    match &layer.source {
        LayerSource::Blob(blob) => {
            check_live_owners(&[(layer.diff_id.clone(), blob.clone())], ns, keep)
        }
        LayerSource::Stream(_) => Ok(()),
    }
}

/// Extracts one base layer into the shared root ([`open_for_root`]).
fn extract_base_layer(shared: &Shared, layer: &KanikoLayer) -> Result<()> {
    let tar = open_for_root(shared, layer)?;
    extract_checked(&shared.root, &shared.ignore, tar, &layer.diff_id)
}

/// `layer`'s compressed bytes, opened for extraction into the shared root,
/// with everything that can fail before an entry reaches the root done. On
/// `/`, which nothing empties at the end of the build (only a new base
/// does, between stages), a streamed layer is first spooled and verified
/// ([`Spooled::of`]): nothing unverified reaches the root.
/// Otherwise (a blob, or any layer on a root other than `/`) it is
/// extracted as it arrives: a base layer that fails part-way leaves its
/// entries until the next base empties the root, or [`Kaniko::finish`]
/// one other than `/`; a cached layer breaks the root
/// ([`KanikoRoot::apply_layer`]).
fn open_for_root(shared: &Shared, layer: &KanikoLayer) -> Result<Box<dyn std::io::Read + Send>> {
    if shared.is_slash() && matches!(layer.source, LayerSource::Stream(_)) {
        let store = shared.tar_store()?.ok_or(Error::State(
            "no store to verify a streamed layer in before it reaches /",
        ))?;
        return shared.spool(&store, layer, false)?.reader();
    }
    layer.open()
}

/// A base layer in an anonymous file of the store, as its compressed blob,
/// its compressed digest and diff ID verified (ADR-0055 amendment) and its
/// tar headers checked (ADR-0123) before anything of it is extracted. The
/// spool is the compressed blob, capped by the descriptor's size: disk, or
/// memory under the `memfd` fallback, holds no more than the download. The
/// file's space goes with it.
#[derive(Debug)]
struct Spooled(SpooledBlob);

#[derive(Debug)]
enum SpooledBlob {
    /// A layer already verified on disk (a blob of the store): read as is.
    Blob(KanikoLayer),
    /// A streamed layer's compressed bytes, with its diff ID.
    File { file: File, diff_id: Digest },
}

impl Spooled {
    /// Spools `layer` when it is streamed. One read of the body writes the
    /// compressed bytes into an anonymous file of `store` and decompresses
    /// them once, which nothing else sees: to check the tar headers as the
    /// extraction frames them and refuses them from a header alone, the
    /// `tar-header` limit and the kernel's limits included
    /// ([`rnk_rootfs::check_headers`]), and to compute the diff ID; with
    /// `ids`, the ids of the user namespace the build runs in, also to find
    /// the owners the extraction with the ignore list `keep` would give that
    /// it does not map, refused once the layer is verified (ADR-0098). The
    /// body is read to its end, even after a bad header, where its source
    /// checks the size and the compressed digest. The first failure of the
    /// body or of its decompression is reported as such, before a bad
    /// header or a wrong diff ID. A blob already on disk is kept as it is.
    fn of(
        store: &Store,
        layer: &KanikoLayer,
        ids: Option<&MappedIds>,
        keep: &IgnoreList,
    ) -> Result<Self> {
        let LayerSource::Stream(_) = &layer.source else {
            return Ok(Self(SpooledBlob::Blob(layer.clone())));
        };
        let mut owners = ids.map(|ids| Unmapped::owners(ids.clone()));
        let what = || PathBuf::from(layer.diff_id.as_str());
        let file = store.scratch_file()?;
        let copy = file.try_clone().map_err(|source| Error::Io {
            op: "spool the stream of layer",
            path: what(),
            source,
        })?;
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let body = Spool {
            inner: layer.open()?,
            copy,
            ended: Arc::clone(&ended),
        };
        let mut decoder = rnk_layer::Decoder::new(body)?;
        // The same decompression reads the headers as the extraction will
        // (ADR-0123), then the rest for the diff ID.
        let mut read = FirstError {
            inner: &mut decoder,
            first: None,
        };
        let headers = match owners.as_mut() {
            None => rnk_rootfs::check_headers(&mut read),
            Some(found) => rnk_rootfs::check_headers_and_owners(&mut read, keep, found),
        };
        // The kept error, not the tar's: a decoder may fail once, then fail
        // otherwise or read as an end.
        if let Some(e) = read.first {
            return Err(rnk_layer::Error::from(e).into());
        }
        let actual = Digest::from_sha256_bytes(decoder.finish()?.as_bytes());
        headers?;
        // The source verifies the digest only at the end of its body.
        if !ended.load(Ordering::Relaxed) {
            return Err(Error::State(
                "a streamed layer was not read to its end, so its digest is unchecked",
            ));
        }
        if actual != layer.diff_id {
            return Err(Error::DiffIdMismatch {
                expected: layer.diff_id.to_string(),
                actual: actual.to_string(),
            });
        }
        if let Some(found) = owners.filter(|f| !f.is_empty()) {
            return Err(rnk_rootfs::Error::Unmapped(found).into());
        }
        Ok(Self(SpooledBlob::File {
            file,
            diff_id: layer.diff_id.clone(),
        }))
    }

    /// Extracts the verified layer into `root`, decompressing the spool a
    /// second time; its diff ID is checked again on the way.
    fn extract(&self, root: &RootPath, ignore: &IgnoreList) -> Result<()> {
        extract_checked(root, ignore, self.reader()?, self.diff_id())
    }

    /// Refuses the owners of the verified layer that `ids` lack, the paths
    /// `keep` covers skipped: for a use that extracts paths the check of
    /// [`Spooled::of`] skipped (ADR-0098).
    fn check_owners(&self, ids: &MappedIds, keep: &IgnoreList) -> Result<()> {
        let span = tracing::debug_span!(
            "rnk_snapshot.check_owners",
            layers = 1_u64,
            layers_scanned = tracing::field::Empty,
        );
        let _entered = span.enter();
        let mut found = Unmapped::owners(ids.clone());
        let mut tar = rnk_layer::Decoder::new(self.reader()?)?;
        rnk_rootfs::check_headers_and_owners(&mut tar, keep, &mut found)?;
        span.record("layers_scanned", 1_u64);
        if found.is_empty() {
            return Ok(());
        }
        Err(rnk_rootfs::Error::Unmapped(found).into())
    }

    /// The verified layer's compressed bytes from the first one, read
    /// through the descriptor held.
    fn reader(&self) -> Result<Box<dyn std::io::Read + Send>> {
        match &self.0 {
            SpooledBlob::Blob(layer) => layer.open(),
            SpooledBlob::File { file, .. } => Ok(Box::new(ReadAt::from_start(file)?)),
        }
    }

    fn diff_id(&self) -> &Digest {
        match &self.0 {
            SpooledBlob::Blob(layer) => &layer.diff_id,
            SpooledBlob::File { diff_id, .. } => diff_id,
        }
    }
}

/// A reader that keeps the first error it returned, which the caller
/// reports as is; the error passed on is a copy of it. An interrupted read
/// is made again.
struct FirstError<R> {
    inner: R,
    first: Option<std::io::Error>,
}

impl<R: std::io::Read> std::io::Read for FirstError<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                // The tar reader does not retry it, and it is no failure.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if self.first.is_none() => {
                    let copy = std::io::Error::new(e.kind(), e.to_string());
                    self.first = Some(e);
                    return Err(copy);
                }
                r => return r,
            }
        }
    }
}

/// The body of a streamed layer, each byte read also written to the spool;
/// records whether it reached its end (a read of 0 bytes into a non-empty
/// buffer).
struct Spool {
    inner: Box<dyn std::io::Read + Send>,
    copy: File,
    ended: Arc<std::sync::atomic::AtomicBool>,
}

impl std::io::Read for Spool {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            if !buf.is_empty() {
                self.ended.store(true, Ordering::Relaxed);
            }
            return Ok(0);
        }
        self.copy.write_all(buf.get(..n).unwrap_or_default())?;
        Ok(n)
    }
}

/// Extracts one base layer into `root`, checking its diff ID. A streamed
/// layer is decompressed and extracted as it arrives; its compressed digest
/// is checked by its source at the end of the body, which then fails the
/// read, and its diff ID here at the end of the tar. Either mismatch fails
/// the stage after part of the layer is in `root`: used only where the
/// root is emptied after a failure (a host root, a fresh keep directory);
/// on `/` see [`extract_base_layer`].
pub(super) fn extract_blob(
    root: &RootPath,
    ignore: &IgnoreList,
    layer: &KanikoLayer,
) -> Result<()> {
    extract_checked(root, ignore, layer.open()?, &layer.diff_id)
}

/// Decompresses `blob` into `root` and checks the tar's digest against
/// `diff_id` at its end. The decompression and the digest run on a thread
/// of their own ([`Decoding`]), overlapping the extraction's writes; the
/// bytes extracted, in their order, are the same.
pub(super) fn extract_checked(
    root: &RootPath,
    ignore: &IgnoreList,
    blob: Box<dyn std::io::Read + Send>,
    diff_id: &Digest,
) -> Result<()> {
    let mut decoder = Decoding::new(blob)?;
    extract_live(root, &mut decoder, ignore)?;
    let actual = Digest::from_sha256_bytes(decoder.finish()?.as_bytes());
    if actual != *diff_id {
        return Err(Error::DiffIdMismatch {
            expected: diff_id.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// How many chunks of plain tar a layer's decompression queues ahead of its
/// extraction ([`Decoding`]). With the chunk the thread fills and the one
/// the extraction reads, memory is bounded at `DECODE_AHEAD + 2` times
/// [`DECODE_CHUNK`] per layer being extracted.
const DECODE_AHEAD: usize = 4;

/// The size of one chunk of plain tar handed from the decompression to the
/// extraction.
const DECODE_CHUNK: usize = 1 << 20;

/// A layer's blob decompressed, and its diff ID computed, on a thread of
/// its own, at most [`DECODE_AHEAD`] chunks queued ahead of the reader: the
/// extraction's writes overlap the decompression (ADR-0081). The thread
/// only reads `blob` and decompresses it; it writes no file. A read error
/// of the blob or of the decompression reaches the reader as an
/// [`std::io::Error`] of the same kind. Dropped before its end, the reader
/// sets `stop`: the blob gives no more bytes, the thread ends at its next
/// read, and is joined. Where no thread can be started (a tight `pids`
/// limit), the reader decompresses itself, as before.
///
/// The decoder, and its `rnk_layer.decode` span, are made and dropped on
/// the caller's thread: a span closed on a thread without the caller's
/// subscriber would never release its parent span.
struct Decoding {
    /// The decoder itself when no thread could be started.
    inline: Option<LayerDecoder>,
    chunks: Option<std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>>,
    chunk: Vec<u8>,
    at: usize,
    /// Set when the reader goes: the blob's next read fails
    /// ([`Stoppable`]), and so does the thread's next read of the decoder.
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Option<(DecodeEnd, LayerDecoder)>>>,
}

/// What the decompression thread ends with: the tar stream's digest.
type DecodeEnd = std::result::Result<rnk_layer::Sha256Digest, rnk_layer::Error>;

/// The decoder the thread reads with, handed back when it ends.
type LayerDecoder = rnk_layer::Decoder<Box<dyn std::io::Read + Send>>;

impl Drop for Decoding {
    fn drop(&mut self) {
        // The blob gives no more bytes, so the thread stops inside a read
        // of the decoder too (one that skips empty gzip members or zstd
        // skippable frames reads on without giving any); its next send
        // fails. Its decoder is dropped here, on the caller's thread.
        self.stop.store(true, Ordering::Relaxed);
        self.chunks = None;
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

/// The blob of a [`Decoding`]: fails every read once its reader is gone.
struct Stoppable {
    inner: Box<dyn std::io::Read + Send>,
    stop: Arc<AtomicBool>,
}

impl std::io::Read for Stoppable {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(stopped());
        }
        self.inner.read(buf)
    }
}

/// The error the decompression ends with when its reader is gone.
fn stopped() -> std::io::Error {
    std::io::Error::other("the extraction stopped reading the layer")
}

#[cfg(test)]
thread_local! {
    /// Tests only: the decompression thread cannot be started, as under a
    /// tight `pids` limit.
    static NO_DECODE_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Starts the decompression thread running `f`.
fn spawn_decode<F, T>(f: F) -> std::io::Result<std::thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    #[cfg(test)]
    if NO_DECODE_THREAD.with(std::cell::Cell::get) {
        return Err(Errno::AGAIN.into());
    }
    std::thread::Builder::new()
        .name("rnk-decode".to_owned())
        .spawn(f)
}

impl Decoding {
    /// Sniffs the compression here, then decompresses on the new thread,
    /// or here when no thread can be started.
    fn new(blob: Box<dyn std::io::Read + Send>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let blob: Box<dyn std::io::Read + Send> = Box::new(Stoppable {
            inner: blob,
            stop: Arc::clone(&stop),
        });
        let decoder = rnk_layer::Decoder::new(blob)?;
        let (tx, chunks) = std::sync::mpsc::sync_channel(DECODE_AHEAD);
        // The decoder goes to the thread once it runs, so a thread that
        // cannot start leaves it here.
        let (give, take) = std::sync::mpsc::sync_channel::<LayerDecoder>(1);
        let thread_stop = Arc::clone(&stop);
        let spawned = spawn_decode(move || {
            take.recv()
                .ok()
                .map(|decoder| decode_into(decoder, &tx, &thread_stop))
        });
        let decoder = match spawned {
            Ok(thread) => match give.send(decoder) {
                Ok(()) => {
                    return Ok(Self {
                        inline: None,
                        chunks: Some(chunks),
                        chunk: Vec::new(),
                        at: 0,
                        stop,
                        thread: Some(thread),
                    });
                }
                // The thread is gone already: it panicked before it could
                // take the decoder.
                Err(std::sync::mpsc::SendError(decoder)) => {
                    drop(thread.join());
                    decoder
                }
            },
            Err(e) => {
                tracing::debug!("no decompression thread ({e}): the extraction decompresses");
                decoder
            }
        };
        Ok(Self {
            inline: Some(decoder),
            chunks: None,
            chunk: Vec::new(),
            at: 0,
            stop,
            thread: None,
        })
    }

    /// Reads to the end of the tar stream and returns its digest, as
    /// [`rnk_layer::Decoder::finish`].
    fn finish(mut self) -> Result<rnk_layer::Sha256Digest> {
        if let Some(decoder) = self.inline.take() {
            return Ok(decoder.finish()?);
        }
        std::io::copy(&mut self, &mut std::io::sink()).map_err(rnk_layer::Error::from)?;
        self.chunks = None;
        let thread = self
            .thread
            .take()
            .ok_or(Error::State("the decompression thread was joined already"))?;
        let (digest, decoder) = thread
            .join()
            .map_err(|_| Error::State("the decompression thread panicked"))?
            .ok_or(Error::State("the decompression thread got no decoder"))?;
        drop(decoder);
        Ok(digest?)
    }
}

/// The decompression thread of [`Decoding`]: full chunks to `tx` until the
/// end of the tar stream, then the stream's digest, and the decoder back.
/// Stops when the reader is gone.
fn decode_into(
    mut decoder: LayerDecoder,
    tx: &std::sync::mpsc::SyncSender<std::io::Result<Vec<u8>>>,
    stop: &AtomicBool,
) -> (DecodeEnd, LayerDecoder) {
    let end = decode_chunks(&mut decoder, tx, stop);
    (end, decoder)
}

/// [`decode_into`]'s loop.
fn decode_chunks(
    decoder: &mut LayerDecoder,
    tx: &std::sync::mpsc::SyncSender<std::io::Result<Vec<u8>>>,
    stop: &AtomicBool,
) -> DecodeEnd {
    use std::io::Read as _;
    loop {
        let mut chunk = vec![0_u8; DECODE_CHUNK];
        let mut filled = 0;
        while filled < chunk.len() {
            if stop.load(Ordering::Relaxed) {
                return Err(rnk_layer::Error::Io(stopped()));
            }
            match decoder.read(chunk.get_mut(filled..).unwrap_or_default()) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let again = std::io::Error::new(e.kind(), e.to_string());
                    // The reader sees the error itself; the join gets a copy.
                    let _ = tx.send(Err(e));
                    return Err(rnk_layer::Error::Io(again));
                }
            }
        }
        let end = filled < chunk.len();
        chunk.truncate(filled);
        if !chunk.is_empty() && tx.send(Ok(chunk)).is_err() {
            return Err(rnk_layer::Error::Io(stopped()));
        }
        if end {
            // The read of 0 bytes above set the diff ID.
            return decoder.diff_id().ok_or_else(|| {
                rnk_layer::Error::Io(std::io::Error::other("the tar stream did not end"))
            });
        }
    }
}

impl std::io::Read for Decoding {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(decoder) = &mut self.inline {
            return decoder.read(buf);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        while self.at >= self.chunk.len() {
            let Some(chunks) = &self.chunks else {
                return Ok(0);
            };
            match chunks.recv() {
                Ok(Ok(chunk)) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                Ok(Err(e)) => return Err(e),
                // The thread is done: the stream's end.
                Err(_) => {
                    self.chunks = None;
                    return Ok(0);
                }
            }
        }
        let rest = self.chunk.get(self.at..).unwrap_or_default();
        let n = rest.len().min(buf.len());
        buf.get_mut(..n)
            .unwrap_or_default()
            .copy_from_slice(rest.get(..n).unwrap_or_default());
        self.at += n;
        Ok(n)
    }
}

/// A stage on the shared root.
#[derive(Debug)]
pub struct KanikoRoot {
    shared: Arc<Shared>,
    id: u64,
    needed: bool,
    open: bool,
    /// The epoch the stage ran in ([`Hold::epoch`]).
    epoch: u64,
    /// The kept copy, once [`StageRootfs::keep`] made one, or the image of
    /// [`Snapshotter::prepare_source`].
    kept: Option<Kept>,
    /// On `/`: the directory a tar or image is unpacked into for
    /// [`StageRootfs::view`], valid while the build's view epoch is the one
    /// recorded.
    view: Option<(u64, RootPath)>,
}

/// What a root a cached layer failed part-way into answers until it is
/// emptied (shared root) or released (per-stage directory).
pub(super) const BROKEN: Error =
    Error::State("a cached layer failed part-way into the root: nothing goes on from it");

impl KanikoRoot {
    fn check_held(&self, hold: &Hold) -> Result<()> {
        if hold.holder == Some(self.id) {
            Ok(())
        } else {
            Err(Error::State("the stage no longer holds the shared root"))
        }
    }

    /// [`Self::check_held`], and no cached layer failed part-way into the
    /// root since it was last emptied.
    fn check_usable(&self, hold: &Hold) -> Result<()> {
        self.check_held(hold)?;
        if hold.broken {
            return Err(BROKEN);
        }
        Ok(())
    }

    /// Applies `layer`, the cached layer of the stage's next step, instead
    /// of running the step (`design/kaniko-mode.md#a-cache-hit`): extracted
    /// into the root as a base layer is, with the build's ignore list and
    /// its diff ID checked; on `/` a streamed layer is verified, its tar
    /// headers included, before any entry reaches the root. The walker's
    /// record is then stale: the next layer of a stage whose layers are
    /// needed starts from a new baseline, so its diff holds only its own
    /// step's changes.
    ///
    /// `layer` comes from [`Snapshotter::base_layer`], or from
    /// [`Snapshotter::streamed_layer`] once per root it is applied to, as a
    /// base layer does.
    ///
    /// # Errors
    ///
    /// [`Error::State`], with nothing changed, while a layer is open, when
    /// the stage no longer holds the root, or when an earlier layer broke
    /// it. A failure before any entry can reach the root (the source
    /// refused; a user namespace or id map that cannot be read; in a user
    /// namespace, an owner it does not map, or a cached blob that does not
    /// open or decompress, or whose tar headers or diff ID are wrong; on
    /// `/`, a streamed layer's digest, diff ID or tar headers) leaves the
    /// root as it was. Any later failure breaks the root: until it is
    /// emptied, no layer, view, keep or child stage goes on from it.
    pub fn apply_layer(&mut self, layer: &KanikoLayer) -> Result<()> {
        if self.open {
            return Err(Error::State("a layer is open"));
        }
        let shared = Arc::clone(&self.shared);
        let mut hold = shared.hold();
        self.check_usable(&hold)?;
        // The step it stands for ends as an instruction does: no view of
        // another stage outlives it.
        clear_views(&shared)?;
        let span = tracing::debug_span!("rnk_snapshot.apply");
        let _entered = span.enter();
        let none = IgnoreList::new();
        // The owner check skips the ignored paths on `/` only (ADR-0144).
        let keep = if shared.is_slash() {
            &shared.ignore
        } else {
            &none
        };
        check_cached_owners(layer, shared.user_namespace()?, keep)?;
        let tar = open_for_root(&shared, layer)?;
        hold.walked = false;
        hold.broken = true;
        extract_checked(&shared.root, &shared.ignore, tar, &layer.diff_id)?;
        hold.broken = false;
        Ok(())
    }

    /// The kept tar or image unpacked into a directory of the state
    /// directory, made inside a window of the store's fence and deleted at
    /// the end of the instruction; reused while that lasts.
    fn unpacked(&mut self) -> Result<&RootPath> {
        let shared = Arc::clone(&self.shared);
        let epoch = shared.view_epoch.load(Ordering::Relaxed);
        if !matches!(&self.view, Some((e, _)) if *e == epoch) {
            let store = shared
                .tar_store()?
                .ok_or(Error::State("no store for kept tars"))?;
            // Verified before any name exists: nothing unverified is ever
            // written into a directory.
            if let Some(Kept::Image { layers, tars }) = &mut self.kept
                && tars.is_none()
            {
                let spooled = layers
                    .iter()
                    .map(|layer| shared.spool(&store, layer, true))
                    .collect::<Result<Vec<_>>>()?;
                *tars = Some(spooled);
            }
            let window = store.named_window()?;
            let name = store.scratch_name()?;
            let (name, root, id) = match make_dir(&shared, &name) {
                Ok(made) => made,
                Err(e) => {
                    // A name still there keeps the window open: no `RUN`
                    // starts while it exists (`enter_run` fails the build),
                    // and the next start removes it.
                    if shared.kept.statx(&name).is_ok() {
                        std::mem::forget(window);
                    }
                    return Err(e);
                }
            };
            // Registered before anything is written: an error below leaves
            // it to the next deletion, with the window still open.
            shared.views().push(View {
                name,
                id,
                _window: window,
            });
            match &self.kept {
                // Our own snapshot of a tree: `.wh.` names are files. Device
                // nodes and FIFOs are made here too: no `RUN` runs while the
                // directory exists.
                Some(Kept::Tar { file }) => {
                    extract(&root, ReadAt::from_start(file)?, Whiteouts::Literal)?;
                }
                Some(Kept::Image {
                    tars: Some(tars), ..
                }) => {
                    let none = IgnoreList::new();
                    for tar in tars {
                        tar.extract(&root, &none)?;
                    }
                }
                Some(Kept::Image { tars: None, .. }) => {
                    return Err(Error::State("an image unpacked before it was verified"));
                }
                Some(Kept::Dir { .. }) | None => {
                    return Err(Error::State("nothing to unpack"));
                }
            }
            self.view = Some((epoch, root));
        }
        match &self.view {
            Some((_, root)) => Ok(root),
            None => Err(Error::State("no unpacked view")),
        }
    }

    /// Walks the root as it is now, as the base of the next diff.
    fn baseline(&self, hold: &mut Hold) -> Result<()> {
        baseline_in(
            &mut hold.walker,
            self.shared.walk,
            &self.shared.root,
            &self.shared.ignore,
        )?;
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
        self.check_usable(&hold)?;
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
        // What a `COPY --from` of this instruction read is gone before the
        // next one, and so before any `RUN`: a reader may ask for its view
        // before the instruction's `begin_layer`.
        clear_views(&self.shared)?;
        let mut hold = self.shared.hold();
        self.check_held(&hold)?;
        if !self.needed {
            hold.walked = false;
            return Ok(KanikoDiff {
                diff: WalkDiff::default(),
                shared: Arc::clone(&self.shared),
            });
        }
        let diff = diff_in(&mut hold.walker, &self.shared.root, &self.shared.ignore)?;
        Ok(KanikoDiff {
            diff,
            shared: Arc::clone(&self.shared),
        })
    }

    fn view(&mut self) -> Result<&RootPath> {
        if matches!(&self.kept, Some(Kept::Tar { .. } | Kept::Image { .. })) {
            return self.unpacked();
        }
        match &self.kept {
            Some(Kept::Dir { root, .. }) => Ok(root),
            Some(Kept::Tar { .. } | Kept::Image { .. }) | None => {
                self.check_usable(&self.shared.hold())?;
                Ok(&self.shared.root)
            }
        }
    }

    /// Copies `what` of the root as this stage left it into a new directory
    /// of the keep area, which [`StageRootfs::view`] then reads, and gives
    /// the root up. Again, with more, while no stage has started on a new
    /// base since; the copy before is then replaced.
    fn keep(&mut self, what: &Keep) -> Result<()> {
        if self.open {
            return Err(Error::State("a layer is open"));
        }
        let shared = Arc::clone(&self.shared);
        let mut hold = shared.hold();
        let holds = hold.holder == Some(self.id);
        if !(holds || hold.holder.is_none()) || hold.epoch != self.epoch {
            return Err(Error::State(
                "the shared root was reused since the stage ran",
            ));
        }
        if hold.broken {
            return Err(BROKEN);
        }
        clear_views(&shared)?;
        let kept = if let Some(store) = shared.tar_store()? {
            Kept::Tar {
                file: tar_out(&shared, what, &store)?,
            }
        } else {
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            let (name, root, _) = make_dir(&shared, &format!("k{id}"))?;
            if let Err(e) = copy_out(&shared, what, &root) {
                Kept::Dir {
                    name,
                    root,
                    root_meta: None,
                }
                .remove(&shared);
                return Err(e);
            }
            let root_meta = shared.root.statx("/").ok().map(|st| rnk_rootfs::Meta {
                uid: Some(st.uid),
                gid: Some(st.gid),
                mode: Some(st.mode & 0o7777),
                ..rnk_rootfs::Meta::default()
            });
            Kept::Dir {
                name,
                root,
                root_meta,
            }
        };
        if holds {
            hold.holder = None;
        }
        drop(hold);
        self.view = None;
        if let Some(old) = self.kept.replace(kept) {
            old.remove(&shared);
        }
        Ok(())
    }

    /// Gives the root up; the next stage on a base empties it, and the end
    /// of the build ([`Kaniko::finish`]) empties a root other than `/`.
    /// Nothing when the
    /// root was handed over to a child stage built on this one. A kept copy
    /// is deleted.
    fn release(self) -> Result<()> {
        clear_views(&self.shared)?;
        {
            let mut hold = self.shared.hold();
            if hold.holder == Some(self.id) {
                hold.holder = None;
            }
        }
        if let Some(k) = self.kept {
            k.remove(&self.shared);
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
