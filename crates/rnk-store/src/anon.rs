//! Files without a name, for a store that keeps nothing a `RUN` can reach
//! (kaniko mode on `/`, ADR-0051), kept on disk rather than in memory
//! (Q-340, ADR-0056).
//!
//! There a `RUN` runs as root on ranikust's own `/`, in its PID namespace:
//! it can read any file that has a path. An anonymous file has none. Three
//! ways to make one, in order:
//!
//! 1. An `O_TMPFILE` inode in the state directory: never named.
//! 2. Where the filesystem refuses `O_TMPFILE` (the overlayfs root of a
//!    Docker container, kernels 6.8 and 5.15 [measured, VM `secrets`
//!    suite]): a file created by a random name with `O_EXCL`, opened, and
//!    unlinked at once, inside a [`Fence::named`] window, so the name never
//!    exists while a `RUN` process does. Its blocks stay on disk, in the
//!    page cache the kernel reclaims, never in ranikust's memory. Only where
//!    the unlink removes the name: behind the closed fence of kaniko mode
//!    on `/`, a filesystem that keeps one for an open file (NFS, FUSE, SMB,
//!    AFS, by `statfs`), a file still linked after its unlink, or an unlink
//!    that fails, is refused with [`Error::NameOutlivesUnlink`] before any
//!    `RUN` starts, never worked around.
//! 3. A `memfd`, with a warning, only when the state directory takes no
//!    file at all (read-only, or permission refused), when the fence was
//!    poisoned by a `RUN` that ended with an error, or when the store opened
//!    on traces of an earlier build cut short ([`AnonFiles::tainted`]): a
//!    `RUN` of that build may have left processes no sweep reached, which
//!    could see a name (ADR-0066). A full disk or quota is an error in
//!    every way, never memory.
//!
//! A crash between the create and the unlink leaves a name with the
//! [`PREFIX`]; the next store opened on the directory removes it
//! ([`remove_leftovers`]). ranikust keeps the files it makes open and names
//! them to its own code as `/proc/self/fd/<n>`, which only ranikust's
//! process resolves; ranikust is not dumpable, so `/proc/<ranikust>/fd` is
//! closed to a `RUN` without `CAP_SYS_PTRACE`.

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, PoisonError};

use rnk_rootfs::{FileType, RootPath};
use rustix::fs::{AtFlags, MemfdFlags, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{Error, Result};
use crate::fence::Fence;

/// The name every named-then-unlinked file and every short-lived unpacked
/// directory of an anonymous store starts with, in the state directory.
pub const PREFIX: &str = ".ranikust-anon-";

/// How files are made, from the first way that works on: an `O_TMPFILE`
/// refused once stays refused.
const TMPFILE: u8 = 0;
const NAMED: u8 = 1;
const MEMFD: u8 = 2;

/// Where anonymous files are made, and the ones kept for the store.
#[derive(Debug)]
pub(crate) struct AnonFiles {
    /// The state directory.
    dir: OwnedFd,
    dir_path: PathBuf,
    /// [`TMPFILE`], [`NAMED`] or [`MEMFD`].
    way: AtomicU8,
    /// No name for this build: why, for the warning.
    no_names: Mutex<Option<&'static str>>,
    /// Every file kept, open until the store goes: their numbers are the
    /// paths handed out.
    kept: Mutex<Vec<OwnedFd>>,
    fence: Fence,
    /// Tests only: a fault the named way meets ([`FAULT_LINK`],
    /// [`FAULT_UNLINK_ACCES`]), 0 for none.
    #[cfg(test)]
    fault: AtomicU8,
}

/// Tests only: each named file gets a second name, `kept`, before its
/// unlink, as a filesystem that keeps one would.
#[cfg(test)]
const FAULT_LINK: u8 = 1;
/// Tests only: the unlink fails with `EACCES`, leaving the name.
#[cfg(test)]
const FAULT_UNLINK_ACCES: u8 = 2;

impl AnonFiles {
    /// Anonymous files in the directory `dir` (opened by the caller,
    /// `O_PATH | O_DIRECTORY`), named `dir_path` in messages; a named file
    /// is made only inside a window of `fence`.
    pub(crate) fn new(dir: OwnedFd, dir_path: &Path, fence: Fence) -> Self {
        Self {
            dir,
            dir_path: dir_path.to_owned(),
            way: AtomicU8::new(TMPFILE),
            no_names: Mutex::new(None),
            kept: Mutex::new(Vec::new()),
            fence,
            #[cfg(test)]
            fault: AtomicU8::new(0),
        }
    }

    /// Never makes a file by name for this build: `why` is logged when the
    /// first file falls back to memory.
    pub(crate) fn tainted(&self, why: &'static str) {
        *self.no_names.lock().unwrap_or_else(PoisonError::into_inner) = Some(why);
    }

    /// Whether files are `memfd`s (memory) rather than files on disk.
    pub(crate) fn in_memory(&self) -> bool {
        self.way.load(Ordering::Relaxed) == MEMFD
    }

    /// A new, empty, anonymous file open for reading and writing, mode
    /// `0600`, close-on-exec. Not kept: [`AnonFiles::keep`] keeps it, or the
    /// caller owns it and it goes with its last descriptor.
    pub(crate) fn create(&self) -> Result<File> {
        if self.way.load(Ordering::Relaxed) == TMPFILE {
            // `O_EXCL`: the file can never be linked into the filesystem,
            // by ranikust or by anyone reaching its descriptor.
            match rustix::fs::openat(
                &self.dir,
                ".",
                OFlags::TMPFILE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(fd) => return Ok(File::from(fd)),
                // No `O_TMPFILE` on this filesystem (EOPNOTSUPP), or in this
                // kernel (EISDIR: the flag reads as `O_DIRECTORY`).
                Err(Errno::OPNOTSUPP | Errno::ISDIR | Errno::INVAL) => {
                    if self
                        .way
                        .compare_exchange(TMPFILE, NAMED, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                        && self.why_no_names().is_none()
                    {
                        tracing::info!(
                            target: "rnk_store",
                            "{}: no O_TMPFILE on this filesystem; state files are made by a \
                             name unlinked at once, while no RUN runs",
                            self.dir_path.display()
                        );
                    }
                }
                Err(e) if takes_no_file(e) => {
                    self.to_memory(&e.to_string());
                }
                // A full disk or quota, or anything else: an error.
                Err(e) => return Err(self.err(e)),
            }
        }
        if self.way.load(Ordering::Relaxed) == NAMED {
            if let Some(why) = self.why_no_names() {
                self.to_memory(why);
            } else if self.fence.is_poisoned() {
                self.to_memory("a RUN ended with an error and may have left processes");
            } else {
                match self.create_named() {
                    Ok(file) => return Ok(file),
                    Err(Error::AnonymousFile { source, .. })
                        if source
                            .raw_os_error()
                            .map(Errno::from_raw_os_error)
                            .is_some_and(takes_no_file) =>
                    {
                        self.to_memory(&source.to_string());
                    }
                    // The fence refused (a bug in the order of work), a
                    // full disk or quota, anything else: an error, never
                    // papered over with memory.
                    Err(e) => return Err(e),
                }
            }
        }
        self.create_memfd()
    }

    fn why_no_names(&self) -> Option<&'static str> {
        *self.no_names.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// From now on, files are `memfd`s; warns once, saying why.
    fn to_memory(&self, why: &str) {
        if self.way.swap(MEMFD, Ordering::Relaxed) != MEMFD {
            tracing::warn!(
                target: "rnk_store",
                "{}: keeping state in memory (memfd), counted against the container's memory \
                 limit: {why}",
                self.dir_path.display()
            );
        }
    }

    /// A file made by a random name and unlinked before the window closes.
    /// Behind a closed fence (kaniko mode on `/`, where a `RUN` could open a
    /// name) it must also have no name left: refused on a filesystem known
    /// to keep one ([`keeps_unlinked_names`]) before any name exists, and on
    /// any other by the file's link count after the unlink. A name left
    /// there, by a failed unlink too, is [`Error::NameOutlivesUnlink`],
    /// never a reason to go on in memory. A persistent store's files (an
    /// open fence) lie where no `RUN` reaches, and need neither check.
    fn create_named(&self) -> Result<File> {
        let fenced = self.fence.is_closed();
        if fenced {
            let st = rustix::fs::fstatfs(&self.dir).map_err(|e| self.err(e))?;
            #[allow(
                clippy::useless_conversion,
                reason = "f_type's integer type differs between targets"
            )]
            let f_type = i64::from(st.f_type);
            if let Some(fs) = keeps_unlinked_names(f_type) {
                return Err(self.name_kept(format!(
                    "it is on {fs}, which takes no O_TMPFILE and keeps a name for a file \
                     unlinked while open"
                )));
            }
        }
        // No randomness ([`Error::Random`]) fails the file, never memory,
        // even as `EPERM` (a seccomp filter): it says nothing about the
        // state directory.
        let name = random_name()?;
        let window = self.fence.named()?;
        let fd = rustix::fs::openat(
            &self.dir,
            name.as_str(),
            OFlags::CREATE
                | OFlags::EXCL
                | OFlags::NOFOLLOW
                | OFlags::RDWR
                | OFlags::CLOEXEC
                | OFlags::NOCTTY,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| self.err(e))?;
        #[cfg(test)]
        let fault = self.fault.load(Ordering::Relaxed);
        #[cfg(test)]
        if fault == FAULT_LINK
            && let Err(e) = rustix::fs::linkat(
                &self.dir,
                name.as_str(),
                &self.dir,
                "kept",
                AtFlags::empty(),
            )
        {
            // The random name stays: so does the window.
            std::mem::forget(window);
            return Err(self.err(e));
        }
        #[cfg(test)]
        let unlinked = if fault == FAULT_UNLINK_ACCES {
            Err(Errno::ACCESS)
        } else {
            rustix::fs::unlinkat(&self.dir, name.as_str(), AtFlags::empty())
        };
        #[cfg(not(test))]
        let unlinked = rustix::fs::unlinkat(&self.dir, name.as_str(), AtFlags::empty());
        if let Err(e) = unlinked {
            // The name stays: the window must too, so no RUN starts while
            // it exists (`enter_run` then fails the build). The next start
            // removes the name ([`remove_leftovers`]).
            std::mem::forget(window);
            return Err(if fenced {
                self.name_kept(format!(
                    "the name of a file made there cannot be removed: {e}"
                ))
            } else {
                self.err(e)
            });
        }
        // An unlink that returned 0 may still leave a name: a filesystem
        // that renames an open file away instead (NFS, FUSE). The window
        // stays then too, and the file is closed, which lets such a
        // filesystem drop the name.
        if fenced {
            let left = match rustix::fs::fstat(&fd) {
                Ok(st) if st.st_nlink == 0 => None,
                Ok(st) => Some(format!(
                    "a file unlinked there still has a name (link count {})",
                    st.st_nlink
                )),
                Err(e) => Some(format!(
                    "cannot check that a file unlinked there has no name left: {e}"
                )),
            };
            if let Some(why) = left {
                std::mem::forget(window);
                return Err(self.name_kept(why));
            }
        }
        drop(window);
        Ok(File::from(fd))
    }

    fn name_kept(&self, why: String) -> Error {
        Error::NameOutlivesUnlink {
            path: self.dir_path.clone(),
            why,
        }
    }

    fn create_memfd(&self) -> Result<File> {
        // Never executable (Linux 6.3); older kernels refuse the flag with
        // `EINVAL`, and get a plain one.
        match rustix::fs::memfd_create(
            c"ranikust-state",
            MemfdFlags::CLOEXEC | MemfdFlags::NOEXEC_SEAL,
        ) {
            Ok(fd) => Ok(File::from(fd)),
            Err(Errno::INVAL) => rustix::fs::memfd_create(c"ranikust-state", MemfdFlags::CLOEXEC)
                .map(File::from)
                .map_err(|e| self.err(e)),
            Err(e) => Err(self.err(e)),
        }
    }

    /// Keeps `file` open for the life of the store and returns its name for
    /// ranikust's own code: `/proc/self/fd/<n>`.
    pub(crate) fn keep(&self, file: File) -> PathBuf {
        let fd = OwnedFd::from(file);
        let path = fd_path(&fd);
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(fd);
        path
    }

    fn err(&self, e: Errno) -> Error {
        Error::AnonymousFile {
            path: self.dir_path.clone(),
            source: e.into(),
        }
    }

    #[cfg(test)]
    fn force(&self, way: u8) {
        self.way.store(way, Ordering::Relaxed);
    }
}

/// Whether `e` says the filesystem takes no file at all here (read-only, or
/// permission refused): state then goes to memory, while a full disk, a
/// quota or anything else stays an error.
fn takes_no_file(e: Errno) -> bool {
    matches!(e, Errno::ROFS | Errno::ACCESS | Errno::PERM)
}

/// The filesystem, named, when `f_type` is one that keeps a name for a file
/// unlinked while open: the NFS client renames it to `.nfs*` until the last
/// close (its "silly rename"), libfuse's high-level API without
/// `hard_remove` to `.fuse_hidden*`, AFS clients to `.__afs*`, and an SMB
/// client may keep it as delete-pending. The named way never runs there
/// behind a closed fence.
fn keeps_unlinked_names(f_type: i64) -> Option<&'static str> {
    const NFS: i64 = 0x6969;
    const FUSE: i64 = 0x6573_5546;
    const SMB: i64 = 0x517b;
    const SMB2: i64 = 0xfe53_4d42;
    const CIFS: i64 = 0xff53_4d42;
    /// kAFS (`AFS_FS_MAGIC`) and OpenAFS.
    const KAFS: i64 = 0x6b41_4653;
    const OPENAFS: i64 = 0x5346_414f;
    match f_type {
        NFS => Some("NFS"),
        FUSE => Some("FUSE"),
        SMB | SMB2 | CIFS => Some("SMB/CIFS"),
        KAFS | OPENAFS => Some("AFS"),
        _ => None,
    }
}

/// `/proc/self/fd/<n>` of `fd`: its name for this process alone.
pub(crate) fn fd_path(fd: &impl AsFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
}

/// [`PREFIX`] and 128 random bits in hex.
pub(crate) fn random_name() -> Result<String> {
    #[cfg(test)]
    if tests::FAILING_RANDOM.get() {
        return random_name_from(|_| Err(Errno::PERM));
    }
    random_name_from(|rest| rustix::rand::getrandom(rest, rustix::rand::GetRandomFlags::empty()))
}

/// [`random_name`] with the bits from `fill`, which may fill less than
/// asked or be interrupted, as `getrandom(2)`.
fn random_name_from(
    mut fill: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
) -> Result<String> {
    let mut bytes = [0_u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        let rest = bytes.get_mut(filled..).unwrap_or_default();
        match fill(rest) {
            Ok(n) => filled += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(Error::Random(e.into())),
        }
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{PREFIX}{hex}"))
}

/// Whether `name` is one this module or an unpacked directory of the
/// snapshotter left behind.
pub(crate) fn is_leftover(name: &OsStr) -> bool {
    name.as_bytes().starts_with(PREFIX.as_bytes())
}

/// Removes what a crashed build left in the state directory `root` (shown
/// as `shown`): files made by name and not unlinked, and directories
/// unpacked for a `COPY --from`, all named with [`PREFIX`]. A file or link
/// is unlinked, never followed; a directory is emptied through
/// descriptors, staying on its mount, and only if the directory opened is
/// the one listed. Returns whether there was anything.
pub(crate) fn remove_leftovers(root: &RootPath, shown: &Path) -> Result<bool> {
    let mut found = false;
    for name in root.read_names("/")? {
        if !is_leftover(&name) {
            continue;
        }
        found = true;
        let rel = Path::new(&name);
        let st = root.statx(rel)?;
        if st.file_type() == FileType::Directory {
            crate::store::remove_dir_tree(root, rel, &st)?;
        } else {
            root.remove_file(rel)?;
        }
        tracing::info!(
            target: "rnk_store",
            "removed {} left by an earlier build",
            shown.join(rel).display()
        );
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Seek as _, Write as _};

    use super::*;

    fn files(dir: &Path) -> AnonFiles {
        let fd = rustix::fs::open(
            dir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        AnonFiles::new(fd, dir, Fence::closed())
    }

    fn reads_back(anon: &AnonFiles) {
        let mut f = anon.create().unwrap();
        f.write_all(b"state").unwrap();
        f.rewind().unwrap();
        let path = anon.keep(f);
        assert!(path.starts_with("/proc/self/fd"));
        let mut back = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut back)
            .unwrap();
        assert_eq!(back, "state");
    }

    #[test]
    fn an_anonymous_file_has_no_name_and_reads_back_through_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        reads_back(&anon);
        // Nothing appears in the directory.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_named_file_is_unlinked_at_once_and_stays_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.force(NAMED);
        reads_back(&anon);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(!anon.in_memory());
    }

    #[test]
    fn a_named_file_whose_name_outlives_the_unlink_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.force(NAMED);
        anon.fault.store(FAULT_LINK, Ordering::Relaxed);
        let err = anon.create().unwrap_err();
        assert!(matches!(err, Error::NameOutlivesUnlink { .. }), "{err}");
        assert!(err.to_string().contains("still has a name"), "{err}");
        assert!(!anon.in_memory(), "never falls back to memory for that");
        // The file's other name is all that is left; the random one went.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [std::ffi::OsString::from("kept")]);
    }

    /// An unlink refused by permission leaves the name: an error of its
    /// own, never the "takes no file" fallback to memory, which would let
    /// the build go on until a `RUN` waits for the name to go.
    #[test]
    fn an_unlink_refused_by_permission_fails_closed_not_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.force(NAMED);
        anon.fault.store(FAULT_UNLINK_ACCES, Ordering::Relaxed);
        let err = anon.create().unwrap_err();
        assert!(matches!(err, Error::NameOutlivesUnlink { .. }), "{err}");
        assert!(!anon.in_memory());
    }

    /// A persistent store's files (an open fence) lie where no `RUN` can
    /// reach: a name kept after the unlink is no reason to fail there.
    #[test]
    fn a_persistent_stores_named_file_needs_no_name_check() {
        let dir = tempfile::tempdir().unwrap();
        let fd = rustix::fs::open(
            dir.path(),
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let anon = AnonFiles::new(fd, dir.path(), Fence::default());
        anon.force(NAMED);
        anon.fault.store(FAULT_LINK, Ordering::Relaxed);
        drop(anon.create().unwrap());
        assert!(!anon.in_memory());
    }

    #[test]
    fn network_and_fuse_filesystems_keep_names_of_unlinked_files() {
        assert_eq!(keeps_unlinked_names(0x6969), Some("NFS"));
        assert_eq!(keeps_unlinked_names(0x6573_5546), Some("FUSE"));
        for smb in [0x517b, 0xfe53_4d42, 0xff53_4d42] {
            assert_eq!(keeps_unlinked_names(smb), Some("SMB/CIFS"), "{smb:x}");
        }
        for afs in [0x6b41_4653, 0x5346_414f] {
            assert_eq!(keeps_unlinked_names(afs), Some("AFS"), "{afs:x}");
        }
        // ext4, tmpfs, xfs, btrfs, overlayfs.
        for local in [0xef53, 0x0102_1994, 0x5846_5342, 0x9123_683e, 0x794c_7630] {
            assert_eq!(keeps_unlinked_names(local), None, "{local:x}");
        }
    }

    #[test]
    fn a_named_file_during_a_run_on_this_thread_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.force(NAMED);
        let _run = anon.fence.enter_run().unwrap();
        assert!(matches!(anon.create(), Err(Error::Anonymous { .. })));
        assert!(!anon.in_memory(), "never falls back to memory for that");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_poisoned_fence_or_a_tainted_start_keeps_state_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.force(NAMED);
        anon.fence.poison();
        drop(anon.create().unwrap());
        assert!(anon.in_memory());
        let anon = files(dir.path());
        anon.force(NAMED);
        anon.tainted("an earlier build was cut short");
        drop(anon.create().unwrap());
        assert!(anon.in_memory());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Characterization: a directory that takes no file at all (permission
    /// refused) sends both the `O_TMPFILE` way and the named way to memory,
    /// never to an error. A process the permission does not stop (a probe
    /// file can be made) keeps the files on disk, and the test checks that.
    #[test]
    fn a_directory_that_takes_no_file_keeps_state_in_memory() {
        use std::os::unix::fs::PermissionsExt as _;
        for way in [TMPFILE, NAMED] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
            // Whether this process can create a file there anyway (root,
            // or `CAP_DAC_OVERRIDE`): then the files stay on disk.
            let probe = dir.path().join("probe");
            let takes_files = File::create(&probe).is_ok();
            if takes_files {
                std::fs::remove_file(&probe).unwrap();
            }
            let anon = files(dir.path());
            anon.force(way);
            drop(anon.create().unwrap());
            assert_eq!(anon.in_memory(), !takes_files, "way {way}");
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn memfd_is_the_last_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.force(MEMFD);
        let mut f = anon.create().unwrap();
        f.write_all(b"x").unwrap();
        let path = anon.keep(f);
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        assert!(anon.in_memory());
    }

    #[test]
    fn leftovers_with_the_prefix_go_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join(format!("{PREFIX}file")), b"x").unwrap();
        std::fs::create_dir_all(p.join(format!("{PREFIX}dir/sub"))).unwrap();
        std::fs::write(p.join(format!("{PREFIX}dir/sub/f")), b"x").unwrap();
        std::os::unix::fs::symlink("/etc", p.join(format!("{PREFIX}link"))).unwrap();
        std::fs::write(p.join("keep"), b"x").unwrap();
        let root = RootPath::open(p).unwrap();
        assert!(remove_leftovers(&root, p).unwrap());
        assert!(
            !remove_leftovers(&root, p).unwrap(),
            "nothing left the second time"
        );
        let left: Vec<_> = std::fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [std::ffi::OsString::from("keep")]);
        assert!(Path::new("/etc").exists(), "the link is never followed");
    }

    thread_local! {
        /// Makes [`random_name`] fail with `EPERM` on this thread, as a
        /// seccomp filter refusing `getrandom` does.
        pub(super) static FAILING_RANDOM: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    /// No randomness fails the file, even as `EPERM`: it says nothing about
    /// the state directory, so it is no reason to keep state in memory.
    #[test]
    fn a_failing_random_source_does_not_fall_back_to_memory() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.force(NAMED);
        FAILING_RANDOM.set(true);
        let made = anon.create();
        FAILING_RANDOM.set(false);
        assert!(matches!(made, Err(Error::Random(_))), "{made:?}");
        assert!(!anon.in_memory(), "never falls back to memory for that");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_failing_random_source_is_named_as_such() {
        let e = random_name_from(|_| Err(Errno::NOSYS)).unwrap_err();
        assert!(matches!(e, Error::Random(_)), "{e:?}");
        assert_eq!(
            e.to_string(),
            format!(
                "drawing a random name with getrandom: {}",
                std::io::Error::from(Errno::NOSYS)
            )
        );
        // An interrupted call is retried, a short one continued.
        let mut calls = 0;
        let name = random_name_from(|rest| {
            calls += 1;
            match calls {
                1 => Err(Errno::INTR),
                _ => {
                    rest[0] = 0xab;
                    Ok(1)
                }
            }
        })
        .unwrap();
        assert_eq!(name, format!("{PREFIX}{}", "ab".repeat(16)));
    }

    #[test]
    fn random_names_carry_the_prefix_and_differ() {
        let a = random_name().unwrap();
        let b = random_name().unwrap();
        assert!(is_leftover(OsStr::new(&a)));
        assert_ne!(a, b);
        assert_eq!(a.len(), PREFIX.len() + 32);
    }
}
