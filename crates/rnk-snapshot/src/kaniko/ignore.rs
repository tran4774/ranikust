//! The kaniko-mode ignore list: what the walk, the extraction into the
//! live root and the deletion between stages leave alone
//! (`design/kaniko-mode.md#ignore-list`, `ADR-0038`).
//!
//! As kaniko (`kaniko@44aa7ddc0`): every mount point below the root from
//! `/proc/self/mountinfo` but the root itself (`pkg/util/fs_util.go:630-665`),
//! its default list (`:72-89`: the kaniko directory, `/etc/mtab`, and what is
//! strictly below `/tmp/apt-key-gpghome`) and each `--ignore-path` as a
//! glob (`cmd/executor/cmd/root.go:155-170`). ranikust adds `/proc`, `/sys`
//! and `/dev` whether mounted or not, all of `/ranikust` for kaniko's
//! `/kaniko`, and the state directory when it lies inside the root.
//!
//! Unlike kaniko, `/var/run` is not on the list by default: it is ignored
//! only where it, or something below it, is a mount point, as every mount
//! point is, so an image's `/var/run -> /run` symlink (alpine 3.12 and
//! later) is extracted and kept (ADR-0052, D4; kaniko's `--ignore-var-run`
//! is refused).
//!
//! Paths are physical, as kaniko compares them.

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rnk_rootfs::IgnoreList;

use crate::error::{Error, Result};

pub use rnk_rootfs::MountInfo;

/// Parses the text of `/proc/self/mountinfo` ([`rnk_rootfs::parse_mountinfo`]).
///
/// # Errors
///
/// [`Error::Ignore`] for a line that does not have the shape `proc(5)`
/// gives: a mount point missed would reach a layer or be emptied, so
/// nothing is guessed.
pub fn parse_mountinfo(text: &[u8]) -> Result<Vec<MountInfo>> {
    rnk_rootfs::parse_mountinfo(text).map_err(|e| Error::Ignore(e.to_string()))
}

/// Reads and parses `/proc/self/mountinfo`.
///
/// # Errors
///
/// [`Error::Io`] when it cannot be read; [`Error::Ignore`] as
/// [`parse_mountinfo`].
pub fn read_mountinfo() -> Result<Vec<MountInfo>> {
    let path = Path::new("/proc/self/mountinfo");
    let text = std::fs::read(path).map_err(|source| Error::Io {
        op: "read",
        path: path.to_owned(),
        source,
    })?;
    parse_mountinfo(&text)
}

/// What the kaniko-mode ignore list is built from, besides the mounts.
///
/// `root` and `state_dir` are compared with mount points and with the
/// physical paths `rnk_rootfs::extract_live` reaches, byte for byte: they
/// must be the kernel's own names for the directories actually used: the
/// caller passes canonical paths it has opened without following a
/// symlink (`RESOLVE_NO_SYMLINKS`), as `ranikust` does with the build
/// root and the state directory, never the flags as typed; a path that is
/// not normalized is refused.
#[derive(Clone, Copy, Debug)]
pub struct IgnoreSpec<'a> {
    /// The build root on the host, physical: `/` in a container, else a
    /// directory `ranikust` made in its state directory (ADR-0050). Mount
    /// points below it are ignored.
    pub root: &'a Path,
    /// The state directory on the host, physical; ignored when it lies
    /// inside the root.
    pub state_dir: Option<&'a Path>,
    /// `--ignore-path`, paths in the image, each a glob per component.
    pub ignore_paths: &'a [PathBuf],
}

/// Paths inside the root that kaniko mode always ignores, it and below.
/// `/.dockerenv` and `/run/.containerenv` are the runtime's markers of a
/// container, not image content: kept out of layers and of every emptying,
/// so a later build in the container still sees where it runs.
const FIXED: [&str; 7] = [
    "/proc",
    "/sys",
    "/dev",
    "/ranikust",
    "/etc/mtab",
    "/.dockerenv",
    "/run/.containerenv",
];

/// kaniko's `PrefixMatchOnly` entry: only what is below it.
const BELOW_ONLY: &str = "/tmp/apt-key-gpghome";

/// Builds the kaniko-mode ignore list for `spec` and the mounts of
/// `mounts` (from [`read_mountinfo`]): physical paths inside the root.
///
/// # Errors
///
/// [`Error::Ignore`] when the root or the state directory is not an
/// absolute, normalized path (no `.`, `..` or empty component), or the
/// state directory is the root or an ancestor of it; [`Error::Rootfs`] for
/// an `--ignore-path` that leaves the root, names it, or is a malformed
/// glob.
pub fn ignore_list(spec: &IgnoreSpec<'_>, mounts: &[MountInfo]) -> Result<IgnoreList> {
    normalized("root", spec.root)?;
    if let Some(state) = spec.state_dir {
        normalized("state directory", state)?;
        if spec.root.starts_with(state) {
            return Err(Error::Ignore(format!(
                "the state directory {} is the root {} or an ancestor of it",
                state.display(),
                spec.root.display()
            )));
        }
    }
    let mut list = IgnoreList::new();
    for m in mounts {
        if let Some(inside) = strictly_inside(spec.root, &m.mount_point) {
            list.insert(inside)?;
        }
    }
    for p in FIXED {
        list.insert(p)?;
    }
    list.insert_below(BELOW_ONLY)?;
    for p in spec.ignore_paths {
        list.insert_glob(p)?;
    }
    if let Some(inside) = spec.state_dir.and_then(|s| strictly_inside(spec.root, s)) {
        list.insert(inside)?;
    }
    Ok(list)
}

/// Refuses `p` unless it is absolute and normalized: `/`, or `/` followed
/// by names that are not empty, `.` or `..`, without a trailing `/`.
fn normalized(what: &str, p: &Path) -> Result<()> {
    let bytes = p.as_os_str().as_bytes();
    let ok = bytes == b"/"
        || bytes.strip_prefix(b"/").is_some_and(|rest| {
            rest.split(|b| *b == b'/')
                .all(|c| !c.is_empty() && c != b"." && c != b"..")
        });
    if ok {
        Ok(())
    } else {
        Err(Error::Ignore(format!(
            "the {what} {} is not an absolute, normalized path",
            p.display()
        )))
    }
}

/// `p` relative to `root` when it lies strictly below it, compared by
/// component on the bytes of both (no symlink resolved).
fn strictly_inside(root: &Path, p: &Path) -> Option<PathBuf> {
    let rel = p.strip_prefix(root).ok()?;
    if rel.as_os_str().as_bytes().is_empty() {
        None
    } else {
        Some(rel.to_owned())
    }
}

#[cfg(test)]
mod tests;
