//! The mounts an rbind of `/sys` copied onto its root mount, found in
//! mountinfo by the `RUN` child after `clone` (`hide_submounts`): nothing
//! is allocated, and a line of any length is read in pieces.

use std::ffi::CStr;

use rustix::io::Errno;

/// Room for a mount point as mountinfo writes it (`\ooo` escapes).
pub(super) const POINT_MAX: usize = 4096;

/// Room for the mounts directly on the rbind of `/sys`: an outer runtime
/// makes a handful (a cgroup hierarchy or two, its masks).
pub(super) const SUBMOUNTS_MAX: usize = 64;

/// A line of mountinfo: the mount's id, its parent's, and its mount point
/// as written, `None` where it is longer than [`POINT_MAX`].
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Line<'a> {
    pub(super) id: u64,
    pub(super) parent: u64,
    pub(super) point: Option<&'a [u8]>,
}

/// Reads mountinfo given in pieces of any size, line by line.
pub(super) struct Lines {
    point: [u8; POINT_MAX],
    len: usize,
    long: bool,
    /// The field being read: 0 the mount id, 1 its parent's, 4 the mount
    /// point, 5 anything after it.
    field: u8,
    id: u64,
    parent: u64,
    /// Whether the id being read has a digit.
    digits: bool,
    /// Whether a line has begun and not ended.
    started: bool,
}

impl Lines {
    pub(super) const fn new() -> Self {
        Self {
            point: [0; POINT_MAX],
            len: 0,
            long: false,
            field: 0,
            id: 0,
            parent: 0,
            digits: false,
            started: false,
        }
    }

    /// Reads `bytes`, giving `each` every line they end. `EINVAL` for a
    /// line that does not begin with two decimal ids, two fields and a mount
    /// point; an error of `each` ends the reading with it.
    pub(super) fn feed(
        &mut self,
        bytes: &[u8],
        each: &mut dyn FnMut(Line<'_>) -> Result<(), Errno>,
    ) -> Result<(), Errno> {
        for &b in bytes {
            self.started = true;
            if b == b'\n' {
                if self.field < 5 {
                    return Err(Errno::INVAL);
                }
                let point = if self.long {
                    None
                } else {
                    self.point.get(..self.len)
                };
                each(Line {
                    id: self.id,
                    parent: self.parent,
                    point,
                })?;
                self.restart();
                continue;
            }
            match self.field {
                0 | 1 if b == b' ' => {
                    if !self.digits {
                        return Err(Errno::INVAL);
                    }
                    self.field += 1;
                    self.digits = false;
                }
                0 | 1 => {
                    let d = match b {
                        b'0'..=b'9' => u64::from(b - b'0'),
                        _ => return Err(Errno::INVAL),
                    };
                    let n = if self.field == 0 {
                        &mut self.id
                    } else {
                        &mut self.parent
                    };
                    *n = n
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(d))
                        .ok_or(Errno::INVAL)?;
                    self.digits = true;
                }
                2 | 3 if b == b' ' => self.field += 1,
                4 if b == b' ' => self.field = 5,
                4 => match self.point.get_mut(self.len) {
                    Some(slot) => {
                        *slot = b;
                        self.len += 1;
                    }
                    None => self.long = true,
                },
                _ => {}
            }
        }
        Ok(())
    }

    /// `EINVAL` where the pieces read ended inside a line.
    pub(super) fn finish(&self) -> Result<(), Errno> {
        if self.started {
            Err(Errno::INVAL)
        } else {
            Ok(())
        }
    }

    fn restart(&mut self) {
        self.len = 0;
        self.long = false;
        self.field = 0;
        self.id = 0;
        self.parent = 0;
        self.digits = false;
        self.started = false;
    }
}

/// The mount point `point`, below the mount point `root`, relative to it,
/// both as mountinfo writes them: written into `out` with its `\ooo`
/// escapes undone and a NUL. `EXDEV` where `point` is not below `root`,
/// `EBUSY` where it is `root` itself, `EINVAL` for a malformed escape or a
/// NUL, `ENAMETOOLONG` where `out` is too small.
pub(super) fn relative<'o>(
    root: &[u8],
    point: &[u8],
    out: &'o mut [u8],
) -> Result<&'o CStr, Errno> {
    if point == root {
        return Err(Errno::BUSY);
    }
    let below = if root == b"/" {
        point.strip_prefix(b"/")
    } else {
        point.strip_prefix(root).and_then(|r| r.strip_prefix(b"/"))
    };
    let rest = below.ok_or(Errno::XDEV)?;
    if rest.is_empty() {
        return Err(Errno::BUSY);
    }
    let mut n = 0;
    let mut i = 0;
    while let Some(&b) = rest.get(i) {
        let byte = if b == b'\\' {
            let digits = rest.get(i + 1..i + 4).ok_or(Errno::INVAL)?;
            let mut v = 0_u32;
            for &d in digits {
                if !(b'0'..=b'7').contains(&d) {
                    return Err(Errno::INVAL);
                }
                v = v * 8 + u32::from(d - b'0');
            }
            i += 4;
            u8::try_from(v).map_err(|_| Errno::INVAL)?
        } else {
            i += 1;
            b
        };
        if byte == 0 {
            return Err(Errno::INVAL);
        }
        *out.get_mut(n).ok_or(Errno::NAMETOOLONG)? = byte;
        n += 1;
    }
    *out.get_mut(n).ok_or(Errno::NAMETOOLONG)? = 0;
    let with_nul = out.get(..=n).ok_or(Errno::NAMETOOLONG)?;
    CStr::from_bytes_with_nul(with_nul).map_err(|_| Errno::INVAL)
}

/// Reads mountinfo from its start each time it is called, giving each line
/// to its argument.
pub(super) type Scan<'s> =
    dyn FnMut(&mut dyn FnMut(Line<'_>) -> Result<(), Errno>) -> Result<(), Errno> + 's;

/// Gives `cover`, in the order mountinfo lists them, the mount point of
/// each mount directly on the mount `bind` (whose parent it is), relative
/// to `bind`'s. None may be left out: `bind` missing, a mount point too
/// long, a mount on `bind` that a later scan no longer lists, and more
/// than [`SUBMOUNTS_MAX`] of them are errors.
pub(super) fn each_submount(
    bind: u64,
    scan: &mut Scan<'_>,
    cover: &mut dyn FnMut(&CStr) -> Result<(), Errno>,
) -> Result<(), Errno> {
    let mut root = [0_u8; POINT_MAX];
    let mut root_len = None;
    let mut ids = [0_u64; SUBMOUNTS_MAX];
    let mut n = 0;
    scan(&mut |line| {
        if line.id == bind {
            let p = line.point.ok_or(Errno::NAMETOOLONG)?;
            root.get_mut(..p.len())
                .ok_or(Errno::NAMETOOLONG)?
                .copy_from_slice(p);
            root_len = Some(p.len());
        } else if line.parent == bind {
            *ids.get_mut(n).ok_or(Errno::TOOBIG)? = line.id;
            n += 1;
        }
        Ok(())
    })?;
    let root = root
        .get(..root_len.ok_or(Errno::NOENT)?)
        .ok_or(Errno::NOENT)?;
    let mut rel = [0_u8; POINT_MAX + 1];
    for &id in ids.get(..n).unwrap_or_default() {
        let mut len = None;
        scan(&mut |line| {
            if line.id == id {
                let p = line.point.ok_or(Errno::NAMETOOLONG)?;
                len = Some(relative(root, p, &mut rel)?.to_bytes_with_nul().len());
            }
            Ok(())
        })?;
        let with_nul = rel
            .get(..len.ok_or(Errno::NOENT)?)
            .ok_or(Errno::NAMETOOLONG)?;
        cover(CStr::from_bytes_with_nul(with_nul).map_err(|_| Errno::INVAL)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mountinfo as an rbind of a rootless Podman container's `/sys` at
    /// `/state/root/sys` (mount 900) shows it: a cgroup2 hierarchy, a
    /// runtime's masks, a mount below the cgroup one, a file's mask, a
    /// mount point with a space, and lines longer than any piece read.
    fn fixture() -> Vec<u8> {
        let long = "x".repeat(9000);
        let mut text = String::new();
        for line in [
            format!("21 1 0:20 / / rw shared:1 - overlay overlay rw,lowerdir={long}"),
            "900 21 0:21 / /state/root/sys ro,nosuid - sysfs sysfs ro".to_owned(),
            "901 900 0:22 / /state/root/sys/fs/cgroup ro - cgroup2 cgroup2 rw".to_owned(),
            "902 900 0:23 / /state/root/sys/firmware ro - tmpfs tmpfs ro".to_owned(),
            "903 901 0:24 / /state/root/sys/fs/cgroup/x ro - tmpfs tmpfs ro".to_owned(),
            "904 900 0:5 /null /state/root/sys/power/state ro - devtmpfs udev rw".to_owned(),
            r"905 900 0:25 / /state/root/sys/kernel/a\040b ro - configfs configfs rw".to_owned(),
            "906 21 0:26 / /state/root/sysx ro - tmpfs tmpfs ro".to_owned(),
            format!("907 21 0:27 / /state/{long} ro - tmpfs tmpfs ro"),
        ] {
            text.push_str(&line);
            text.push('\n');
        }
        text.into_bytes()
    }

    /// Reads `text` in pieces of `piece` bytes.
    fn read_in(
        text: &[u8],
        piece: usize,
        each: &mut dyn FnMut(Line<'_>) -> Result<(), Errno>,
    ) -> Result<(), Errno> {
        let mut lines = Lines::new();
        for chunk in text.chunks(piece) {
            lines.feed(chunk, each)?;
        }
        lines.finish()
    }

    /// A line's mount id, its parent's and its mount point.
    type Seen = (u64, u64, Option<Vec<u8>>);

    fn lines_of(text: &[u8], piece: usize) -> Result<Vec<Seen>, Errno> {
        let mut seen = Vec::new();
        read_in(text, piece, &mut |l| {
            seen.push((l.id, l.parent, l.point.map(<[u8]>::to_vec)));
            Ok(())
        })?;
        Ok(seen)
    }

    fn submounts_of(text: &[u8], bind: u64) -> Result<Vec<String>, Errno> {
        let mut covered = Vec::new();
        each_submount(bind, &mut |each| read_in(text, 7, each), &mut |rel| {
            covered.push(rel.to_str().unwrap().to_owned());
            Ok(())
        })?;
        Ok(covered)
    }

    #[test]
    fn mountinfo_read_in_any_pieces_gives_the_same_lines() {
        let text = fixture();
        let whole = lines_of(&text, text.len()).unwrap();
        assert_eq!(whole.len(), 9);
        assert_eq!(whole[1], (900, 21, Some(b"/state/root/sys".to_vec())));
        assert_eq!(
            whole[6],
            (905, 900, Some(br"/state/root/sys/kernel/a\040b".to_vec()))
        );
        // Longer than the room for it: no mount point.
        assert_eq!(whole[8], (907, 21, None));
        for piece in [1, 2, 3, 5, 64, 1024, 4097] {
            assert_eq!(lines_of(&text, piece).unwrap(), whole, "{piece}");
        }
    }

    #[test]
    fn a_malformed_or_cut_mountinfo_is_refused() {
        for text in [
            &b"x1 2 0:1 / /a rw - t t rw\n"[..],
            b"1 2x 0:1 / /a rw - t t rw\n",
            b" 2 0:1 / /a rw - t t rw\n",
            b"1 2 0:1 /\n",
            b"1 2 0:1 / /a\n",
            b"99999999999999999999 2 0:1 / /a rw - t t rw\n",
            b"1 2 0:1 / /a rw - t t rw",
        ] {
            assert_eq!(lines_of(text, 3), Err(Errno::INVAL), "{text:?}");
        }
        assert_eq!(lines_of(b"", 3), Ok(Vec::new()));
    }

    /// The mounts directly on the rbind, each relative to it and its
    /// escapes undone; not the one below the cgroup mount, nor a mount
    /// beside the rbind whose path begins the same.
    #[test]
    fn the_submounts_of_the_bind_are_each_given_relative_to_it() {
        assert_eq!(
            submounts_of(&fixture(), 900).unwrap(),
            ["fs/cgroup", "firmware", "power/state", "kernel/a b"]
        );
        assert_eq!(submounts_of(&fixture(), 901).unwrap(), ["x"]);
        assert_eq!(
            submounts_of(b"1 0 0:1 / / rw - t t rw\n2 1 0:2 / /sys rw - t t rw\n", 1).unwrap(),
            ["sys"]
        );
    }

    #[test]
    fn a_submount_that_cannot_be_placed_fails() {
        let one = |line: &str| format!("900 21 0:21 / /r/sys ro - sysfs sysfs ro\n{line}\n");
        for (line, e) in [
            ("901 900 0:1 / /r/sysfs ro - t t ro", Errno::XDEV),
            ("901 900 0:1 / /r/sys ro - t t ro", Errno::BUSY),
            (r"901 900 0:1 / /r/sys/a\04 ro - t t ro", Errno::INVAL),
            (r"901 900 0:1 / /r/sys/a\000 ro - t t ro", Errno::INVAL),
            (r"901 900 0:1 / /r/sys/a\400 ro - t t ro", Errno::INVAL),
        ] {
            assert_eq!(submounts_of(one(line).as_bytes(), 900), Err(e), "{line}");
        }
        let long = format!("901 900 0:1 / /r/sys/{} ro - t t ro", "y".repeat(POINT_MAX));
        assert_eq!(
            submounts_of(one(&long).as_bytes(), 900),
            Err(Errno::NAMETOOLONG)
        );
        // The bind itself missing.
        assert_eq!(
            submounts_of(b"901 900 0:1 / /r/sys/a ro - t t ro\n", 900),
            Err(Errno::NOENT)
        );
        // More than there is room for.
        let mut many = String::from("900 21 0:21 / /r/sys ro - sysfs sysfs ro\n");
        for i in 0..=SUBMOUNTS_MAX {
            many.push_str(&format!("{} 900 0:1 / /r/sys/{i} ro - t t ro\n", 1000 + i));
        }
        assert_eq!(submounts_of(many.as_bytes(), 900), Err(Errno::TOOBIG));
    }

    /// A mount on the bind that a later reading no longer lists is an
    /// error, not one left out; an error of `cover` ends the walk.
    #[test]
    fn a_submount_gone_between_readings_fails() {
        let first =
            b"900 21 0:21 / /r/sys ro - sysfs sysfs ro\n901 900 0:1 / /r/sys/a ro - t t ro\n";
        let later = b"900 21 0:21 / /r/sys ro - sysfs sysfs ro\n";
        let mut scans = 0;
        let r = each_submount(
            900,
            &mut |each| {
                scans += 1;
                read_in(if scans == 1 { first } else { later }, 5, each)
            },
            &mut |_| Ok(()),
        );
        assert_eq!(r, Err(Errno::NOENT));
        let r = each_submount(900, &mut |each| read_in(first, 5, each), &mut |_| {
            Err(Errno::PERM)
        });
        assert_eq!(r, Err(Errno::PERM));
    }
}
