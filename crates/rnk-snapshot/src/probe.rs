//! The overlay probe (steps 1 and 4 of
//! `design/modes-and-detection.md#probe-order---modeauto`): a real overlay
//! mount with the ADR-0016 options where the stage roots will live, first
//! with `fsconfig(lowerdir+)`, else with `mount(2)`.

use std::path::{Path, PathBuf};

use rnk_rootfs::{FileType, IgnoreList, OpenHow, RootPath, delete_contents};

use crate::error::{Error, Result};
use crate::lowers::{self, OPAQUE};
use crate::mount::{self, MountApi, Spec};

/// What the overlay probe found; `rnk-snapshot`'s part of the build's
/// outcome (Q-017, interim (b)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OverlayCaps {
    /// How lower layers are passed to the kernel.
    pub api: MountApi,
}

/// Probes overlay in `dir`, an empty directory of the build on the
/// filesystem that will hold the upper directories (the state directory).
///
/// Runs in the build's private mount namespace, where `ranikust` calls it
/// before any thread exists: it mounts there directly, so a failed attempt
/// leaves at most a mount in that namespace, which goes with the process.
/// Mounts a small overlay with the ADR-0016 options, with `lowerdir+` and
/// then, if the kernel refuses it (Linux before 6.8 fails it only at
/// `FSCONFIG_CMD_CREATE`), with `mount(2)`; then writes through it and
/// checks that the upper directory holds what overlay writes with
/// `userxattr`: a new file, a whiteout and an opaque directory. The
/// probe's directories are deleted afterwards.
///
/// # Errors
///
/// [`Error::Probe`] naming the failed step: the mount with both APIs
/// (upper on overlayfs or NFS, nested too deep, no privilege), or a write
/// through it; [`Error::ProbeCheck`] when the upper directory does not
/// hold what it should.
pub fn probe(dir: &Path) -> Result<OverlayCaps> {
    let root = RootPath::open(dir)?;
    let r = probe_in(&root, dir);
    let cleanup = delete_contents(&root, &IgnoreList::new());
    let caps = r?;
    cleanup?;
    Ok(caps)
}

fn probe_in(root: &RootPath, dir: &Path) -> Result<OverlayCaps> {
    for d in [
        "lower",
        "lower/gone",
        "lower/dir",
        "upper",
        "work",
        "merged",
    ] {
        root.create_dir(d, 0o755)?;
    }
    for f in ["lower/file", "lower/dir/old"] {
        drop(root.open_at(f, &OpenHow::write().create_new(0o644))?);
    }
    let lowers = [dir.join("lower")];
    let (upper, work, merged) = (dir.join("upper"), dir.join("work"), dir.join("merged"));
    let spec = Spec {
        lowers: &lowers,
        upper: Some((&upper, &work)),
        target: &merged,
    };
    let api = attempt(&spec, MountApi::LowerdirPlus)
        .map(|()| MountApi::LowerdirPlus)
        .or_else(|plus| {
            attempt(&spec, MountApi::Mount2)
                .map(|()| MountApi::Mount2)
                .map_err(|e| {
                    tracing::info!(target: "rnk_snapshot", "overlay probe: lowerdir+: {plus}");
                    e
                })
        })
        .map_err(|e| Error::Probe {
            step: "mount",
            source: Box::new(e),
        })?;
    let written = write_through(&merged);
    mount::unmount(&merged).map_err(|e| Error::Probe {
        step: "unmount",
        source: Box::new(e),
    })?;
    written.map_err(|e| Error::Probe {
        step: "write through the overlay",
        source: Box::new(e),
    })?;
    check_upper(&root.sub_root("upper")?)?;
    Ok(OverlayCaps { api })
}

fn attempt(spec: &Spec<'_>, api: MountApi) -> Result<()> {
    let span = tracing::debug_span!("rnk_snapshot.probe", ok = false, api = ?api);
    let _entered = span.enter();
    mount::mount(spec, api, &mut |p: &Path| -> Result<PathBuf> {
        Ok(p.to_owned())
    })?;
    span.record("ok", true);
    Ok(())
}

/// A new file (the root is copied up), a deleted file (a whiteout) and a
/// directory deleted and made again (opaque).
fn write_through(merged: &Path) -> Result<()> {
    let m = RootPath::open(merged)?;
    drop(m.open_at("new", &OpenHow::write().create_new(0o644))?);
    m.remove_file("file")?;
    m.remove_file("dir/old")?;
    m.remove_dir("dir")?;
    m.create_dir("dir", 0o755)?;
    m.remove_dir("gone")?;
    Ok(())
}

fn check_upper(upper: &RootPath) -> Result<()> {
    let check = |ok: bool, what| {
        if ok {
            Ok(())
        } else {
            Err(Error::ProbeCheck(what))
        }
    };
    check(
        upper.statx("new")?.file_type() == FileType::Regular,
        "a file made through the overlay is not in the upper directory",
    )?;
    check(
        lowers::is_whiteout(&upper.statx("file")?) && lowers::is_whiteout(&upper.statx("gone")?),
        "a deleted entry left no whiteout in the upper directory",
    )?;
    check(
        upper
            .xattrs("dir")?
            .iter()
            .any(|(n, v)| n == OPAQUE && v.as_slice() == b"y"),
        "a directory made again is not marked user.overlay.opaque (userxattr)",
    )
}
