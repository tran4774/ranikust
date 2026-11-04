//! `/proc/self/mountinfo` (`proc(5)`), parsed in one place for the crates
//! that read it: `rnk-snapshot`'s ignore list and `rnk-sandbox`'s
//! measurement of the binary's mount (ADR-0101).

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;

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
    /// No write goes through this mount: `ro` first in its own options
    /// (field 6) or in its filesystem's (the field after the source).
    pub read_only: bool,
}

/// A line of `/proc/self/mountinfo` that does not have the shape
/// `proc(5)` gives it.
#[derive(Debug, thiserror::Error)]
#[error("/proc/self/mountinfo line {line}: unexpected format: {text}")]
#[non_exhaustive]
pub struct MountInfoError {
    /// The line's number, from 1.
    pub line: usize,
    /// The line, lossily decoded.
    pub text: String,
}

/// Parses the text of `/proc/self/mountinfo`. The kernel escapes space,
/// tab, newline and backslash in paths as `\NNN` octal; the optional fields
/// (`shared:N`, `master:N`, …) run up to a lone `-`. Fields after the file
/// system's options are ignored: a file system may print an option value
/// with a space in it.
///
/// # Errors
///
/// [`MountInfoError`] for a line that does not have this shape, or whose
/// file system's options do not start with `ro` or `rw`: a caller that
/// missed a mount could guess wrong, so nothing is guessed.
pub fn parse_mountinfo(text: &[u8]) -> Result<Vec<MountInfo>, MountInfoError> {
    text.split(|b| *b == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(i, line)| {
            parse_line(line).ok_or_else(|| MountInfoError {
                line: i + 1,
                text: String::from_utf8_lossy(line).into_owned(),
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
    let options = fields.next()?;
    // Optional fields, then the separator.
    fields.by_ref().find(|f| *f == b"-")?;
    let fs_type = String::from_utf8_lossy(&unescape(fields.next()?)?).into_owned();
    let _source = fields.next()?;
    let super_options = fields.next()?;
    // Linux writes `ro` or `rw` first: anything else is a source shifted by
    // a space (one that lands on `ro` or `rw` goes unseen).
    let fs_rw = matches!(first_option(super_options), Some(b"ro" | b"rw"));
    // `fs_root` is not always a path (`mnt:[4026533352]` for nsfs).
    if !mount_point.starts_with(b"/") || !fs_rw {
        return None;
    }
    let ro = |opts: &[u8]| first_option(opts) == Some(b"ro".as_slice());
    Some(MountInfo {
        mount_id,
        parent_id,
        device,
        fs_root: PathBuf::from(OsString::from_vec(fs_root)),
        mount_point: PathBuf::from(OsString::from_vec(mount_point)),
        fs_type,
        read_only: ro(options) || ro(super_options),
    })
}

fn first_option(opts: &[u8]) -> Option<&[u8]> {
    opts.split(|b| *b == b',').next()
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_mount_is_read_only_by_its_own_options_or_its_filesystems() {
        let text = b"\
1 0 8:1 / / rw,relatime - ext4 /dev/sda1 rw
2 1 8:1 /a /a ro,relatime - ext4 /dev/sda1 rw
3 1 8:2 / /b rw,relatime shared:4 - ext4 /dev/sdb1 ro,errors=remount-ro
4 1 8:1 /my\\040dir /c\\040d rw - ext4 /dev/sda1 rw,errors=remount-ro
";
        let mounts = parse_mountinfo(text).unwrap();
        let ro: Vec<bool> = mounts.iter().map(|m| m.read_only).collect();
        assert_eq!(ro, [false, true, true, false]);
        assert_eq!(mounts[3].fs_root, Path::new("/my dir"));
        assert_eq!(mounts[3].mount_point, Path::new("/c d"));
    }

    #[test]
    fn a_line_with_missing_or_shifted_options_is_refused() {
        for bad in [
            "1 0 8:1 / / rw - ext4 /dev/sda1",
            "1 0 8:1 / / rw - ext4",
            // A source with a space: the options read are not options.
            "1 0 8:1 / / rw - ext4 /dev/my disk rw",
        ] {
            let e = parse_mountinfo(bad.as_bytes()).unwrap_err();
            assert_eq!(e.line, 1, "{bad:?}");
        }
        // A mount's own options that do not start with `ro` or `rw` are
        // only read for `ro`.
        let odd = parse_mountinfo(b"1 0 8:1 / / ,noatime - ext4 /dev/sda1 rw\n").unwrap();
        assert!(!odd[0].read_only);
    }

    /// An option value printed with a space spills into more fields; only
    /// the first option, `ro` or `rw`, tells a read-only file system.
    #[test]
    fn a_spilled_option_is_kept_out_of_read_only() {
        let text = b"\
1 0 0:40 / /n rw - nfs srv:/x rw,fsc=a b,ro
2 0 0:41 / /m rw - nfs srv:/y ro,fsc=a b
3 0 0:42 / /o rw - nfs srv:/z rw,fsc=a,ro b
";
        let mounts = parse_mountinfo(text).unwrap();
        let ro: Vec<bool> = mounts.iter().map(|m| m.read_only).collect();
        assert_eq!(ro, [false, true, false]);
    }
}
