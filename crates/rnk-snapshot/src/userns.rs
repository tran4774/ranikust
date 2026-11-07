//! The user namespace this process runs in, when it is not the initial one:
//! extraction there makes no device node (ADR-0106) and keys the layers it
//! extracts by the namespace's id map (ADR-0104), so a layer extracted with
//! one set of ids is never a lower under another.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;

use rnk_oci::Digest;
use rustix::fs::{CWD, FsWord, Mode, OFlags, PROC_SUPER_MAGIC};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// The inode of the initial user namespace on nsfs (`PROC_USER_INIT_INO`,
/// fixed since Linux 3.8).
const INIT_USER_NS_INO: u64 = 0xEFFF_FFFD;

/// nsfs's `f_type` (`NSFS_MAGIC`).
const NSFS_MAGIC: FsWord = 0x6e73_6673;

/// Read no more of a map: the kernel writes at most 340 lines.
const MAP_MAX: u64 = 64 * 1024;

/// Puts a mapped layer's key in a space of its own: no chain ID, the text
/// `<digest> <digest>` hashed or a tar's hash, starts with it.
const KEY_DOMAIN: &[u8] = b"ranikust layer extracted in a user namespace, v1\0";

/// A user namespace other than the initial one, with its maps as this
/// process reads them: relative to the namespace that created it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserNamespace {
    /// `/proc/self/uid_map` in [`canonical`] form.
    uid_map: String,
    /// `/proc/self/gid_map` in [`canonical`] form.
    gid_map: String,
}

impl UserNamespace {
    /// The user namespace this process runs in, or `None` in the initial
    /// one. Told by the namespace's inode rather than by its map: a
    /// namespace that maps every id to itself still lacks the initial one's
    /// capabilities, `CAP_MKNOD` among them.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when `/proc/self` is not procfs, its `ns/user` not
    /// nsfs, or a map cannot be read or parsed.
    pub(crate) fn current() -> Result<Option<Self>> {
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
        if rustix::fs::fstat(&ns)
            .map_err(|e| io("ns/user", e.into()))?
            .st_ino
            == INIT_USER_NS_INO
        {
            return Ok(None);
        }
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

    /// The store key of the layer whose chain ID is `chain`, extracted in
    /// this namespace.
    pub(crate) fn layer_key(&self, chain: &Digest) -> Digest {
        let mut h = Sha256::new();
        h.update(KEY_DOMAIN);
        h.update(chain.to_string());
        h.update(b"\0");
        h.update(&self.uid_map);
        h.update(b"\0");
        h.update(&self.gid_map);
        Digest::from_sha256_bytes(&h.finalize().into())
    }
}

/// `map`'s lines, each `inside outside count`, sorted and with single
/// spaces: one text for one map, whatever order it was written in and
/// however the kernel pads it. `None` unless every line is three numbers,
/// a count above zero, and there is at least one.
fn canonical(map: &str) -> Option<String> {
    let mut lines = map
        .lines()
        .map(|line| {
            let mut n = line.split_ascii_whitespace().map(|w| w.parse::<u32>().ok());
            match (n.next(), n.next(), n.next(), n.next()) {
                (Some(Some(inside)), Some(Some(outside)), Some(Some(count)), None) if count > 0 => {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(uid_map: &str, gid_map: &str) -> UserNamespace {
        UserNamespace {
            uid_map: canonical(uid_map).unwrap(),
            gid_map: canonical(gid_map).unwrap(),
        }
    }

    #[test]
    fn a_map_reads_the_same_however_it_is_padded_or_ordered() {
        let kernel = "         1     100000      65536\n         0       1000          1\n";
        assert_eq!(
            canonical(kernel).as_deref(),
            Some("0 1000 1\n1 100000 65536\n")
        );
        assert_eq!(canonical("0 1000 1\n1 100000 65536"), canonical(kernel));
        for bad in [
            "",
            "\n",
            "0 1000\n",
            "0 1000 1 2\n",
            "0 1000 0\n",
            "0 -1 1\n",
            "0 1000 1\nx 1 1\n",
            "0 4294967296 1\n",
        ] {
            assert_eq!(canonical(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_mapped_key_depends_on_the_chain_and_both_maps_and_is_no_chain_id() {
        let chain = Digest::from_sha256_bytes(&[1; 32]);
        let other = Digest::from_sha256_bytes(&[2; 32]);
        let a = ns("0 1000 1\n1 100000 65536\n", "0 1000 1\n1 100000 65536\n");
        let key = a.layer_key(&chain);
        assert_ne!(key, chain);
        assert_eq!(key, a.clone().layer_key(&chain));
        assert_ne!(key, a.layer_key(&other));
        let gid = ns("0 1000 1\n1 100000 65536\n", "0 1000 1\n1 200000 65536\n");
        let uid = ns("0 1000 1\n1 200000 65536\n", "0 1000 1\n1 100000 65536\n");
        assert_ne!(key, gid.layer_key(&chain));
        assert_ne!(key, uid.layer_key(&chain));
        assert_ne!(gid.layer_key(&chain), uid.layer_key(&chain));
        // One id each way, the identity map included: no namespace shares
        // the initial one's chain IDs.
        let identity = ns("0 0 4294967295\n", "0 0 4294967295\n");
        assert_ne!(identity.layer_key(&chain), chain);
    }

    /// Whatever namespace the tests run in: `None` exactly in the initial
    /// one, which `/proc/self/ns/user` names by its inode.
    #[test]
    fn the_current_namespace_is_told_by_its_inode() {
        let link = std::fs::read_link("/proc/self/ns/user").unwrap();
        let initial = link.as_os_str() == "user:[4026531837]";
        let current = UserNamespace::current().unwrap();
        assert_eq!(current.is_none(), initial, "{link:?}: {current:?}");
        if let Some(ns) = current {
            let own = |name: &str| canonical(&std::fs::read_to_string(name).unwrap()).unwrap();
            assert_eq!(ns.uid_map, own("/proc/self/uid_map"));
            assert_eq!(ns.gid_map, own("/proc/self/gid_map"));
        }
    }
}
