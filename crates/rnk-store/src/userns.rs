//! Whether this process runs in a user namespace other than the initial
//! one, and its id maps there: the store's directories must then carry ids
//! it maps (ADR-0099), `rnk-snapshot` keys the layers it extracts there by
//! the maps (ADR-0104) and leaves device nodes out of them (ADR-0106), and
//! the store guards those layers with the maps' canary (ADR-0128).

use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{CWD, FsWord, Mode, OFlags, PROC_SUPER_MAGIC};

use crate::error::{Error, Result};

/// The inode of the initial user namespace on nsfs (`PROC_USER_INIT_INO`,
/// fixed since Linux 3.8).
const INIT_USER_NS_INO: u64 = 0xEFFF_FFFD;

/// nsfs's `f_type` (`NSFS_MAGIC`).
const NSFS_MAGIC: FsWord = 0x6e73_6673;

/// Read no more of a map: the kernel writes at most 340 lines.
const MAP_MAX: u64 = 64 * 1024;

/// The uid and gid maps of a user namespace other than the initial one, as
/// this process reads them in `/proc/self`: relative to the namespace that
/// created it, so nested namespaces with one map under parents with
/// different ones have equal maps here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdMaps {
    /// `uid_map` in [`canonical`] form.
    uid_map: String,
    /// `gid_map` in [`canonical`] form.
    gid_map: String,
}

impl IdMaps {
    /// The maps of the user namespace this process runs in, or `None` in
    /// the initial one, as [`user_namespace_proc`] tells.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] as [`user_namespace_proc`], and when a map cannot be
    /// read or is not one [`IdMaps::parse`] takes.
    pub fn current() -> Result<Option<Self>> {
        let Some(dir) = user_namespace_proc()? else {
            return Ok(None);
        };
        let proc = Path::new("/proc/self");
        let io = |path: &str, source: io::Error| Error::Io {
            op: "read",
            path: proc.join(path),
            source,
        };
        let map = |name: &'static str| -> Result<String> {
            let fd =
                rustix::fs::openat(&dir, name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
                    .map_err(|e| io(name, e.into()))?;
            let mut text = String::new();
            File::from(fd)
                .take(MAP_MAX)
                .read_to_string(&mut text)
                .map_err(|e| io(name, e))?;
            canonical(&text).ok_or_else(|| {
                io(
                    name,
                    io::Error::new(io::ErrorKind::InvalidData, "not an id map"),
                )
            })
        };
        Ok(Some(Self {
            uid_map: map("uid_map")?,
            gid_map: map("gid_map")?,
        }))
    }

    /// The maps `uid_map` and `gid_map`, each line `inside outside count`
    /// in any order and padding; `None` unless each has a line, and every
    /// line is three numbers with a count above zero and ranges that end
    /// below `u32::MAX`, as the kernel's do.
    #[must_use]
    pub fn parse(uid_map: &str, gid_map: &str) -> Option<Self> {
        Some(Self {
            uid_map: canonical(uid_map)?,
            gid_map: canonical(gid_map)?,
        })
    }

    /// The uid map: one `inside outside count` line per extent, sorted,
    /// with single spaces.
    #[must_use]
    pub fn uid_map(&self) -> &str {
        &self.uid_map
    }

    /// The gid map, in the form of [`IdMaps::uid_map`].
    #[must_use]
    pub fn gid_map(&self) -> &str {
        &self.gid_map
    }

    /// The first and the last id inside of each uid extent, in order.
    pub(crate) fn uid_extents(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        extents(&self.uid_map)
    }

    /// The first and the last id inside of each gid extent, in order.
    pub(crate) fn gid_extents(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        extents(&self.gid_map)
    }

    /// The uid of the parent namespace that the uid `id` inside is, or
    /// `None` where no extent holds it.
    pub(crate) fn uid_outside(&self, id: u32) -> Option<u32> {
        outside(&self.uid_map, id)
    }

    /// The gid of the parent namespace that the gid `id` inside is, as
    /// [`IdMaps::uid_outside`].
    pub(crate) fn gid_outside(&self, id: u32) -> Option<u32> {
        outside(&self.gid_map, id)
    }
}

/// `map`'s lines, each `inside outside count`, sorted and with single
/// spaces: one text for one map, whatever order it was written in and
/// however the kernel pads it. `None` unless every line is three numbers,
/// a count above zero and ranges that end below `u32::MAX` (the kernel
/// refuses others, and `u32::MAX` is `chown`'s "unchanged"), and there is
/// at least one.
fn canonical(map: &str) -> Option<String> {
    let mut lines = map
        .lines()
        .map(|line| {
            let mut n = line.split_ascii_whitespace().map(|w| w.parse::<u32>().ok());
            match (n.next(), n.next(), n.next(), n.next()) {
                (Some(Some(inside)), Some(Some(outside)), Some(Some(count)), None)
                    if count > 0
                        && inside.checked_add(count).is_some()
                        && outside.checked_add(count).is_some() =>
                {
                    Some([inside, outside, count])
                }
                _ => None,
            }
        })
        .collect::<Option<Vec<_>>>()?;
    if lines.is_empty() {
        return None;
    }
    lines.sort_unstable();
    Some(
        lines
            .iter()
            .map(|[inside, outside, count]| format!("{inside} {outside} {count}\n"))
            .collect(),
    )
}

/// The `inside outside count` lines of a [`canonical`] map.
fn lines(map: &str) -> impl Iterator<Item = [u32; 3]> + '_ {
    map.lines().filter_map(|line| {
        let mut n = line.split(' ').map(|w| w.parse::<u32>().ok());
        let (Some(Some(inside)), Some(Some(outside)), Some(Some(count))) =
            (n.next(), n.next(), n.next())
        else {
            return None;
        };
        Some([inside, outside, count])
    })
}

/// The first and the last id inside of each line of a [`canonical`] map.
fn extents(map: &str) -> impl Iterator<Item = (u32, u32)> + '_ {
    lines(map).map(|[inside, _, count]| (inside, inside + (count - 1)))
}

/// The id outside that `id` inside is in a [`canonical`] map, whose lines
/// end below `u32::MAX` on both sides.
fn outside(map: &str, id: u32) -> Option<u32> {
    lines(map).find_map(|[inside, outside, count]| {
        (inside..=inside + (count - 1))
            .contains(&id)
            .then(|| outside + (id - inside))
    })
}

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

    #[test]
    fn a_map_reads_the_same_however_it_is_padded_or_ordered() {
        let kernel = "         1     100000      65536\n         0       1000          1\n";
        assert_eq!(
            canonical(kernel).as_deref(),
            Some("0 1000 1\n1 100000 65536\n")
        );
        assert_eq!(canonical("0 1000 1\n1 100000 65536"), canonical(kernel));
        // The identity map, the widest the kernel writes.
        assert_eq!(
            canonical("0 0 4294967295\n").as_deref(),
            Some("0 0 4294967295\n")
        );
        for bad in [
            "",
            "\n",
            "0 1000\n",
            "0 1000 1 2\n",
            "0 1000 0\n",
            "0 -1 1\n",
            "0 1000 1\nx 1 1\n",
            "0 4294967296 1\n",
            // Ranges that reach `u32::MAX`, which the kernel refuses.
            "1 0 4294967295\n",
            "0 1 4294967295\n",
        ] {
            assert_eq!(canonical(bad), None, "{bad:?}");
        }
        assert_eq!(IdMaps::parse("0 0 1\n", "x"), None);
        assert_eq!(IdMaps::parse("x", "0 0 1\n"), None);
    }

    #[test]
    fn each_extent_gives_its_first_and_its_last_id() {
        let maps = IdMaps::parse("1 100000 65536\n0 1000 1\n", "0 0 4294967295\n").unwrap();
        assert_eq!(maps.uid_extents().collect::<Vec<_>>(), [(0, 0), (1, 65536)]);
        assert_eq!(maps.gid_extents().collect::<Vec<_>>(), [(0, 4_294_967_294)]);
    }

    /// An id inside is the parent's id its extent gives, the ends of each
    /// extent included; one no extent holds is none.
    #[test]
    fn an_id_inside_is_the_parent_s_its_extent_gives() {
        let maps = IdMaps::parse("1 100000 65536\n0 1000 1\n", "0 0 4294967295\n").unwrap();
        let uids: Vec<_> = [0, 1, 2, 65536, 65537]
            .map(|id| maps.uid_outside(id))
            .into();
        assert_eq!(
            uids,
            [
                Some(1000),
                Some(100_000),
                Some(100_001),
                Some(165_535),
                None
            ]
        );
        assert_eq!(maps.gid_outside(4_294_967_294), Some(4_294_967_294));
        assert_eq!(maps.gid_outside(u32::MAX), None);
    }

    /// In a user namespace other than the initial one, `current` holds this
    /// process's maps; in the initial one, none.
    #[test]
    fn the_current_maps_are_this_process_s() {
        let maps = IdMaps::current().unwrap();
        assert_eq!(maps.is_some(), in_user_namespace().unwrap());
        if let Some(maps) = maps {
            let own = |name: &str| canonical(&std::fs::read_to_string(name).unwrap()).unwrap();
            assert_eq!(maps.uid_map(), own("/proc/self/uid_map"));
            assert_eq!(maps.gid_map(), own("/proc/self/gid_map"));
        }
    }

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
