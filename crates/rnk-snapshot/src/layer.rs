//! Base layers extracted into the store's `layers/`, in the overlay form.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use rnk_oci::Digest;
use rnk_rootfs::{FileTime, Meta, RootPath, extract_overlay};
use rnk_store::{EntryKey, Store};

use crate::error::{Error, Result};
use crate::lowers;

/// An extracted layer in the store, `layers/<algorithm>/<hex>/`: immutable,
/// only ever a lower directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LayerDir {
    pub(crate) diff_id: Digest,
    pub(crate) path: PathBuf,
}

impl LayerDir {
    /// The diff ID the layer is stored under.
    #[must_use]
    pub fn diff_id(&self) -> &Digest {
        &self.diff_id
    }

    /// Its directory in the store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The xattr on the root of an extracted layer whose tar has no entry for
/// the root (`./`): the stage root then takes its metadata from the layers
/// below, as extracting the image onto one tree would. Only the stage
/// root's metadata reads it; a layer's root is never part of a layer.
pub(crate) const INHERIT_ROOT: &str = "user.ranikust.inherit-root";

/// The metadata of an image root that no layer sets: `0755`, the epoch,
/// and the owner the directory was created with: ranikust's, root in
/// overlay mode as root.
pub(crate) fn default_root() -> Meta {
    Meta {
        uid: None,
        gid: None,
        mode: Some(0o755),
        atime: Some(FileTime::from_secs(0)),
        mtime: Some(FileTime::from_secs(0)),
        xattrs: Vec::new(),
    }
}

/// The metadata the merged root of `stack` (topmost first) shows: the
/// topmost layer whose root is not marked [`INHERIT_ROOT`], else
/// [`default_root`]. Overlay's own xattrs and the mark are left out.
pub(crate) fn root_meta(stack: &[RootPath]) -> Result<Meta> {
    for layer in stack {
        let xattrs = layer.xattrs("")?;
        if xattrs.iter().any(|(n, _)| n == INHERIT_ROOT) {
            continue;
        }
        let st = layer.statx("")?;
        return Ok(Meta {
            uid: Some(st.uid),
            gid: Some(st.gid),
            mode: Some(st.mode),
            atime: Some(st.mtime),
            mtime: Some(st.mtime),
            xattrs: lowers::content_xattrs(xattrs),
        });
    }
    Ok(default_root())
}

/// Extracts the layer blob `blob` (gzip, zstd or plain tar) into the store
/// as `diff_id`, above `below` (the layers under it, in image order: the
/// one right below last), unless the store has it already.
///
/// The store's per-digest lock keeps two builds from extracting one layer
/// twice. The uncompressed tar must hash to `diff_id`, or nothing is
/// published. A hardlink into a layer below is copied from `below` (Q-055);
/// the stored layer then depends on the stack it was first extracted on,
/// though the store keys it by diff ID alone [open: Q-150].
///
/// # Errors
///
/// [`Error::Store`] wrapping [`Error::DiffIdMismatch`], a decoding or an
/// extraction error.
pub(crate) fn extract_layer(
    store: &Store,
    diff_id: &Digest,
    blob: impl Read,
    below: &[LayerDir],
) -> Result<LayerDir> {
    let path = store.fill(&EntryKey::Layer(diff_id.clone()), |tmp| -> Result<u64> {
        let root = tmp
            .root()
            .ok_or(Error::State("the store gave no directory for a layer"))?;
        let lower_roots = below
            .iter()
            .rev()
            .map(|l| RootPath::open(&l.path))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let refs: Vec<&RootPath> = lower_roots.iter().collect();
        let mut decoder = rnk_layer::Decoder::new(blob)?;
        let extracted = extract_overlay(root, &mut decoder, &refs)?;
        let size = decoder.tar_size();
        let actual = Digest::from_sha256_bytes(decoder.finish()?.as_bytes());
        if &actual != diff_id {
            return Err(Error::DiffIdMismatch {
                expected: diff_id.to_string(),
                actual: actual.to_string(),
            });
        }
        if !extracted.root_set {
            root.set_meta(
                "",
                &Meta {
                    xattrs: vec![(OsString::from(INHERIT_ROOT), b"1".to_vec())],
                    ..Meta::default()
                },
            )?;
        }
        Ok(size)
    })?;
    Ok(LayerDir {
        diff_id: diff_id.clone(),
        path,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use rustix::fs::XattrFlags;

    use super::*;

    #[test]
    fn the_root_comes_from_the_topmost_layer_that_sets_it() {
        let dir = tempfile::tempdir().unwrap();
        for l in ["inherit", "set"] {
            fs::create_dir(dir.path().join(l)).unwrap();
        }
        let p = |l: &str| dir.path().join(l);
        rustix::fs::setxattr(p("inherit"), INHERIT_ROOT, b"1", XattrFlags::empty()).unwrap();
        fs::set_permissions(p("set"), fs::Permissions::from_mode(0o750)).unwrap();
        rustix::fs::setxattr(p("set"), "user.overlay.opaque", b"y", XattrFlags::empty()).unwrap();
        rustix::fs::setxattr(p("set"), "user.kept", b"k", XattrFlags::empty()).unwrap();
        let stack = [
            RootPath::open(p("inherit")).unwrap(),
            RootPath::open(p("set")).unwrap(),
        ];
        let m = root_meta(&stack).unwrap();
        assert_eq!(m.mode.map(|m| m & 0o7777), Some(0o750));
        // Overlay's own xattrs never reach the new upper root: an opaque
        // root would hide every lower layer.
        assert_eq!(m.xattrs, [(OsString::from("user.kept"), b"k".to_vec())]);
        assert_eq!(root_meta(&stack[..1]).unwrap(), default_root());
        assert_eq!(root_meta(&[]).unwrap(), default_root());
    }
}
