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
//!   layers' blobs (ADR-0055, Q-350). A `COPY --from` reads either from a
//!   directory unpacked in the state directory, under a name the store
//!   gives, inside a window of the store's fence: no `RUN` runs while it
//!   exists, and the next instruction of any stage deletes it first
//!   ([`StageRootfs::begin_layer`]). A stage built `FROM` a kept one gets the
//!   tar extracted into the emptied root, with device nodes and FIFOs left
//!   out as for any extraction into a live root (ADR-0041).

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use rnk_layer::{Compression, EncodeOptions, LayerDigests};
use rnk_oci::Digest;
use rnk_rootfs::{
    CopyOpts, FileType, IgnoreList, RootPath, Whiteouts, copy_filtered, delete_contents, extract,
    extract_live,
};
use rnk_store::{NamedGuard, Store};

use super::walk::{SnapshotMode, WalkDiff, WalkOptions, Walker};
use crate::error::{Error, Result};
use crate::export::DiffCounts;
use crate::stage::{Base, ExportLayer, Keep, Snapshotter, StageRootfs};

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
    /// Counts the times a stage started on a new base: a stage can keep
    /// its root only while the epoch it ran in lasts.
    epoch: u64,
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
    /// tars in this store's anonymous files.
    tars: OnceLock<Arc<Store>>,
    /// Directories unpacked for a `COPY --from` on `/`, each holding the
    /// store's fence open, deleted before the next instruction.
    views: Mutex<Vec<View>>,
    /// Bumped each time the views are deleted: a stage's cached view of an
    /// older epoch is gone.
    view_epoch: AtomicU64,
}

/// A directory unpacked in the state directory for a `COPY --from` on `/`.
#[derive(Debug)]
struct View {
    name: String,
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
                }),
                next_id: AtomicU64::new(0),
                kept,
                tars: OnceLock::new(),
                views: Mutex::new(Vec::new()),
                view_epoch: AtomicU64::new(0),
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

impl Kaniko {
    /// Keeps stages as tars in `store`'s anonymous files instead of
    /// directories (kaniko mode on `/`, ADR-0055); the keep area given to
    /// [`Kaniko::new`] then holds only short-lived unpacked directories and
    /// must be the state directory, on the ignore list.
    ///
    /// # Errors
    ///
    /// [`Error::State`] when called twice.
    pub fn keep_in_anonymous_files(&self, store: Arc<Store>) -> Result<()> {
        self.shared
            .tars
            .set(store)
            .map_err(|_| Error::State("the keep store is set already"))
    }
}

impl Shared {
    fn is_slash(&self) -> bool {
        self.path == Path::new("/")
    }

    fn views(&self) -> MutexGuard<'_, Vec<View>> {
        self.views.lock().unwrap_or_else(PoisonError::into_inner)
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
        let gone = shared
            .kept
            .sub_root(&view.name)
            .and_then(|r| delete_contents(&r, &IgnoreList::new()))
            .and_then(|()| shared.kept.remove_dir(&view.name));
        if let Err(e) = gone {
            views.push(view);
            return Err(e.into());
        }
    }
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
                            &|p, _| !shared.ignore.covers(p),
                        )?;
                        if !shared.is_slash()
                            && let Some(meta) = root_meta
                        {
                            shared.root.set_meta("", meta)?;
                        }
                    }
                    Some(Kept::Tar { file }) => {
                        extract_live(&shared.root, reopen(file)?, &shared.ignore)?;
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
                    extract_blob(&shared.root, &shared.ignore, layer)?;
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
            // On `/`: the blobs, already anonymous files of the store,
            // unpacked only when a `COPY --from` reads them.
            Kept::Image {
                layers: layers.to_vec(),
            }
        } else {
            let (name, root) = make_dir(shared, &format!("k{id}"))?;
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
    /// A plain tar of the stage in an anonymous file (`/`, ADR-0055).
    Tar { file: File },
    /// The layers of a `COPY --from` image, unpacked when read (`/`).
    Image { layers: Vec<KanikoLayer> },
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

/// A new, empty directory `name` of the keep area. It is opened by name,
/// which follows a symlink, so the directory opened must be the one just
/// made (type, device, inode and mount, not following a link): a name
/// swapped for a link to `.` would otherwise make its removal empty the
/// whole keep area.
fn make_dir(shared: &Shared, name: &str) -> Result<(String, RootPath)> {
    shared.kept.create_dir(name, 0o700)?;
    let opened = shared.kept.statx(name).and_then(|made| {
        let root = shared.kept.sub_root(name)?;
        let st = root.statx("/")?;
        Ok((made, st, root))
    });
    match opened {
        Ok((made, st, root))
            if made.file_type() == FileType::Directory
                && (made.dev_major, made.dev_minor, made.ino)
                    == (st.dev_major, st.dev_minor, st.ino)
                && root.mnt_id() == shared.kept.mnt_id() =>
        {
            Ok((name.to_owned(), root))
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

/// A new descriptor of an anonymous file, read from its start, through this
/// process's own `/proc/self/fd` (checked to be procfs when the store
/// opened).
fn reopen(file: &File) -> Result<File> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    File::open(&path).map_err(|source| Error::Io {
        op: "reopen",
        path,
        source,
    })
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
    let mut diff = walker.diff(&shared.root, &shared.ignore)?;
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
    let filter = |p: &Path, _: bool| !shared.ignore.covers(p);
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

/// Extracts one base layer into `root`, checking its diff ID.
fn extract_blob(root: &RootPath, ignore: &IgnoreList, layer: &KanikoLayer) -> Result<()> {
    let file = File::open(&layer.blob).map_err(|source| Error::Io {
        op: "open",
        path: layer.blob.clone(),
        source,
    })?;
    let mut decoder = rnk_layer::Decoder::new(file)?;
    extract_live(root, &mut decoder, ignore)?;
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

impl KanikoRoot {
    fn check_held(&self, hold: &Hold) -> Result<()> {
        if hold.holder == Some(self.id) {
            Ok(())
        } else {
            Err(Error::State("the stage no longer holds the shared root"))
        }
    }

    /// The kept tar or image unpacked into a directory of the state
    /// directory, made inside a window of the store's fence and deleted
    /// before the next instruction; reused while that lasts.
    fn unpacked(&mut self) -> Result<&RootPath> {
        let shared = Arc::clone(&self.shared);
        let epoch = shared.view_epoch.load(Ordering::Relaxed);
        if !matches!(&self.view, Some((e, _)) if *e == epoch) {
            let store = shared
                .tars
                .get()
                .ok_or(Error::State("no store for kept tars"))?;
            let window = store.named_window()?;
            let name = store.scratch_name()?;
            let (name, root) = make_dir(&shared, &name)?;
            // Registered before anything is written: an error below leaves
            // it to the next deletion, with the window still open.
            shared.views().push(View {
                name,
                _window: window,
            });
            match &self.kept {
                Some(Kept::Tar { file }) => extract(&root, reopen(file)?, Whiteouts::Apply)?,
                Some(Kept::Image { layers }) => {
                    let none = IgnoreList::new();
                    for layer in layers {
                        extract_blob(&root, &none, layer)?;
                    }
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
        // What a `COPY --from` read is gone before the instruction runs.
        clear_views(&self.shared)?;
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
        if matches!(&self.kept, Some(Kept::Tar { .. } | Kept::Image { .. })) {
            return self.unpacked();
        }
        match &self.kept {
            Some(Kept::Dir { root, .. }) => Ok(root),
            Some(Kept::Tar { .. } | Kept::Image { .. }) | None => {
                self.check_held(&self.shared.hold())?;
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
        clear_views(&shared)?;
        let kept = if let Some(store) = shared.tars.get() {
            Kept::Tar {
                file: tar_out(&shared, what, store)?,
            }
        } else {
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            let (name, root) = make_dir(&shared, &format!("k{id}"))?;
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
