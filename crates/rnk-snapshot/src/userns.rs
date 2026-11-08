//! The user namespace the store was opened in, when it is not the initial
//! one: extraction there makes no device node (ADR-0106) and keys the
//! layers it extracts by the namespace's id maps (ADR-0104), so a layer
//! extracted with one map is never a lower under another. The maps are
//! relative to the parent namespace: nested namespaces with one map under
//! parents with different ones share keys, which the store's canary of the
//! maps guards (ADR-0128).

use rnk_oci::Digest;
use rnk_rootfs::MappedIds;
use rnk_store::IdMaps;
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// Puts a mapped layer's key in a space of its own: the input of no chain
/// ID of several layers (`<digest> <digest>`) starts with these bytes, and
/// a chain ID of one layer equal to a key would need a layer whose tar is
/// exactly the key's input, fixed by the chain and the maps. Version 2
/// came with the canary: no layer extracted before it is reused, since no
/// canary guards it.
const KEY_DOMAIN: &[u8] = b"ranikust layer extracted in a user namespace, v2\0";

/// A user namespace other than the initial one, with its maps as the
/// store read them: relative to the namespace that created it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserNamespace {
    maps: IdMaps,
    /// The ids inside that the maps give: the owners an extraction here can
    /// set (ADR-0098).
    ids: MappedIds,
}

impl UserNamespace {
    /// The user namespace this process runs in, or `None` in the initial
    /// one, as [`IdMaps::current`] reads it: for kaniko mode, whose store
    /// keeps no extracted layer.
    ///
    /// # Errors
    ///
    /// [`Error::Store`] when `/proc/self` cannot tell the namespace or its
    /// maps.
    pub(crate) fn current() -> Result<Option<Self>> {
        IdMaps::current()?.as_ref().map(Self::new).transpose()
    }

    /// The user namespace whose maps are `maps`, as
    /// [`rnk_store::Store::id_maps`] gives them.
    ///
    /// # Errors
    ///
    /// [`Error::State`] where [`MappedIds`] does not take the maps, which
    /// [`IdMaps`] checked already.
    pub(crate) fn new(maps: &IdMaps) -> Result<Self> {
        let ids = MappedIds::parse(maps.uid_map(), maps.gid_map())
            .ok_or(Error::State("the store's id maps give no ids"))?;
        Ok(Self {
            maps: maps.clone(),
            ids,
        })
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
        h.update(self.maps.uid_map());
        h.update(b"\0");
        h.update(self.maps.gid_map());
        Digest::from_sha256_bytes(&h.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(uid_map: &str, gid_map: &str) -> UserNamespace {
        UserNamespace::new(&IdMaps::parse(uid_map, gid_map).unwrap()).unwrap()
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
}
