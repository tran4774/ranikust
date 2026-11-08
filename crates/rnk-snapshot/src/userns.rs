//! The user namespace this process runs in, when it is not the initial one:
//! extraction there makes no device node (ADR-0106) and keys the layers it
//! extracts by the namespace's id map (ADR-0104), so a layer extracted with
//! one map is never a lower under another. The map is relative to the parent
//! namespace: nested namespaces with one map under parents with different
//! ones share keys. For the layer keys, the owner checks and the binary's
//! guidance the maps are read here and parsed by [`MappedIds::parse`]
//! (ADR-0098).

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
pub struct UserNamespace {
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
    /// cannot be told; [`Error::Io`] naming `uid_map` or `gid_map` when one
    /// cannot be read or parsed.
    pub fn current() -> Result<Option<Self>> {
        let Some(dir) = rnk_store::user_namespace_proc()? else {
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
        Self::from_maps(&map("uid_map")?, &map("gid_map")?).map(Some)
    }

    /// The namespace whose `uid_map` and `gid_map` read `uid_map` and
    /// `gid_map`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] naming the map that does not parse.
    pub(crate) fn from_maps(uid_map: &str, gid_map: &str) -> Result<Self> {
        let not_a_map = |name| {
            map_err(
                name,
                io::Error::new(io::ErrorKind::InvalidData, "not an id map"),
            )
        };
        let ids = MappedIds::parse(uid_map, gid_map).ok_or_else(|| {
            // Each map read alone tells which one does not parse.
            not_a_map(if MappedIds::parse(uid_map, uid_map).is_none() {
                "uid_map"
            } else {
                "gid_map"
            })
        })?;
        Ok(Self {
            uid_map: canonical(ids.uid_lines()),
            gid_map: canonical(ids.gid_lines()),
            ids,
        })
    }

    /// The ids inside the namespace that its maps give.
    #[must_use]
    pub fn ids(&self) -> &MappedIds {
        &self.ids
    }

    /// The uid map in canonical form: each line `inside outside count`
    /// with single spaces, the lines sorted, whatever order the map was
    /// written in and however the kernel pads it.
    #[must_use]
    pub fn uid_map(&self) -> &str {
        &self.uid_map
    }

    /// The gid map, as [`UserNamespace::uid_map`].
    #[must_use]
    pub fn gid_map(&self) -> &str {
        &self.gid_map
    }

    /// The uid outside the namespace that its root, uid 0 inside, has:
    /// `None` when the namespace does not map uid 0.
    #[must_use]
    pub fn root_uid_outside(&self) -> Option<u32> {
        self.ids
            .uid_lines()
            .iter()
            .find_map(|&[inside, outside, _]| (inside == 0).then_some(outside))
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

/// The error of the map `name` of `/proc/self`.
fn map_err(name: &str, source: io::Error) -> Error {
    Error::Io {
        op: "read",
        path: Path::new("/proc/self").join(name),
        source,
    }
}

/// `lines`, sorted, each `inside outside count\n`: one text for one map,
/// the input of the layer key.
fn canonical(lines: &[[u32; 3]]) -> String {
    lines
        .iter()
        .map(|[inside, outside, count]| format!("{inside} {outside} {count}\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(uid_map: &str, gid_map: &str) -> UserNamespace {
        UserNamespace::from_maps(uid_map, gid_map).unwrap()
    }

    #[test]
    fn a_map_reads_the_same_however_it_is_padded_or_ordered() {
        let kernel = "         1     100000      65536\n         0       1000          1\n";
        let padded = ns(kernel, kernel);
        assert_eq!(padded.uid_map(), "0 1000 1\n1 100000 65536\n");
        assert_eq!(padded.gid_map(), padded.uid_map());
        assert_eq!(padded, ns("0 1000 1\n1 100000 65536", kernel));
        assert_eq!(padded.root_uid_outside(), Some(1000));
        assert_eq!(ns("0 0 1\n", "0 0 1\n").root_uid_outside(), Some(0));
        assert_eq!(ns("1 1000 1\n", "0 0 1\n").root_uid_outside(), None);
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
            "4294967295 0 2\n",
        ] {
            for (uid_map, gid_map, name) in [(bad, good, "uid_map"), (good, bad, "gid_map")] {
                let e = UserNamespace::from_maps(uid_map, gid_map).unwrap_err();
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
        // The key's input is unchanged: layers already extracted keep it.
        assert_eq!(
            key.to_string(),
            "sha256:67b5e4e77cbbab7489aa2a70843aebef9ac293daf674985aa1e6b37ed12bf50c"
        );
    }

    /// In a user namespace other than the initial one, `current` holds this
    /// process's maps.
    #[test]
    fn the_current_namespace_has_this_process_s_maps() {
        if let Some(ns) = UserNamespace::current().unwrap() {
            let read = |name: &str| std::fs::read_to_string(name).unwrap();
            let own =
                UserNamespace::from_maps(&read("/proc/self/uid_map"), &read("/proc/self/gid_map"))
                    .unwrap();
            assert_eq!(ns, own);
        }
    }
}
