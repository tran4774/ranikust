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

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, ResolveFlags};
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

/// Creates the state directory if missing (mode `0700`) and checks it,
/// changing nothing in a directory that fails:
///
/// 1. `/` is refused outright, by identity;
/// 2. before anything is created, the nearest existing directory on its
///    path and every directory above it pass [`check_chain`]'s rule, walked
///    by descriptor (`..`), never by path;
/// 3. the state directory, opened without following a symlink, must be
///    empty (just created, or empty already) or hold the store's regular
///    marker file [`rnk_store::STATE_MARKER`]: any other directory is not
///    taken, so a mistyped `--state-dir=/usr` is left as it is;
/// 4. the whole chain again from its descriptor: owned by the effective
///    user, not writable by group or others;
/// 5. then its mode is set to `0700` when group or others may search it,
///    and must read back `0700`-tight (a filesystem that ignores `chmod`
///    is refused); `builds/`, where the roots live, likewise.
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
    let is_state = existing == state_dir;
    let fd = open_dir(existing, OFlags::PATH)?;
    if is_state {
        refuse_slash(&fd, state_dir)?;
    }
    check_chain(&walk(&fd, existing)?, euid, is_state)?;
    if is_state {
        check_contents(&open_dir(state_dir, OFlags::RDONLY)?, state_dir)?;
    }
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
    // Readable, so it can be listed and its mode set through it.
    let fd = open_dir(state_dir, OFlags::RDONLY)?;
    refuse_slash(&fd, state_dir)?;
    check_contents(&fd, state_dir)?;
    check_chain(&walk(&fd, state_dir)?, euid, true)?;
    make_private(&fd, state_dir, euid)?;
    match rustix::fs::openat(
        &fd,
        "builds",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(builds) => make_private(&builds, &state_dir.join("builds"), euid),
        Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(Refused(format!(
            "cannot open {}: {e}",
            state_dir.join("builds").display()
        ))),
    }
}

/// Refuses `fd` when it is the root directory `/`: never a state directory,
/// whose mode would be changed.
fn refuse_slash(fd: &OwnedFd, state_dir: &Path) -> Result<(), Refused> {
    let me = rustix::fs::fstat(fd).map_err(|e| stat_err(state_dir, e))?;
    let slash = rustix::fs::stat("/").map_err(|e| stat_err(Path::new("/"), e))?;
    if (me.st_dev, me.st_ino) == (slash.st_dev, slash.st_ino) {
        return Err(Refused(format!(
            "the state directory {} is /: {PRIVATE_STATE}",
            state_dir.display()
        )));
    }
    Ok(())
}

/// Accepts an empty directory or one holding the store's regular marker
/// file; refuses anything else unchanged.
fn check_contents(fd: &OwnedFd, state_dir: &Path) -> Result<(), Refused> {
    let err = |e: Errno| Refused(format!("cannot list {}: {e}", state_dir.display()));
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(fd).map_err(err)? {
        let name = entry.map_err(err)?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(name);
        }
    }
    if names.is_empty() {
        return Ok(());
    }
    let marker = rnk_store::STATE_MARKER;
    if names.iter().any(|n| n == marker.as_bytes()) {
        let st = rustix::fs::statat(fd, marker, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| stat_err(state_dir, e))?;
        if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile {
            return Ok(());
        }
    }
    let earlier = if names.iter().any(|n| n == b".ranikust-root") {
        " (an earlier ranikust's state directory or workdir, marked .ranikust-root, is not \
         reused: delete it first)"
    } else {
        ""
    };
    Err(Refused(format!(
        "{} is not empty and has no regular file {marker}: it is not a state directory, \
         and ranikust leaves it as it is{earlier}; pass an empty or new --state-dir",
        state_dir.display()
    )))
}

/// Sets `fd`'s mode to `0700` when group or others may reach into it, and
/// checks it reads back so: a filesystem that ignores `chmod` would leave
/// the build roots open to other users.
fn make_private(fd: &OwnedFd, dir: &Path, euid: u32) -> Result<(), Refused> {
    let st = rustix::fs::fstat(fd).map_err(|e| stat_err(dir, e))?;
    if st.st_uid != euid || st.st_mode & 0o022 != 0 {
        return Err(Refused(format!(
            "unsafe state directory: {} is owned by uid {} with mode {:o}; {PRIVATE_STATE}",
            dir.display(),
            st.st_uid,
            st.st_mode & 0o7777
        )));
    }
    if st.st_mode & 0o077 == 0 {
        return Ok(());
    }
    rustix::fs::fchmod(fd, Mode::from_raw_mode(0o700))
        .map_err(|e| Refused(format!("cannot make {} private: {e}", dir.display())))?;
    let st = rustix::fs::fstat(fd).map_err(|e| stat_err(dir, e))?;
    if st.st_mode & 0o077 != 0 {
        return Err(Refused(format!(
            "{} stays mode {:o} after chmod 0700 (a filesystem that ignores modes?): the build \
             roots would be open to other users; {PRIVATE_STATE}",
            dir.display(),
            st.st_mode & 0o7777
        )));
    }
    tracing::info!(
        "{} is now mode 0700: it holds the build roots",
        dir.display()
    );
    Ok(())
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
