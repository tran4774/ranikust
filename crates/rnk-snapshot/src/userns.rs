//! The user namespace this process runs in, when it is not the initial one:
//! extraction there makes no device node (ADR-0106) and keys the layers it
//! extracts by the namespace's id map (ADR-0104), so a layer extracted with
//! one map is never a lower under another. The map is relative to the parent
//! namespace: nested namespaces with one map under parents with different
//! ones share keys.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;

use rnk_oci::Digest;
use rnk_rootfs::MappedIds;
use rustix::fs::{Mode, OFlags};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// Read no more of a map: the kernel writes at most 340 lines.
const MAP_MAX: u64 = 64 * 1024;

/// Puts a mapped layer's key in a space of its own: the input of no chain
/// ID of several layers (`<digest> <digest>`) starts with these bytes, and
/// a chain ID of one layer equal to a key would need a layer whose tar is
/// exactly the key's input, fixed by the chain and the maps.
const KEY_DOMAIN: &[u8] = b"ranikust layer extracted in a user namespace, v1\0";

/// A user namespace other than the initial one, with its maps as this
/// process reads them: relative to the namespace that created it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserNamespace {
    /// `/proc/self/uid_map` in [`canonical`] form.
    uid_map: String,
    /// `/proc/self/gid_map` in [`canonical`] form.
    gid_map: String,
    /// The ids inside that the maps give: the owners an extraction here can
    /// set (ADR-0098).
    ids: MappedIds,
}

impl UserNamespace {
    /// The user namespace this process runs in, or `None` in the initial
    /// one, as [`rnk_store::user_namespace_proc`] tells.
    ///
    /// # Errors
    ///
    /// [`Error::Store`] when `/proc/self` is not procfs or the namespace
    /// cannot be told; [`Error::Io`] when a map cannot be read or parsed.
    pub(crate) fn current() -> Result<Option<Self>> {
        let Some(dir) = rnk_store::user_namespace_proc()? else {
            return Ok(None);
        };
        let proc = Path::new("/proc/self");
        let io = |path: &str, source: io::Error| Error::Io {
            op: "read",
            path: proc.join(path),
            source,
        };
        let not_a_map = |name| {
            io(
                name,
                io::Error::new(io::ErrorKind::InvalidData, "not an id map"),
            )
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
            canonical(&text).ok_or_else(|| not_a_map(name))
        };
        let (uid_map, gid_map) = (map("uid_map")?, map("gid_map")?);
        let ids = MappedIds::parse(&uid_map, &gid_map).ok_or_else(|| not_a_map("uid_map"))?;
        Ok(Some(Self {
            uid_map,
            gid_map,
            ids,
        }))
    }

    /// The ids inside the namespace that its maps give.
    pub(crate) fn ids(&self) -> &MappedIds {
        &self.ids
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
            ids: MappedIds::parse(uid_map, gid_map).unwrap(),
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
    fn a_mapped_key_depends_on_the_chain_and_both_maps_and_differs_from_its_chain_id() {
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
        // One id each way, the identity map included: the key differs from
        // the chain ID.
        let identity = ns("0 0 4294967295\n", "0 0 4294967295\n");
        assert_ne!(identity.layer_key(&chain), chain);
    }

    /// In a user namespace other than the initial one, `current` holds this
    /// process's maps.
    #[test]
    fn the_current_namespace_has_this_process_s_maps() {
        if let Some(ns) = UserNamespace::current().unwrap() {
            let own = |name: &str| canonical(&std::fs::read_to_string(name).unwrap()).unwrap();
            assert_eq!(ns.uid_map, own("/proc/self/uid_map"));
            assert_eq!(ns.gid_map, own("/proc/self/gid_map"));
            assert_eq!(
                Some(ns.ids()),
                MappedIds::parse(&ns.uid_map, &ns.gid_map).as_ref()
            );
        }
    }
}
