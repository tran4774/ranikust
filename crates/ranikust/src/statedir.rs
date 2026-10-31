//! The state directory's checks, run during process set-up before anything
//! is written in it (ADR-0050). Every build root and upper directory
//! ranikust makes lies in the state directory, so it is the private
//! directory that keeps the image's setuid files out of other users' reach
//! (ADR-0047's rule, moved here), and the store reaches its files by path,
//! so no other user may rename a directory on the way to it.

use std::fmt;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

/// Why a state directory cannot be used.
#[derive(Debug)]
pub(crate) struct Refused(pub(crate) String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a refused state directory should become.
const PRIVATE_STATE: &str = "use the default /var/lib/ranikust, or a --state-dir below \
     directories that only root may write (ADR-0050)";

/// Creates the state directory if missing (mode `0700`) and checks it:
///
/// 1. before anything is created, the nearest existing directory on its
///    path and every directory above it pass [`check_chain`]'s rule for
///    parents, walked by descriptor (`..`), never by path;
/// 2. after, the state directory itself: a real directory owned by the
///    effective user, not writable by group or others; its mode is set to
///    `0700` when group or others may search it, so it is the private
///    directory of everything below it; then the whole chain again.
///
/// `state_dir` must be absolute.
///
/// # Errors
///
/// [`Refused`] naming the directory and the rule it breaks.
pub(crate) fn prepare(state_dir: &Path) -> Result<(), Refused> {
    let euid = rustix::process::geteuid().as_raw();
    let existing = state_dir
        .ancestors()
        .find(|p| std::fs::symlink_metadata(p).is_ok())
        .unwrap_or(Path::new("/"));
    let fd = open_dir(existing, OFlags::PATH)?;
    let chain = walk(&fd, existing)?;
    check_chain(&chain, euid, existing == state_dir)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)
        .map_err(|e| {
            Refused(format!(
                "cannot create the state directory {}: {e}",
                state_dir.display()
            ))
        })?;
    // Readable, so its mode can be set through the descriptor.
    let fd = open_dir(state_dir, OFlags::RDONLY)?;
    let st = rustix::fs::fstat(&fd).map_err(|e| stat_err(state_dir, e))?;
    if st.st_uid == euid && st.st_mode & 0o022 == 0 && st.st_mode & 0o077 != 0 {
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700)).map_err(|e| {
            Refused(format!(
                "cannot make the state directory {} private: {e}",
                state_dir.display()
            ))
        })?;
        tracing::info!(
            "the state directory {} is now mode 0700: it holds the build roots",
            state_dir.display()
        );
    }
    check_chain(&walk(&fd, state_dir)?, euid, true)
}

/// Opens `dir` without following a symlink anywhere on its path.
fn open_dir(dir: &Path, how: OFlags) -> Result<OwnedFd, Refused> {
    rustix::fs::openat2(
        CWD,
        dir,
        how | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| {
        Refused(format!(
            "cannot open {} as a directory without following a symlink: {e}",
            dir.display()
        ))
    })
}

fn stat_err(dir: &Path, e: Errno) -> Refused {
    Refused(format!("cannot read the parents of {}: {e}", dir.display()))
}

/// The directory `fd` and its real parents up to `/`, walked with `..`
/// from the descriptor (each opened `O_PATH`); `path` names them in
/// messages only.
fn walk(fd: &OwnedFd, path: &Path) -> Result<Vec<ChainDir>, Refused> {
    let mut names = path.ancestors();
    let mut chain = Vec::new();
    let mut at: OwnedFd = rustix::fs::openat(
        fd,
        ".",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| stat_err(path, e))?;
    loop {
        let st = rustix::fs::fstat(&at).map_err(|e| stat_err(path, e))?;
        chain.push(ChainDir {
            path: names
                .next()
                .map_or_else(|| PathBuf::from("?"), Path::to_owned),
            uid: st.st_uid,
            mode: st.st_mode & 0o7777,
        });
        let parent = rustix::fs::openat(
            &at,
            "..",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| stat_err(path, e))?;
        let up = rustix::fs::fstat(&parent).map_err(|e| stat_err(path, e))?;
        // `/`'s `..` is itself.
        if (up.st_dev, up.st_ino) == (st.st_dev, st.st_ino) {
            break;
        }
        at = parent;
    }
    Ok(chain)
}

/// The owner and mode of one directory of the state directory's chain.
#[derive(Clone, Debug)]
pub(crate) struct ChainDir {
    pub(crate) path: PathBuf,
    pub(crate) uid: u32,
    /// The raw `st_mode` permission bits, sticky included.
    pub(crate) mode: u32,
}

/// ADR-0050's rule on `chain`, nearest first, up to `/`. With `has_state`,
/// the first entry is the state directory: owned by `euid` and not writable
/// by group or others. Every other entry is a parent: owned by root or
/// `euid`, and not writable by group or others unless it is root's sticky
/// directory (`/tmp`), where no one may rename what root owns.
///
/// # Errors
///
/// [`Refused`] naming the first directory that breaks the rule.
pub(crate) fn check_chain(chain: &[ChainDir], euid: u32, has_state: bool) -> Result<(), Refused> {
    let refuse = |d: &ChainDir, why: &str| {
        Refused(format!(
            "unsafe state directory: {} {why}; the build roots and the store live there, so no \
             other user may write it or a directory above it; {PRIVATE_STATE}",
            d.path.display()
        ))
    };
    for (i, d) in chain.iter().enumerate() {
        if i == 0 && has_state {
            if d.uid != euid {
                return Err(refuse(d, &format!("is owned by uid {}, not {euid}", d.uid)));
            }
            if d.mode & 0o022 != 0 {
                return Err(refuse(d, "is writable by group or others"));
            }
            continue;
        }
        if d.uid != 0 && d.uid != euid {
            return Err(refuse(d, &format!("is owned by uid {}", d.uid)));
        }
        let sticky_root = d.uid == 0 && d.mode & 0o1000 != 0;
        if d.mode & 0o022 != 0 && !sticky_root {
            return Err(refuse(d, "is writable by group or others"));
        }
    }
    Ok(())
}

/// Whether `dir` is on overlayfs, which cannot hold an overlay upper
/// directory: a container's own root (ADR-0050).
pub(crate) fn on_overlayfs(dir: &Path) -> bool {
    const OVERLAYFS_SUPER_MAGIC: rustix::fs::FsWord = 0x794c_7630;
    open_dir(dir, OFlags::PATH)
        .ok()
        .and_then(|fd| rustix::fs::fstatfs(&fd).ok())
        .is_some_and(|fs| fs.f_type == OVERLAYFS_SUPER_MAGIC)
}

/// Raises `RLIMIT_NOFILE`'s soft limit to its hard limit: the kaniko-mode
/// walk holds a directory fd per worker and depth (`ADR-0038`). Returns the
/// new soft limit, `None` when unlimited.
///
/// # Errors
///
/// The errno of `setrlimit`.
pub(crate) fn raise_nofile_limit() -> Result<Option<u64>, Errno> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let limit = getrlimit(Resource::Nofile);
    if limit.current == limit.maximum {
        return Ok(limit.current);
    }
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: limit.maximum,
            maximum: limit.maximum,
        },
    )?;
    Ok(limit.maximum)
}

#[cfg(test)]
mod tests;
