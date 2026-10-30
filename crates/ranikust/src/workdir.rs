//! The kaniko-mode workdir checks, run during process set-up before
//! anything is written to the workdir (`design/kaniko-mode.md#host-safety-checks`,
//! ADR-0038): in kaniko mode the workdir is the root a build fills and
//! empties, so a mistyped `--workdir` must never be a directory the host
//! needs. `rnk-rootfs` provides the primitives; the policy is here.

use std::ffi::OsStr;
use std::fmt;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use rnk_rootfs::{FileType, OpenHow, RootPath};
use rustix::fs::FlockOperation;
use rustix::io::Errno;

/// The marker of a workdir ranikust may empty: a regular file at its top.
pub(crate) const MARKER: &str = ".ranikust-root";

/// Directories a workdir must not be, nor be an ancestor of (exp-09's
/// list). Compared by `(dev, ino)` as well as by name, so `//`, a symlink or
/// a bind mount of one of them is caught too.
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

/// Why a workdir cannot be used.
#[derive(Debug)]
pub(crate) struct Refused(pub(crate) String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A kaniko-mode workdir that passed the checks, locked for this build. The
/// lock is released when the process exits, also on a crash; dropping this
/// releases it earlier.
#[derive(Debug)]
pub(crate) struct Workdir {
    /// The canonical path.
    path: PathBuf,
    /// The workdir as a root, opened without following symlinks.
    root: RootPath,
    /// The readable fd holding `flock(LOCK_EX)`.
    _lock: OwnedFd,
    /// The workdir was empty: [`Workdir::mark`] gives it the marker.
    unmarked: bool,
}

impl Workdir {
    /// The canonical path of the workdir, for messages only: everything
    /// done in the workdir, deletion above all, goes through
    /// [`Workdir::root`], the directory that was checked, never through a
    /// reopened path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The workdir as a root.
    pub(crate) fn root(&self) -> &RootPath {
        &self.root
    }

    /// Gives an empty workdir the marker, right before the build first
    /// writes to it, so a run that stops earlier leaves nothing behind.
    /// The workdir is listed again after: anything another process put
    /// there since the check removes the marker and refuses.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the marker cannot be created or the workdir is no
    /// longer empty.
    pub(crate) fn mark(&mut self) -> Result<(), Refused> {
        if !self.unmarked {
            return Ok(());
        }
        let shown = self.path.display();
        self.root
            .open_at(MARKER, &OpenHow::write().create_new(0o644))
            .map_err(|e| Refused(format!("cannot create {shown}/{MARKER}: {e}")))?;
        let names = self
            .root
            .read_names("/")
            .map_err(|e| Refused(format!("cannot list the workdir {shown}: {e}")))?;
        if names.iter().any(|n| n != OsStr::new(MARKER)) {
            let removed = self.root.remove_file(MARKER);
            return Err(Refused(format!(
                "the workdir {shown} was filled while ranikust checked it; the marker {}",
                match removed {
                    Ok(()) => "was removed again".to_owned(),
                    Err(e) => format!("could not be removed: {e}"),
                }
            )));
        }
        self.unmarked = false;
        Ok(())
    }
}

/// `(dev, ino)` of a directory: what identifies it whatever its name.
type Id = (u32, u32, u64);

/// Runs the kaniko-mode workdir checks on `dir` and locks it:
///
/// 1. canonicalize it, then refuse `/` and any directory that is, or is an
///    ancestor of, one of [`SYSTEM_DIRS`], by name and by `(dev, ino)` of
///    the opened directory against each system directory and its ancestors;
/// 2. open it with `RESOLVE_NO_SYMLINKS` and take `flock(LOCK_EX | LOCK_NB)`:
///    one build per workdir;
/// 3. refuse a non-empty workdir without a regular file [`MARKER`] (a
///    workdir of only mount points is not empty); an empty one gets the
///    marker from [`Workdir::mark`].
///
/// `dir` must exist. Nothing is written.
///
/// # Errors
///
/// [`Refused`] naming the check that failed.
pub(crate) fn claim(dir: &Path) -> Result<Workdir, Refused> {
    let path = std::fs::canonicalize(dir)
        .map_err(|e| Refused(format!("cannot resolve the workdir {}: {e}", dir.display())))?;
    let shown = || {
        if path == dir {
            path.display().to_string()
        } else {
            format!("{} ({})", dir.display(), path.display())
        }
    };
    if path == Path::new("/") {
        return Err(Refused(format!(
            "the workdir {} is the host's /: kaniko mode empties its workdir",
            shown()
        )));
    }
    if let Some(sys) = system_by_name(&path) {
        return Err(refuse_system(&shown(), sys));
    }
    let root = RootPath::open(&path)
        .map_err(|e| Refused(format!("cannot open the workdir {}: {e}", shown())))?;
    let st = root
        .statx("/")
        .map_err(|e| Refused(format!("cannot stat the workdir {}: {e}", shown())))?;
    let id: Id = (st.dev_major, st.dev_minor, st.ino);
    if let Some(sys) = same_as_system(id) {
        return Err(refuse_system(&shown(), &sys));
    }
    let lock = root
        .open_at("/", &OpenHow::read().directory())
        .map_err(|e| Refused(format!("cannot open the workdir {}: {e}", shown())))?;
    match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(Errno::WOULDBLOCK) => {
            return Err(Refused(format!(
                "another build is using the workdir {}: one kaniko-mode build per workdir",
                shown()
            )));
        }
        Err(e) => {
            return Err(Refused(format!("cannot lock the workdir {}: {e}", shown())));
        }
    }
    let unmarked = check_marker(&root, &shown())?;
    Ok(Workdir {
        path,
        root,
        _lock: lock,
        unmarked,
    })
}

/// The system directory that the canonical `path` is, or is an ancestor
/// of, by name.
fn system_by_name(path: &Path) -> Option<&'static str> {
    SYSTEM_DIRS
        .iter()
        .copied()
        .find(|sys| Path::new(sys).starts_with(path))
}

/// Step 3 of [`claim`], with the lock held: whether the workdir is empty
/// and still needs the marker.
fn check_marker(root: &RootPath, shown: &str) -> Result<bool, Refused> {
    let names = root
        .read_names("/")
        .map_err(|e| Refused(format!("cannot list the workdir {shown}: {e}")))?;
    if names.iter().any(|n| n == OsStr::new(MARKER)) {
        let st = root
            .statx(MARKER)
            .map_err(|e| Refused(format!("cannot stat {shown}/{MARKER}: {e}")))?;
        return match st.file_type() {
            FileType::Regular => Ok(false),
            other => Err(Refused(format!(
                "{shown}/{MARKER} is a {other:?}, not a regular file"
            ))),
        };
    }
    if !names.is_empty() {
        return Err(Refused(format!(
            "the workdir {shown} is not empty ({} entries) and has no {MARKER}: kaniko mode \
             empties its workdir, so it takes an empty directory, or one with the marker \
             (`touch {shown}/{MARKER}`) when its contents may be deleted",
            names.len()
        )));
    }
    Ok(true)
}

fn refuse_system(shown: &str, sys: &str) -> Refused {
    Refused(format!(
        "the workdir {shown} is the system directory {sys} or one of its ancestors: kaniko \
         mode empties its workdir; pass a dedicated directory"
    ))
}

/// The system directory, or ancestor of one, that is the directory `id`;
/// missing system directories are skipped. Symlinks are followed, so a
/// `/lib` linking to `/usr/lib` is compared as `/usr/lib`.
fn same_as_system(id: Id) -> Option<String> {
    SYSTEM_DIRS.iter().find_map(|sys| {
        Path::new(sys)
            .ancestors()
            .find(|dir| dir_id(dir) == Some(id))
            .map(|dir| {
                if dir == Path::new(sys) {
                    (*sys).to_owned()
                } else {
                    format!("{} (an ancestor of {sys})", dir.display())
                }
            })
    })
}

fn dir_id(dir: &Path) -> Option<Id> {
    let md = std::fs::metadata(dir).ok()?;
    let dev = md.dev();
    Some((rustix::fs::major(dev), rustix::fs::minor(dev), md.ino()))
}

/// The owner and mode of one directory of a workdir's chain.
#[derive(Clone, Debug)]
pub(crate) struct ChainDir {
    pub(crate) path: PathBuf,
    pub(crate) uid: u32,
    /// The raw `st_mode` permission bits, sticky included.
    pub(crate) mode: u32,
}

/// Whether a build as root may fill the workdir: an image can bring setuid
/// and setgid files, which must stay out of other users' reach
/// (ADR-0047). Run as root on the claimed workdir before anything is
/// written; refuses unless [`check_chain`] passes. Warns when the
/// workdir's filesystem honours setuid bits or device nodes.
///
/// # Errors
///
/// [`Refused`] naming the directory and the rule it breaks.
pub(crate) fn check_private(w: &Workdir) -> Result<(), Refused> {
    let st = w
        .root
        .statx("/")
        .map_err(|e| Refused(format!("cannot stat the workdir {}: {e}", w.path.display())))?;
    let mut chain = vec![ChainDir {
        path: w.path.clone(),
        uid: st.uid,
        mode: st.mode & 0o7777,
    }];
    for dir in w.path.ancestors().skip(1) {
        let md = std::fs::metadata(dir)
            .map_err(|e| Refused(format!("cannot stat {}: {e}", dir.display())))?;
        chain.push(ChainDir {
            path: dir.to_owned(),
            uid: md.uid(),
            mode: md.mode() & 0o7777,
        });
    }
    check_chain(&chain)?;
    match rustix::fs::statvfs(&w.path) {
        Ok(vfs) => {
            use rustix::fs::StatVfsMountFlags as F;
            if !vfs.f_flag.contains(F::NOSUID | F::NODEV) {
                tracing::warn!(
                    "the workdir {} is on a filesystem mounted without nosuid,nodev: setuid \
                     files the image brings work there while the build runs",
                    w.path.display()
                );
            }
        }
        Err(e) => tracing::warn!("cannot statvfs the workdir {}: {e}", w.path.display()),
    }
    Ok(())
}

/// ADR-0047's rule on `chain`, the workdir first, then each ancestor up
/// to `/`:
///
/// 1. the workdir is owned by root and not writable by group or others;
/// 2. no ancestor is writable by group or others, unless it is root's and
///    sticky (`/tmp`);
/// 3. every directory is owned by root or by one and the same other user
///    (the operator, as for `/home/<user>/w`);
/// 4. an ancestor lets neither group nor others search it (`chmod 700`),
///    so no other user reaches the workdir: a `/home/<user>/w` qualifies
///    once `/home/<user>` is `0700`.
///
/// # Errors
///
/// [`Refused`] naming the first rule broken.
pub(crate) fn check_chain(chain: &[ChainDir]) -> Result<(), Refused> {
    let refuse = |d: &ChainDir, why: &str| {
        Refused(format!(
            "the workdir's chain is not private: {} {why}; kaniko mode fills the workdir with \
             the image's files, setuid ones included, so other users must not reach it \
             (ADR-0047)",
            d.path.display()
        ))
    };
    let Some((workdir, ancestors)) = chain.split_first() else {
        return Ok(());
    };
    if workdir.uid != 0 {
        return Err(refuse(
            workdir,
            &format!("is owned by uid {}, not root", workdir.uid),
        ));
    }
    if workdir.mode & 0o022 != 0 {
        return Err(refuse(workdir, "is writable by group or others"));
    }
    let mut other_owner: Option<u32> = None;
    for d in ancestors {
        let sticky_root = d.uid == 0 && d.mode & 0o1000 != 0;
        if d.mode & 0o022 != 0 && !sticky_root {
            return Err(refuse(d, "is writable by group or others"));
        }
        if d.uid != 0 {
            match other_owner {
                None => other_owner = Some(d.uid),
                Some(u) if u == d.uid => {}
                Some(u) => {
                    return Err(refuse(
                        d,
                        &format!(
                            "is owned by uid {}, while another is owned by uid {u}",
                            d.uid
                        ),
                    ));
                }
            }
        }
    }
    if !ancestors.iter().any(|d| d.mode & 0o011 == 0) {
        return Err(Refused(format!(
            "the workdir's chain is not private: every directory above {} lets group or others \
             search it; put the workdir in a directory of mode 0700 (ADR-0047)",
            workdir.path.display()
        )));
    }
    Ok(())
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
