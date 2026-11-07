//! Whether this process runs in a user namespace other than the initial
//! one: the store's directories must then carry ids it maps (ADR-0099), and
//! `rnk-snapshot` keys the layers it extracts there by the map (ADR-0104).

use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{CWD, FsWord, Mode, OFlags, PROC_SUPER_MAGIC};

use crate::error::{Error, Result};

/// The inode of the initial user namespace on nsfs (`PROC_USER_INIT_INO`,
/// fixed since Linux 3.8).
const INIT_USER_NS_INO: u64 = 0xEFFF_FFFD;

/// nsfs's `f_type` (`NSFS_MAGIC`).
const NSFS_MAGIC: FsWord = 0x6e73_6673;

/// Whether this process runs in a user namespace other than the initial
/// one.
///
/// # Errors
///
/// As [`user_namespace_proc`].
pub(crate) fn in_user_namespace() -> Result<bool> {
    Ok(user_namespace_proc()?.is_some())
}

/// `/proc/self`, checked to be procfs, as an `O_PATH` descriptor when this
/// process runs in a user namespace other than the initial one, for
/// reading that namespace's maps; `None` in the initial one. Told by the
/// namespace's inode rather than by its map: a namespace that maps every
/// id to itself still lacks the initial one's capabilities, `CAP_MKNOD`
/// among them.
///
/// # Errors
///
/// [`Error::Io`] when `/proc/self` cannot be opened or is not procfs, or its
/// `ns/user` cannot be opened, is not nsfs or cannot be read.
pub fn user_namespace_proc() -> Result<Option<OwnedFd>> {
    let proc = Path::new("/proc/self");
    let io = |path: &str, source: io::Error| Error::Io {
        op: "read",
        path: proc.join(path),
        source,
    };
    let dir = rustix::fs::openat(
        CWD,
        proc,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io("", e.into()))?;
    if rustix::fs::fstatfs(&dir)
        .map_err(|e| io("", e.into()))?
        .f_type
        != PROC_SUPER_MAGIC
    {
        return Err(io("", io::Error::other("not procfs")));
    }
    let ns = rustix::fs::openat(
        &dir,
        "ns/user",
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io("ns/user", e.into()))?;
    if rustix::fs::fstatfs(&ns)
        .map_err(|e| io("ns/user", e.into()))?
        .f_type
        != NSFS_MAGIC
    {
        return Err(io("ns/user", io::Error::other("not nsfs")));
    }
    let initial = rustix::fs::fstat(&ns)
        .map_err(|e| io("ns/user", e.into()))?
        .st_ino
        == INIT_USER_NS_INO;
    Ok((!initial).then_some(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever namespace the tests run in: `true` exactly outside the
    /// initial one, which `/proc/self/ns/user` names by its inode.
    #[test]
    fn the_namespace_is_told_by_its_inode() {
        let link = std::fs::read_link("/proc/self/ns/user").unwrap();
        let initial = link.as_os_str() == "user:[4026531837]";
        assert_eq!(in_user_namespace().unwrap(), !initial, "{link:?}");
        if let Some(proc) = user_namespace_proc().unwrap() {
            assert_eq!(rustix::fs::fstatfs(&proc).unwrap().f_type, PROC_SUPER_MAGIC);
        }
    }
}
