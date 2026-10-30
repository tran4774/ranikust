//! Overlay mounts with the ADR-0016 options: `fsconfig(lowerdir+)` where
//! the kernel has it, else `mount(2)` with relative lower names, from a
//! thread of its own working directory, as containerd mounts many lowers
//! (ADR-0032, Q-151).
//!
//! Every path here is ranikust's own: lower layers in the store's
//! `layers/`, upper, work and mount point under `builds/<id>/` (mode 0700,
//! created by this process). Mounts happen in the build's private mount
//! namespace (`rnk_sandbox::enter_build_namespace`).

use std::ffi::{CString, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rnk_rootfs::{OpenHow, RootPath};
use rustix::fs::CWD;
use rustix::io::Errno;
use rustix::mount::{
    FsMountFlags, FsOpenFlags, MountAttrFlags, MountFlags, MoveMountFlags, UnmountFlags,
};
use rustix::thread::UnshareFlags;

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
    /// `mount(2)` with one option string, the lowers named `0`, `1`, ...
    /// relative to a directory of links to them: at most `MOUNT_DATA_MAX`
    /// bytes, which 500 lowers fit.
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

/// Mounts `spec` with `api`. For [`MountApi::Mount2`], `links` is a
/// private directory of the build where the mount makes, and removes
/// again, its directory of links to the lowers.
pub(crate) fn mount(spec: &Spec<'_>, api: MountApi, links: &RootPath) -> Result<()> {
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
        MountApi::Mount2 => mount_data(spec, links),
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

/// The `mount(2)` option string, with the lowers named by their index:
/// `lowerdir=0:1:...`, relative to the directory of links the mount runs
/// in. Upper and work stay absolute.
fn data(spec: &Spec<'_>) -> OsString {
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
    for i in 0..spec.lowers.len() {
        if i > 0 {
            d.push(b':');
        }
        d.extend_from_slice(i.to_string().as_bytes());
    }
    if let Some((upper, work)) = spec.upper {
        d.extend_from_slice(b",upperdir=");
        d.extend_from_slice(upper.as_os_str().as_bytes());
        d.extend_from_slice(b",workdir=");
        d.extend_from_slice(work.as_os_str().as_bytes());
    }
    OsString::from_vec(d)
}

/// Tells the directories of links of this process's mounts apart.
static NEXT_LINKS: AtomicU64 = AtomicU64::new(0);

/// `mount(2)` as containerd mounts many lowers
/// (`core/mount/mount_linux.go`, `compactLowerdirOption` and `mountAt`):
/// the lowers get short relative names, and the mount runs on a thread of
/// its own that unshares its filesystem context (`CLONE_FS`) and changes
/// into the directory the names are relative to, so the process's working
/// directory never changes. ranikust's names are links `0`, `1`, ... in a
/// fresh directory under `links`, which overlay follows once, at mount
/// time; the directory is removed again after the mount, whatever its
/// result.
fn mount_data(spec: &Spec<'_>, links: &RootPath) -> Result<()> {
    let data = data(spec);
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
    // Paths of ranikust's own directories and decimal names: no NUL byte.
    let data = CString::new(data.into_vec()).map_err(|_| mount_err(Errno::INVAL))?;
    let flags = if spec.upper.is_some() {
        MountFlags::empty()
    } else {
        MountFlags::RDONLY
    };
    let name = format!("lowers-{}", NEXT_LINKS.fetch_add(1, Ordering::Relaxed));
    links.create_dir(&name, 0o700)?;
    let mounted = make_links(links, &name, spec.lowers).and_then(|dir| {
        std::thread::scope(|s| {
            s.spawn(|| mount_in(&dir, spec.target, flags, &data))
                .join()
                // The workspace aborts on panic, so the thread never
                // unwinds; a join error is not expected.
                .unwrap_or(Err(Errno::INVAL))
        })
        .map_err(mount_err)
    });
    let removed = remove_links(links, &name, spec.lowers.len());
    mounted?;
    removed
}

/// Fills `links/<name>/` with `<i> -> lowers[i]` and opens it as a path
/// handle for `fchdir`.
fn make_links(links: &RootPath, name: &str, lowers: &[PathBuf]) -> Result<OwnedFd> {
    let dir = Path::new(name);
    for (i, lower) in lowers.iter().enumerate() {
        links.symlink(lower, dir.join(i.to_string()))?;
    }
    Ok(links.open_at(dir, &OpenHow::path().directory().no_follow())?)
}

/// Removes what [`make_links`] made, also when it stopped half-way.
fn remove_links(links: &RootPath, name: &str, count: usize) -> Result<()> {
    let dir = Path::new(name);
    for i in 0..count {
        match links.remove_file(dir.join(i.to_string())) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(links.remove_dir(dir)?)
}

/// The mount itself, on its own thread: a private filesystem context, the
/// working directory `dir`, then `mount(2)`. The thread ends right after,
/// and its context with it.
fn mount_in(
    dir: &OwnedFd,
    target: &Path,
    flags: MountFlags,
    data: &CString,
) -> rustix::io::Result<()> {
    // SAFETY: `FS` does not unshare the descriptor table, the only flag
    // `unshare_unsafe` is unsafe for.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS) }?;
    rustix::process::fchdir(dir)?;
    rustix::mount::mount("overlay", target, "overlay", flags, data.as_c_str())
}

/// Checks that `dir`, under which every lower, upper and work directory
/// lives, holds nothing overlay's option syntax treats as syntax: `,` and `=` separate options, `:` lowers, `\` escapes, and
/// control characters have no business in a path given to the kernel.
/// The names ranikust adds below it (digests, build ids, `diff-<k>`) never
/// do.
pub(crate) fn check_path(dir: &Path) -> Result<()> {
    let reason = dir.as_os_str().as_bytes().iter().find_map(|&b| match b {
        b',' => Some("its path holds a ','"),
        b':' => Some("its path holds a ':'"),
        b'\\' => Some("its path holds a '\\'"),
        b'=' => Some("its path holds a '='"),
        b if b.is_ascii_control() => Some("its path holds a control character"),
        _ => None,
    });
    match reason {
        Some(reason) => Err(Error::UnsafePath {
            path: dir.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
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
    fn data_has_every_option_and_lowers_by_index_topmost_first() {
        let lowers = [PathBuf::from("/s/a"), PathBuf::from("/s/b")];
        let d = data(&spec(&lowers, Some((Path::new("/s/u"), Path::new("/s/w")))));
        assert_eq!(
            d,
            "userxattr,redirect_dir=nofollow,metacopy=off,index=off,xino=off,uuid=off,volatile,\
             lowerdir=0:1,upperdir=/s/u,workdir=/s/w"
        );
    }

    #[test]
    fn read_only_mount_leaves_out_volatile() {
        let lowers = [PathBuf::from("/a"), PathBuf::from("/b")];
        let d = data(&spec(&lowers, None));
        assert!(!d.to_string_lossy().contains("volatile"), "{d:?}");
        assert!(d.to_string_lossy().ends_with("lowerdir=0:1"), "{d:?}");
    }

    #[test]
    fn five_hundred_lowers_fit_one_page() {
        let lowers: Vec<PathBuf> = (0..MAX_LOWERS)
            .map(|i| PathBuf::from(format!("/{}/{i}", "x".repeat(200))))
            .collect();
        let long = PathBuf::from(format!("/{}", "s".repeat(1000)));
        let d = data(&spec(&lowers, Some((&long, &long))));
        assert!(d.len() <= MOUNT_DATA_MAX, "{}", d.len());
    }

    #[test]
    fn too_long_data_is_refused_before_the_kernel_truncates_it() {
        let lowers = [PathBuf::from("/a")];
        let long = PathBuf::from(format!("/{}", "s".repeat(2100)));
        let dir = tempfile::tempdir().unwrap();
        let links = RootPath::open(dir.path()).unwrap();
        let e = mount(
            &spec(&lowers, Some((&long, &long))),
            MountApi::Mount2,
            &links,
        )
        .unwrap_err();
        assert!(matches!(e, Error::OptionsTooLong { lowers: 1, .. }), "{e}");
        // Refused before any link was made.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_failed_mount_leaves_no_links() {
        // Unprivileged, mount(2) fails with EPERM; the links go anyway.
        let dir = tempfile::tempdir().unwrap();
        let links = RootPath::open(dir.path()).unwrap();
        let lowers = [dir.path().join("a"), dir.path().join("b")];
        let target = dir.path().join("t");
        let e = mount(
            &Spec {
                lowers: &lowers,
                upper: None,
                target: &target,
            },
            MountApi::Mount2,
            &links,
        )
        .unwrap_err();
        assert!(matches!(e, Error::Mount { .. }), "{e}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn paths_with_option_syntax_are_refused() {
        assert!(check_path(Path::new("/home/u/w/.ranikust")).is_ok());
        for bad in ["/x,lowerdir=/", "/a:b", "/a\\b", "/a=b", "/a\nb", "/a\tb"] {
            let e = check_path(Path::new(bad)).unwrap_err();
            assert!(matches!(e, Error::UnsafePath { .. }), "{bad}: {e}");
        }
    }

    #[test]
    fn more_than_500_lowers_are_refused() {
        let lowers: Vec<PathBuf> = (0..=MAX_LOWERS)
            .map(|i| PathBuf::from(format!("/{i}")))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let links = RootPath::open(dir.path()).unwrap();
        let e = mount(&spec(&lowers, None), MountApi::LowerdirPlus, &links).unwrap_err();
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
}
