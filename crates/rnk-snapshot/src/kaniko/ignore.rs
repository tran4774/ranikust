//! The kaniko-mode ignore list: what the walk, the extraction into the
//! live root and the deletion between stages leave alone
//! (`design/kaniko-mode.md#ignore-list`, `ADR-0038`).
//!
//! As kaniko (`kaniko@44aa7ddc0`): every mount point below the root from
//! `/proc/self/mountinfo` but the root itself (`pkg/util/fs_util.go:630-665`),
//! its default list (`:72-89`: the kaniko directory, `/etc/mtab`, and what is
//! strictly below `/tmp/apt-key-gpghome`), `/var/run` while
//! `--ignore-var-run` is true and each `--ignore-path` as a glob
//! (`cmd/executor/cmd/root.go:155-170`). ranikust adds `/proc`, `/sys` and
//! `/dev` whether mounted or not, all of `/ranikust` for kaniko's `/kaniko`,
//! and the state directory when it lies inside the root.
//!
//! Paths are physical, as kaniko compares them: with `/var/run -> /run`,
//! `/var/run` ignores the symlink, not `/run`, which is ignored only when it
//! is a mount point.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use rnk_rootfs::IgnoreList;

use crate::error::{Error, Result};

/// One line of `/proc/self/mountinfo` (`proc(5)`), the fields ranikust uses.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MountInfo {
    /// Field 1, the mount id (`statx`'s `stx_mnt_id`).
    pub mount_id: u64,
    /// Field 2, the parent's mount id.
    pub parent_id: u64,
    /// Field 3, the filesystem's device, `major:minor` as written.
    pub device: String,
    /// Field 4, the directory of the filesystem mounted here: `/` for a
    /// whole filesystem, a subdirectory for a bind mount; unescaped, and not
    /// a path for some pseudo filesystems (`mnt:[…]` for nsfs).
    pub fs_root: PathBuf,
    /// Field 5, the mount point relative to the process's root, unescaped.
    pub mount_point: PathBuf,
    /// The filesystem type, after the `-` separator.
    pub fs_type: String,
}

/// Parses the text of `/proc/self/mountinfo`. The kernel escapes space,
/// tab, newline and backslash in paths as `\NNN` octal; the optional fields
/// (`shared:N`, `master:N`, …) run up to a lone `-`.
///
/// # Errors
///
/// [`Error::Ignore`] for a line that does not have this shape: a mount
/// point missed would reach a layer or be emptied, so nothing is guessed.
pub fn parse_mountinfo(text: &[u8]) -> Result<Vec<MountInfo>> {
    text.split(|b| *b == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(i, line)| {
            parse_line(line).ok_or_else(|| {
                Error::Ignore(format!(
                    "/proc/self/mountinfo line {}: unexpected format: {}",
                    i + 1,
                    String::from_utf8_lossy(line)
                ))
            })
        })
        .collect()
}

fn parse_line(line: &[u8]) -> Option<MountInfo> {
    let mut fields = line.split(|b| *b == b' ');
    let mount_id = number(fields.next()?)?;
    let parent_id = number(fields.next()?)?;
    let device = String::from_utf8_lossy(fields.next()?).into_owned();
    let fs_root = unescape(fields.next()?)?;
    let mount_point = unescape(fields.next()?)?;
    let _options = fields.next()?;
    // Optional fields, then the separator.
    fields.by_ref().find(|f| *f == b"-")?;
    let fs_type = String::from_utf8_lossy(&unescape(fields.next()?)?).into_owned();
    // `fs_root` is not always a path (`mnt:[4026533352]` for nsfs).
    if !mount_point.starts_with(b"/") {
        return None;
    }
    Some(MountInfo {
        mount_id,
        parent_id,
        device,
        fs_root: PathBuf::from(OsString::from_vec(fs_root)),
        mount_point: PathBuf::from(OsString::from_vec(mount_point)),
        fs_type,
    })
}

fn number(field: &[u8]) -> Option<u64> {
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// Undoes the kernel's `\NNN` octal escapes (`seq_file`'s `mangle_path`).
fn unescape(field: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while let Some(&b) = field.get(i) {
        if b == b'\\' {
            let digits = field.get(i + 1..i + 4)?;
            if !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                return None;
            }
            let v = digits
                .iter()
                .fold(0_u32, |v, d| v * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(v).ok()?);
            i += 4;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Some(out)
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
/// symlink (`RESOLVE_NO_SYMLINKS`), as `ranikust` does with the claimed
/// workdir and the state directory, never the flags as typed; a path that
/// is not normalized is refused.
#[derive(Clone, Copy, Debug)]
pub struct IgnoreSpec<'a> {
    /// The build root on the host, physical: `/` in a container, the
    /// workdir on a host. Mount points below it are ignored.
    pub root: &'a Path,
    /// The state directory on the host, physical; ignored when it lies
    /// inside the root.
    pub state_dir: Option<&'a Path>,
    /// `--ignore-path`, paths in the image, each a glob per component.
    pub ignore_paths: &'a [PathBuf],
    /// `--ignore-var-run` (kaniko's default: true).
    pub ignore_var_run: bool,
}

/// Paths inside the root that kaniko mode always ignores, it and below.
const FIXED: [&str; 5] = ["/proc", "/sys", "/dev", "/ranikust", "/etc/mtab"];

/// kaniko's `PrefixMatchOnly` entry: only what is below it.
const BELOW_ONLY: &str = "/tmp/apt-key-gpghome";

/// The workdir marker (`ranikust`'s workdir checks), ignored in a root that
/// is not `/`: the walk never reports it and deletion never removes it.
const WORKDIR_MARKER: &str = "/.ranikust-root";

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
    if spec.root != Path::new("/") {
        list.insert(WORKDIR_MARKER)?;
    }
    if spec.ignore_var_run {
        list.insert("/var/run")?;
    }
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
