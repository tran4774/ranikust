//! Lower layers in the overlay form, and what overlay's merged view of a
//! stack of them shows at a path.
//!
//! Every layer is read through its own [`RootPath`]: a layer's content came
//! from an image or from a `RUN`, so nothing in it is followed.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

use rnk_rootfs::{FileType, RootPath, Stat};
use rustix::io::Errno;

use crate::error::Result;

/// The xattr that marks an opaque directory in the overlay form
/// (`userxattr`, `ADR-0016`).
pub(crate) const OPAQUE: &str = "user.overlay.opaque";
/// The xattr overlay sets on a renamed directory; never with
/// `redirect_dir=nofollow`.
pub(crate) const REDIRECT: &str = "user.overlay.redirect";
/// The prefix of overlay's own xattrs with `userxattr`.
const USER_OVERLAY: &[u8] = b"user.overlay.";
/// The escaped form of a content xattr named `user.overlay.*`.
const ESCAPED: &[u8] = b"user.overlay.overlay.";
/// The prefix of overlay's own xattrs without `userxattr`.
const TRUSTED_OVERLAY: &[u8] = b"trusted.overlay.";

/// Whether `st` is an overlay whiteout: the character device 0/0.
pub(crate) fn is_whiteout(st: &Stat) -> bool {
    st.file_type() == FileType::CharDevice && st.rdev_major == 0 && st.rdev_minor == 0
}

/// Whether the directory `p` of `root` is opaque (`user.overlay.opaque=y`).
pub(crate) fn is_opaque(root: &RootPath, p: &Path) -> Result<bool> {
    Ok(root
        .xattrs(p)?
        .iter()
        .any(|(n, v)| n == OPAQUE && v.as_slice() == b"y"))
}

/// The xattr a layer keeps.
const CAPABILITY: &[u8] = b"security.capability";

/// The xattrs an exported layer carries of an entry in the overlay form:
/// `security.capability` only, as BuildKit (containerd's `ChangeWriter`
/// writes that one xattr) and kaniko; overlay's own and every other xattr
/// are left out (ADR-0032, Q-025).
pub(crate) fn layer_xattrs(all: Vec<(OsString, Vec<u8>)>) -> Vec<(OsString, Vec<u8>)> {
    all.into_iter()
        .filter(|(n, _)| n.as_bytes() == CAPABILITY)
        .collect()
}

/// The xattrs of an entry in the overlay form to copy onto another entry
/// in the overlay form (the new upper root): overlay's own dropped, as
/// overlay's copy-up does, and the escaped `user.overlay.overlay.*` kept
/// escaped. Un-escaping them there would hand an image's xattrs to overlay
/// as its own markers.
pub(crate) fn overlay_form_xattrs(all: Vec<(OsString, Vec<u8>)>) -> Vec<(OsString, Vec<u8>)> {
    all.into_iter()
        .filter(|(n, _)| {
            let b = n.as_bytes();
            b.starts_with(ESCAPED)
                || (!b.starts_with(USER_OVERLAY) && !b.starts_with(TRUSTED_OVERLAY))
        })
        .collect()
}

/// A stack of lower layers, topmost first, as overlay's `lowerdir=`.
pub(crate) struct Lowers<'a> {
    pub(crate) layers: &'a [RootPath],
}

/// What the merged view of the lower stack shows at a path.
pub(crate) struct Found<'a> {
    /// The layer the entry comes from.
    pub(crate) layer: &'a RootPath,
    /// Its `statx`.
    pub(crate) stat: Stat,
}

impl<'a> Lowers<'a> {
    /// The entry at `p` (relative, no `..`) in the merged view: the topmost
    /// layer that has it, unless a higher layer hides it with a whiteout, an
    /// opaque directory or a non-directory on the way. `None` when nothing
    /// shows there. The root itself is the topmost layer's root.
    pub(crate) fn lookup(&self, p: &Path) -> Result<Option<Found<'a>>> {
        let names: Vec<&OsStr> = p
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n),
                Component::RootDir
                | Component::CurDir
                | Component::ParentDir
                | Component::Prefix(_) => None,
            })
            .collect();
        let Some(top) = self.layers.first() else {
            return Ok(None);
        };
        if names.is_empty() {
            return Ok(Some(Found {
                layer: top,
                stat: top.statx("")?,
            }));
        }
        for layer in self.layers {
            // Whether a directory on the way is opaque in this layer: then
            // the layers below cannot show the path either.
            let mut opaque = false;
            let mut prefix = PathBuf::new();
            for (i, name) in names.iter().enumerate() {
                prefix.push(name);
                let st = match layer.statx(&prefix) {
                    Ok(st) => st,
                    Err(e) if is_missing(&e) => {
                        if opaque {
                            return Ok(None);
                        }
                        break;
                    }
                    Err(e) => return Err(e.into()),
                };
                if is_whiteout(&st) {
                    return Ok(None);
                }
                if i + 1 == names.len() {
                    return Ok(Some(Found { layer, stat: st }));
                }
                if st.file_type() != FileType::Directory {
                    return Ok(None);
                }
                opaque = opaque || is_opaque(layer, &prefix)?;
            }
        }
        Ok(None)
    }

    /// The names the merged view shows in the directory `dir` (relative,
    /// no `..`): every name some layer has there, kept when
    /// [`Lowers::lookup`] shows it, so whiteouts, opaque directories and
    /// non-directories on the way hide what they hide in overlay.
    pub(crate) fn list(&self, dir: &Path) -> Result<BTreeSet<OsString>> {
        let mut names = BTreeSet::new();
        for layer in self.layers {
            match layer.statx(dir) {
                Ok(st) if st.file_type() == FileType::Directory => {
                    names.extend(crate::export::list(layer, dir)?);
                }
                Ok(_) => {}
                Err(e) if is_missing(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut shown = BTreeSet::new();
        for name in names {
            if self.lookup(&dir.join(&name))?.is_some() {
                shown.insert(name);
            }
        }
        Ok(shown)
    }
}

/// `ENOENT` or `ENOTDIR`: the layer does not have the path.
pub(crate) fn is_missing(e: &rnk_rootfs::Error) -> bool {
    matches!(
        e.raw_os_error().map(Errno::from_raw_os_error),
        Some(Errno::NOENT | Errno::NOTDIR)
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rustix::fs::{CWD, FileType as RawType, Mode, XattrFlags};

    use super::*;

    /// Layers `top`, `mid`, `bottom` in a temporary directory.
    struct Stack {
        dir: tempfile::TempDir,
        roots: Vec<RootPath>,
    }

    impl Stack {
        fn new(build: impl Fn(&Path)) -> Self {
            let dir = tempfile::tempdir().unwrap();
            for l in ["top", "mid", "bottom"] {
                fs::create_dir(dir.path().join(l)).unwrap();
            }
            build(dir.path());
            let roots = ["top", "mid", "bottom"]
                .iter()
                .map(|l| RootPath::open(dir.path().join(l)).unwrap())
                .collect();
            Self { dir, roots }
        }

        /// Which layer shows `p`, if any.
        fn shows(&self, p: &str) -> Option<&'static str> {
            let found = Lowers {
                layers: &self.roots,
            }
            .lookup(Path::new(p))
            .unwrap()?;
            let i = self
                .roots
                .iter()
                .position(|r| std::ptr::eq(r, found.layer))
                .unwrap();
            Some(["top", "mid", "bottom"][i])
        }
    }

    fn file(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"").unwrap();
    }

    fn whiteout(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        rustix::fs::mknodat(CWD, p, RawType::CharacterDevice, Mode::empty(), 0).unwrap();
    }

    fn opaque(p: &Path) {
        fs::create_dir_all(p).unwrap();
        rustix::fs::setxattr(p, OPAQUE, b"y", XattrFlags::empty()).unwrap();
    }

    #[test]
    fn the_topmost_layer_with_the_path_shows_it() {
        let s = Stack::new(|d| {
            file(&d.join("bottom/a/f"));
            file(&d.join("mid/a/f"));
            file(&d.join("bottom/a/g"));
        });
        assert_eq!(s.shows("a/f"), Some("mid"));
        assert_eq!(s.shows("a/g"), Some("bottom"));
        assert_eq!(s.shows("a"), Some("mid"));
        assert_eq!(s.shows("nothing"), None);
        assert_eq!(s.shows(""), Some("top"));
    }

    #[test]
    fn whiteouts_opaque_dirs_and_non_dirs_hide_what_is_below() {
        let s = Stack::new(|d| {
            file(&d.join("bottom/gone/f"));
            whiteout(&d.join("mid/gone"));
            file(&d.join("bottom/o/old"));
            file(&d.join("bottom/o/both"));
            opaque(&d.join("mid/o"));
            file(&d.join("mid/o/both"));
            file(&d.join("bottom/x/y"));
            file(&d.join("top/x"));
        });
        assert_eq!(s.shows("gone"), None);
        assert_eq!(s.shows("gone/f"), None);
        assert_eq!(s.shows("o/old"), None);
        assert_eq!(s.shows("o/both"), Some("mid"));
        assert_eq!(s.shows("o"), Some("mid"));
        assert_eq!(s.shows("x/y"), None);
        assert_eq!(s.shows("x"), Some("top"));
        drop(s.dir);
    }

    #[test]
    fn layer_xattrs_keep_only_security_capability() {
        let x = |n: &str| (OsString::from(n), b"v".to_vec());
        let kept = layer_xattrs(vec![
            x("security.capability"),
            x("security.selinux"),
            x("trusted.overlay.origin"),
            x("trusted.other"),
            x("user.overlay.opaque"),
            x("user.overlay.overlay.mine"),
            x("user.other"),
        ]);
        let names: Vec<_> = kept.iter().map(|(n, _)| n.to_str().unwrap()).collect();
        assert_eq!(names, ["security.capability"]);
    }
}
