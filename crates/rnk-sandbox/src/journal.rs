//! Stub mount points, their journal, and the per-`RUN` scratch files.
//!
//! An image may lack a mount target (`alpine` has no `/etc/resolv.conf`).
//! The empty file or directory created as the target is a real entry of the
//! root: it would reach the layer, and it survives a killed build. So, as in
//! BuildKit's `executor/stubs.go`, each `RUN` with mounts has a journal,
//! `<state>/sandbox/runs/<id>.journal`, locked with `flock` while the `RUN`
//! lives, next to `<id>/`, its writable `/etc` copies. One record per line,
//! tab-separated, paths in hex:
//!
//! - `root <path>`: the host path of the root, first.
//! - `stub <d|f> <path> <atime> <mtime>`: written and synced *before* the
//!   stub exists, with the times its parent had before.
//! - `swap <path> <aside|-> <atime> <mtime>`: a file ranikust puts at a
//!   fixed path of an [`Isolation::Direct`](crate::Isolation::Direct) root
//!   other than `/` (one of [`PLACEABLE`]: `resolv.conf`, `hosts`,
//!   `hostname`, the `/dev` nodes), written and synced *before* anything
//!   changes. An existing entry is first renamed aside in the same
//!   directory, to `aside` ([`ASIDE_PREFIX`]); `-` when there was none.
//! - `placed <path> <mtime>`: the parent's mtime just after; all `placed`
//!   records are synced before the `RUN` starts.
//!
//! Entries are undone after the `RUN`, newest first: a stub is removed while
//! still empty; a swapped path gets its aside entry back by `rename`, or,
//! without one, the placed file is removed. The parent's times go back to
//! the old ones only if its mtime still is the `placed` one (no `placed`
//! record: the build died before the `RUN`). At start-up, journals no live
//! process holds are replayed the same way.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use rnk_rootfs::{FileTime, FileType, Meta, OpenHow, RootPath};
use rustix::fs::FlockOperation;
use rustix::io::Errno;

use crate::error::{Error, Result, io_err};
use crate::spec::MountKind;

/// Symlinks followed while placing one target, as the kernel's limit.
const MAX_SYMLINKS: u32 = 40;

/// Extension of a journal.
const JOURNAL_EXT: &str = "journal";
/// Extension of a journal being created, before it is locked and renamed.
const NEW_EXT: &str = "journal-new";

/// The most bytes a journal holds, written or replayed. Each entry takes
/// two lines, each with one path of at most `PATH_MAX` in hex (8 KiB) and
/// two times: about a thousand stubs at the longest paths, tens of
/// thousands at ordinary ones. A `RUN` whose mounts need more fails before
/// placing the entry; a replay leaves a larger file unread, with a warning.
const MAX_JOURNAL_BYTES: u64 = 16 << 20;

/// Scratch files of a run, removed by name, never recursively.
pub(crate) const SCRATCH_FILES: [&str; 3] = ["resolv.conf", "hosts", "hostname"];

/// The only paths a `swap` record may name: a planted journal cannot make a
/// replay remove or replace anything else.
pub(crate) const PLACEABLE: [&str; 9] = [
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/hostname",
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];

/// How the name of an entry renamed aside starts; the journal's id and the
/// entry's name follow.
pub(crate) const ASIDE_PREFIX: &str = ".ranikust-aside-";

/// What a journal entry undoes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Placed {
    /// An empty mount target that did not exist.
    Stub(MountKind),
    /// A file or node put at one of [`PLACEABLE`]; `aside` is what was there
    /// before, renamed in the same directory.
    Swap { aside: Option<PathBuf> },
}

/// What to put at a [`PLACEABLE`] path.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Content<'a> {
    /// A regular file, mode 0644.
    File(&'a [u8]),
    /// A character device, mode 0666.
    CharDev { major: u32, minor: u32 },
}

/// One journaled entry: a stub, or a swapped path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StubRecord {
    pub(crate) kind: Placed,
    /// Absolute path inside the root.
    pub(crate) path: PathBuf,
    /// The parent's times before the stub was created.
    pub(crate) parent_atime: FileTime,
    pub(crate) parent_mtime: FileTime,
    /// The parent's mtime right after, once recorded.
    pub(crate) placed_mtime: Option<FileTime>,
}

/// The journal and scratch directory of one `RUN`.
#[derive(Debug)]
pub(crate) struct Journal {
    file: File,
    id: String,
    path: PathBuf,
    scratch: PathBuf,
    root: RootPath,
    stubs: Vec<StubRecord>,
    /// Bytes written so far, at most [`MAX_JOURNAL_BYTES`].
    len: u64,
    /// `placed` records written since the last sync.
    unsynced: bool,
}

impl Journal {
    /// Creates and locks `<runs_dir>/<id>.journal` for the root `root_dir`,
    /// and the empty scratch directory `<runs_dir>/<id>/`. The journal is
    /// written and locked under a temporary name and renamed afterwards, so
    /// a replay never sees it unlocked.
    pub(crate) fn create(
        runs_dir: &Path,
        id: &str,
        root_dir: &Path,
        root: RootPath,
    ) -> Result<Self> {
        let path = runs_dir.join(format!("{id}.{JOURNAL_EXT}"));
        let new = runs_dir.join(format!("{id}.{NEW_EXT}"));
        let file = File::options()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&new)
            .map_err(|e| io_err("create the stub journal", &new, e))?;
        rustix::fs::flock(&file, FlockOperation::LockExclusive)
            .map_err(|e| io_err("lock the stub journal", &new, e.into()))?;
        if path.exists() {
            let _ = fs::remove_file(&new);
            return Err(io_err(
                "create the stub journal",
                &path,
                io::Error::from(io::ErrorKind::AlreadyExists),
            ));
        }
        fs::rename(&new, &path).map_err(|e| io_err("publish the stub journal", &path, e))?;
        File::open(runs_dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_err("sync", runs_dir, e))?;
        let mut journal = Self {
            file,
            id: id.to_owned(),
            scratch: runs_dir.join(id),
            path,
            // What the journal places and undoes stays on the root's own
            // mount: a mount below it is never written through.
            root: root.stay_on_mount(),
            stubs: Vec::new(),
            len: 0,
            unsynced: false,
        };
        journal.append(&format!("root\t{}\n", hex(root_dir.as_os_str().as_bytes())))?;
        fs::create_dir(&journal.scratch)
            .map_err(|e| io_err("create the run directory", &journal.scratch, e))?;
        Ok(journal)
    }

    /// The directory for the run's scratch files.
    pub(crate) fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// Stubs placed so far.
    pub(crate) fn stub_count(&self) -> usize {
        self.stubs.len()
    }

    /// Makes `target` exist in the root, as a directory or an empty file,
    /// creating each missing component as a journaled stub. Symlinks on the
    /// way, dangling ones included, are followed inside the root, so the stub
    /// lands where the mount will resolve the target.
    pub(crate) fn ensure(&mut self, target: &Path, kind: MountKind) -> Result<usize> {
        let mut created = 0;
        let mut cur = PathBuf::from("/");
        let mut rest = components(target);
        let mut hops = 0;
        while let Some(name) = rest.pop_front() {
            if name == ".." {
                cur.pop();
                continue;
            }
            let next = cur.join(&name);
            let last = rest.is_empty();
            match self.root.statx(&next) {
                Ok(st) => match st.file_type() {
                    FileType::Symlink => {
                        hops += 1;
                        if hops > MAX_SYMLINKS {
                            return Err(io_err(
                                "place a mount target",
                                &next,
                                io::Error::from_raw_os_error(libc::ELOOP),
                            ));
                        }
                        let link = self.root.read_link(&next)?;
                        if link.is_absolute() {
                            cur = PathBuf::from("/");
                        }
                        for c in components(&link).into_iter().rev() {
                            rest.push_front(c);
                        }
                    }
                    FileType::Directory => cur = next,
                    FileType::Regular
                    | FileType::CharDevice
                    | FileType::BlockDevice
                    | FileType::Fifo
                    | FileType::Socket
                    | FileType::Unknown => {
                        if !last {
                            return Err(io_err(
                                "place a mount target",
                                &next,
                                io::Error::from_raw_os_error(libc::ENOTDIR),
                            ));
                        }
                        // An existing non-directory target: the mount decides.
                        cur = next;
                    }
                },
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                    let kind = if last { kind } else { MountKind::Directory };
                    self.place(&cur, &next, kind)?;
                    created += 1;
                    cur = next;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(created)
    }

    fn place(&mut self, parent: &Path, path: &Path, kind: MountKind) -> Result<()> {
        let before = self.root.statx(parent)?;
        let (code, mode) = match kind {
            MountKind::Directory => ('d', 0o755),
            MountKind::File => ('f', 0o644),
        };
        self.append(&format!(
            "stub\t{code}\t{}\t{}\t{}\n",
            hex(path.as_os_str().as_bytes()),
            time(before.atime),
            time(before.mtime),
        ))?;
        self.sync()?;
        match kind {
            MountKind::Directory => self.root.create_dir(path, mode)?,
            MountKind::File => drop(
                self.root
                    .open_at(path, &OpenHow::write().create_new(mode))?,
            ),
        }
        // The record is complete before anything else can fail, so cleanup
        // finds the stub.
        self.stubs.push(StubRecord {
            kind: Placed::Stub(kind),
            path: path.to_owned(),
            parent_atime: before.atime,
            parent_mtime: before.mtime,
            placed_mtime: None,
        });
        // The umask must not decide whether a non-root `USER` can traverse a
        // stub directory.
        self.chmod(path, mode)?;
        self.record_placed(parent, path)
    }

    /// Puts `content` at `path`, one of [`PLACEABLE`], for the `RUN`: what is
    /// there is renamed aside first, and both steps are journaled before
    /// either happens. `false`, with everything undone, when the kernel
    /// refuses to create a device node (`EPERM` in a user namespace): the
    /// `RUN` then runs without it.
    pub(crate) fn swap_in(&mut self, path: &Path, content: Content<'_>) -> Result<bool> {
        const OP: &str = "place a file for the RUN";
        let invalid = || io_err(OP, path, io::Error::from(io::ErrorKind::InvalidInput));
        if !is_placeable(path) {
            return Err(invalid());
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(invalid());
        };
        // Not followed: through a symlinked `/etc`, the aside and its restore
        // would land elsewhere.
        let before = self.root.statx(parent)?;
        if before.file_type() != FileType::Directory {
            return Err(io_err(
                OP,
                parent,
                io::Error::from_raw_os_error(libc::ENOTDIR),
            ));
        }
        let aside = match self.root.statx(path) {
            Ok(st) if st.file_type() == FileType::Directory => {
                return Err(io_err(OP, path, io::Error::from_raw_os_error(libc::EISDIR)));
            }
            Ok(_) => {
                let mut aside = OsString::from(format!("{ASIDE_PREFIX}{}-", self.id));
                aside.push(name);
                Some(parent.join(aside))
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => None,
            Err(e) => return Err(e.into()),
        };
        self.append(&format!(
            "swap\t{}\t{}\t{}\t{}\n",
            hex(path.as_os_str().as_bytes()),
            aside
                .as_ref()
                .map_or_else(|| "-".to_owned(), |a| hex(a.as_os_str().as_bytes())),
            time(before.atime),
            time(before.mtime),
        ))?;
        self.sync()?;
        self.stubs.push(StubRecord {
            kind: Placed::Swap {
                aside: aside.clone(),
            },
            path: path.to_owned(),
            parent_atime: before.atime,
            parent_mtime: before.mtime,
            placed_mtime: None,
        });
        if let Some(aside) = &aside
            && let Err(e) = self.root.rename_noreplace(path, aside)
        {
            // Nothing moved: an undo would only see the aside name missing.
            self.stubs.pop();
            return Err(e.into());
        }
        match content {
            Content::File(data) => {
                let fd = self
                    .root
                    .open_at(path, &OpenHow::write().no_follow().create_new(0o644))?;
                File::from(fd)
                    .write_all(data)
                    .map_err(|e| io_err("write", path, e))?;
                self.chmod(path, 0o644)?;
            }
            Content::CharDev { major, minor } => {
                let dir = self.root.open_at(parent, &OpenHow::path().directory())?;
                match rustix::fs::mknodat(
                    &dir,
                    name,
                    rustix::fs::FileType::CharacterDevice,
                    rustix::fs::Mode::from_raw_mode(0o666),
                    rustix::fs::makedev(major, minor),
                ) {
                    Ok(()) => self.chmod(path, 0o666)?,
                    Err(Errno::PERM) => {
                        self.undo_last()?;
                        return Ok(false);
                    }
                    Err(e) => return Err(io_err("mknod", path, e.into())),
                }
            }
        }
        self.record_placed(parent, path)?;
        Ok(true)
    }

    /// Sets the permission bits whatever the umask was.
    fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
        Ok(self.root.set_meta(
            path,
            &Meta {
                mode: Some(mode),
                ..Meta::default()
            },
        )?)
    }

    /// Undoes and forgets the newest entry: a swap that could not complete.
    fn undo_last(&mut self) -> Result<()> {
        if let Some(last) = self.stubs.last() {
            undo(&self.root, last)?;
        }
        self.stubs.pop();
        Ok(())
    }

    /// Journals the parent's mtime right after placing the newest entry.
    fn record_placed(&mut self, parent: &Path, path: &Path) -> Result<()> {
        let after = self.root.statx(parent)?;
        self.append(&format!(
            "placed\t{}\t{}\n",
            hex(path.as_os_str().as_bytes()),
            time(after.mtime)
        ))?;
        if let Some(last) = self.stubs.last_mut() {
            last.placed_mtime = Some(after.mtime);
        }
        self.unsynced = true;
        Ok(())
    }

    /// Syncs the `placed` records; call before the `RUN` starts.
    pub(crate) fn commit(&mut self) -> Result<()> {
        if self.unsynced {
            self.sync()?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// Removes the stubs and the scratch files, then the journal. On error the
    /// journal stays for the next start-up.
    pub(crate) fn finish(self) -> Result<usize> {
        let removed = remove_stubs(&self.root, &self.stubs)?;
        remove_scratch(&self.scratch)?;
        fs::remove_file(&self.path)
            .map_err(|e| io_err("remove the stub journal", &self.path, e))?;
        Ok(removed)
    }

    fn append(&mut self, line: &str) -> Result<()> {
        let len = u64::try_from(line.len())
            .ok()
            .and_then(|n| self.len.checked_add(n))
            .filter(|&len| len <= MAX_JOURNAL_BYTES)
            .ok_or_else(|| io_err("write the stub journal", &self.path, too_large()))?;
        self.file
            .write_all(line.as_bytes())
            .map_err(|e| io_err("write the stub journal", &self.path, e))?;
        self.len = len;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.file
            .sync_data()
            .map_err(|e| io_err("sync the stub journal", &self.path, e))
    }
}

/// What a start-up replay did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Replayed {
    /// Journals of dead runs processed and removed.
    pub(crate) journals: usize,
    /// Stubs removed from their roots.
    pub(crate) stubs: usize,
}

/// Replays every journal in `runs_dir` that no live process holds.
///
/// The state directory is not trusted further than its owner: a journal is
/// opened without following a symlink, its root must lie below `confine`
/// (the state directory, where every root of a build lives), and its scratch
/// directory is removed through descriptors, never through a symlink. A
/// journal that fails these checks is left in place with a warning. The one
/// exception is `shared_root`, the kaniko-mode workdir the caller locked and
/// checked: a journal of an [`Isolation::Direct`](crate::Isolation::Direct)
/// `RUN` on it is replayed too, never one naming `/`.
pub(crate) fn replay(
    runs_dir: &Path,
    confine: &Path,
    shared_root: Option<&RootPath>,
) -> Result<Replayed> {
    let mut done = Replayed::default();
    let entries = match fs::read_dir(runs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(done),
        Err(e) => return Err(io_err("list the stub journals", runs_dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err("list the stub journals", runs_dir, e))?;
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        if ext == NEW_EXT {
            // Its creator died before renaming it, or is about to: removed
            // only when nobody holds the lock.
            remove_unlocked(&path);
            continue;
        }
        if ext != JOURNAL_EXT {
            continue;
        }
        match replay_one(&path, confine, shared_root) {
            Ok(Some(stubs)) => {
                done.journals += 1;
                done.stubs += stubs;
            }
            Ok(None) => {}
            // One bad journal must not stop the build; it stays for later.
            Err(e) => tracing::warn!("rnk-sandbox: cannot replay {}: {e}", path.display()),
        }
    }
    Ok(done)
}

/// Removes a journal that was never published, unless it is locked.
fn remove_unlocked(path: &Path) {
    let file = match open_journal(path, false) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!("rnk-sandbox: left {} in place: {e}", path.display());
            return;
        }
    };
    if rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).is_ok() {
        let _ = fs::remove_file(path);
    }
}

/// Opens the journal `path` for I/O only if it is a regular file of at most
/// [`MAX_JOURNAL_BYTES`], as the store opens its lock files: the entry is
/// pinned with `O_PATH | O_NOFOLLOW`, its type and size are checked on that
/// descriptor, and only then is it reopened, through this thread's own
/// `/proc/thread-self/fd`, with `O_NONBLOCK | O_NOCTTY`. A FIFO planted at
/// a journal's name would block the replay in `open`, or, opened
/// read-write, in a `read` whose only writer is the replay itself; a
/// device node would be opened as root. Neither ever is: the open fails
/// with `InvalidData`, and a larger file with `FileTooLarge`, before
/// anything opens or locks it.
fn open_journal(path: &Path, write: bool) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let pinned = rustix::fs::open(
        path,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let st = rustix::fs::fstat(&pinned)?;
    if rustix::fs::FileType::from_raw_mode(st.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file, so not a journal ranikust wrote",
        ));
    }
    if u64::try_from(st.st_size).map_or(true, |size| size > MAX_JOURNAL_BYTES) {
        return Err(too_large());
    }
    // This thread's table: the one the pinned descriptor is in, even if a
    // thread ever unshares its files.
    let proc = rustix::fs::open(
        "/proc/thread-self/fd",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if rustix::fs::fstatfs(&proc)?.f_type != rustix::fs::PROC_SUPER_MAGIC {
        return Err(io::Error::other("/proc/thread-self/fd is not on procfs"));
    }
    let access = if write { OFlags::RDWR } else { OFlags::RDONLY };
    let num = std::os::fd::AsRawFd::as_raw_fd(&pinned).to_string();
    let fd = rustix::fs::openat(
        &proc,
        num.as_str(),
        access | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

/// The error for a journal over [`MAX_JOURNAL_BYTES`].
fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!(
            "too large: a stub journal holds at most {} MiB",
            MAX_JOURNAL_BYTES >> 20
        ),
    )
}

/// Replays one journal; `None` when a live run still holds it.
fn replay_one(
    path: &Path,
    confine: &Path,
    shared_root: Option<&RootPath>,
) -> Result<Option<usize>> {
    // Read-write: where `flock` is emulated with record locks (NFS), an
    // exclusive lock needs a descriptor open for writing.
    let mut file =
        open_journal(path, true).map_err(|e| io_err("open the stub journal", path, e))?;
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK) => return Ok(None),
        Err(e) => return Err(io_err("lock the stub journal", path, e.into())),
    }
    let replayed = replay_locked(&mut file, path, confine, shared_root);
    // Released explicitly, not only by closing: the lock belongs to the open
    // file description, which a child cloned meanwhile by another thread
    // shares until it executes or exits; a journal left behind would then
    // look live to the next replay. The unlock cannot fail on a lock held.
    let _ = rustix::fs::flock(&file, FlockOperation::Unlock);
    replayed
}

/// [`replay_one`] with the journal locked.
fn replay_locked(
    file: &mut File,
    path: &Path,
    confine: &Path,
    shared_root: Option<&RootPath>,
) -> Result<Option<usize>> {
    // Its owner removed it between our open and our lock: already clean.
    let st =
        rustix::fs::fstat(&*file).map_err(|e| io_err("stat the stub journal", path, e.into()))?;
    if st.st_nlink == 0 {
        return Ok(None);
    }
    // `open_journal` checked the size; bounded again while reading, since
    // the file may have grown since.
    let mut text = Vec::new();
    io::Read::take(&mut *file, MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut text)
        .map_err(|e| io_err("read the stub journal", path, e))?;
    if u64::try_from(text.len()).map_or(true, |len| len > MAX_JOURNAL_BYTES) {
        return Err(io_err("read the stub journal", path, too_large()));
    }
    let (root_dir, stubs) = parse(&text);
    let mut removed = 0;
    if let Some(root_dir) = root_dir {
        if let Some(shared) = shared_root
            && is_shared_root(&root_dir, shared)
        {
            // The checked descriptor, never the journal's path reopened. On
            // it only what `place_direct` makes: swaps and the `/etc` and
            // `/dev` stubs.
            let stubs: Vec<StubRecord> = stubs
                .into_iter()
                .filter(|s| match &s.kind {
                    Placed::Swap { .. } => true,
                    Placed::Stub(MountKind::Directory) => {
                        s.path == Path::new("/etc") || s.path == Path::new("/dev")
                    }
                    Placed::Stub(MountKind::File) => false,
                })
                .collect();
            removed = remove_stubs(shared, &stubs)?;
        } else if let Some(root_dir) = confined_root(&root_dir, confine)? {
            // `None` above: the root is gone, and its stubs with it.
            match RootPath::open(&root_dir) {
                Ok(root) => removed = remove_stubs(&root.stay_on_mount(), &stubs)?,
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    remove_scratch(&path.with_extension(""))?;
    fs::remove_file(path).map_err(|e| io_err("remove the stub journal", path, e))?;
    Ok(Some(removed))
}

/// Undoes the entries newest first and restores their parents' times.
fn remove_stubs(root: &RootPath, stubs: &[StubRecord]) -> Result<usize> {
    let mut removed = 0;
    for stub in stubs.iter().rev() {
        if undo(root, stub)? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Undoes one entry; `true` when something was removed or put back, after
/// which the parent's times are restored unless something else changed
/// them.
fn undo(root: &RootPath, stub: &StubRecord) -> Result<bool> {
    let parent = stub.path.parent().unwrap_or(Path::new("/"));
    // `root` stays on its mount, so no parent below another mount resolves
    // (`EXDEV`); an entry that is itself a mount point (a mount root, or on
    // another mount) is left alone.
    let parent_st = match root.statx(parent) {
        Ok(st) => Some(st),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => return Ok(false),
        // Any other error (a non-exhaustive type, maybe no errno): the parent's times are unknown.
        Err(_) => None,
    };
    if parent_st.is_some_and(|st| st.mnt_id != root.mnt_id()) {
        return Ok(false);
    }
    let on_root_mount = |p: &Path| {
        root.statx(p)
            .is_ok_and(|st| st.mnt_id == root.mnt_id() && !st.mount_root)
    };
    let mut targets = vec![stub.path.as_path()];
    if let Placed::Swap { aside: Some(aside) } = &stub.kind {
        targets.push(aside);
    }
    if targets
        .iter()
        .any(|p| root.statx(p).is_ok() && !on_root_mount(p))
    {
        return Ok(false);
    }
    let parent_mtime = parent_st.map(|st| st.mtime);
    let gone = match &stub.kind {
        Placed::Stub(MountKind::Directory) => ok_unless(
            root.remove_dir(&stub.path),
            &[libc::ENOENT, libc::ENOTEMPTY, libc::ENOTDIR, libc::EEXIST],
        )?,
        Placed::Stub(MountKind::File) => match root.statx(&stub.path) {
            Ok(st) if st.file_type() == FileType::Regular && st.size == 0 => {
                ok_unless(root.remove_file(&stub.path), &[libc::ENOENT])?
            }
            // Gone, unreadable, or no longer the empty file placed: leave it.
            _ => false,
        },
        // The original goes back over whatever is there now: `rename`
        // replaces a file, a node or a symlink. No aside: never moved, or
        // already back.
        Placed::Swap { aside: Some(aside) } => match root.statx(aside) {
            Ok(_) => {
                root.rename(aside, &stub.path)?;
                true
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => false,
            Err(e) => return Err(e.into()),
        },
        // Nothing was there before: what is there now is what ranikust
        // placed, or what the `RUN` made of it, and goes. A directory stays.
        Placed::Swap { aside: None } => match root.statx(&stub.path) {
            Ok(st) if st.file_type() != FileType::Directory => {
                ok_unless(root.remove_file(&stub.path), &[libc::ENOENT])?
            }
            // A directory, gone or unreadable: leave it.
            _ => false,
        },
    };
    if !gone {
        return Ok(false);
    }
    let restore = match stub.placed_mtime {
        Some(placed) => parent_mtime == Some(placed),
        // Never placed: the build died before the `RUN` started.
        None => true,
    };
    if restore {
        root.set_meta(
            parent,
            &Meta {
                atime: Some(stub.parent_atime),
                mtime: Some(stub.parent_mtime),
                ..Meta::default()
            },
        )?;
    }
    Ok(true)
}

/// Whether `path` is one of [`PLACEABLE`].
fn is_placeable(path: &Path) -> bool {
    PLACEABLE.iter().any(|p| Path::new(p) == path)
}

/// Whether `aside` is a name [`Journal::swap_in`] gives what was at `path`:
/// in the same directory, [`ASIDE_PREFIX`] first and the name last.
fn is_aside_of(aside: &Path, path: &Path) -> bool {
    let (Some(a), Some(p)) = (aside.file_name(), path.file_name()) else {
        return false;
    };
    aside.parent() == path.parent()
        && a.as_bytes().starts_with(ASIDE_PREFIX.as_bytes())
        && a.as_bytes().ends_with(p.as_bytes())
}

/// `Ok(true)` on success, `Ok(false)` for an errno in `expected`.
fn ok_unless(r: rnk_rootfs::Result<()>, expected: &[i32]) -> Result<bool> {
    match r {
        Ok(()) => Ok(true),
        Err(e) if e.raw_os_error().is_some_and(|n| expected.contains(&n)) => Ok(false),
        Err(e) => Err(Error::Rootfs(e)),
    }
}

/// Whether the journal's root, opened without following a symlink, is the
/// directory `shared` holds: same device, inode and mount.
fn is_shared_root(root_dir: &Path, shared: &RootPath) -> bool {
    let (Ok(root), Ok(want)) = (
        RootPath::open(root_dir).and_then(|r| r.statx("/")),
        shared.statx("/"),
    ) else {
        return false;
    };
    identity(&root) == identity(&want) && root.mnt_id == want.mnt_id
}

/// `(dev major, dev minor, ino)`: what identifies a directory whatever path
/// or bind mount reaches it.
fn identity(st: &rnk_rootfs::Stat) -> (u32, u32, u64) {
    (st.dev_major, st.dev_minor, st.ino)
}

/// Refuses a root that is the host's `/`, `/etc` or `/dev` by device and
/// inode, through whatever path or bind mount (`/proc/1/root` included) it
/// was opened: placing files there would rename and replace the host's.
pub(crate) fn refuse_host_system_dir(root: &RootPath, shown: &Path) -> Result<()> {
    let st = root.statx("/")?;
    for host in ["/", "/etc", "/dev"] {
        let Ok(h) = rustix::fs::statx(
            rustix::fs::CWD,
            host,
            rustix::fs::AtFlags::empty(),
            rustix::fs::StatxFlags::BASIC_STATS,
        ) else {
            continue;
        };
        if (h.stx_dev_major, h.stx_dev_minor, h.stx_ino) == identity(&st) {
            return Err(Error::InvalidSpec(format!(
                "{} is the host's {host}: nothing is placed there",
                shown.display()
            )));
        }
    }
    Ok(())
}

/// The journal's root, resolved, if it still exists and lies strictly below
/// `confine`; an error for a root elsewhere, which a journal planted in the
/// state directory could name to remove host files.
fn confined_root(root_dir: &Path, confine: &Path) -> Result<Option<PathBuf>> {
    let root = match root_dir.canonicalize() {
        Ok(root) => root,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err("resolve the journaled root", root_dir, e)),
    };
    let base = confine
        .canonicalize()
        .map_err(|e| io_err("resolve the state directory", confine, e))?;
    if root == base || !root.starts_with(&base) {
        return Err(io_err(
            "replay a journal whose root is outside the state directory",
            root_dir,
            io::Error::from(io::ErrorKind::PermissionDenied),
        ));
    }
    Ok(Some(root))
}

/// Removes the scratch files of the run directory `dir` by name, then the
/// directory, through descriptors: `dir` is opened below its parent without
/// following a symlink, so a planted `runs/<id> -> /etc` removes nothing.
fn remove_scratch(dir: &Path) -> Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(io_err(
            "remove the run directory",
            dir,
            io::Error::from(io::ErrorKind::InvalidInput),
        ));
    };
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let runs = rustix::fs::open(parent, flags, Mode::empty())
        .map_err(|e| io_err("open the runs directory", parent, e.into()))?;
    let run = match rustix::fs::openat(&runs, name, flags, Mode::empty()) {
        Ok(run) => run,
        Err(Errno::NOENT) => return Ok(()),
        Err(e) => return Err(io_err("open the run directory", dir, e.into())),
    };
    for file in SCRATCH_FILES {
        match rustix::fs::unlinkat(&run, file, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(e) => return Err(io_err("remove a run file", dir.join(file), e.into())),
        }
    }
    match rustix::fs::unlinkat(&runs, name, AtFlags::REMOVEDIR) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(io_err("remove the run directory", dir, e.into())),
    }
}

/// The root and the stubs a journal names; lines that do not parse (a torn
/// last write) are skipped.
fn parse(text: &[u8]) -> (Option<PathBuf>, Vec<StubRecord>) {
    let mut root = None;
    let mut stubs: Vec<StubRecord> = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["root", path] => root = unhex(path).map(path_from),
            ["stub", kind, path, atime, mtime] => {
                let kind = match *kind {
                    "d" => MountKind::Directory,
                    "f" => MountKind::File,
                    // Not a code `place` writes: a planted line.
                    _ => continue,
                };
                let (Some(path), Some(parent_atime), Some(parent_mtime)) =
                    (unhex(path), parse_time(atime), parse_time(mtime))
                else {
                    continue;
                };
                stubs.push(StubRecord {
                    kind: Placed::Stub(kind),
                    path: path_from(path),
                    parent_atime,
                    parent_mtime,
                    placed_mtime: None,
                });
            }
            ["swap", path, aside, atime, mtime] => {
                let (Some(path), Some(parent_atime), Some(parent_mtime)) =
                    (unhex(path), parse_time(atime), parse_time(mtime))
                else {
                    continue;
                };
                let path = path_from(path);
                let aside = match *aside {
                    "-" => None,
                    a => match unhex(a) {
                        Some(a) => Some(path_from(a)),
                        None => continue,
                    },
                };
                // Only what `swap_in` writes: a planted record naming any
                // other path is skipped.
                if !is_placeable(&path) || aside.as_ref().is_some_and(|a| !is_aside_of(a, &path)) {
                    continue;
                }
                stubs.push(StubRecord {
                    kind: Placed::Swap { aside },
                    path,
                    parent_atime,
                    parent_mtime,
                    placed_mtime: None,
                });
            }
            ["placed", path, mtime] => {
                let (Some(path), Some(mtime)) = (unhex(path), parse_time(mtime)) else {
                    continue;
                };
                // The writer journals `placed` right after the entry it
                // completes, so only the newest record can match: a planted
                // journal of unmatched lines costs one comparison each.
                if let Some(last) = stubs.last_mut()
                    && last.placed_mtime.is_none()
                    && last.path.as_os_str().as_bytes() == path.as_slice()
                {
                    last.placed_mtime = Some(mtime);
                }
            }
            // The empty tail after the last newline, a torn last write or a planted line.
            _ => {}
        }
    }
    (root, stubs)
}

/// The normal components of a path inside the root, `..` kept, as a queue.
fn components(p: &Path) -> VecDeque<OsString> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

fn path_from(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0xf)]));
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |c: u8| {
                char::from(c)
                    .to_digit(16)
                    .and_then(|d| u8::try_from(d).ok())
            };
            Some((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect()
}

fn time(t: FileTime) -> String {
    format!("{}.{:09}", t.secs, t.nanos)
}

fn parse_time(s: &str) -> Option<FileTime> {
    let (secs, nanos) = s.split_once('.')?;
    let nanos: u32 = nanos.parse().ok()?;
    if nanos >= 1_000_000_000 {
        return None;
    }
    Some(FileTime {
        secs: secs.parse().ok()?,
        nanos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        root_dir: PathBuf,
        runs: PathBuf,
    }

    impl Fixture {
        /// The state directory the roots lie in.
        fn base(&self) -> &Path {
            self.dir.path()
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root_dir = dir.path().join("root");
        let runs = dir.path().join("runs");
        fs::create_dir_all(root_dir.join("etc")).unwrap();
        fs::create_dir(&runs).unwrap();
        Fixture {
            root_dir: root_dir.canonicalize().unwrap(),
            runs,
            dir,
        }
    }

    /// Host files a planted journal aims at, outside the state directory.
    fn victim() -> tempfile::TempDir {
        let victim = tempfile::tempdir().unwrap();
        for name in SCRATCH_FILES {
            fs::write(victim.path().join(name), b"host").unwrap();
        }
        victim
    }

    fn assert_untouched(victim: &tempfile::TempDir) {
        for name in SCRATCH_FILES {
            assert_eq!(
                fs::read(victim.path().join(name)).unwrap(),
                b"host",
                "{name}"
            );
        }
    }

    #[test]
    fn replay_does_not_follow_a_planted_run_directory_symlink() {
        let f = fixture();
        let victim = victim();
        fs::write(f.runs.join("x.journal"), b"").unwrap();
        std::os::unix::fs::symlink(victim.path(), f.runs.join("x")).unwrap();
        replay(&f.runs, f.base(), None).unwrap();
        assert_untouched(&victim);
        // Left for a human: the journal could not be replayed.
        assert!(f.runs.join("x.journal").exists());
    }

    #[test]
    fn replay_refuses_a_journaled_root_outside_the_state_directory() {
        let f = fixture();
        let victim = victim();
        let text = format!(
            "root\t{}\nstub\tf\t{}\t1.0\t2.0\n",
            hex(victim.path().as_os_str().as_bytes()),
            hex(b"/hosts"),
        );
        fs::write(f.runs.join("x.journal"), text.as_bytes()).unwrap();
        fs::write(victim.path().join("empty"), b"").unwrap();
        let text = format!(
            "root\t{}\nstub\tf\t{}\t1.0\t2.0\n",
            hex(victim.path().as_os_str().as_bytes()),
            hex(b"/empty"),
        );
        fs::write(f.runs.join("y.journal"), text.as_bytes()).unwrap();
        // Also through a symlink inside the state directory.
        std::os::unix::fs::symlink(victim.path(), f.base().join("link")).unwrap();
        let text = format!(
            "root\t{}\nstub\tf\t{}\t1.0\t2.0\n",
            hex(f.base().join("link").as_os_str().as_bytes()),
            hex(b"/empty"),
        );
        fs::write(f.runs.join("z.journal"), text.as_bytes()).unwrap();
        assert_eq!(
            replay(&f.runs, f.base(), None).unwrap(),
            Replayed::default()
        );
        assert_untouched(&victim);
        assert!(victim.path().join("empty").exists());
        for j in ["x", "y", "z"] {
            assert!(f.runs.join(format!("{j}.journal")).exists(), "{j}");
        }
    }

    #[test]
    fn replay_does_not_follow_a_journal_symlink() {
        let f = fixture();
        let victim = victim();
        let target = victim.path().join("hosts");
        std::os::unix::fs::symlink(&target, f.runs.join("x.journal")).unwrap();
        std::os::unix::fs::symlink(&target, f.runs.join(format!("y.{NEW_EXT}"))).unwrap();
        replay(&f.runs, f.base(), None).unwrap();
        assert_untouched(&victim);
    }

    /// Makes a FIFO at `p`, in this process.
    fn mkfifo(p: &Path) {
        use rustix::fs::{CWD, FileType, Mode, mknodat};
        mknodat(CWD, p, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
    }

    /// [`replay`] on a thread: its result if it returns within 10 s, `None`
    /// if it blocked.
    fn replay_within(f: &Fixture) -> Option<Replayed> {
        let (runs, base) = (f.runs.clone(), f.base().to_owned());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(replay(&runs, &base, None).unwrap());
        });
        rx.recv_timeout(std::time::Duration::from_secs(10)).ok()
    }

    fn is_fifo(p: &Path) -> bool {
        use std::os::unix::fs::FileTypeExt as _;
        fs::symlink_metadata(p).unwrap().file_type().is_fifo()
    }

    #[test]
    fn a_fifo_planted_as_an_unpublished_journal_is_never_opened() {
        let f = fixture();
        let fifo = f.runs.join(format!("x.{NEW_EXT}"));
        mkfifo(&fifo);
        let done = replay_within(&f);
        if done.is_none() {
            // Lets the open blocked on the FIFO go before failing.
            drop(rustix::fs::open(
                &fifo,
                rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            ));
        }
        assert_eq!(done, Some(Replayed::default()), "blocked on a FIFO");
        // Not ranikust's: left in place, with a warning.
        assert!(is_fifo(&fifo));
    }

    #[test]
    fn a_fifo_planted_as_a_journal_is_never_opened() {
        let f = fixture();
        let fifo = f.runs.join("x.journal");
        mkfifo(&fifo);
        // Opened read-write, the FIFO would have its only writer in the
        // replay itself, and the read would never end.
        assert_eq!(
            replay_within(&f),
            Some(Replayed::default()),
            "blocked on a FIFO"
        );
        assert!(is_fifo(&fifo));
    }

    #[test]
    fn a_journal_larger_than_any_run_writes_is_left_unread() {
        let f = fixture();
        let big = f.runs.join("x.journal");
        File::create(&big)
            .unwrap()
            .set_len(MAX_JOURNAL_BYTES + 1)
            .unwrap();
        assert_eq!(
            replay(&f.runs, f.base(), None).unwrap(),
            Replayed::default()
        );
        assert!(big.exists());
    }

    #[test]
    fn a_journal_stops_growing_at_the_replay_bound() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        let line = "x".repeat(usize::try_from(MAX_JOURNAL_BYTES).unwrap());
        let err = j.append(&line).unwrap_err().to_string();
        assert!(err.contains("too large"), "{err}");
        let len = fs::metadata(f.runs.join("1.journal")).unwrap().len();
        assert!(len < MAX_JOURNAL_BYTES, "{len}");
        j.finish().unwrap();
    }

    /// A file over the bound is refused on its pinned descriptor, before
    /// anything opens it for I/O or locks it.
    #[test]
    fn a_journal_over_the_bound_is_refused_before_it_is_opened() {
        let f = fixture();
        let big = f.runs.join("x.journal");
        File::create(&big)
            .unwrap()
            .set_len(MAX_JOURNAL_BYTES + 1)
            .unwrap();
        for write in [false, true] {
            let err = open_journal(&big, write).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::FileTooLarge, "{err}");
        }
        File::create(&big)
            .unwrap()
            .set_len(MAX_JOURNAL_BYTES)
            .unwrap();
        open_journal(&big, true).unwrap();
    }

    /// A journal within the bound whose `placed` lines match no record:
    /// parsing takes time in proportion to its lines, never to their square.
    #[test]
    fn a_planted_journal_within_the_bound_parses_in_linear_time() {
        use std::io::Write as _;
        let f = fixture();
        let mut text = Vec::new();
        while text.len() < 7_900_000 {
            text.extend_from_slice(b"stub\td\t2f\t0.0\t0.0\n");
        }
        while text.len() < 16_000_000 {
            text.extend_from_slice(b"placed\t2f61\t0.0\n");
        }
        assert!(u64::try_from(text.len()).unwrap() <= MAX_JOURNAL_BYTES);
        File::create(f.runs.join("x.journal"))
            .unwrap()
            .write_all(&text)
            .unwrap();
        assert_eq!(
            replay_within(&f),
            Some(Replayed {
                journals: 1,
                stubs: 0
            }),
            "parsing took longer than 10 s"
        );
    }

    #[test]
    fn a_placed_record_completes_only_the_newest_entry() {
        let text = format!(
            "stub\td\t{a}\t1.0\t2.0\nplaced\t{a}\t3.0\nstub\td\t{b}\t1.0\t2.0\nplaced\t{a}\t4.0\n",
            a = hex(b"/a"),
            b = hex(b"/b"),
        );
        let (_, stubs) = parse(text.as_bytes());
        let placed: Vec<_> = stubs.iter().map(|s| s.placed_mtime).collect();
        assert_eq!(placed, [Some(FileTime::from_secs(3)), None]);
    }

    /// A device node at a journal's name is never opened: as root, opening
    /// one acts on the host's device. `c 1 3` is `/dev/null`.
    #[test]
    #[ignore = "T2: mknod of a device node as real root; run in the test VM only"]
    fn a_device_node_planted_as_a_journal_is_never_opened() {
        use rustix::fs::{CWD, FileType, Mode, OFlags, makedev, mknodat};
        let f = fixture();
        let nodes = [
            f.runs.join("d.journal"),
            f.runs.join(format!("e.{NEW_EXT}")),
        ];
        for node in &nodes {
            mknodat(
                CWD,
                node,
                FileType::CharacterDevice,
                Mode::from_raw_mode(0o600),
                makedev(1, 3),
            )
            .unwrap();
            // Control: this filesystem lets a device node be opened, so the
            // replay leaving it unopened is a result, not a mount option.
            rustix::fs::open(node, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty())
                .expect("the test directory must allow device nodes (not nodev)");
        }
        assert_eq!(
            replay(&f.runs, f.base(), None).unwrap(),
            Replayed::default()
        );
        for node in &nodes {
            use std::os::unix::fs::FileTypeExt as _;
            let ft = fs::symlink_metadata(node).unwrap().file_type();
            assert!(ft.is_char_device(), "{}", node.display());
        }
    }

    fn set_old_times(root: &RootPath, p: &str) -> FileTime {
        let t = FileTime::from_secs(1_000_000);
        root.set_meta(
            p,
            &Meta {
                atime: Some(t),
                mtime: Some(t),
                ..Meta::default()
            },
        )
        .unwrap();
        t
    }

    #[test]
    fn stubs_are_journaled_removed_and_times_restored() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let old = set_old_times(&root, "/etc");
        let root_old = set_old_times(&root, "/");
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        assert_eq!(
            j.ensure(Path::new("/etc/resolv.conf"), MountKind::File)
                .unwrap(),
            1
        );
        assert_eq!(
            j.ensure(Path::new("/etc/hosts"), MountKind::File).unwrap(),
            1
        );
        assert_eq!(
            j.ensure(Path::new("/proc"), MountKind::Directory).unwrap(),
            1
        );
        assert_eq!(
            j.ensure(Path::new("/var/cache/apt"), MountKind::Directory)
                .unwrap(),
            3
        );
        // Already there: nothing to place.
        assert_eq!(
            j.ensure(Path::new("/etc/hosts"), MountKind::File).unwrap(),
            0
        );
        j.commit().unwrap();
        assert_eq!(j.stub_count(), 6);
        assert!(j.scratch().is_dir());
        let meta = fs::metadata(f.root_dir.join("var/cache")).unwrap();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
            0o755
        );

        let text = fs::read(f.runs.join("1.journal")).unwrap();
        let (root_dir, stubs) = parse(&text);
        assert_eq!(root_dir.as_deref(), Some(f.root_dir.as_path()));
        assert_eq!(stubs.len(), 6);
        assert!(stubs.iter().all(|s| s.placed_mtime.is_some()));
        assert_eq!(stubs[0].path, Path::new("/etc/resolv.conf"));
        assert_eq!(stubs[0].parent_mtime, old);

        assert_eq!(j.finish().unwrap(), 6);
        let root = RootPath::open(&f.root_dir).unwrap();
        assert!(root.statx("/etc/hosts").is_err());
        assert!(root.statx("/var").is_err());
        assert_eq!(root.statx("/etc").unwrap().mtime, old);
        assert_eq!(root.statx("/").unwrap().mtime, root_old);
        assert_eq!(fs::read_dir(&f.runs).unwrap().count(), 0);
    }

    #[test]
    fn dangling_symlinks_are_followed_inside_the_root() {
        let f = fixture();
        std::os::unix::fs::symlink(
            "/run/systemd/resolve/stub-resolv.conf",
            f.root_dir.join("etc/resolv.conf"),
        )
        .unwrap();
        // A relative link climbing above the root is clamped to it.
        std::os::unix::fs::symlink("../../../../../hosts.real", f.root_dir.join("etc/hosts"))
            .unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        assert_eq!(
            j.ensure(Path::new("/etc/resolv.conf"), MountKind::File)
                .unwrap(),
            4
        );
        assert_eq!(
            j.ensure(Path::new("/etc/hosts"), MountKind::File).unwrap(),
            1
        );
        assert!(
            f.root_dir
                .join("run/systemd/resolve/stub-resolv.conf")
                .is_file()
        );
        assert!(f.root_dir.join("hosts.real").is_file());
        assert!(j.finish().is_ok());
        assert!(!f.root_dir.join("run").exists());
        assert!(!f.root_dir.join("hosts.real").exists());
        assert!(f.root_dir.join("etc/resolv.conf").is_symlink());
    }

    #[test]
    fn symlink_loops_and_files_in_the_way_are_refused() {
        let f = fixture();
        std::os::unix::fs::symlink("b", f.root_dir.join("a")).unwrap();
        std::os::unix::fs::symlink("a", f.root_dir.join("b")).unwrap();
        fs::write(f.root_dir.join("file"), b"x").unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        assert!(j.ensure(Path::new("/a"), MountKind::File).is_err());
        assert!(j.ensure(Path::new("/file/x"), MountKind::File).is_err());
        assert_eq!(j.stub_count(), 0);
    }

    fn mknod(p: &Path, ft: rustix::fs::FileType, dev: u64) {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            p,
            ft,
            rustix::fs::Mode::from_raw_mode(0o600),
            dev,
        )
        .unwrap();
    }

    /// Runs `check` on its own thread, failing after 30 s: an `open` of a
    /// planted FIFO blocks forever.
    fn bounded<T: Send + 'static>(check: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, wait) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = done.send(check());
        });
        match wait.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(out) => out,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("blocked: a planted node was opened")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match thread.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => unreachable!("the thread sends before it ends"),
            },
        }
    }

    /// A non-directory `ft`, planted at `/etc/node` by `plant`, met by
    /// `ensure` directly and through symlinks: `ENOTDIR` where a directory
    /// is needed, the target itself as the last component, never opened,
    /// replaced or undone.
    fn ensure_around(ft: FileType, plant: impl Fn(&Path)) {
        use std::os::unix::fs::MetadataExt as _;
        let f = fixture();
        let node = f.root_dir.join("etc/node");
        plant(&node);
        std::os::unix::fs::symlink("node", f.root_dir.join("etc/rel")).unwrap();
        std::os::unix::fs::symlink("/etc/node", f.root_dir.join("abs")).unwrap();
        let ino = fs::symlink_metadata(&node).unwrap().ino();
        let root = RootPath::open(&f.root_dir).unwrap();
        assert_eq!(root.statx("/etc/node").unwrap().file_type(), ft);
        let j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        let j = bounded(move || {
            let mut j = j;
            for kind in [MountKind::File, MountKind::Directory] {
                for target in [
                    "/etc/node/x",
                    "/etc/node/x/y",
                    "/etc/node/../x",
                    "/etc/rel/x",
                    "/abs/x",
                ] {
                    match j.ensure(Path::new(target), kind) {
                        Err(Error::Io { source, .. })
                            if source.raw_os_error() == Some(libc::ENOTDIR) => {}
                        other => panic!("{ft:?} in the middle of {target}: {other:?}"),
                    }
                }
                for target in ["/etc/node", "/etc/rel", "/abs"] {
                    match j.ensure(Path::new(target), kind) {
                        Ok(0) => {}
                        other => panic!("{ft:?} as the target {target}: {other:?}"),
                    }
                }
            }
            j
        });
        assert_eq!(j.stub_count(), 0, "{ft:?}");
        let (_, stubs) = parse(&fs::read(f.runs.join("1.journal")).unwrap());
        assert!(stubs.is_empty(), "{ft:?}: {stubs:?}");
        assert_eq!(j.finish().unwrap(), 0, "{ft:?}");
        let root = RootPath::open(&f.root_dir).unwrap();
        assert_eq!(root.statx("/etc/node").unwrap().file_type(), ft);
        assert_eq!(fs::symlink_metadata(&node).unwrap().ino(), ino, "{ft:?}");
        let mut names: Vec<_> = fs::read_dir(f.root_dir.join("etc"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["node", "rel"], "{ft:?}");
        assert_eq!(fs::read_dir(&f.root_dir).unwrap().count(), 2, "{ft:?}");
    }

    /// Red team, T10/T1: a node where `ensure` needs a directory, or as its target.
    #[test]
    fn red_team_a_node_in_a_mount_target_is_enotdir_or_the_target_itself() {
        use rustix::fs::FileType as Raw;
        ensure_around(FileType::Regular, |p| fs::write(p, b"image").unwrap());
        ensure_around(FileType::Fifo, |p| mknod(p, Raw::Fifo, 0));
        ensure_around(FileType::Socket, |p| {
            drop(std::os::unix::net::UnixListener::bind(p).unwrap());
        });
        // 0/0, an overlay whiteout: the one device node a normal user may make.
        ensure_around(FileType::CharDevice, |p| {
            mknod(p, Raw::CharacterDevice, rustix::fs::makedev(0, 0));
        });
    }

    /// Red team, T10/T1: a device node where `ensure` needs a directory, or as its target.
    #[test]
    #[ignore = "T2c: default: mknod of a block and a real character device needs CAP_MKNOD"]
    fn red_team_a_device_node_in_a_mount_target_is_enotdir_or_the_target_itself() {
        use rustix::fs::FileType as Raw;
        ensure_around(FileType::BlockDevice, |p| {
            mknod(p, Raw::BlockDevice, rustix::fs::makedev(7, 0));
        });
        ensure_around(FileType::CharDevice, |p| {
            mknod(p, Raw::CharacterDevice, rustix::fs::makedev(1, 3));
        });
    }

    /// Enough `../` to climb from any test root to the host's `/`.
    fn climb() -> PathBuf {
        PathBuf::from("../".repeat(32))
    }

    /// Red team, T10/T3: a `RUN` swaps its stubs for symlinks out of the root and for nodes.
    #[test]
    fn red_team_stubs_swapped_for_links_out_or_nodes_are_left_with_the_host() {
        use rustix::fs::FileType as Raw;
        let f = fixture();
        let host = tempfile::tempdir().unwrap();
        let h = host.path().canonicalize().unwrap();
        fs::create_dir(h.join("d")).unwrap();
        fs::create_dir(h.join("apt")).unwrap();
        fs::write(h.join("f"), b"").unwrap();
        let host_root = RootPath::open(&h).unwrap();
        let old = set_old_times(&host_root, "/");
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        for (target, kind) in [
            ("/data", MountKind::Directory),
            ("/empty", MountKind::File),
            ("/var/cache/apt", MountKind::Directory),
            ("/fifo", MountKind::File),
            ("/sock", MountKind::Directory),
            ("/wh", MountKind::File),
        ] {
            j.ensure(Path::new(target), kind).unwrap();
        }
        j.commit().unwrap();
        assert_eq!(j.stub_count(), 8);

        let r = &f.root_dir;
        let rel = h.strip_prefix("/").unwrap();
        fs::remove_dir(r.join("data")).unwrap();
        std::os::unix::fs::symlink(climb().join(rel).join("d"), r.join("data")).unwrap();
        fs::remove_file(r.join("empty")).unwrap();
        std::os::unix::fs::symlink(h.join("f"), r.join("empty")).unwrap();
        fs::remove_dir(r.join("var/cache/apt")).unwrap();
        fs::remove_dir(r.join("var/cache")).unwrap();
        std::os::unix::fs::symlink(&h, r.join("var/cache")).unwrap();
        fs::remove_file(r.join("fifo")).unwrap();
        mknod(&r.join("fifo"), Raw::Fifo, 0);
        fs::remove_dir(r.join("sock")).unwrap();
        drop(std::os::unix::net::UnixListener::bind(r.join("sock")).unwrap());
        fs::remove_file(r.join("wh")).unwrap();
        mknod(
            &r.join("wh"),
            Raw::CharacterDevice,
            rustix::fs::makedev(0, 0),
        );

        assert_eq!(bounded(move || j.finish().unwrap()), 0);
        assert!(h.join("d").is_dir());
        assert!(h.join("apt").is_dir());
        assert!(h.join("f").is_file());
        assert_eq!(host_root.statx("/").unwrap().mtime, old);
        let root = RootPath::open(&f.root_dir).unwrap();
        for (path, ft) in [
            ("/data", FileType::Symlink),
            ("/empty", FileType::Symlink),
            ("/var/cache", FileType::Symlink),
            ("/fifo", FileType::Fifo),
            ("/sock", FileType::Socket),
            ("/wh", FileType::CharDevice),
        ] {
            assert_eq!(root.statx(path).unwrap().file_type(), ft, "{path}");
        }
    }

    /// Red team, T10/T1: planted stub records climb with `..`, name host paths or pass a symlink out.
    #[test]
    fn red_team_planted_stub_records_stay_in_the_root() {
        let f = fixture();
        let host = tempfile::tempdir().unwrap();
        let h = host.path().canonicalize().unwrap();
        fs::create_dir(h.join("d")).unwrap();
        fs::write(h.join("f"), b"").unwrap();
        let host_root = RootPath::open(&h).unwrap();
        let old = set_old_times(&host_root, "/");
        let rel = h.strip_prefix("/").unwrap();
        std::os::unix::fs::symlink(climb().join(rel), f.root_dir.join("out")).unwrap();
        std::os::unix::fs::symlink(&h, f.root_dir.join("abs")).unwrap();
        let up = Path::new("/").join(climb()).join(rel);
        let mut text = format!("root\t{}\n", hex(f.root_dir.as_os_str().as_bytes()));
        for dir in [h.clone(), up, PathBuf::from("/out"), PathBuf::from("/abs")] {
            for (code, name) in [("d", "d"), ("f", "f")] {
                let p = dir.join(name);
                text.push_str(&format!(
                    "stub\t{code}\t{}\t1.0\t2.0\nplaced\t{}\t3.0\n",
                    hex(p.as_os_str().as_bytes()),
                    hex(p.as_os_str().as_bytes()),
                ));
            }
        }
        // A torn last record.
        text.push_str(&format!(
            "stub\td\t{}\t1.",
            hex(h.join("d").as_os_str().as_bytes())
        ));
        fs::write(f.runs.join("x.journal"), text.as_bytes()).unwrap();
        assert_eq!(parse(text.as_bytes()).1.len(), 8);
        assert_eq!(
            replay(&f.runs, f.base(), None).unwrap(),
            Replayed {
                journals: 1,
                stubs: 0
            }
        );
        assert!(h.join("d").is_dir());
        assert!(h.join("f").is_file());
        assert_eq!(host_root.statx("/").unwrap().mtime, old);
        assert!(!f.runs.join("x.journal").exists());
    }

    #[test]
    fn written_stubs_and_runs_files_are_kept() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        j.ensure(Path::new("/data/x"), MountKind::File).unwrap();
        // The `RUN` wrote into the stub directory: it is content now.
        fs::write(f.root_dir.join("data/new"), b"y").unwrap();
        fs::write(f.root_dir.join("data/x"), b"z").unwrap();
        assert_eq!(j.finish().unwrap(), 0);
        assert!(f.root_dir.join("data/new").is_file());
        assert!(f.root_dir.join("data/x").is_file());
    }

    /// A crash of the journal's owner: the journal is never finished and its
    /// lock goes away. Unlocked explicitly, not only closed: a `flock`
    /// belongs to the open file description, and a child another test
    /// clones meanwhile shares it until it executes or exits, so closing
    /// this descriptor alone may leave the lock held (a dead process's lock
    /// is gone with every copy).
    fn crash(journal: Journal) {
        let Journal { file, .. } = journal;
        rustix::fs::flock(&file, FlockOperation::Unlock).unwrap();
        drop(file);
    }

    /// Why [`crash`] unlocks: a copy of the description, as a cloned child
    /// holds, keeps the lock after the owner's descriptor is closed, and the
    /// journal counts as live; `LOCK_UN` releases it for every copy.
    #[test]
    fn a_shared_description_keeps_the_lock_until_unlocked() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut dead = Journal::create(&f.runs, "dead", &f.root_dir, root).unwrap();
        dead.ensure(Path::new("/etc/hostname"), MountKind::File)
            .unwrap();
        let Journal { file, .. } = dead;
        let copy = file.try_clone().unwrap();
        drop(file);
        assert_eq!(
            replay(&f.runs, f.base(), None).unwrap(),
            Replayed::default()
        );
        assert!(f.root_dir.join("etc/hostname").exists());
        rustix::fs::flock(&copy, FlockOperation::Unlock).unwrap();
        assert_eq!(replay(&f.runs, f.base(), None).unwrap().journals, 1);
        assert!(!f.root_dir.join("etc/hostname").exists());
    }

    #[test]
    fn replay_removes_stubs_of_dead_runs_only() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let old = set_old_times(&root, "/etc");
        let mut dead = Journal::create(&f.runs, "dead", &f.root_dir, root).unwrap();
        dead.ensure(Path::new("/etc/hostname"), MountKind::File)
            .unwrap();
        fs::write(dead.scratch().join("hosts"), b"h").unwrap();
        crash(dead);

        let root = RootPath::open(&f.root_dir).unwrap();
        let mut live = Journal::create(&f.runs, "live", &f.root_dir, root).unwrap();
        live.ensure(Path::new("/srv"), MountKind::Directory)
            .unwrap();

        let done = replay(&f.runs, f.base(), None).unwrap();
        assert_eq!(
            done,
            Replayed {
                journals: 1,
                stubs: 1
            }
        );
        assert!(!f.root_dir.join("etc/hostname").exists());
        let root = RootPath::open(&f.root_dir).unwrap();
        assert_eq!(root.statx("/etc").unwrap().mtime, old);
        assert!(!f.runs.join("dead.journal").exists());
        assert!(!f.runs.join("dead").exists());
        // The live run's journal and stub are untouched.
        assert!(f.root_dir.join("srv").is_dir());
        assert!(f.runs.join("live.journal").exists());
        assert_eq!(live.finish().unwrap(), 1);
    }

    #[test]
    fn unpublished_journals_are_removed_unless_locked() {
        let f = fixture();
        let dead = f.runs.join(format!("dead.{NEW_EXT}"));
        fs::write(&dead, b"").unwrap();
        let live = f.runs.join(format!("live.{NEW_EXT}"));
        let held = File::create(&live).unwrap();
        rustix::fs::flock(&held, FlockOperation::LockExclusive).unwrap();
        assert_eq!(
            replay(&f.runs, f.base(), None).unwrap(),
            Replayed::default()
        );
        assert!(!dead.exists());
        assert!(live.exists());
        // Published names never collide.
        let root = RootPath::open(&f.root_dir).unwrap();
        let j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        assert!(Journal::create(&f.runs, "1", &f.root_dir, root).is_err());
        assert!(f.runs.join("1.journal").exists());
        assert!(!f.runs.join(format!("1.{NEW_EXT}")).exists());
        j.finish().unwrap();
    }

    #[test]
    fn parse_skips_torn_lines_and_unknown_records() {
        let text = format!(
            "root\t{}\nstub\tf\t{}\t1.000000002\t3.000000004\nplaced\t{}\t5.000000006\nstub\td\t2f78\t1.0\nbogus\n",
            hex(b"/r"),
            hex(b"/etc/hosts"),
            hex(b"/etc/hosts"),
        );
        let (root, stubs) = parse(text.as_bytes());
        assert_eq!(root.as_deref(), Some(Path::new("/r")));
        assert_eq!(
            stubs,
            [StubRecord {
                kind: Placed::Stub(MountKind::File),
                path: "/etc/hosts".into(),
                parent_atime: FileTime { secs: 1, nanos: 2 },
                parent_mtime: FileTime { secs: 3, nanos: 4 },
                placed_mtime: Some(FileTime { secs: 5, nanos: 6 }),
            }]
        );
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("2f"), Some(b"/".to_vec()));
        assert_eq!(parse_time("-1.5"), Some(FileTime { secs: -1, nanos: 5 }));
        assert_eq!(parse_time("1.1000000000"), None);
    }

    /// Names of the entries of `dir` that ranikust renamed aside.
    fn asides(dir: &Path) -> Vec<OsString> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.as_bytes().starts_with(ASIDE_PREFIX.as_bytes()))
            .collect()
    }

    #[test]
    fn swapped_files_are_put_back_with_their_inode_and_parent_times() {
        use std::os::unix::fs::MetadataExt as _;
        let f = fixture();
        fs::write(f.root_dir.join("etc/hosts"), b"image").unwrap();
        std::os::unix::fs::symlink("../run/resolv.conf", f.root_dir.join("etc/resolv.conf"))
            .unwrap();
        let ino = fs::metadata(f.root_dir.join("etc/hosts")).unwrap().ino();
        let root = RootPath::open(&f.root_dir).unwrap();
        let old = set_old_times(&root, "/etc");
        let mut j = Journal::create(&f.runs, "7", &f.root_dir, root).unwrap();
        for (path, data) in [
            ("/etc/hosts", b"ours-h".as_slice()),
            ("/etc/resolv.conf", b"ours-r"),
            ("/etc/hostname", b"ours-n"),
        ] {
            assert!(j.swap_in(Path::new(path), Content::File(data)).unwrap());
        }
        j.commit().unwrap();
        // What the `RUN` sees: ranikust's files, regular, 0644.
        assert_eq!(fs::read(f.root_dir.join("etc/hosts")).unwrap(), b"ours-h");
        let resolv = fs::symlink_metadata(f.root_dir.join("etc/resolv.conf")).unwrap();
        assert!(resolv.is_file());
        assert_eq!(resolv.mode() & 0o777, 0o644);
        assert_eq!(asides(&f.root_dir.join("etc")).len(), 2);
        // The `RUN` rewrites a placed file; it is ranikust's, and goes.
        fs::write(f.root_dir.join("etc/hostname"), b"changed").unwrap();

        assert_eq!(j.finish().unwrap(), 3);
        let hosts = fs::symlink_metadata(f.root_dir.join("etc/hosts")).unwrap();
        assert_eq!(hosts.ino(), ino);
        assert_eq!(fs::read(f.root_dir.join("etc/hosts")).unwrap(), b"image");
        assert_eq!(
            fs::read_link(f.root_dir.join("etc/resolv.conf")).unwrap(),
            Path::new("../run/resolv.conf")
        );
        assert!(!f.root_dir.join("etc/hostname").exists());
        assert!(asides(&f.root_dir.join("etc")).is_empty());
        let root = RootPath::open(&f.root_dir).unwrap();
        assert_eq!(root.statx("/etc").unwrap().mtime, old);
        assert_eq!(fs::read_dir(&f.runs).unwrap().count(), 0);
    }

    #[test]
    fn a_parent_the_run_changed_keeps_its_new_times() {
        let f = fixture();
        let root = RootPath::open(&f.root_dir).unwrap();
        let old = set_old_times(&root, "/etc");
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        j.swap_in(Path::new("/etc/hosts"), Content::File(b"h"))
            .unwrap();
        j.commit().unwrap();
        fs::write(f.root_dir.join("etc/new"), b"x").unwrap();
        j.finish().unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        assert_ne!(root.statx("/etc").unwrap().mtime, old);
        assert!(f.root_dir.join("etc/new").exists());
    }

    #[test]
    fn only_placeable_paths_under_a_real_directory_are_swapped() {
        let f = fixture();
        fs::write(f.root_dir.join("passwd"), b"p").unwrap();
        fs::create_dir(f.root_dir.join("real")).unwrap();
        std::os::unix::fs::symlink("real", f.root_dir.join("dev")).unwrap();
        fs::create_dir(f.root_dir.join("etc/hostname")).unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        for path in ["/passwd", "/etc/passwd", "/etc/../passwd"] {
            assert!(
                j.swap_in(Path::new(path), Content::File(b"x")).is_err(),
                "{path}"
            );
        }
        // A symlinked `/dev` and a directory in the way are refused.
        let null = Content::CharDev { major: 1, minor: 3 };
        assert!(j.swap_in(Path::new("/dev/null"), null).is_err());
        assert!(
            j.swap_in(Path::new("/etc/hostname"), Content::File(b"x"))
                .is_err()
        );
        assert_eq!(j.stub_count(), 0);
        j.finish().unwrap();
        assert_eq!(fs::read(f.root_dir.join("passwd")).unwrap(), b"p");
        assert_eq!(fs::read_dir(f.root_dir.join("real")).unwrap().count(), 0);
    }

    /// As a normal user `mknod` is refused: the original entry is back at
    /// once and the `RUN` goes without the node. As root (the VM) the node
    /// is made and removed after.
    #[test]
    fn a_refused_device_node_is_undone_at_once() {
        use std::os::unix::fs::FileTypeExt as _;
        let f = fixture();
        fs::create_dir(f.root_dir.join("dev")).unwrap();
        fs::write(f.root_dir.join("dev/null"), b"image").unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        let null = Content::CharDev { major: 1, minor: 3 };
        let made = j.swap_in(Path::new("/dev/null"), null).unwrap();
        let now = fs::symlink_metadata(f.root_dir.join("dev/null")).unwrap();
        if made {
            assert!(now.file_type().is_char_device());
            assert_eq!(j.stub_count(), 1);
        } else {
            assert!(now.is_file());
            assert_eq!(j.stub_count(), 0);
        }
        j.finish().unwrap();
        assert_eq!(fs::read(f.root_dir.join("dev/null")).unwrap(), b"image");
        assert!(asides(&f.root_dir.join("dev")).is_empty());
    }

    #[test]
    fn replay_puts_swapped_files_back_on_the_shared_root_only() {
        let f = fixture();
        // The kaniko-mode workdir: outside the state directory.
        let workdir = tempfile::tempdir().unwrap();
        let wd = workdir.path().canonicalize().unwrap();
        fs::create_dir(wd.join("etc")).unwrap();
        fs::write(wd.join("etc/hosts"), b"image").unwrap();
        let root = RootPath::open(&wd).unwrap();
        let old = set_old_times(&root, "/etc");
        let mut dead = Journal::create(&f.runs, "dead", &wd, root).unwrap();
        dead.swap_in(Path::new("/etc/hosts"), Content::File(b"ours"))
            .unwrap();
        dead.swap_in(Path::new("/etc/resolv.conf"), Content::File(b"r"))
            .unwrap();
        dead.commit().unwrap();
        crash(dead);

        // Without the shared root, it stays for a start that has it.
        assert_eq!(
            replay(&f.runs, f.base(), None).unwrap(),
            Replayed::default()
        );
        assert_eq!(fs::read(wd.join("etc/hosts")).unwrap(), b"ours");
        // Another directory is not the journal's root, whatever its name.
        let other = tempfile::tempdir().unwrap();
        let other = RootPath::open(other.path()).unwrap();
        replay(&f.runs, f.base(), Some(&other)).unwrap();
        assert!(f.runs.join("dead.journal").exists());
        // `/` is refused before any replay, by identity.
        let host = RootPath::open("/").unwrap();
        assert!(refuse_host_system_dir(&host, Path::new("/")).is_err());
        let etc = RootPath::open("/etc").unwrap();
        assert!(refuse_host_system_dir(&etc, Path::new("/etc")).is_err());

        let shared = RootPath::open(&wd).unwrap().stay_on_mount();
        assert!(refuse_host_system_dir(&shared, &wd).is_ok());
        let done = replay(&f.runs, f.base(), Some(&shared)).unwrap();
        assert_eq!(
            done,
            Replayed {
                journals: 1,
                stubs: 2
            }
        );
        assert_eq!(fs::read(wd.join("etc/hosts")).unwrap(), b"image");
        assert!(!wd.join("etc/resolv.conf").exists());
        assert!(asides(&wd.join("etc")).is_empty());
        let root = RootPath::open(&wd).unwrap();
        assert_eq!(root.statx("/etc").unwrap().mtime, old);
        assert!(!f.runs.join("dead.journal").exists());
    }

    #[test]
    fn planted_swap_records_cannot_name_other_paths() {
        let f = fixture();
        fs::write(f.root_dir.join("etc/passwd"), b"p").unwrap();
        fs::write(f.root_dir.join("etc/shadow"), b"s").unwrap();
        let text = format!(
            "root\t{}\nswap\t{}\t-\t1.0\t2.0\nswap\t{}\t{}\t1.0\t2.0\nswap\t{}\t{}\t1.0\t2.0\n",
            hex(f.root_dir.as_os_str().as_bytes()),
            hex(b"/etc/passwd"),
            hex(b"/etc/hosts"),
            hex(b"/etc/shadow"),
            hex(b"/etc/hostname"),
            hex(b"/dev/.ranikust-aside-x-hostname"),
        );
        let (_, stubs) = parse(text.as_bytes());
        assert!(stubs.is_empty(), "{stubs:?}");
        fs::write(f.runs.join("x.journal"), text.as_bytes()).unwrap();
        replay(&f.runs, f.base(), None).unwrap();
        assert_eq!(fs::read(f.root_dir.join("etc/passwd")).unwrap(), b"p");
        assert_eq!(fs::read(f.root_dir.join("etc/shadow")).unwrap(), b"s");
        assert!(!f.root_dir.join("etc/hosts").exists());
    }

    /// On a shared root a replay undoes only what `place_direct` makes: a
    /// planted stub record elsewhere is skipped.
    #[test]
    fn replay_on_the_shared_root_skips_other_stub_records() {
        let f = fixture();
        let workdir = tempfile::tempdir().unwrap();
        let wd = workdir.path().canonicalize().unwrap();
        fs::create_dir(wd.join("keep")).unwrap();
        fs::write(wd.join("empty"), b"").unwrap();
        fs::create_dir(wd.join("etc")).unwrap();
        let text = format!(
            "root\t{}\nstub\td\t{}\t1.0\t2.0\nstub\tf\t{}\t1.0\t2.0\nstub\td\t{}\t1.0\t2.0\n",
            hex(wd.as_os_str().as_bytes()),
            hex(b"/keep"),
            hex(b"/empty"),
            hex(b"/etc"),
        );
        fs::write(f.runs.join("x.journal"), text.as_bytes()).unwrap();
        let shared = RootPath::open(&wd).unwrap().stay_on_mount();
        let done = replay(&f.runs, f.base(), Some(&shared)).unwrap();
        assert_eq!(done.journals, 1);
        assert!(wd.join("keep").is_dir());
        assert!(wd.join("empty").is_file());
        assert!(!wd.join("etc").exists(), "the empty /etc stub goes");
    }

    /// A swap whose rename aside fails leaves no entry behind.
    #[test]
    fn a_failed_rename_aside_is_forgotten() {
        let f = fixture();
        fs::write(f.root_dir.join("etc/hosts"), b"image").unwrap();
        let root = RootPath::open(&f.root_dir).unwrap();
        let mut j = Journal::create(&f.runs, "1", &f.root_dir, root).unwrap();
        // The aside name is taken: `RENAME_NOREPLACE` refuses.
        let aside = format!("etc/{ASIDE_PREFIX}1-hosts");
        fs::write(f.root_dir.join(&aside), b"taken").unwrap();
        assert!(
            j.swap_in(Path::new("/etc/hosts"), Content::File(b"x"))
                .is_err()
        );
        assert_eq!(j.stub_count(), 0);
        j.finish().unwrap();
        assert_eq!(fs::read(f.root_dir.join("etc/hosts")).unwrap(), b"image");
        assert_eq!(fs::read(f.root_dir.join(&aside)).unwrap(), b"taken");
    }
}
