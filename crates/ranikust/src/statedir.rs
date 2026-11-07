//! The state directory's checks, run during process set-up before anything
//! is written in it (ADR-0050). Every build root and upper directory
//! ranikust makes lies in the state directory, so it is the private
//! directory that keeps the image's setuid files out of other users' reach
//! (ADR-0047's rule, moved here), and the store reaches its files by path,
//! so no other user may rename a directory on the way to it.

use std::ffi::OsStr;
use std::fmt;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

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

/// What a refused state directory should become, for the effective user;
/// the refused one may be the default itself.
fn private_state(euid: u32) -> &'static str {
    if euid == 0 {
        "use a --state-dir below directories that only root may write, as the default \
         /var/lib/ranikust is on most hosts (ADR-0050)"
    } else {
        "use a --state-dir below directories that only you or root may write, as the default \
         ~/.local/share/ranikust is where your home is private (ADR-0050)"
    }
}

/// Creates the state directory if missing (mode `0700`) and checks it,
/// changing nothing in a directory that fails:
///
/// 1. `/` is refused outright, by identity;
/// 2. before anything is created, the nearest existing directory on its
///    path and every directory above it pass [`check_chain`]'s rule, walked
///    by descriptor (`..`), never by path; what is missing is then made
///    below that descriptor, one name at a time ([`create_missing`]);
/// 3. the state directory, opened without following a symlink, must be
///    empty (just created, or empty already) or hold the store's regular
///    marker file [`rnk_store::STATE_MARKER`] (or [`rnk_store::PRUNING_MARKER`],
///    which a `prune --all` cut short leaves), and if unmarked must not be
///    one of [`SYSTEM_DIRS`]: any other directory is not taken, so a
///    mistyped `--state-dir=/usr` or `/mnt` is left as it is;
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
        refuse_slash(&fd, state_dir, euid)?;
    }
    check_chain(&walk(&fd, existing)?, euid, is_state)?;
    // Readable, so it can be listed and its mode set through it.
    let fd = create_missing(fd, existing, state_dir, euid)?;
    refuse_slash(&fd, state_dir, euid)?;
    accept_existing(&fd, state_dir, euid)?;
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

/// What [`check`] found at the state directory's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// Nothing.
    Missing,
    /// An empty directory.
    Empty,
    /// A directory holding what [`check_contents`] accepts: the store's
    /// marker, or only what an anonymous store leaves, which the store
    /// refuses itself; and no name the store does not make there.
    Entries,
}

/// [`prepare`]'s checks on a state directory that is not created and
/// whose mode is not changed, for `prune --all`, which makes nothing: when
/// something is at `state_dir`, it must be a directory, opened without
/// following a symlink anywhere on its path, not `/`, whose chain passes
/// [`check_chain`]'s rule as the state directory, and empty or accepted by
/// [`check_contents`], holding no name the store does not make there
/// ([`rnk_store::foreign_names`]) and no `.ranikust-pruning` but a regular
/// file, which [`rnk_store::Store::prune_all`] refuses too. [`SYSTEM_DIRS`]
/// guard the mode [`prepare`] sets, so they are not refused here.
/// `state_dir` must be absolute.
///
/// # Errors
///
/// [`Refused`] naming the directory and the rule it breaks.
pub(crate) fn check(state_dir: &Path) -> Result<Found, Refused> {
    let euid = rustix::process::geteuid().as_raw();
    match std::fs::symlink_metadata(state_dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Found::Missing),
        Err(e) => {
            return Err(Refused(format!(
                "cannot look up the state directory {}: {e}",
                state_dir.display()
            )));
        }
    }
    let fd = open_dir(state_dir, OFlags::RDONLY)?;
    refuse_slash(&fd, state_dir, euid)?;
    check_chain(&walk(&fd, state_dir)?, euid, true)?;
    let names = list_names(&fd, state_dir)?;
    if names.is_empty() {
        return Ok(Found::Empty);
    }
    check_names(&fd, state_dir, &names, PRUNE_ADVICE)?;
    if let Some(why) = rnk_store::foreign_names(names.iter().map(|n| OsStr::from_bytes(n))) {
        return Err(Refused(format!("{} {why}", state_dir.display())));
    }
    // A regular marker beside it passed: the store refuses it all the same.
    let pruning = rnk_store::PRUNING_MARKER;
    if names.iter().any(|n| n == pruning.as_bytes()) {
        let st = rustix::fs::statat(&fd, pruning, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| stat_err(state_dir, e))?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(Refused(format!(
                "{} holds {pruning}, which is not a regular file and which ranikust prune --all \
                 neither takes nor removes: remove it, then run ranikust prune --all again",
                state_dir.display()
            )));
        }
    }
    Ok(Found::Entries)
}

/// Creates what is missing of `state_dir` below `existing`, its nearest
/// existing directory, open as `parent` and already checked; returns the
/// state directory, opened for reading.
///
/// Each missing name is made with `mkdirat` in the directory opened before
/// it, then opened from that descriptor, one name at a time, with no
/// symlink followed and without entering another mount: no path is
/// resolved again after the check, so a symlink another user plants at a
/// missing name in a sticky `/tmp` makes nothing elsewhere. A name that
/// appeared meanwhile is taken only if it passes [`check_chain`]'s rule for
/// a parent.
fn create_missing(
    parent: OwnedFd,
    existing: &Path,
    state_dir: &Path,
    euid: u32,
) -> Result<OwnedFd, Refused> {
    let rest = state_dir.strip_prefix(existing).map_err(|_| {
        Refused(format!(
            "{} is not below {}",
            state_dir.display(),
            existing.display()
        ))
    })?;
    let mut names = Vec::new();
    for c in rest.components() {
        let Component::Normal(name) = c else {
            return Err(Refused(format!(
                "the state directory {} must be a plain absolute path, without . or ..",
                state_dir.display()
            )));
        };
        names.push(name);
    }
    if names.is_empty() {
        // It exists: reopened from its checked descriptor, not by path.
        return rustix::fs::openat(
            &parent,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| stat_err(state_dir, e));
    }
    let mut at = parent;
    let mut path = existing.to_path_buf();
    for (i, name) in names.iter().enumerate() {
        path.push(name);
        match rustix::fs::mkdirat(&at, *name, Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => {
                return Err(Refused(format!(
                    "cannot create the state directory {}: {e}",
                    path.display()
                )));
            }
        }
        let last = i + 1 == names.len();
        let how = if last { OFlags::RDONLY } else { OFlags::PATH };
        let next = rustix::fs::openat2(
            &at,
            *name,
            how | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(|e| {
            Refused(format!(
                "cannot open {} as a directory without following a symlink: {e}",
                path.display()
            ))
        })?;
        if !last {
            let st = rustix::fs::fstat(&next).map_err(|e| stat_err(&path, e))?;
            let dir = ChainDir {
                path: path.clone(),
                uid: st.st_uid,
                mode: st.st_mode & 0o7777,
            };
            check_chain(std::slice::from_ref(&dir), euid, false)?;
        }
        at = next;
    }
    Ok(at)
}

/// Refuses `fd` when it is the root directory `/`: never a state directory,
/// whose mode would be changed.
fn refuse_slash(fd: &OwnedFd, state_dir: &Path, euid: u32) -> Result<(), Refused> {
    let me = rustix::fs::fstat(fd).map_err(|e| stat_err(state_dir, e))?;
    let slash = rustix::fs::stat("/").map_err(|e| stat_err(Path::new("/"), e))?;
    if (me.st_dev, me.st_ino) == (slash.st_dev, slash.st_ino) {
        return Err(Refused(format!(
            "the state directory {} is /: {}",
            state_dir.display(),
            private_state(euid)
        )));
    }
    Ok(())
}

/// Directories an unmarked state directory must not be: emptied of nothing,
/// but made `0700`, they would lock other users out (`--state-dir=/mnt` on
/// a minimal host). Compared by `(dev, ino)`, so a symlink or another name
/// of one is caught too; a fresh volume mounted elsewhere passes.
pub(crate) const SYSTEM_DIRS: [&str; 30] = [
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/media",
    "/mnt",
    "/nix",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/snap",
    "/srv",
    "/sys",
    "/tmp",
    "/usr",
    "/usr/bin",
    "/usr/lib",
    "/usr/local",
    "/var",
    "/var/cache",
    "/var/lib",
    "/var/log",
    "/var/tmp",
];

/// [`check_contents`], and for a directory without the store's marker,
/// not one of [`SYSTEM_DIRS`].
fn accept_existing(fd: &OwnedFd, state_dir: &Path, euid: u32) -> Result<(), Refused> {
    if check_contents(fd, state_dir)? {
        return Ok(());
    }
    let me = rustix::fs::fstat(fd).map_err(|e| stat_err(state_dir, e))?;
    let system = SYSTEM_DIRS.iter().find(|sys| {
        rustix::fs::stat(**sys).is_ok_and(|st| (st.st_dev, st.st_ino) == (me.st_dev, me.st_ino))
    });
    match system {
        Some(sys) => Err(Refused(format!(
            "the state directory {} is the system directory {sys}: ranikust would make it \
             mode 0700; {}",
            state_dir.display(),
            private_state(euid)
        ))),
        None => Ok(()),
    }
}

/// Accepts an empty directory or one holding the store's regular marker
/// file, telling which (`true`: marked); refuses anything else unchanged.
/// The marker of a `prune --all` that did not finish counts: the store
/// refuses a build there itself, saying so, and `prune --all` finishes.
fn check_contents(fd: &OwnedFd, state_dir: &Path) -> Result<bool, Refused> {
    check_names(fd, state_dir, &list_names(fd, state_dir)?, BUILD_ADVICE)
}

/// The advice a build's refusal of an unmarked directory ends with.
const BUILD_ADVICE: &str = "pass an empty or new --state-dir";

/// The advice `prune --all`'s refusal of one ends with.
const PRUNE_ADVICE: &str = "ranikust prune --all empties only a state directory ranikust made";

/// The names in the directory `fd`, `.` and `..` left out.
fn list_names(fd: &OwnedFd, state_dir: &Path) -> Result<Vec<Vec<u8>>, Refused> {
    let err = |e: Errno| Refused(format!("cannot list {}: {e}", state_dir.display()));
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(fd).map_err(err)? {
        let name = entry.map_err(err)?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(name);
        }
    }
    Ok(names)
}

/// [`check_contents`] on `names`, the names in `fd`; a refusal ends with
/// `advice`.
fn check_names(
    fd: &OwnedFd,
    state_dir: &Path,
    names: &[Vec<u8>],
    advice: &str,
) -> Result<bool, Refused> {
    // Only what an anonymous store (kaniko mode on `/`) leaves when a
    // build is cut short: its marker and its unpacked names, which it
    // removes or heeds at its next start (ADR-0056, Q-360).
    let own = |n: &Vec<u8>| {
        n == rnk_store::BUILDING_MARKER.as_bytes()
            || n.starts_with(rnk_store::ANON_PREFIX.as_bytes())
    };
    if names.iter().all(own) {
        return Ok(false);
    }
    let marker = rnk_store::STATE_MARKER;
    for m in [marker, rnk_store::PRUNING_MARKER] {
        if names.iter().any(|n| n == m.as_bytes()) {
            let st = rustix::fs::statat(fd, m, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|e| stat_err(state_dir, e))?;
            if FileType::from_raw_mode(st.st_mode) == FileType::RegularFile {
                return Ok(true);
            }
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
         and ranikust leaves it as it is{earlier}; {advice}",
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
            "unsafe state directory: {} is owned by uid {} with mode {:o}; {}",
            dir.display(),
            st.st_uid,
            st.st_mode & 0o7777,
            private_state(euid)
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
             roots would be open to other users; {}",
            dir.display(),
            st.st_mode & 0o7777,
            private_state(euid)
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
             other user may write it or a directory above it; {}",
            d.path.display(),
            private_state(euid)
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
/// walk holds a directory fd per worker and depth (`ADR-0038`), and
/// `prune --all`'s deletion one per depth. Returns the new soft limit,
/// `None` when unlimited.
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
