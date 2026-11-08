//! The state directory's checks, run during process set-up before anything
//! is written in it (ADR-0050). Every build root and upper directory
//! ranikust makes lies in the state directory, so it is the private
//! directory that keeps the image's setuid files out of other users' reach
//! (ADR-0047's rule, moved here), and the store reaches its files by path,
//! so no other user may rename a directory on the way to it.

use std::fmt;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, RenameFlags, ResolveFlags};
use rustix::io::Errno;

/// Why a state directory cannot be used.
#[derive(Debug)]
pub(crate) struct Refused(pub(crate) String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whose the state directory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    /// The effective user, this uid.
    Me(u32),
    /// Under `--userns` (ADR-0104), root's build runs as this uid and gid
    /// of the host, its user namespace's root: it must own the state
    /// directory and search every directory above it, which only root may
    /// own or write, sticky or not, since it may rename what it owns.
    Mapped { uid: u32, gid: u32 },
}

impl Owner {
    /// This process's effective user.
    pub(crate) fn me() -> Self {
        Self::Me(rustix::process::geteuid().as_raw())
    }

    fn uid(self) -> u32 {
        match self {
            Self::Me(uid) | Self::Mapped { uid, .. } => uid,
        }
    }

    /// What a refused state directory should become.
    fn advice(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Me(euid) => private_state(euid).into(),
            Self::Mapped { uid, gid } => format!(
                "with --userns, use a --state-dir of its own, owned by uid {uid} and gid {gid} \
                 (ranikust makes a missing one for them), below directories that only root owns \
                 and may write and that uid {uid} may search, such as /var/lib/ranikust-userns; \
                 root's own state directory is never given to it (ADR-0104)"
            )
            .into(),
        }
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
    prepare_for(state_dir, Owner::me())
}

/// [`prepare`] for `owner`. For [`Owner::Mapped`], the rule is
/// [`check_chain`]'s for it, and what this process (root) makes is given
/// to it: the state directory, made `0700` and owned by its uid and gid,
/// and each missing directory above it, root's with mode `0711`, so that
/// it may search them. An existing state directory is never given, nor
/// one at, below or above root's default ([`refuse_roots_default`]),
/// missing or not.
///
/// # Errors
///
/// [`Refused`] naming the directory and the rule it breaks.
pub(crate) fn prepare_for(state_dir: &Path, owner: Owner) -> Result<(), Refused> {
    stage(state_dir, owner)?.finish()
}

/// [`prepare_for`] up to its step 5: the state directory made where
/// missing and checked, its mode not yet changed, so that a caller's own
/// read-only check refuses it as found.
///
/// # Errors
///
/// [`Refused`] naming the directory and the rule it breaks.
pub(crate) fn stage(state_dir: &Path, owner: Owner) -> Result<Staged, Refused> {
    refuse_roots_default(state_dir, owner)?;
    let existing = nearest_existing(state_dir);
    let is_state = existing == state_dir;
    let fd = open_dir(existing, OFlags::PATH)?;
    if is_state {
        refuse_slash(&fd, state_dir, owner)?;
    }
    check_chain(&walk(&fd, existing)?, owner, is_state)?;
    // Readable, so it can be listed and its mode set through it.
    let fd = create_missing(fd, existing, state_dir, owner)?;
    refuse_slash(&fd, state_dir, owner)?;
    accept_existing(&fd, state_dir, owner)?;
    check_chain(&walk(&fd, state_dir)?, owner, true)?;
    Ok(Staged {
        fd,
        state_dir: state_dir.to_owned(),
        owner,
    })
}

/// The nearest directory on `state_dir`'s path that exists, where a
/// missing one would be made; `/` at most.
pub(crate) fn nearest_existing(state_dir: &Path) -> &Path {
    state_dir
        .ancestors()
        .find(|p| std::fs::symlink_metadata(p).is_ok())
        .unwrap_or(Path::new("/"))
}

/// A state directory [`stage`] checked, whose mode [`Staged::finish`]
/// sets.
#[must_use = "the state directory is not private until finished"]
pub(crate) struct Staged {
    fd: OwnedFd,
    state_dir: PathBuf,
    owner: Owner,
}

impl Staged {
    /// [`prepare_for`]'s step 5, through the descriptor checked.
    ///
    /// # Errors
    ///
    /// [`Refused`] naming the directory and the rule it breaks.
    pub(crate) fn finish(self) -> Result<(), Refused> {
        let Self {
            fd,
            state_dir,
            owner,
        } = self;
        make_private(&fd, &state_dir, owner)?;
        match rustix::fs::openat(
            &fd,
            "builds",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(builds) => make_private(&builds, &state_dir.join("builds"), owner),
            Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(Refused(format!(
                "cannot open {}: {e}",
                state_dir.join("builds").display()
            ))),
        }
    }
}

/// Refuses, for [`Owner::Mapped`], a state directory, absolute, at, below
/// or above root's default ([`crate::config::DEFAULT_STATE_DIR`]),
/// however its path is spelt: made for the mapped ids, any of them would
/// refuse root's own builds there. Checks the path alone.
fn refuse_roots_default(state_dir: &Path, owner: Owner) -> Result<(), Refused> {
    match owner {
        Owner::Me(_) => Ok(()),
        Owner::Mapped { .. } => {
            let default = Path::new(crate::config::DEFAULT_STATE_DIR);
            // `..` is resolved lexically: no symlink is followed on the way
            // (`open_dir`), so that is where it leads.
            let clean = crate::config::clean(state_dir);
            let how = if clean == default {
                "is root's default".to_owned()
            } else if clean.starts_with(default) {
                format!("is below root's default {}", default.display())
            } else if default.starts_with(&clean) {
                format!("is above root's default {}", default.display())
            } else {
                return Ok(());
            };
            Err(Refused(format!(
                "the state directory {} {how}: {}",
                state_dir.display(),
                owner.advice()
            )))
        }
    }
}

/// What [`check`] found at the state directory's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// Nothing.
    Missing,
    /// An empty directory.
    Empty,
    /// A directory [`rnk_store::Store::prune_all`] empties, as
    /// [`rnk_store::Store::check_prune`] found it.
    Entries(rnk_store::PruneCheck),
}

/// [`prepare`]'s checks on a state directory that is not created and
/// whose mode is not changed, for `prune --all`, which makes nothing: when
/// something is at `state_dir`, it must be a directory, opened without
/// following a symlink anywhere on its path, not `/`, whose chain passes
/// [`check_chain`]'s rule as the state directory; then its contents are
/// [`rnk_store::Store::check_prune`]'s, so that what `prune_all` would
/// refuse is refused here, in its words. [`SYSTEM_DIRS`] guard the mode
/// [`prepare`] sets, so they are not refused here. `state_dir` must be
/// absolute.
///
/// # Errors
///
/// [`Refused`] naming the directory and the rule it breaks.
pub(crate) fn check(state_dir: &Path) -> Result<Found, Refused> {
    let owner = Owner::me();
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
    refuse_slash(&fd, state_dir, owner)?;
    check_chain(&walk(&fd, state_dir)?, owner, true)?;
    match rnk_store::Store::check_prune(state_dir) {
        Ok(found) if found.entries => Ok(Found::Entries(found)),
        Ok(_) => Ok(Found::Empty),
        Err(e) => Err(Refused(e.to_string())),
    }
}

/// The build's rule for the state directory at `state_dir`, which `check`
/// describes, before a refusal prints commands that act on it: opened
/// again without following a symlink, it is that directory; it and its
/// parents pass [`check_chain`] with its owner as the user; and it is none
/// of [`SYSTEM_DIRS`], marked or not. Returns the permission bits the rule
/// judged, which the commands check too.
///
/// # Errors
///
/// [`Refused`] naming the rule it breaks and, for the build's rule, what
/// to do instead: a build's advice, a `--state-dir` elsewhere, does not
/// empty this one.
pub(crate) fn check_for_commands(
    state_dir: &Path,
    check: &rnk_store::PruneCheck,
) -> Result<u32, Refused> {
    let system: Vec<&Path> = SYSTEM_DIRS.iter().map(Path::new).collect();
    check_for_commands_beside(state_dir, check, &system)
}

/// [`check_for_commands`] with `system` as the system directories.
fn check_for_commands_beside(
    state_dir: &Path,
    check: &rnk_store::PruneCheck,
    system: &[&Path],
) -> Result<u32, Refused> {
    let fd = open_dir(state_dir, OFlags::PATH)?;
    let st = rustix::fs::fstat(&fd).map_err(|e| stat_err(state_dir, e))?;
    if (st.st_dev, st.st_ino, st.st_uid) != (check.dev, check.ino, check.uid) {
        return Err(Refused(format!(
            "{} changed while ranikust checked it",
            state_dir.display()
        )));
    }
    let chain = walk(&fd, state_dir)?;
    if let Some((d, why)) = breach(&chain, Owner::Me(check.uid), true) {
        return Err(Refused(format!(
            "{} {why}, which a build refuses: no other user may write a state directory or a \
             directory above it; fix the owner or mode, or remove the directory by hand",
            d.path.display()
        )));
    }
    let me = (st.st_dev, st.st_ino);
    let found = system
        .iter()
        .find(|sys| rustix::fs::stat(**sys).is_ok_and(|s| (s.st_dev, s.st_ino) == me));
    if let Some(sys) = found {
        return Err(Refused(format!(
            "{} is the system directory {}",
            state_dir.display(),
            sys.display()
        )));
    }
    // `walk` reads the state directory first.
    chain.first().map(|d| d.mode).ok_or_else(|| {
        Refused(format!(
            "{} could not be read as a directory",
            state_dir.display()
        ))
    })
}

/// Creates what is missing of `state_dir` below `existing`, its nearest
/// existing directory, open as `parent` and already checked; returns the
/// state directory, opened for reading. For [`Owner::Mapped`], a missing
/// directory above it is made searchable (`0711`) and the state directory
/// given to the owner, each only when made here.
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
    owner: Owner,
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
    let mapped = match owner {
        Owner::Mapped { uid, gid } => Some((uid, gid)),
        Owner::Me(_) => None,
    };
    let mut at = parent;
    let mut path = existing.to_path_buf();
    for (i, name) in names.iter().enumerate() {
        path.push(name);
        let last = i + 1 == names.len();
        let given = match mapped {
            Some(ids) => make_given(&at, name, &path, last, ids)?,
            None => match rustix::fs::mkdirat(&at, *name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(Errno::EXIST) => None,
                Err(e) => {
                    return Err(Refused(format!(
                        "cannot create the state directory {}: {e}",
                        path.display()
                    )));
                }
            },
        };
        let next = match given {
            Some(next) => next,
            None => open_beneath(&at, name, &path, last || mapped.is_some())?,
        };
        if !last {
            let st = rustix::fs::fstat(&next).map_err(|e| stat_err(&path, e))?;
            let dir = ChainDir {
                path: path.clone(),
                uid: st.st_uid,
                gid: st.st_gid,
                mode: st.st_mode & 0o7777,
            };
            check_chain(std::slice::from_ref(&dir), owner, false)?;
        }
        at = next;
    }
    Ok(at)
}

/// Opens the directory `name` of `at` (at `path`) with no symlink
/// followed and without entering another mount; for reading with
/// `readable` (root, which reads any directory, changes the mode or owner
/// of what it made through the descriptor it checks), else `O_PATH`.
fn open_beneath(
    at: &OwnedFd,
    name: &std::ffi::OsStr,
    path: &Path,
    readable: bool,
) -> Result<OwnedFd, Refused> {
    let how = if readable {
        OFlags::RDONLY
    } else {
        OFlags::PATH
    };
    rustix::fs::openat2(
        at,
        name,
        how | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_XDEV,
    )
    .map_err(|e| {
        Refused(format!(
            "cannot open {} as a directory without following a symlink: {e}",
            path.display()
        ))
    })
}

/// The prefix of the name [`make_given`] makes a directory under before
/// it takes its own.
const NEW_PREFIX: &str = ".ranikust-new-";

/// Makes the missing directory `name` of `at` (at `path`) for
/// [`Owner::Mapped`]'s `ids`: under a name of its own first
/// ([`NEW_PREFIX`]), given through its descriptor (the state directory,
/// `last`, to `ids`; a parent the mode `0711`), then moved to `name`
/// without replacing anything, so that a process killed on the way leaves
/// nothing at `name` that the next run would refuse. Returns it, opened
/// for reading; `None` where `name` exists or appeared meanwhile, for the
/// caller to open and check. What was made is removed on failure, only
/// while empty.
fn make_given(
    at: &OwnedFd,
    name: &std::ffi::OsStr,
    path: &Path,
    last: bool,
    (uid, gid): (u32, u32),
) -> Result<Option<OwnedFd>, Refused> {
    match rustix::fs::statat(at, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => return Ok(None),
        Err(Errno::NOENT) => {}
        Err(e) => return Err(stat_err(path, e)),
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let new = format!("{NEW_PREFIX}{}-{nanos}", std::process::id());
    let cannot = |what: &str, e: Errno| Refused(format!("cannot {what} {}: {e}", path.display()));
    rustix::fs::mkdirat(at, new.as_str(), Mode::from_raw_mode(0o700))
        .map_err(|e| cannot("create the state directory", e))?;
    let moved = open_beneath(at, new.as_ref(), path, true).and_then(|fd| {
        let given = if last {
            rustix::fs::fchown(
                &fd,
                Some(rustix::fs::Uid::from_raw(uid)),
                Some(rustix::fs::Gid::from_raw(gid)),
            )
            .map_err(|e| cannot("give --userns's ids", e))
        } else {
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o711))
                .map_err(|e| cannot("make searchable", e))
        };
        given?;
        #[cfg(test)]
        tests::before_rename();
        match rustix::fs::renameat_with(at, new.as_str(), at, name, RenameFlags::NOREPLACE) {
            Ok(()) => Ok(Some(fd)),
            Err(Errno::EXIST) => Ok(None),
            Err(e) => Err(cannot("move into place", e)),
        }
    });
    if !matches!(moved, Ok(Some(_))) {
        // Unused or failed, it goes; `moved` says why, and a failure here
        // leaves one empty directory: `rmdir` takes nothing put in it.
        let _ = rustix::fs::unlinkat(at, new.as_str(), AtFlags::REMOVEDIR);
    }
    moved
}

/// Refuses `fd` when it is the root directory `/`: never a state directory,
/// whose mode would be changed.
fn refuse_slash(fd: &OwnedFd, state_dir: &Path, owner: Owner) -> Result<(), Refused> {
    let me = rustix::fs::fstat(fd).map_err(|e| stat_err(state_dir, e))?;
    let slash = rustix::fs::stat("/").map_err(|e| stat_err(Path::new("/"), e))?;
    if (me.st_dev, me.st_ino) == (slash.st_dev, slash.st_ino) {
        return Err(Refused(format!(
            "the state directory {} is /: {}",
            state_dir.display(),
            owner.advice()
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
fn accept_existing(fd: &OwnedFd, state_dir: &Path, owner: Owner) -> Result<(), Refused> {
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
            owner.advice()
        ))),
        None => Ok(()),
    }
}

/// Accepts an empty directory or one holding the store's regular marker
/// file, telling which (`true`: marked); refuses anything else unchanged.
/// The marker of a `prune --all` that did not finish counts: the store
/// refuses a build there itself, saying so, and `prune --all` finishes.
fn check_contents(fd: &OwnedFd, state_dir: &Path) -> Result<bool, Refused> {
    let err = |e: Errno| Refused(format!("cannot list {}: {e}", state_dir.display()));
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(fd).map_err(err)? {
        let name = entry.map_err(err)?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(name);
        }
    }
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
                .map_err(|e| Refused(format!("cannot read {m} in {}: {e}", state_dir.display())))?;
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
         and ranikust leaves it as it is{earlier}; pass an empty or new --state-dir",
        state_dir.display()
    )))
}

/// Sets `fd`'s mode to `0700` when group or others may reach into it, and
/// checks it reads back so: a filesystem that ignores `chmod` would leave
/// the build roots open to other users.
fn make_private(fd: &OwnedFd, dir: &Path, owner: Owner) -> Result<(), Refused> {
    let st = rustix::fs::fstat(fd).map_err(|e| stat_err(dir, e))?;
    if st.st_uid != owner.uid() || st.st_mode & 0o022 != 0 {
        return Err(Refused(format!(
            "unsafe state directory: {} is owned by uid {} with mode {:o}; {}",
            dir.display(),
            st.st_uid,
            st.st_mode & 0o7777,
            owner.advice()
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
            owner.advice()
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
/// from the descriptor (each opened `O_PATH`); `path`, absolute, names
/// them in messages only.
fn walk(fd: &OwnedFd, path: &Path) -> Result<Vec<ChainDir>, Refused> {
    // With no symlink followed on `path`, its `..` are the real parents',
    // so its lexical ancestors name the directories walked.
    let named = crate::config::clean(path);
    let mut names = named.ancestors();
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
            gid: st.st_gid,
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
    pub(crate) gid: u32,
    /// The raw `st_mode` permission bits, sticky included.
    pub(crate) mode: u32,
}

/// ADR-0050's rule on `chain`, nearest first, up to `/`. With `has_state`,
/// the first entry is the state directory: owned by `owner` and not
/// writable by group or others. Every other entry is a parent: owned by
/// root or the effective user ([`Owner::Me`]), and not writable by group or
/// others unless it is root's sticky directory (`/tmp`), where no one may
/// rename what root owns. For [`Owner::Mapped`], a parent is owned by root,
/// not writable by group or others even where sticky, and searchable by
/// its uid and gid (ADR-0003, ADR-0104).
///
/// # Errors
///
/// [`Refused`] naming the first directory that breaks the rule.
pub(crate) fn check_chain(
    chain: &[ChainDir],
    owner: Owner,
    has_state: bool,
) -> Result<(), Refused> {
    match breach(chain, owner, has_state) {
        None => Ok(()),
        Some((d, why)) => Err(Refused(format!(
            "unsafe state directory: {} {why}; the build roots and the store live there, so no \
             other user may write it or a directory above it; {}",
            d.path.display(),
            owner.advice()
        ))),
    }
}

/// The first directory of `chain` that breaks [`check_chain`]'s rule, with
/// how, as "is …".
fn breach(chain: &[ChainDir], owner: Owner, has_state: bool) -> Option<(&ChainDir, String)> {
    let uid = owner.uid();
    for (i, d) in chain.iter().enumerate() {
        if i == 0 && has_state {
            if d.uid != uid {
                return Some((d, format!("is owned by uid {}, not {uid}", d.uid)));
            }
            if d.mode & 0o022 != 0 {
                return Some((d, "is writable by group or others".to_owned()));
            }
            continue;
        }
        if let Owner::Mapped { uid, gid } = owner {
            // The build's uid could rename what it owns in a sticky
            // directory, and root reaches the state directory by path.
            if d.uid != 0 {
                return Some((d, format!("is owned by uid {}, not root", d.uid)));
            }
            if d.mode & 0o022 != 0 {
                return Some((d, "is writable by group or others".to_owned()));
            }
            if !searchable(d, uid, gid) {
                return Some((
                    d,
                    format!(
                        "is not searchable by uid {uid} and gid {gid}, the build's root on \
                         the host under --userns"
                    ),
                ));
            }
            continue;
        }
        if d.uid != 0 && d.uid != uid {
            return Some((d, format!("is owned by uid {}", d.uid)));
        }
        let sticky_root = d.uid == 0 && d.mode & 0o1000 != 0;
        if d.mode & 0o022 != 0 && !sticky_root {
            return Some((d, "is writable by group or others".to_owned()));
        }
    }
    None
}

/// Whether `uid`, with `gid` and no supplementary group, may search `d`
/// by its mode: the owner's bit for its owner, else the group's for its
/// group, else the others'. An ACL that grants more is not read.
fn searchable(d: &ChainDir, uid: u32, gid: u32) -> bool {
    let bit = if d.uid == uid {
        0o100
    } else if d.gid == gid {
        0o010
    } else {
        0o001
    };
    d.mode & bit != 0
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

/// Raises `RLIMIT_NOFILE`'s soft limit to its hard limit, for kaniko mode:
/// its walk holds a directory fd per worker and depth (`ADR-0038`), and
/// `rnk_rootfs::copy` two per depth. Deletion and `prune --all` need no
/// raise: they hold a fixed number. Returns the new soft limit, `None` when
/// unlimited.
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
