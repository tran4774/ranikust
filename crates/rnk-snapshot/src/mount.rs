//! Overlay mounts with the ADR-0016 options: `fsconfig(lowerdir+)` where
//! the kernel has it, else `mount(2)` with short names.
//!
//! Every path here is ranikust's own: lower layers in the store's
//! `layers/`, upper, work and mount point under `builds/<id>/` (mode 0700,
//! created by this process). Mounts happen in the build's private mount
//! namespace (`rnk_sandbox::enter_build_namespace`).

use std::ffi::{CString, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use rnk_rootfs::RootPath;
use rustix::fs::CWD;
use rustix::io::Errno;
use rustix::mount::{
    FsMountFlags, FsOpenFlags, MountAttrFlags, MountFlags, MoveMountFlags, UnmountFlags,
};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// The options of every overlay mount (`ADR-0016`).
pub const OVERLAY_OPTIONS: &str =
    "userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile";

/// The most lower layers one overlay mount takes; layer 501 is `EINVAL`.
pub const MAX_LOWERS: usize = 500;

/// The longest option string `mount(2)` takes: one page, less the NUL.
/// Longer data is truncated and the kernel answers `ENOENT`.
const MOUNT_DATA_MAX: usize = 4095;

/// How the kernel takes the lower layers (probe step 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MountApi {
    /// `fsopen`, one `fsconfig(FSCONFIG_SET_STRING, "lowerdir+")` per layer,
    /// `FSCONFIG_CMD_CREATE`, `fsmount`, `move_mount` (Linux 6.8): no
    /// length limit.
    LowerdirPlus,
    /// `mount(2)` with one option string of short `l/<name>` paths: at most
    /// [`MOUNT_DATA_MAX`] bytes.
    Mount2,
}

/// One overlay mount.
pub(crate) struct Spec<'a> {
    /// Lower layers, topmost first; at least one.
    pub(crate) lowers: &'a [PathBuf],
    /// Upper and work directories; `None` for a read-only mount, which then
    /// leaves out `volatile` and needs two lowers or more.
    pub(crate) upper: Option<(&'a Path, &'a Path)>,
    /// The mount point.
    pub(crate) target: &'a Path,
}

/// Mounts `spec` with `api`. `short` gives the short name of a lower for
/// [`MountApi::Mount2`].
pub(crate) fn mount(
    spec: &Spec<'_>,
    api: MountApi,
    short: &mut dyn FnMut(&Path) -> Result<PathBuf>,
) -> Result<()> {
    let span = tracing::debug_span!("rnk_snapshot.mount", lowers = spec.lowers.len());
    let _entered = span.enter();
    if spec.lowers.len() > MAX_LOWERS {
        return Err(Error::TooManyLowers {
            lowers: spec.lowers.len(),
            max: MAX_LOWERS,
        });
    }
    match api {
        MountApi::LowerdirPlus => mount_fsconfig(spec),
        MountApi::Mount2 => mount_data(spec, short),
    }
}

/// The ADR-0016 options as `(key, value)`; `volatile` only with an upper.
fn options(with_upper: bool) -> impl Iterator<Item = (&'static str, Option<&'static str>)> {
    OVERLAY_OPTIONS
        .split(',')
        .filter(move |o| with_upper || *o != "volatile")
        .map(|o| match o.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (o, None),
        })
}

fn mount_fsconfig(spec: &Spec<'_>) -> Result<()> {
    let fail = |step: String, errno: Errno| Error::Mount {
        target: spec.target.to_owned(),
        lowers: spec.lowers.len(),
        how: "lowerdir+",
        step,
        errno,
        options: None,
    };
    let fs = rustix::mount::fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)
        .map_err(|e| fail("fsopen".into(), e))?;
    for (k, v) in options(spec.upper.is_some()) {
        match v {
            Some(v) => rustix::mount::fsconfig_set_string(&fs, k, v),
            None => rustix::mount::fsconfig_set_flag(&fs, k),
        }
        .map_err(|e| fail(format!("fsconfig {k}{}", kernel_messages(&fs)), e))?;
    }
    for lower in spec.lowers {
        rustix::mount::fsconfig_set_string(&fs, "lowerdir+", lower)
            .map_err(|e| fail(format!("fsconfig lowerdir+{}", kernel_messages(&fs)), e))?;
    }
    if let Some((upper, work)) = spec.upper {
        rustix::mount::fsconfig_set_string(&fs, "upperdir", upper)
            .map_err(|e| fail("fsconfig upperdir".into(), e))?;
        rustix::mount::fsconfig_set_string(&fs, "workdir", work)
            .map_err(|e| fail("fsconfig workdir".into(), e))?;
    }
    // Linux 5.15 accepts `lowerdir+` above and refuses only here (exp-13).
    rustix::mount::fsconfig_create(&fs)
        .map_err(|e| fail(format!("create{}", kernel_messages(&fs)), e))?;
    let attrs = if spec.upper.is_some() {
        MountAttrFlags::empty()
    } else {
        MountAttrFlags::MOUNT_ATTR_RDONLY
    };
    let mnt = rustix::mount::fsmount(&fs, FsMountFlags::FSMOUNT_CLOEXEC, attrs)
        .map_err(|e| fail("fsmount".into(), e))?;
    rustix::mount::move_mount(
        &mnt,
        "",
        CWD,
        spec.target,
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
    )
    .map_err(|e| fail("move_mount".into(), e))
}

/// The messages the kernel queued on the filesystem context `fs` (lines
/// such as `e overlay: ...`, read until it has none), as ` (<msg>; ...)`,
/// or nothing: they say why a `fsconfig` call failed.
fn kernel_messages(fs: &impl rustix::fd::AsFd) -> String {
    let mut msgs = Vec::new();
    let mut buf = [0u8; 1024];
    // At most a few messages: the context's log holds eight.
    for _ in 0..8 {
        match rustix::io::read(fs, &mut buf) {
            Ok(n) if n > 0 => msgs.push(String::from_utf8_lossy(&buf[..n]).trim().to_owned()),
            Ok(_) | Err(_) => break,
        }
    }
    if msgs.is_empty() {
        String::new()
    } else {
        format!(" ({})", msgs.join("; "))
    }
}

/// The `mount(2)` option string, with the lowers' short names.
fn data(spec: &Spec<'_>, short: &mut dyn FnMut(&Path) -> Result<PathBuf>) -> Result<OsString> {
    let mut d = Vec::new();
    for (k, v) in options(spec.upper.is_some()) {
        d.extend_from_slice(k.as_bytes());
        if let Some(v) = v {
            d.push(b'=');
            d.extend_from_slice(v.as_bytes());
        }
        d.push(b',');
    }
    d.extend_from_slice(b"lowerdir=");
    for (i, lower) in spec.lowers.iter().enumerate() {
        if i > 0 {
            d.push(b':');
        }
        d.extend_from_slice(short(lower)?.as_os_str().as_bytes());
    }
    if let Some((upper, work)) = spec.upper {
        d.extend_from_slice(b",upperdir=");
        d.extend_from_slice(upper.as_os_str().as_bytes());
        d.extend_from_slice(b",workdir=");
        d.extend_from_slice(work.as_os_str().as_bytes());
    }
    Ok(OsString::from_vec(d))
}

fn mount_data(spec: &Spec<'_>, short: &mut dyn FnMut(&Path) -> Result<PathBuf>) -> Result<()> {
    let data = data(spec, short)?;
    let len = data.len();
    if len > MOUNT_DATA_MAX {
        return Err(Error::OptionsTooLong {
            target: spec.target.to_owned(),
            lowers: spec.lowers.len(),
            len,
            max: MOUNT_DATA_MAX,
        });
    }
    let mount_err = |errno| Error::Mount {
        target: spec.target.to_owned(),
        lowers: spec.lowers.len(),
        how: "mount(2)",
        step: "mount".into(),
        errno,
        options: Some(len),
    };
    // Paths of ranikust's own directories: no NUL byte.
    let data = CString::new(data.into_vec()).map_err(|_| mount_err(Errno::INVAL))?;
    rustix::mount::mount(
        "overlay",
        spec.target,
        "overlay",
        if spec.upper.is_some() {
            MountFlags::empty()
        } else {
            MountFlags::RDONLY
        },
        data.as_c_str(),
    )
    .map_err(|errno| Error::Mount {
        target: spec.target.to_owned(),
        lowers: spec.lowers.len(),
        how: "mount(2)",
        step: "mount".into(),
        errno,
        options: Some(len),
    })
}

/// Detaches the mount at `target`: open descriptors below it (the stage's
/// `RootPath`) no longer keep it busy, and it goes away with the last one.
pub(crate) fn unmount(target: &Path) -> Result<()> {
    let span = tracing::debug_span!("rnk_snapshot.umount");
    let _entered = span.enter();
    rustix::mount::unmount(target, UnmountFlags::DETACH | UnmountFlags::NOFOLLOW).map_err(|errno| {
        Error::Unmount {
            target: target.to_owned(),
            errno,
        }
    })
}

/// The short names of `mount(2)`: `<state>/l/<name>`, a symlink to a lower
/// layer, so that a mount's option string stays below one page.
pub(crate) struct ShortNames<'a> {
    /// The state directory.
    pub(crate) state: &'a RootPath,
    /// Its host path.
    pub(crate) state_path: &'a Path,
}

/// Length of a short name: 12 characters of base 32, 60 bits of the
/// target's sha256.
const SHORT_LEN: usize = 12;

impl ShortNames<'_> {
    /// The short path of `target`, creating its link if missing. Names come
    /// from the target's hash, so two builds sharing the store agree on them;
    /// a link that points elsewhere (a hash collision, or something planted)
    /// is refused.
    pub(crate) fn get(&self, target: &Path) -> Result<PathBuf> {
        let name = short_name(target);
        let rel = Path::new("l").join(&name);
        self.state.create_dir_all("l", 0o700)?;
        match self.state.symlink(target, &rel) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => {
                let found = self.state.read_link(&rel)?;
                if found != target {
                    return Err(Error::Export {
                        path: rel,
                        reason: "a short name of the mount(2) fallback points elsewhere",
                    });
                }
            }
            Err(e) => return Err(e.into()),
        }
        Ok(self.state_path.join(rel))
    }

    /// Removes the short name of `target`, if it is there.
    pub(crate) fn remove(&self, target: &Path) {
        let rel = Path::new("l").join(short_name(target));
        if let Ok(found) = self.state.read_link(&rel)
            && found == target
        {
            let _ = self.state.remove_file(&rel);
        }
    }
}

fn short_name(target: &Path) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let hash = Sha256::digest(target.as_os_str().as_bytes());
    let mut bits = u64::from_be_bytes(hash[..8].try_into().unwrap_or([0; 8]));
    let mut name = String::with_capacity(SHORT_LEN);
    for _ in 0..SHORT_LEN {
        name.push(char::from(
            ALPHABET[usize::from(u8::try_from(bits & 31).unwrap_or(0))],
        ));
        bits >>= 5;
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec<'a>(lowers: &'a [PathBuf], upper: Option<(&'a Path, &'a Path)>) -> Spec<'a> {
        Spec {
            lowers,
            upper,
            target: Path::new("/s/rootfs"),
        }
    }

    #[test]
    fn data_has_every_option_and_short_lowers_topmost_first() {
        let lowers = [PathBuf::from("/s/a"), PathBuf::from("/s/b")];
        let d = data(
            &spec(&lowers, Some((Path::new("/s/u"), Path::new("/s/w")))),
            &mut |p| Ok(Path::new("/l").join(p.file_name().unwrap())),
        )
        .unwrap();
        assert_eq!(
            d,
            "userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile,\
             lowerdir=/l/a:/l/b,upperdir=/s/u,workdir=/s/w"
        );
    }

    #[test]
    fn read_only_mount_leaves_out_volatile() {
        let lowers = [PathBuf::from("/a"), PathBuf::from("/b")];
        let d = data(&spec(&lowers, None), &mut |p| Ok(p.to_owned())).unwrap();
        assert!(!d.to_string_lossy().contains("volatile"), "{d:?}");
        assert!(d.to_string_lossy().ends_with("lowerdir=/a:/b"), "{d:?}");
    }

    #[test]
    fn too_long_data_is_refused_before_the_kernel_truncates_it() {
        let lowers: Vec<PathBuf> = (0..200)
            .map(|i| PathBuf::from(format!("/{}/{i}", "x".repeat(30))))
            .collect();
        let e = mount(&spec(&lowers, None), MountApi::Mount2, &mut |p| {
            Ok(p.to_owned())
        })
        .unwrap_err();
        assert!(
            matches!(e, Error::OptionsTooLong { lowers: 200, .. }),
            "{e}"
        );
    }

    #[test]
    fn more_than_500_lowers_are_refused() {
        let lowers: Vec<PathBuf> = (0..=MAX_LOWERS)
            .map(|i| PathBuf::from(format!("/{i}")))
            .collect();
        let e = mount(&spec(&lowers, None), MountApi::LowerdirPlus, &mut |p| {
            Ok(p.to_owned())
        })
        .unwrap_err();
        assert!(
            matches!(
                e,
                Error::TooManyLowers {
                    lowers: 501,
                    max: 500
                }
            ),
            "{e}"
        );
    }

    #[test]
    fn short_names_are_stable_short_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let state = RootPath::open(dir.path()).unwrap();
        let names = ShortNames {
            state: &state,
            state_path: dir.path(),
        };
        let a = names.get(Path::new("/store/layers/sha256/aa")).unwrap();
        assert_eq!(a, names.get(Path::new("/store/layers/sha256/aa")).unwrap());
        assert_eq!(a.file_name().unwrap().len(), SHORT_LEN);
        assert_eq!(
            std::fs::read_link(&a).unwrap(),
            Path::new("/store/layers/sha256/aa")
        );
        assert_ne!(a, names.get(Path::new("/store/layers/sha256/bb")).unwrap());
        // A link planted under the name is not trusted.
        let planted = Path::new("/elsewhere");
        let name = a.file_name().unwrap();
        std::fs::remove_file(&a).unwrap();
        std::os::unix::fs::symlink(planted, dir.path().join("l").join(name)).unwrap();
        assert!(names.get(Path::new("/store/layers/sha256/aa")).is_err());
        names.remove(Path::new("/store/layers/sha256/aa"));
        assert!(dir.path().join("l").join(name).symlink_metadata().is_ok());
    }
}
