//! The canary of a user namespace's id maps (ADR-0128): a directory of
//! entries owned by the first and the last id of each extent of the maps,
//! made before the first layer extracted under them is published. A layer's
//! key holds the maps relative to the parent namespace only (ADR-0104), so
//! a change above it, or a `chown` of the state directory on disk, keeps
//! the key; it shows in the owners the canary's entries have now.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rnk_rootfs::{FileType, OpenHow, RootPath, chown_id};
use rustix::io::Errno;
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};
use crate::key::LAYER_LAYOUT;
use crate::userns::IdMaps;

/// Puts a canary's name in a space of its own.
const DOMAIN: &[u8] = b"ranikust canary of a user namespace's id maps, v1\0";

/// The directory of the canaries, beside the layers they guard: never an
/// algorithm's name, which `layers/<layout>/` holds otherwise.
const CANARIES: &str = "canary";

/// The mode of an entry: only its owner tells anything.
const ENTRY_MODE: u32 = 0o444;

/// One entry of a canary: its name and the owner it must have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// `uid-<id>` or `gid-<id>`.
    pub(crate) name: String,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

/// What [`check`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// No canary of these maps: no layer was published under them.
    Absent,
    /// A canary whose every entry has its owner.
    Matches,
}

/// `layers/<layout>/canary/<hex>`, the canary of `maps`, relative to the
/// state directory; its lock is this below `locks/`.
pub(crate) fn path(maps: &IdMaps) -> PathBuf {
    let mut h = Sha256::new();
    h.update(DOMAIN);
    h.update(maps.uid_map());
    h.update(b"\0");
    h.update(maps.gid_map());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Path::new("layers")
        .join(LAYER_LAYOUT)
        .join(CANARIES)
        .join(hex)
}

/// The entries of the canary of `maps`: `uid-<id>` owned by the uid `id`
/// for the first and the last id of each uid extent, and `gid-<id>` by the
/// gid `id` for each gid extent's; each owned besides by the lowest id the
/// other map gives.
pub(crate) fn entries(maps: &IdMaps) -> Vec<Entry> {
    let ends = |extents: &mut dyn Iterator<Item = (u32, u32)>| -> BTreeSet<u32> {
        extents.flat_map(|(first, last)| [first, last]).collect()
    };
    let uids = ends(&mut maps.uid_extents());
    let gids = ends(&mut maps.gid_extents());
    // A map has a line, so each set has an id.
    let (uid0, gid0) = (
        uids.first().copied().unwrap_or_default(),
        gids.first().copied().unwrap_or_default(),
    );
    let by_uid = uids.iter().map(|&uid| Entry {
        name: format!("uid-{uid}"),
        uid,
        gid: gid0,
    });
    let by_gid = gids.iter().map(|&gid| Entry {
        name: format!("gid-{gid}"),
        uid: uid0,
        gid,
    });
    by_uid.chain(by_gid).collect()
}

/// Where a canary is checked from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// The user namespace of the maps: an entry's owner is its id.
    Inside,
    /// That namespace's parent, before the namespace exists: an entry's
    /// owner is the id the maps give its id there.
    Parent,
}

/// Checks the canary of `maps` in the state directory `state`, writing
/// nothing: absent, or there with every entry of [`entries`] a regular file
/// of the owner it must have, as this process's user namespace, whose maps
/// they are, shows it.
///
/// # Errors
///
/// [`Error::IdMapChanged`] for a canary that is not a directory of the
/// state directory's mount, cannot be searched, or lacks an entry or has
/// one of another type or owner; [`Error::Rootfs`] when it cannot be read
/// otherwise.
pub(crate) fn check(state: &RootPath, state_path: &Path, maps: &IdMaps) -> Result<Found> {
    check_as(state, state_path, maps, View::Inside)
}

/// [`check`] from the parent of a user namespace whose maps are `maps`,
/// relative to this process's namespace: one this process may make. Each
/// entry must be owned by the ids `maps` give its own here, which is what
/// the namespace would see as its own.
///
/// # Errors
///
/// As [`check`].
pub(crate) fn check_from_parent(
    state: &RootPath,
    state_path: &Path,
    maps: &IdMaps,
) -> Result<Found> {
    check_as(state, state_path, maps, View::Parent)
}

/// [`check`] from `view`.
fn check_as(state: &RootPath, state_path: &Path, maps: &IdMaps, view: View) -> Result<Found> {
    let rel = path(maps);
    let changed = |why: String| Error::IdMapChanged {
        path: state_path.to_owned(),
        why,
    };
    let denied = |e: &rnk_rootfs::Error| {
        [Errno::ACCESS, Errno::PERM]
            .iter()
            .any(|n| e.raw_os_error() == Some(n.raw_os_error()))
    };
    let on_mount = |st: &rnk_rootfs::Stat| !st.mount_root && st.mnt_id == state.mnt_id();
    match state.statx(&rel) {
        Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
            return Ok(Found::Absent);
        }
        Err(e) if denied(&e) => {
            return Err(changed(format!("{} cannot be read: {e}", rel.display())));
        }
        Err(e) => return Err(e.into()),
        Ok(st) if st.file_type() != FileType::Directory || !on_mount(&st) => {
            return Err(changed(format!(
                "{} is not a directory of the state directory's mount",
                rel.display()
            )));
        }
        Ok(_) => {}
    }
    for entry in entries(maps) {
        let p = rel.join(&entry.name);
        let st = match state.statx(&p) {
            Ok(st) => st,
            Err(e) if e.raw_os_error() == Some(Errno::NOENT.raw_os_error()) => {
                return Err(changed(format!("{} is missing", p.display())));
            }
            Err(e) if denied(&e) => {
                return Err(changed(format!("{} cannot be read: {e}", p.display())));
            }
            Err(e) => return Err(e.into()),
        };
        if st.file_type() != FileType::Regular || !on_mount(&st) {
            return Err(changed(format!(
                "{} is not a regular file of the state directory's mount",
                p.display()
            )));
        }
        let (want, of) = match view {
            View::Inside => (Some((entry.uid, entry.gid)), String::new()),
            View::Parent => (
                maps.uid_outside(entry.uid).zip(maps.gid_outside(entry.gid)),
                format!(
                    " (the build's user namespace's {}:{})",
                    entry.uid, entry.gid
                ),
            ),
        };
        // An entry's ids are the ends of the maps' extents, so the maps give
        // each: none would be a bug, refused all the same.
        let Some((uid, gid)) = want else {
            return Err(changed(format!("{} has no id here", p.display())));
        };
        if (st.uid, st.gid) != (uid, gid) {
            return Err(changed(format!(
                "{} is owned by {}:{}, not {uid}:{gid}{of}",
                p.display(),
                st.uid,
                st.gid,
            )));
        }
    }
    Ok(Found::Matches)
}

/// Makes the canary of `maps`, which the caller found absent under its
/// lock: each entry of [`entries`] made in the new directory `tmp` and
/// given its owner through its descriptor, then `tmp` renamed into place,
/// never over an existing canary. The caller removes `tmp` on failure.
///
/// # Errors
///
/// [`Error::Rootfs`] when an entry cannot be made or given its owner
/// ([`rnk_rootfs::Error::Unmapped`] for an owner the namespace does not
/// map), or the directory cannot be renamed.
pub(crate) fn create(state: &RootPath, maps: &IdMaps, tmp: &Path, dir_mode: u32) -> Result<()> {
    state.create_dir(tmp, dir_mode)?;
    for entry in entries(maps) {
        let p = tmp.join(&entry.name);
        let fd = state.open_at(&p, &OpenHow::read().create_new(ENTRY_MODE))?;
        rustix::fs::fchown(
            &fd,
            chown_id(entry.uid).map(rustix::fs::Uid::from_raw),
            chown_id(entry.gid).map(rustix::fs::Gid::from_raw),
        )
        .map_err(|e| match e {
            Errno::INVAL => rnk_rootfs::Error::Unmapped(rnk_rootfs::Unmapped::chown(
                &p,
                Some(entry.uid),
                Some(entry.gid),
            )),
            e => rnk_rootfs::Error::Io {
                op: "chown",
                path: p.clone(),
                source: e.into(),
            },
        })?;
    }
    let rel = path(maps);
    if let Some(parent) = rel.parent() {
        state.create_dir_all(parent, dir_mode)?;
    }
    state.rename_noreplace(tmp, &rel)?;
    Ok(())
}

#[cfg(test)]
mod tests;
