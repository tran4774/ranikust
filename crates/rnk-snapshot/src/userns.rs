//! The user namespace this process runs in, when it is not the initial one:
//! extraction there makes no device node (ADR-0106) and keys the layers it
//! extracts by the namespace's id maps (ADR-0104), so a layer extracted with
//! one map is never a lower under another. The maps are relative to the
//! parent namespace: nested namespaces with one map under parents with
//! different ones share keys, which the store's canary of the maps guards
//! (ADR-0128). For the layer keys, the owner checks and the binary's
//! guidance the maps are those [`IdMaps`] reads and [`MappedIds::parse`]
//! parses (ADR-0144).

use std::sync::OnceLock;

use rnk_oci::Digest;
use rnk_rootfs::MappedIds;
use rnk_store::IdMaps;
use sha2::{Digest as _, Sha256};

use crate::error::Result;

/// Puts a mapped layer's key in a space of its own: the input of no chain
/// ID of several layers (`<digest> <digest>`) starts with these bytes, and
/// a chain ID of one layer equal to a key would need a layer whose tar is
/// exactly the key's input, fixed by the chain and the maps. Version 2
/// came with the canary: no layer extracted before it is reused, since no
/// canary guards it.
const KEY_DOMAIN: &[u8] = b"ranikust layer extracted in a user namespace, v2\0";

/// A user namespace other than the initial one, with its maps as this
/// process or the store read them: relative to the namespace that created
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserNamespace {
    maps: IdMaps,
}

impl UserNamespace {
    /// The user namespace this process runs in, or `None` in the initial
    /// one, as [`IdMaps::current`] reads it.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Store`] when `/proc/self` is not procfs or the
    /// namespace cannot be told, or naming `uid_map` or `gid_map` when one
    /// cannot be read or parsed.
    pub fn current() -> Result<Option<Self>> {
        Ok(IdMaps::current()?.as_ref().map(Self::new))
    }

    /// [`UserNamespace::current`] read into `cell` at its first use and
    /// kept: a snapshotter holds its layers to one namespace's ids.
    ///
    /// # Errors
    ///
    /// As [`UserNamespace::current`]; an error is not kept, so the next
    /// call reads again.
    pub(crate) fn once(cell: &OnceLock<Option<Self>>) -> Result<Option<&Self>> {
        if let Some(ns) = cell.get() {
            return Ok(ns.as_ref());
        }
        // Another thread may have read it first: the same namespace.
        let _ = cell.set(Self::current()?);
        Ok(cell.get().and_then(Option::as_ref))
    }

    /// The user namespace whose maps are `maps`, as
    /// [`rnk_store::Store::id_maps`] gives them.
    pub(crate) fn new(maps: &IdMaps) -> Self {
        Self { maps: maps.clone() }
    }

    /// The namespace whose maps read `uid_map` and `gid_map`.
    #[cfg(test)]
    pub(crate) fn from_maps(uid_map: &str, gid_map: &str) -> Option<Self> {
        IdMaps::parse(uid_map, gid_map).as_ref().map(Self::new)
    }

    /// The ids inside the namespace that its maps give.
    #[must_use]
    pub fn ids(&self) -> &MappedIds {
        self.maps.ids()
    }

    /// The uid map in canonical form: each line `inside outside count`
    /// with single spaces, the lines sorted, whatever order the map was
    /// written in and however the kernel pads it.
    #[must_use]
    pub fn uid_map(&self) -> &str {
        self.maps.uid_map()
    }

    /// The gid map, as [`UserNamespace::uid_map`].
    #[must_use]
    pub fn gid_map(&self) -> &str {
        self.maps.gid_map()
    }

    /// The uid outside the namespace that its root, uid 0 inside, has:
    /// `None` when the namespace does not map uid 0.
    #[must_use]
    pub fn root_uid_outside(&self) -> Option<u32> {
        self.ids()
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
        h.update(self.uid_map());
        h.update(b"\0");
        h.update(self.gid_map());
        Digest::from_sha256_bytes(&h.finalize().into())
    }
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

    #[test]
    fn a_mapped_key_depends_on_the_chain_and_both_maps_and_differs_from_its_chain_id() {
        let chain = Digest::from_sha256_bytes(&[1; 32]);
        let other = Digest::from_sha256_bytes(&[2; 32]);
        let a = ns("0 1000 1\n1 100000 65536\n", "0 1000 1\n1 100000 65536\n");
        let key = a.layer_key(&chain);
        assert_ne!(key, chain);
        assert_eq!(key, a.clone().layer_key(&chain));
        assert_ne!(key, a.layer_key(&other));
        // One map, however it is written.
        assert_eq!(
            key,
            ns("1 100000 65536\n0 1000 1", "  0 1000 1\n1 100000 65536\n").layer_key(&chain)
        );
        let gid = ns("0 1000 1\n1 100000 65536\n", "0 1000 1\n1 200000 65536\n");
        let uid = ns("0 1000 1\n1 200000 65536\n", "0 1000 1\n1 100000 65536\n");
        assert_ne!(key, gid.layer_key(&chain));
        assert_ne!(key, uid.layer_key(&chain));
        assert_ne!(gid.layer_key(&chain), uid.layer_key(&chain));
        // One id each way, the identity map included: the key differs from
        // the chain ID.
        let identity = ns("0 0 4294967295\n", "0 0 4294967295\n");
        assert_ne!(identity.layer_key(&chain), chain);
        // The key's input is pinned: a change of it extracts every layer
        // again.
        assert_eq!(
            key.to_string(),
            "sha256:873284fc7177152d99958b3545fcdee6fd56cddaf66acd94643b7e75d62a41b3"
        );
    }

    /// The key of a layer extracted before the canary (version 1 of the
    /// tag) is never one of version 2's.
    #[test]
    fn a_key_of_the_first_version_is_never_reused() {
        let chain = Digest::from_sha256_bytes(&[1; 32]);
        let maps = ("0 1000 1\n1 100000 65536\n", "0 1000 1\n1 100000 65536\n");
        let mut h = Sha256::new();
        h.update(b"ranikust layer extracted in a user namespace, v1\0");
        h.update(chain.to_string());
        h.update(b"\0");
        h.update(maps.0);
        h.update(b"\0");
        h.update(maps.1);
        let v1 = Digest::from_sha256_bytes(&h.finalize().into());
        assert_eq!(
            v1.to_string(),
            "sha256:67b5e4e77cbbab7489aa2a70843aebef9ac293daf674985aa1e6b37ed12bf50c"
        );
        assert_ne!(ns(maps.0, maps.1).layer_key(&chain), v1);
    }

    /// The ids are those the maps give.
    #[test]
    fn the_ids_are_the_maps_ones() {
        let n = ns("0 1000 1\n1 100000 10\n", "0 1000 1\n");
        assert!(n.ids().maps_uid(10));
        assert!(!n.ids().maps_uid(11));
        assert!(n.ids().maps_gid(0));
        assert!(!n.ids().maps_gid(1));
    }

    /// In a user namespace other than the initial one, `current` holds this
    /// process's maps; in the initial one, none.
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
