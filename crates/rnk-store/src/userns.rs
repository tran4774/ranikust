//! Whether this process runs in a user namespace other than the initial
//! one, and its id maps there: the store's directories must then carry ids
//! it maps (ADR-0099), `rnk-snapshot` keys the layers it extracts there by
//! the maps (ADR-0104) and leaves device nodes out of them (ADR-0106), and
//! the store guards those layers with the maps' canary (ADR-0128). The maps
//! are read here, once per store, and parsed by [`MappedIds::parse`]
//! (ADR-0098).

use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::OwnedFd;
use std::path::Path;

use rnk_rootfs::MappedIds;
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
    /// The ids inside that the maps give.
    ids: MappedIds,
}

impl IdMaps {
    /// The maps of the user namespace this process runs in, or `None` in
    /// the initial one, as [`user_namespace_proc`] tells.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] as [`user_namespace_proc`], and naming `uid_map` or
    /// `gid_map` when one cannot be read or is not one [`IdMaps::parse`]
    /// takes.
    pub fn current() -> Result<Option<Self>> {
        let Some(dir) = user_namespace_proc()? else {
            return Ok(None);
        };
        let map = |name: &'static str| -> Result<String> {
            let fd =
                rustix::fs::openat(&dir, name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
                    .map_err(|e| map_err(name, e.into()))?;
            let mut text = String::new();
            File::from(fd)
                .take(MAP_MAX)
                .read_to_string(&mut text)
                .map_err(|e| map_err(name, e))?;
            Ok(text)
        };
        Self::named(&map("uid_map")?, &map("gid_map")?).map(Some)
    }

    /// [`IdMaps::parse`], with the map that does not parse named.
    fn named(uid_map: &str, gid_map: &str) -> Result<Self> {
        Self::parse(uid_map, gid_map).ok_or_else(|| {
            // Each map read alone tells which one does not parse.
            let name = if Self::parse(uid_map, uid_map).is_none() {
                "uid_map"
            } else {
                "gid_map"
            };
            map_err(
                name,
                io::Error::new(io::ErrorKind::InvalidData, "not an id map"),
            )
        })
    }

    /// The maps `uid_map` and `gid_map`, each line `inside outside count`
    /// in any order and padding, as [`MappedIds::parse`] takes them; `None`
    /// where it does not, and where a range reaches `u32::MAX` on either
    /// side, which the kernel refuses (and `u32::MAX` is `chown`'s
    /// "unchanged").
    #[must_use]
    pub fn parse(uid_map: &str, gid_map: &str) -> Option<Self> {
        let ids = MappedIds::parse(uid_map, gid_map)?;
        let below_max = |lines: &[[u32; 3]]| {
            lines.iter().all(|&[inside, outside, count]| {
                inside.checked_add(count).is_some() && outside.checked_add(count).is_some()
            })
        };
        (below_max(ids.uid_lines()) && below_max(ids.gid_lines())).then(|| Self {
            uid_map: canonical(ids.uid_lines()),
            gid_map: canonical(ids.gid_lines()),
            ids,
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

    /// The ids inside that the maps give.
    #[must_use]
    pub fn ids(&self) -> &MappedIds {
        &self.ids
    }

    /// The first and the last id inside of each uid extent, in order.
    pub(crate) fn uid_extents(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        extents(self.ids.uid_lines())
    }

    /// The first and the last id inside of each gid extent, in order.
    pub(crate) fn gid_extents(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        extents(self.ids.gid_lines())
    }

    /// The uid of the parent namespace that the uid `id` inside is, or
    /// `None` where no extent holds it.
    pub(crate) fn uid_outside(&self, id: u32) -> Option<u32> {
        outside(self.ids.uid_lines(), id)
    }

    /// The gid of the parent namespace that the gid `id` inside is, as
    /// [`IdMaps::uid_outside`].
    pub(crate) fn gid_outside(&self, id: u32) -> Option<u32> {
        outside(self.ids.gid_lines(), id)
    }
}

/// The error of the map `name` of `/proc/self`.
fn map_err(name: &str, source: io::Error) -> Error {
    Error::Io {
        op: "read",
        path: Path::new("/proc/self").join(name),
        source,
    }
}

/// `lines`, sorted, each `inside outside count\n`: one text for one map,
/// whatever order it was written in and however the kernel pads it.
fn canonical(lines: &[[u32; 3]]) -> String {
    lines
        .iter()
        .map(|[inside, outside, count]| format!("{inside} {outside} {count}\n"))
        .collect()
}

/// The first and the last id inside of each of `lines`, whose counts are
/// above zero and whose ranges end below `u32::MAX` ([`IdMaps::parse`]).
fn extents(lines: &[[u32; 3]]) -> impl Iterator<Item = (u32, u32)> + '_ {
    lines
        .iter()
        .map(|&[inside, _, count]| (inside, inside + (count - 1)))
}

/// The id outside that `id` inside is in `lines`, as [`extents`] takes
/// them.
fn outside(lines: &[[u32; 3]], id: u32) -> Option<u32> {
    lines.iter().find_map(|&[inside, outside, count]| {
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
        let padded = IdMaps::parse(kernel, kernel).unwrap();
        assert_eq!(padded.uid_map(), "0 1000 1\n1 100000 65536\n");
        assert_eq!(padded.gid_map(), padded.uid_map());
        assert_eq!(
            Some(padded),
            IdMaps::parse("0 1000 1\n1 100000 65536", kernel)
        );
        // The identity map, the widest the kernel writes.
        let identity = IdMaps::parse("0 0 4294967295\n", "0 0 4294967295\n").unwrap();
        assert_eq!(identity.uid_map(), "0 0 4294967295\n");
    }

    /// A map that does not parse is named, whichever of the two it is.
    #[test]
    fn a_map_that_does_not_parse_is_named() {
        let good = "0 1000 1\n";
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
            "4294967295 0 2\n",
            "1 0 4294967295\n",
            "0 1 4294967295\n",
        ] {
            for (uid_map, gid_map, name) in [(bad, good, "uid_map"), (good, bad, "gid_map")] {
                assert_eq!(IdMaps::parse(uid_map, gid_map), None, "{bad:?}");
                let e = IdMaps::named(uid_map, gid_map).unwrap_err();
                assert_eq!(
                    e.to_string(),
                    format!("read /proc/self/{name}: not an id map"),
                    "{bad:?}"
                );
                assert!(matches!(e, Error::Io { op: "read", .. }), "{e:?}");
            }
        }
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
            let read = |name: &str| std::fs::read_to_string(name).unwrap();
            let own = IdMaps::parse(&read("/proc/self/uid_map"), &read("/proc/self/gid_map"));
            assert_eq!(Some(maps), own);
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
