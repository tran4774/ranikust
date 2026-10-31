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
//!    page cache the kernel reclaims, never in ranikust's memory.
//! 3. A `memfd`, only when the state directory takes no file at all (a
//!    read-only or full filesystem): memory, with a warning.
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

use rnk_rootfs::{FileType, IgnoreList, RootPath, delete_contents};
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
    /// Every file kept, open until the store goes: their numbers are the
    /// paths handed out.
    kept: Mutex<Vec<OwnedFd>>,
    fence: Fence,
}

impl AnonFiles {
    /// Anonymous files in the directory `dir` (opened by the caller,
    /// `O_PATH | O_DIRECTORY`), named `dir_path` in messages; a named file
    /// is made only inside a window of `fence`.
    pub(crate) fn new(dir: OwnedFd, dir_path: &Path, fence: Fence) -> Self {
        Self {
            dir,
            dir_path: dir_path.to_owned(),
            way: AtomicU8::new(TMPFILE),
            kept: Mutex::new(Vec::new()),
            fence,
        }
    }

    /// Whether files are `memfd`s (memory) rather than files on disk.
    #[cfg(test)]
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
                    {
                        tracing::info!(
                            target: "rnk_store",
                            "{}: no O_TMPFILE on this filesystem; state files are made by a \
                             name unlinked at once, while no RUN runs",
                            self.dir_path.display()
                        );
                    }
                }
                Err(e) => return Err(self.err(e)),
            }
        }
        if self.way.load(Ordering::Relaxed) == NAMED {
            match self.create_named() {
                Ok(file) => return Ok(file),
                // The fence refused: a bug in the order of work, never
                // papered over with memory.
                Err(e @ Error::Anonymous(_)) => return Err(e),
                Err(e) => {
                    if self
                        .way
                        .compare_exchange(NAMED, MEMFD, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        tracing::warn!(
                            target: "rnk_store",
                            "{}: cannot make state files on disk ({e}); keeping state in \
                             memory (memfd), counted against the container's memory limit",
                            self.dir_path.display()
                        );
                    }
                }
            }
        }
        self.create_memfd()
    }

    /// A file made by a random name and unlinked before the window closes.
    fn create_named(&self) -> Result<File> {
        let name = random_name()?;
        let _window = self.fence.named()?;
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
        // A failed unlink keeps the file and its name: refuse it, and leave
        // the name to [`remove_leftovers`] of the next start.
        rustix::fs::unlinkat(&self.dir, name.as_str(), AtFlags::empty())
            .map_err(|e| self.err(e))?;
        Ok(File::from(fd))
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

/// `/proc/self/fd/<n>` of `fd`: its name for this process alone.
pub(crate) fn fd_path(fd: &impl AsFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
}

/// [`PREFIX`] and 128 random bits in hex.
pub(crate) fn random_name() -> Result<String> {
    let mut bytes = [0_u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        let rest = bytes.get_mut(filled..).unwrap_or_default();
        match rustix::rand::getrandom(rest, rustix::rand::GetRandomFlags::empty()) {
            Ok(n) => filled += n,
            Err(Errno::INTR) => {}
            Err(e) => {
                return Err(Error::AnonymousFile {
                    path: PathBuf::from("getrandom"),
                    source: e.into(),
                });
            }
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
/// descriptors, staying on its mount.
pub(crate) fn remove_leftovers(root: &RootPath, shown: &Path) -> Result<()> {
    for name in root.read_names("/")? {
        if !is_leftover(&name) {
            continue;
        }
        let rel = Path::new(&name);
        let st = root.statx(rel)?;
        if st.file_type() == FileType::Directory && !st.mount_root && st.mnt_id == root.mnt_id() {
            delete_contents(&root.sub_root(rel)?, &IgnoreList::new())?;
            root.remove_dir(rel)?;
        } else if st.file_type() == FileType::Directory {
            return Err(Error::BadEntry {
                path: shown.join(rel),
                reason: "a leftover directory on another mount",
            });
        } else {
            root.remove_file(rel)?;
        }
        tracing::info!(
            target: "rnk_store",
            "removed {} left by an earlier build",
            shown.join(rel).display()
        );
    }
    Ok(())
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
    fn a_named_file_during_a_run_on_this_thread_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let anon = files(dir.path());
        anon.force(NAMED);
        let _run = anon.fence.enter_run().unwrap();
        assert!(matches!(anon.create(), Err(Error::Anonymous(_))));
        assert!(!anon.in_memory(), "never falls back to memory for that");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
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
        remove_leftovers(&root, p).unwrap();
        let left: Vec<_> = std::fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [std::ffi::OsString::from("keep")]);
        assert!(Path::new("/etc").exists(), "the link is never followed");
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
