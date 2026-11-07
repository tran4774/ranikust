use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tar::{EntryType, Header};

use super::*;
use crate::test_util::{Fixture, errno, snapshot};

/// Builds tar streams with raw (unchecked) names, as a hostile layer would.
struct Tar {
    out: Vec<u8>,
    uid: u64,
    gid: u64,
}

impl Tar {
    fn new(f: &Fixture) -> Self {
        let (uid, gid) = f.ids();
        Self {
            out: Vec::new(),
            uid: u64::from(uid),
            gid: u64::from(gid),
        }
    }

    fn header(&self, name: &[u8], t: EntryType, mode: u32, size: u64) -> Header {
        let mut h = Header::new_gnu();
        let old = h.as_old_mut();
        old.name = [0; 100];
        old.name[..name.len()].copy_from_slice(name);
        h.set_entry_type(t);
        h.set_mode(mode);
        h.set_uid(self.uid);
        h.set_gid(self.gid);
        h.set_mtime(1_000_000);
        h.set_size(size);
        h
    }

    fn push(&mut self, mut h: Header, data: &[u8]) -> &mut Self {
        h.set_cksum();
        self.out.extend_from_slice(h.as_bytes());
        self.out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.out.extend(std::iter::repeat_n(0, pad));
        self
    }

    fn dir(&mut self, name: &str, mode: u32) -> &mut Self {
        let h = self.header(name.as_bytes(), EntryType::Directory, mode, 0);
        self.push(h, b"")
    }

    fn file(&mut self, name: &str, mode: u32, data: &[u8]) -> &mut Self {
        let size = u64::try_from(data.len()).unwrap();
        let h = self.header(name.as_bytes(), EntryType::Regular, mode, size);
        self.push(h, data)
    }

    fn link(&mut self, t: EntryType, name: &str, target: &[u8]) -> &mut Self {
        let mut h = self.header(name.as_bytes(), t, 0o777, 0);
        let old = h.as_old_mut();
        old.linkname = [0; 100];
        old.linkname[..target.len()].copy_from_slice(target);
        self.push(h, b"")
    }

    fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.link(EntryType::Symlink, name, target.as_bytes())
    }

    fn hardlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.link(EntryType::Link, name, target.as_bytes())
    }

    fn node(&mut self, name: &str, t: EntryType, major: u32, minor: u32) -> &mut Self {
        let mut h = self.header(name.as_bytes(), t, 0o644, 0);
        h.set_device_major(major).unwrap();
        h.set_device_minor(minor).unwrap();
        self.push(h, b"")
    }

    /// A PAX header for the next entry.
    fn pax(&mut self, records: &[(&str, &[u8])]) -> &mut Self {
        let mut data = Vec::new();
        for (k, v) in records {
            let body = k.len() + v.len() + 3; // " " "=" "\n"
            let mut len = body + 1;
            while len != body + len.to_string().len() {
                len = body + len.to_string().len();
            }
            data.extend_from_slice(format!("{len} {k}=").as_bytes());
            data.extend_from_slice(v);
            data.push(b'\n');
        }
        let size = u64::try_from(data.len()).unwrap();
        let h = self.header(b"pax", EntryType::XHeader, 0o644, size);
        self.push(h, &data)
    }

    fn bytes(&self) -> Vec<u8> {
        let mut v = self.out.clone();
        v.extend(std::iter::repeat_n(0, 1024));
        v
    }
}

fn mode_of(p: &Path) -> u32 {
    fs::symlink_metadata(p).unwrap().mode() & 0o7777
}

fn xattr(p: &Path, name: &str) -> Option<Vec<u8>> {
    let mut buf = vec![0; 256];
    let n = rustix::fs::lgetxattr(p, name, &mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// A `security.capability` that cannot be set (no `CAP_SETFCAP`, as here
/// unprivileged; a filesystem without xattrs) fails the extraction, naming
/// the file, instead of leaving a binary without its capability
/// (kaniko#738, kaniko#2201): the image's layer keeps it either way, but a
/// `RUN` would run the binary without it.
#[test]
fn a_file_capability_that_cannot_be_set_fails_the_extraction() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root may set security.capability");
        return;
    }
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    // VFS_CAP_REVISION_2, cap_net_raw (13) permitted and effective.
    let cap: [u8; 20] = [
        0x01, 0x00, 0x00, 0x02, 0x00, 0x20, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    t.pax(&[("SCHILY.xattr.security.capability", &cap)])
        .file("ping", 0o755, b"elf");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Overlay).unwrap_err();
    let text = e.to_string();
    assert!(text.contains("setxattr") && text.contains("ping"), "{text}");
}

/// A layer's `trusted.*` xattrs are never set, in any mode: a filesystem
/// may give them a meaning it leaves the privileged caller to check (XFS
/// keeps its raw on-disk ACLs as `trusted.SGI_ACL_FILE` and
/// `trusted.SGI_ACL_DEFAULT`), as containerd's `archive.Apply` refuses
/// them before the syscall. Other namespaces are still set. Unprivileged,
/// an attempt would fail the strict extraction with `EPERM`; as root it
/// would leave the xattr there, which the check reads back.
#[test]
fn trusted_xattrs_from_a_layer_are_never_set() {
    for mode in ["Overlay", "Apply", "Literal", "Live"] {
        let f = Fixture::new();
        let mut t = Tar::new(&f);
        t.pax(&[
            ("SCHILY.xattr.trusted.SGI_ACL_FILE", b"\x00\x00\x00\x05"),
            ("SCHILY.xattr.user.app", b"d"),
        ])
        .dir("d/", 0o775)
        .pax(&[
            ("SCHILY.xattr.trusted.x", b"t"),
            ("SCHILY.xattr.user.app", b"f"),
        ])
        .file("d/f", 0o644, b"x");
        let tar = t.bytes();
        match mode {
            "Overlay" => extract(&f.root, tar.as_slice(), Whiteouts::Overlay),
            "Apply" => extract(&f.root, tar.as_slice(), Whiteouts::Apply),
            "Literal" => extract(&f.root, tar.as_slice(), Whiteouts::Literal),
            _ => extract_live(&f.root, tar.as_slice(), &IgnoreList::new()),
        }
        .unwrap_or_else(|e| panic!("{mode}: {e}"));
        assert_eq!(xattr(&f.host("d"), "trusted.SGI_ACL_FILE"), None, "{mode}");
        assert_eq!(xattr(&f.host("d/f"), "trusted.x"), None, "{mode}");
        assert_eq!(xattr(&f.host("d"), "user.app").unwrap(), b"d", "{mode}");
        assert_eq!(xattr(&f.host("d/f"), "user.app").unwrap(), b"f", "{mode}");
        assert_eq!(mode_of(&f.host("d")), 0o775, "{mode}");
    }
}

#[test]
fn regular_layer_keeps_raw_modes_times_and_links() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.dir("./", 0o750)
        .dir("tmp/", 0o1777)
        .dir("usr/bin/", 0o755)
        .file("usr/bin/su", 0o4755, b"setuid")
        .file("usr/bin/g", 0o2755, b"setgid")
        .dir("ro/", 0o555)
        .file("ro/inside", 0o444, b"read-only")
        .symlink("usr/bin/sh", "dash")
        .file("usr/bin/dash", 0o755, b"#!")
        .hardlink("usr/bin/hl", "usr/bin/dash")
        .hardlink("usr/bin/sh2", "/usr/bin/sh")
        .node("fifo", EntryType::Fifo, 0, 0)
        .pax(&[
            ("SCHILY.xattr.user.app", b"v1"),
            ("mtime", b"1700000000.25"),
        ])
        .file("x", 0o640, b"xattr");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Overlay).unwrap();

    assert_eq!(mode_of(&f.host), 0o750);
    assert_eq!(mode_of(&f.host("tmp")), 0o1777);
    assert_eq!(mode_of(&f.host("usr/bin/su")), 0o4755);
    assert_eq!(mode_of(&f.host("usr/bin/g")), 0o2755);
    assert_eq!(mode_of(&f.host("ro")), 0o555);
    assert_eq!(fs::read(f.host("ro/inside")).unwrap(), b"read-only");
    assert_eq!(fs::read(f.host("usr/bin/su")).unwrap(), b"setuid");
    assert_eq!(
        fs::read_link(f.host("usr/bin/sh")).unwrap(),
        Path::new("dash")
    );
    let dash = fs::metadata(f.host("usr/bin/dash")).unwrap();
    assert_eq!(dash.nlink(), 2);
    assert_eq!(
        fs::metadata(f.host("usr/bin/hl")).unwrap().ino(),
        dash.ino()
    );
    // A hardlink to a symlink links the symlink itself.
    let sh2 = fs::symlink_metadata(f.host("usr/bin/sh2")).unwrap();
    assert!(sh2.file_type().is_symlink());
    assert!(
        fs::symlink_metadata(f.host("fifo"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    assert_eq!(xattr(&f.host("x"), "user.app").unwrap(), b"v1");
    let x = f.root.statx("x").unwrap();
    assert_eq!(
        x.mtime,
        FileTime {
            secs: 1_700_000_000,
            nanos: 250_000_000
        }
    );
    // Directory times are set after their entries were added.
    for d in ["tmp", "usr/bin", "ro", ""] {
        assert_eq!(f.root.statx(d).unwrap().mtime.secs, 1_000_000, "{d:?}");
    }
    let (uid, gid) = f.ids();
    let su = f.root.statx("usr/bin/su").unwrap();
    assert_eq!((su.uid, su.gid), (uid, gid));
}

#[test]
fn names_that_leave_the_root_are_refused() {
    for name in ["../evil", "a/../../evil", "/../evil", "./../../evil"] {
        let f = Fixture::new();
        let (out, before) = f.outside();
        let mut t = Tar::new(&f);
        t.file(name, 0o644, b"x");
        let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
        assert!(matches!(e, Error::Entry { .. }), "{name}: {e}");
        assert_eq!(snapshot(&out), before);
        assert!(!f.host.with_file_name("evil").exists());
    }
    for target in ["../../outside/sentinel", "a/../../x"] {
        let f = Fixture::new();
        let mut t = Tar::new(&f);
        t.hardlink("h", target);
        let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
        assert!(matches!(e, Error::Entry { .. }), "{target}: {e}");
    }
}

#[test]
fn absolute_and_dotted_names_are_cleaned_into_the_root() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.file("/abs/file", 0o644, b"1")
        .file("./a/./b", 0o644, b"2")
        .file("c/../d", 0o644, b"3");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    assert_eq!(fs::read(f.host("abs/file")).unwrap(), b"1");
    assert_eq!(fs::read(f.host("a/b")).unwrap(), b"2");
    assert_eq!(fs::read(f.host("d")).unwrap(), b"3");
}

#[test]
fn symlinks_in_the_layer_cannot_redirect_entries_outside() {
    let f = Fixture::new();
    let (out, before) = f.outside();
    let out_s = out.to_str().unwrap().to_owned();
    let mut t = Tar::new(&f);
    t.symlink("lnk", &out_s)
        .symlink("slash", "/")
        .symlink("up", "../../..")
        .file("slash/etc/x", 0o644, b"in")
        .file("up/y", 0o644, b"in");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    assert_eq!(fs::read(f.host("etc/x")).unwrap(), b"in");
    assert_eq!(fs::read(f.host("y")).unwrap(), b"in");

    // Through a symlink to the host path: missing inside, refused.
    let mut t = Tar::new(&f);
    t.file("lnk/pwn", 0o644, b"pwn");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT), "{e}");
    let mut t = Tar::new(&f);
    t.hardlink("lnk/h", "etc/x");
    assert!(extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).is_err());
    assert_eq!(snapshot(&out), before);
}

#[test]
fn an_entry_replaces_a_symlink_without_following_it() {
    let f = Fixture::new();
    let (out, before) = f.outside();
    let target = out.join("sentinel");
    symlink(&target, f.host("f")).unwrap();
    symlink(&out, f.host("d")).unwrap();
    let mut t = Tar::new(&f);
    t.file("f", 0o600, b"new").dir("d/", 0o700);
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    assert_eq!(fs::read(f.host("f")).unwrap(), b"new");
    assert!(fs::symlink_metadata(f.host("d")).unwrap().is_dir());
    assert_eq!(snapshot(&out), before);
}

#[test]
fn replacing_keeps_other_hardlinks_and_merges_directories() {
    let f = Fixture::new();
    fs::write(f.host("f"), b"old").unwrap();
    fs::hard_link(f.host("f"), f.host("g")).unwrap();
    fs::create_dir(f.host("d")).unwrap();
    fs::write(f.host("d/lower"), b"").unwrap();
    let mut t = Tar::new(&f);
    t.file("f", 0o644, b"new")
        .dir("d/", 0o700)
        .dir("d/", 0o711)
        .dir("x/", 0o755)
        .file("x/y", 0o644, b"")
        .file("x", 0o604, b"now a file")
        .file("z", 0o644, b"")
        .dir("z/", 0o701);
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    assert_eq!(fs::read(f.host("f")).unwrap(), b"new");
    assert_eq!(fs::read(f.host("g")).unwrap(), b"old");
    assert!(f.host("d/lower").exists());
    assert_eq!(mode_of(&f.host("d")), 0o711);
    assert_eq!(fs::read(f.host("x")).unwrap(), b"now a file");
    assert_eq!(mode_of(&f.host("x")), 0o604);
    assert_eq!(mode_of(&f.host("z")), 0o701);
}

#[test]
fn overlay_form_whiteouts_and_xattr_escaping() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.dir("d/", 0o755)
        .file("d/.wh..wh..opq", 0o644, b"")
        .file(".wh.gone", 0o644, b"")
        .file("d/.wh.x", 0o644, b"")
        .file("e", 0o644, b"")
        .file(".wh.e", 0o644, b"")
        .file("new/.wh.deep", 0o644, b"")
        .file(".wh..wh.plnk", 0o644, b"")
        .pax(&[
            ("SCHILY.xattr.user.overlay.origin", b"o"),
            ("SCHILY.xattr.user.app", b"a"),
        ])
        .file("content", 0o644, b"");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Overlay).unwrap();
    for w in ["gone", "d/x", "e", "new/deep"] {
        let md = fs::symlink_metadata(f.host(w)).unwrap();
        assert!(md.file_type().is_char_device(), "{w}");
        assert_eq!(md.rdev(), 0, "{w}");
    }
    assert_eq!(xattr(&f.host("d"), "user.overlay.opaque").unwrap(), b"y");
    assert!(!f.host(".wh..wh.plnk").exists());
    assert!(!f.host("d/.wh..wh..opq").exists());
    let c = f.host("content");
    assert_eq!(xattr(&c, "user.overlay.overlay.origin").unwrap(), b"o");
    assert_eq!(xattr(&c, "user.overlay.origin"), None);
    assert_eq!(xattr(&c, "user.app").unwrap(), b"a");
}

#[test]
fn applied_whiteouts_hide_lower_entries_only() {
    let f = Fixture::new();
    let (out, before) = f.outside();
    // The lower layers, already in the root.
    fs::create_dir_all(f.host("a/sub")).unwrap();
    fs::create_dir_all(f.host("a/sub2")).unwrap();
    fs::write(f.host("a/old"), b"").unwrap();
    fs::write(f.host("a/sub/lower"), b"").unwrap();
    fs::write(f.host("a/sub2/lower2"), b"").unwrap();
    fs::create_dir_all(f.host("gone_dir/x")).unwrap();
    fs::write(f.host("gone_dir/x/f"), b"").unwrap();
    fs::write(f.host("gone_file"), b"").unwrap();
    symlink(&out, f.host("s")).unwrap();
    fs::create_dir_all(f.host("b/keepdir")).unwrap();
    fs::write(f.host("b/keepdir/lower"), b"").unwrap();

    let mut t = Tar::new(&f);
    t.dir("a/", 0o755)
        .file("a/new", 0o644, b"")
        .dir("a/sub/", 0o755)
        .file("a/sub/mine", 0o644, b"")
        .file("a/.wh..wh..opq", 0o644, b"")
        .file("a/after", 0o644, b"")
        .file(".wh.gone_dir", 0o644, b"")
        .file(".wh.gone_file", 0o644, b"")
        .file(".wh.missing", 0o644, b"")
        .file("nodir/.wh.missing", 0o644, b"")
        .file(".wh.s", 0o644, b"")
        .file("b/keepdir/mine", 0o644, b"")
        .file("b/.wh..wh..opq", 0o644, b"");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();

    let left = crate::test_util::tree(&f.host);
    let expect: std::collections::BTreeSet<PathBuf> = [
        "a",
        "a/new",
        "a/sub",
        "a/sub/mine",
        "a/after",
        "b",
        "b/keepdir",
        "b/keepdir/mine",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    assert_eq!(left, expect);
    assert_eq!(snapshot(&out), before);
}

/// What a layer deletes again is no longer its own: an opaque whiteout
/// after it keeps only what the layer still has, a name it made again
/// included, and the directories leading there.
#[test]
fn an_opaque_whiteout_keeps_what_the_layer_still_has() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("d")).unwrap();
    fs::write(f.host("d/lower"), b"").unwrap();
    let mut t = Tar::new(&f);
    t.file("d/gone/x/f", 0o644, b"")
        .file("d/.wh.gone", 0o644, b"")
        .file("d/again", 0o644, b"")
        .file("d/.wh.again", 0o644, b"")
        .file("d/again/deep/f", 0o644, b"")
        .file("d/kept/f", 0o644, b"")
        .file("d/kept/.wh.f", 0o644, b"")
        .file("d/kept/g", 0o644, b"")
        .file("d/.wh..wh..opq", 0o644, b"");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    let left = crate::test_util::tree(&f.host);
    let expect: std::collections::BTreeSet<PathBuf> = [
        "d",
        "d/again",
        "d/again/deep",
        "d/again/deep/f",
        "d/kept",
        "d/kept/g",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    assert_eq!(left, expect);
}

/// A directory that led to an entry the layer created stays the layer's
/// when that entry goes: it is still on disk, and overlay's merged view
/// keeps it under an opaque marker (the layer's own copy of it, holding
/// only the whiteout), so applying the layer keeps it too, empty.
#[test]
fn an_opaque_whiteout_keeps_a_directory_whose_created_entry_went() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("d/x")).unwrap();
    fs::write(f.host("d/x/lower"), b"").unwrap();
    fs::write(f.host("d/y"), b"").unwrap();
    let mut t = Tar::new(&f);
    t.file("d/x/f", 0o644, b"")
        .file("d/x/.wh.f", 0o644, b"")
        .file("d/.wh..wh..opq", 0o644, b"");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    let left = crate::test_util::tree(&f.host);
    let expect: std::collections::BTreeSet<PathBuf> =
        ["d", "d/x"].into_iter().map(PathBuf::from).collect();
    assert_eq!(left, expect);
}

#[test]
fn hardlink_targets_must_exist_in_the_root() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("h", "/etc/passwd");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Overlay).unwrap_err();
    assert!(
        matches!(e, Error::Entry { ref reason, .. } if reason.contains("neither")),
        "{e}"
    );
    let mut t = Tar::new(&f);
    t.hardlink("self", "self");
    assert!(extract(&f.root, t.bytes().as_slice(), Whiteouts::Overlay).is_err());
}

#[test]
fn long_names_and_pax_times() {
    let f = Fixture::new();
    let long = format!("{}/{}", "d".repeat(150), "f".repeat(120));
    let mut t = Tar::new(&f);
    t.pax(&[
        ("path", long.as_bytes()),
        ("mtime", b"-1.5"),
        ("atime", b"12.000000001"),
    ])
    .file("short", 0o644, b"long");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    let st = f.root.statx(&long).unwrap();
    assert_eq!(
        st.mtime,
        FileTime {
            secs: -2,
            nanos: 500_000_000
        }
    );
    assert_eq!(st.atime, FileTime { secs: 12, nanos: 1 });
    // Read after the statx: reading moves an old atime (relatime).
    assert_eq!(fs::read(f.host(&long)).unwrap(), b"long");
}

#[test]
fn malformed_streams_and_entries_fail() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.file("a", 0o644, b"hello");
    let mut bytes = t.bytes();
    bytes.truncate(600);
    let e = extract(&f.root, bytes.as_slice(), Whiteouts::Apply).unwrap_err();
    assert!(matches!(e, Error::Tar(_) | Error::Io { .. }), "{e}");

    let mut t = Tar::new(&f);
    let h = t.header(b"odd", EntryType::new(b'Q'), 0o644, 0);
    t.push(h, b"");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
    assert!(matches!(e, Error::Entry { .. }), "{e}");

    // A GNU sparse entry could claim any size and have its holes written as
    // zeros: refused.
    let mut t = Tar::new(&f);
    // One data byte at offset 2^62 - 1: everything before it is a hole.
    let mut h = t.header(b"sparse", EntryType::GNUSparse, 0o644, 1);
    let gnu = h.as_gnu_mut().unwrap();
    gnu.set_real_size(1 << 62);
    gnu.sparse[0].set_offset((1 << 62) - 1);
    gnu.sparse[0].set_length(1);
    t.push(h, b"x");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
    assert!(
        matches!(e, Error::Entry { ref reason, .. } if reason.contains("sparse")),
        "{e}"
    );
    assert!(!f.host("sparse").exists());

    let mut t = Tar::new(&f);
    t.file("./", 0o644, b"");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
    assert!(matches!(e, Error::Entry { .. }), "{e}");

    let mut t = Tar::new(&f);
    t.file(".wh..", 0o644, b"");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Overlay).unwrap_err();
    assert!(matches!(e, Error::Entry { .. }), "{e}");

    let mut t = Tar::new(&f);
    t.pax(&[("mtime", b"12x")]).file("m", 0o644, b"");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
    assert!(matches!(e, Error::Entry { .. }), "{e}");
}

#[test]
fn pax_time_parsing() {
    let t = |s: &str| pax_time(s.as_bytes());
    assert_eq!(t("1"), Some(FileTime::from_secs(1)));
    assert_eq!(
        t("1.5"),
        Some(FileTime {
            secs: 1,
            nanos: 500_000_000
        })
    );
    assert_eq!(
        t("-1.5"),
        Some(FileTime {
            secs: -2,
            nanos: 500_000_000
        })
    );
    assert_eq!(
        t("-0.25"),
        Some(FileTime {
            secs: -1,
            nanos: 750_000_000
        })
    );
    assert_eq!(t("-3"), Some(FileTime::from_secs(-3)));
    assert_eq!(
        t("1.1234567899"),
        Some(FileTime {
            secs: 1,
            nanos: 123_456_789
        })
    );
    assert_eq!(t("1."), Some(FileTime::from_secs(1)));
    for bad in [
        "",
        "-",
        ".5",
        "1.x",
        "a",
        "1 ",
        "--1",
        "99999999999999999999",
    ] {
        assert_eq!(t(bad), None, "{bad:?}");
    }
}

/// A thread keeps exchanging the directory the layer writes into with a
/// symlink to a directory outside: nothing outside may change.
#[test]
fn concurrent_swap_during_extraction_stays_inside() {
    for _ in 0..10 {
        let f = Fixture::new();
        let (out, before) = f.outside();
        fs::create_dir(f.host("d")).unwrap();
        symlink(&out, f.host("swap")).unwrap();
        let mut t = Tar::new(&f);
        for i in 0..200 {
            t.file(&format!("d/f{i}"), 0o644, b"x")
                .dir(&format!("d/x{i}/"), 0o755)
                .file(&format!("d/.wh.sub{i}"), 0o644, b"")
                .file("d/.wh..wh..opq", 0o644, b"");
        }
        let bytes = t.bytes();
        let host_dir = fs::File::open(&f.host).unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let r = rustix::fs::renameat_with(
                        &host_dir,
                        "d",
                        &host_dir,
                        "swap",
                        rustix::fs::RenameFlags::EXCHANGE,
                    );
                    if r.is_err() {
                        break;
                    }
                }
            });
            // Failing is fine; writing outside is not.
            let _ = extract(&f.root, bytes.as_slice(), Whiteouts::Apply);
            stop.store(true, Ordering::Relaxed);
        });
        assert_eq!(snapshot(&out), before);
    }
}

/// Existing entries that look like another mount (the root claims another
/// mount id) are never replaced, merged into or whited out.
#[test]
fn entries_on_a_mount_point_are_refused() {
    let f = Fixture::new();
    fs::write(f.host("file"), b"lower").unwrap();
    fs::create_dir(f.host("dir")).unwrap();
    fs::write(f.host("dir/keep"), b"").unwrap();
    let dir_mode = mode_of(&f.host("dir"));
    let root = RootPath::open(&f.host)
        .unwrap()
        .with_mnt_id(f.root.mnt_id() + 1);
    // (entry name, directory entry?, whiteout form)
    let cases = [
        ("file", false, Whiteouts::Apply),
        ("dir/", true, Whiteouts::Apply),
        (".wh.dir", false, Whiteouts::Apply),
        (".wh.file", false, Whiteouts::Overlay),
    ];
    for (name, dir, wh) in cases {
        let mut t = Tar::new(&f);
        if dir {
            t.dir(name, 0o700);
        } else {
            t.file(name, 0o644, b"new");
        }
        let e = extract(&root, t.bytes().as_slice(), wh).unwrap_err();
        assert!(
            matches!(e, Error::Entry { ref reason, .. } if reason.contains("mount")),
            "{name}: {e}"
        );
    }
    assert_eq!(fs::read(f.host("file")).unwrap(), b"lower");
    assert!(f.host("dir/keep").exists());
    assert_eq!(mode_of(&f.host("dir")), dir_mode);
}

/// Run by the T1 harness in a mount namespace, with `$RNK_T1_ROOT` prepared
/// as for the deletion test (`bind/`, `tmpfs/`, the `.rnk-t1-root` marker):
/// extraction never writes below a mount inside the root.
#[test]
#[ignore = "T1: privileged: needs a bind mount and a tmpfs inside the root (mount namespace)"]
fn extraction_never_writes_below_a_mount() {
    let dir = std::env::var_os("RNK_T1_ROOT").expect("RNK_T1_ROOT is set by the T1 harness");
    let marker = fs::symlink_metadata(Path::new(&dir).join(".rnk-t1-root"));
    assert!(marker.is_ok_and(|m| m.is_file()), "no .rnk-t1-root marker");
    let root = RootPath::open(&dir).unwrap();
    let (uid, gid) = {
        let md = fs::metadata(&dir).unwrap();
        (md.uid(), md.gid())
    };
    for name in ["bind/x", "tmpfs/x", "tmpfs/", ".wh.bind"] {
        let mut t = Tar {
            out: Vec::new(),
            uid: u64::from(uid),
            gid: u64::from(gid),
        };
        t.file(name, 0o644, b"");
        let e = extract(&root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
        assert!(matches!(e, Error::Entry { .. }), "{name}: {e}");
    }
    assert!(!Path::new(&dir).join("bind/x").exists());
    assert!(!Path::new(&dir).join("tmpfs/x").exists());
}

/// Run by the T1 harness inside a user namespace mapping a range of ids:
/// foreign owners, and setuid surviving the chown.
#[test]
#[ignore = "T1: default: chown to foreign ids needs a user namespace with an id map"]
fn foreign_owners_are_kept_raw() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.uid = 0;
    t.gid = 0;
    t.file("root-owned", 0o4755, b"");
    t.uid = 1000;
    t.gid = 1000;
    t.dir("home/", 0o700);
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Overlay).unwrap();
    let st = f.root.statx("root-owned").unwrap();
    assert_eq!((st.uid, st.gid, st.permissions()), (0, 0, 0o4755));
    let st = f.root.statx("home").unwrap();
    assert_eq!((st.uid, st.gid, st.permissions()), (1000, 1000, 0o700));
}

/// T2c (the VM or a default rootful Docker run): device nodes need `CAP_MKNOD` in the
/// initial user namespace.
#[test]
#[ignore = "T2c: default: mknod of real devices needs CAP_MKNOD"]
fn device_nodes_are_created() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.node("null", EntryType::Char, 1, 3)
        .node("loop0", EntryType::Block, 7, 0);
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    let null = fs::symlink_metadata(f.host("null")).unwrap();
    assert!(null.file_type().is_char_device());
    assert_eq!(null.rdev(), rustix::fs::makedev(1, 3));
    assert!(
        fs::symlink_metadata(f.host("loop0"))
            .unwrap()
            .file_type()
            .is_block_device()
    );
}

/// Without privileges, the same entries fail cleanly instead.
#[test]
fn device_nodes_and_foreign_owners_fail_cleanly_unprivileged() {
    let f = Fixture::new();
    if f.ids().0 == 0 {
        return; // Running as root: the T1/T2 tests cover this.
    }
    let mut t = Tar::new(&f);
    t.node("null", EntryType::Char, 1, 3);
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::PERM), "{e}");
    let mut t = Tar::new(&f);
    t.uid = 0;
    t.file("r", 0o644, b"");
    let e = extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::PERM), "{e}");
}

/// A whiteout (char 0/0) at `p`, as an extracted lower layer holds it.
fn whiteout_at(p: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        p,
        RawFileType::CharacterDevice,
        Mode::empty(),
        rustix::fs::makedev(0, 0),
    )
    .unwrap();
}

#[test]
fn a_hardlink_into_a_lower_layer_is_copied_from_it() {
    let lower = Fixture::new();
    fs::create_dir_all(lower.host("usr/bin")).unwrap();
    fs::write(lower.host("usr/bin/dash"), b"#!dash").unwrap();
    rustix::fs::setxattr(
        lower.host("usr/bin/dash"),
        "user.overlay.overlay.origin",
        b"o",
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
    let m = Meta {
        mode: Some(0o4755),
        mtime: Some(FileTime::from_secs(1_234)),
        atime: Some(FileTime::from_secs(1_234)),
        ..Meta::default()
    };
    lower.root.set_meta("usr/bin/dash", &m).unwrap();
    let before = snapshot(&lower.host);

    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("usr/bin/sh", "usr/bin/dash")
        .hardlink("bin/sh2", "/usr/bin/dash")
        .file("usr/bin/own", 0o644, b"own")
        .hardlink("usr/bin/own2", "usr/bin/own");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Create,
    )
    .unwrap();

    // A regular file with the lower inode's content and metadata.
    let sh = fs::symlink_metadata(f.host("usr/bin/sh")).unwrap();
    assert!(sh.is_file());
    assert_eq!(fs::read(f.host("usr/bin/sh")).unwrap(), b"#!dash");
    assert_eq!(mode_of(&f.host("usr/bin/sh")), 0o4755);
    assert_eq!(f.root.statx("usr/bin/sh").unwrap().mtime.secs, 1_234);
    // Stored xattrs are copied as stored: still escaped.
    assert_eq!(
        xattr(&f.host("usr/bin/sh"), "user.overlay.overlay.origin").unwrap(),
        b"o"
    );
    // The second link to the same target links the copy.
    assert_eq!(sh.nlink(), 2);
    assert_eq!(fs::metadata(f.host("bin/sh2")).unwrap().ino(), sh.ino());
    // Links within the layer stay links.
    assert_eq!(fs::metadata(f.host("usr/bin/own2")).unwrap().nlink(), 2);
    assert!(!f.host("usr/bin/dash").exists());
    // The lower layer is only read.
    assert_eq!(snapshot(&lower.host), before);
}

/// A copy-up from a lower layer gets none of the lower inode's `trusted.*`
/// (a layer extracted before they were left out, or a filesystem that
/// lists its own to root), as the layer's own entries get none
/// (`ADR-0075`).
#[test]
#[ignore = "T2c: sysadmin: setting trusted.* needs CAP_SYS_ADMIN"]
fn a_hardlink_copied_from_a_lower_layer_gets_no_trusted_xattrs() {
    let lower = Fixture::new();
    fs::write(lower.host("dash"), b"#!dash").unwrap();
    for (n, v) in [("trusted.x", &b"t"[..]), ("user.app", b"u")] {
        rustix::fs::setxattr(lower.host("dash"), n, v, rustix::fs::XattrFlags::empty()).unwrap();
    }
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("sh", "dash");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Create,
    )
    .unwrap();
    assert_eq!(xattr(&f.host("sh"), "trusted.x"), None);
    assert_eq!(xattr(&f.host("sh"), "user.app").unwrap(), b"u");
}

#[test]
fn every_name_of_a_lower_inode_shares_one_copy() {
    // A lower file with many names, and a layer linking each name: one
    // copy, not one per name.
    let lower = Fixture::new();
    fs::write(lower.host("big"), b"content").unwrap();
    for n in ["n1", "n2", "n3"] {
        fs::hard_link(lower.host("big"), lower.host(n)).unwrap();
    }
    rustix::fs::setxattr(
        lower.host("big"),
        "user.overlay.origin",
        b"overlay's own",
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("a", "big")
        .hardlink("b", "n1")
        .hardlink("c", "n2")
        .hardlink("d", "n3");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Create,
    )
    .unwrap();
    let a = fs::metadata(f.host("a")).unwrap();
    assert_eq!(a.nlink(), 4);
    for n in ["b", "c", "d"] {
        assert_eq!(fs::metadata(f.host(n)).unwrap().ino(), a.ino(), "{n}");
    }
    // Overlay's own xattrs stay in the lower layer.
    assert_eq!(xattr(&f.host("a"), "user.overlay.origin"), None);
}

/// A copy the layer then whited out or replaced is not the copy any more:
/// a later link to another name of the same lower inode copies it again
/// instead of linking whatever is at the old copy's name.
#[test]
fn a_copy_the_layer_removed_is_not_linked_again() {
    let lower = Fixture::new();
    fs::write(lower.host("big"), b"content").unwrap();
    for n in ["n1", "n2"] {
        fs::hard_link(lower.host("big"), lower.host(n)).unwrap();
    }
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("a", "big")
        .file(".wh.a", 0o644, b"")
        .hardlink("b", "n1")
        .file("b", 0o644, b"replaced")
        .hardlink("c", "n2");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Create,
    )
    .unwrap();
    // `a` is overlay's whiteout now, `b` the layer's own file.
    assert!(
        fs::symlink_metadata(f.host("a"))
            .unwrap()
            .file_type()
            .is_char_device()
    );
    assert_eq!(fs::read(f.host("b")).unwrap(), b"replaced");
    let c = fs::symlink_metadata(f.host("c")).unwrap();
    assert!(c.is_file());
    assert_eq!(fs::read(f.host("c")).unwrap(), b"content");
    assert_eq!(c.nlink(), 1);
}

/// A copy made through a symlink, whose directory the layer then whited out
/// by its other name: the copy's directory is gone (a whiteout device
/// stands there), so a later link copies the lower file again instead of
/// failing the extraction.
#[test]
fn a_copy_whose_directory_went_by_another_name_is_copied_again() {
    let lower = Fixture::new();
    fs::write(lower.host("big"), b"content").unwrap();
    fs::hard_link(lower.host("big"), lower.host("n1")).unwrap();
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.dir("d/", 0o755)
        .symlink("s", "d")
        .hardlink("s/a", "big")
        .file(".wh.d", 0o644, b"")
        .hardlink("c", "n1");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Create,
    )
    .unwrap();
    assert_eq!(fs::read(f.host("c")).unwrap(), b"content");
    assert_eq!(fs::metadata(f.host("c")).unwrap().nlink(), 1);
}

#[test]
fn the_nearest_lower_layer_shows_the_target() {
    let top = Fixture::new();
    let bottom = Fixture::new();
    fs::write(top.host("t"), b"top").unwrap();
    fs::write(bottom.host("t"), b"bottom").unwrap();
    fs::write(bottom.host("only_bottom"), b"bottom").unwrap();
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("a", "t").hardlink("b", "only_bottom");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&top.root, &bottom.root],
        DeviceNodes::Create,
    )
    .unwrap();
    assert_eq!(fs::read(f.host("a")).unwrap(), b"top");
    assert_eq!(fs::read(f.host("b")).unwrap(), b"bottom");
}

/// Checks that `target` is hidden from a hardlink in a layer built by
/// `layer`, over a top lower layer prepared by `setup` and a bottom one
/// that has `t` and `d/e/t`.
fn assert_hidden(setup: &dyn Fn(&Fixture), target: &str, layer: &dyn Fn(&mut Tar)) {
    let top = Fixture::new();
    let bottom = Fixture::new();
    fs::create_dir_all(bottom.host("d/e")).unwrap();
    fs::write(bottom.host("d/e/t"), b"bottom").unwrap();
    fs::write(bottom.host("t"), b"bottom").unwrap();
    setup(&top);
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    layer(&mut t);
    t.hardlink("h", target);
    let e = extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&top.root, &bottom.root],
        DeviceNodes::Create,
    )
    .unwrap_err();
    assert!(
        matches!(e, Error::Entry { ref reason, .. }
            if reason.contains("hidden") || reason.contains("whited out")),
        "{target}: {e}"
    );
    assert!(!f.host("h").exists(), "{target}");
}

fn make_opaque(top: &Fixture) {
    fs::create_dir(top.host("d")).unwrap();
    rustix::fs::setxattr(
        top.host("d"),
        "user.overlay.opaque",
        b"y",
        rustix::fs::XattrFlags::empty(),
    )
    .unwrap();
}

#[test]
fn whiteouts_opaque_directories_and_files_in_the_way_hide_lower_targets() {
    let none = |_: &mut Tar| {};
    // A whiteout in the higher lower layer.
    assert_hidden(&|top| whiteout_at(&top.host("t")), "t", &none);
    // An opaque directory above it, with or without the deeper directory.
    assert_hidden(&make_opaque, "d/e/t", &none);
    assert_hidden(
        &|top| {
            make_opaque(top);
            fs::create_dir(top.host("d/e")).unwrap();
        },
        "d/e/t",
        &none,
    );
    // A file, or a symlink, where a lower layer has a directory.
    assert_hidden(
        &|top| fs::write(top.host("d"), b"").unwrap(),
        "d/e/t",
        &none,
    );
    assert_hidden(
        &|top| symlink("/elsewhere", top.host("d")).unwrap(),
        "d/e/t",
        &none,
    );
    // This layer's own whiteout and opaque marker.
    assert_hidden(&|_| {}, "t", &|t: &mut Tar| {
        t.file(".wh.t", 0o644, b"");
    });
    assert_hidden(&|_| {}, "d/e/t", &|t: &mut Tar| {
        t.dir("d/", 0o755).file("d/.wh..wh..opq", 0o644, b"");
    });
}

#[test]
fn lower_targets_never_leave_the_lower_roots() {
    let lower = Fixture::new();
    let (out, before) = lower.outside();
    // Symlinks in the lower layer toward the host's outside directory.
    symlink(&out, lower.host("dir")).unwrap();
    symlink(out.join("sentinel"), lower.host("file")).unwrap();
    fs::create_dir(lower.host("adir")).unwrap();
    let f = Fixture::new();
    for target in ["dir/sentinel", "missing", "adir"] {
        let mut t = Tar::new(&f);
        t.hardlink("h", target);
        let e = extract_overlay(
            &f.root,
            t.bytes().as_slice(),
            &[&lower.root],
            DeviceNodes::Create,
        )
        .unwrap_err();
        assert!(matches!(e, Error::Entry { .. }), "{target}: {e}");
        assert!(!f.host("h").exists(), "{target}");
    }
    // A symlink target is copied as a symlink, never followed.
    let mut t = Tar::new(&f);
    t.hardlink("h", "file");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Create,
    )
    .unwrap();
    assert_eq!(fs::read_link(f.host("h")).unwrap(), out.join("sentinel"));
    assert_eq!(snapshot(&out), before);
}

#[test]
fn extract_overlay_reports_whether_the_layer_set_the_root() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.file("f", 0o644, b"x");
    let r = extract_overlay(&f.root, t.bytes().as_slice(), &[], DeviceNodes::Create).unwrap();
    assert!(!r.root_set);

    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.dir("./", 0o555).file("f", 0o644, b"x");
    let r = extract_overlay(&f.root, t.bytes().as_slice(), &[], DeviceNodes::Create).unwrap();
    assert!(r.root_set);
    assert_eq!(f.root.statx("").unwrap().permissions(), 0o555);
}

/// ADR-0106: in a user namespace, block and character devices are left out;
/// FIFOs, whiteouts and opaque markers are still made, unprivileged as
/// there. `tests/spans.rs` checks the count; the budget for the names
/// remembered is `past_the_names_budget_a_link_to_a_device_is_linked_copied_or_refused`.
#[test]
fn skipped_device_nodes_leave_fifos_and_whiteouts() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.dir("d/", 0o755)
        .file("d/.wh..wh..opq", 0o644, b"")
        .node("fifo", EntryType::Fifo, 0, 0)
        .node("null", EntryType::Char, 1, 3)
        .node("zero", EntryType::Char, 0, 0)
        .node("loop0", EntryType::Block, 7, 0)
        .file(".wh.gone", 0o644, b"")
        .file("kept", 0o644, b"ok");
    extract_overlay(&f.root, t.bytes().as_slice(), &[], DeviceNodes::Skip).unwrap();
    for name in ["null", "zero", "loop0"] {
        assert!(fs::symlink_metadata(f.host(name)).is_err(), "{name}");
    }
    assert!(
        fs::symlink_metadata(f.host("fifo"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    let gone = fs::symlink_metadata(f.host("gone")).unwrap();
    assert!(gone.file_type().is_char_device());
    assert_eq!(gone.rdev(), 0);
    assert_eq!(
        xattr(&f.host("d"), "user.overlay.opaque").as_deref(),
        Some(&b"y"[..])
    );
    assert_eq!(fs::read(f.host("kept")).unwrap(), b"ok");

    // A hardlink to a device left out, or to such a link, is left out too:
    // never copied from a lower layer's file at the device's name. Once the
    // layer puts something else at that name, a link links it.
    let lower = Fixture::new();
    fs::write(lower.host("sda"), b"lower").unwrap();
    fs::write(lower.host("tty"), b"lower").unwrap();
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.node("sda", EntryType::Block, 8, 0)
        .hardlink("sda-link", "sda")
        .hardlink("sda-link2", "sda-link")
        .node("tty", EntryType::Char, 5, 0)
        .file("tty", 0o644, b"file")
        .hardlink("tty-link", "tty");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Skip,
    )
    .unwrap();
    for name in ["sda", "sda-link", "sda-link2"] {
        assert!(fs::symlink_metadata(f.host(name)).is_err(), "{name}");
    }
    assert_eq!(fs::read(f.host("tty-link")).unwrap(), b"file");
    assert_eq!(
        fs::metadata(f.host("tty")).unwrap().ino(),
        fs::metadata(f.host("tty-link")).unwrap().ino()
    );
    // A device after an entry of this layer at its name leaves that entry
    // there; a link to the name is left out with the device.
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.file("x", 0o644, b"first")
        .node("x", EntryType::Char, 1, 3)
        .hardlink("x-link", "x");
    extract_overlay(&f.root, t.bytes().as_slice(), &[], DeviceNodes::Skip).unwrap();
    assert_eq!(fs::read(f.host("x")).unwrap(), b"first");
    assert!(fs::symlink_metadata(f.host("x-link")).is_err());

    // A device the layer whites out is gone, as when they are made.
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.node("sda", EntryType::Block, 8, 0)
        .file(".wh.sda", 0o644, b"")
        .hardlink("sda-link", "sda");
    let e = extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Skip,
    )
    .unwrap_err();
    assert!(e.to_string().contains("whited out"), "{e}");

    // A device as the layer's root is refused, as when they are made.
    for devices in [DeviceNodes::Skip, DeviceNodes::Create] {
        let f = Fixture::new();
        let mut t = Tar::new(&f);
        t.node("./", EntryType::Char, 1, 3);
        let e = extract_overlay(&f.root, t.bytes().as_slice(), &[], devices).unwrap_err();
        assert!(e.to_string().contains(ROOT_NOT_DIR), "{devices:?}: {e}");
    }

    // Control: asked to create them, an unprivileged process cannot.
    if f.ids().0 != 0 {
        let f = Fixture::new();
        let mut t = Tar::new(&f);
        t.node("null", EntryType::Char, 1, 3);
        let e =
            extract_overlay(&f.root, t.bytes().as_slice(), &[], DeviceNodes::Create).unwrap_err();
        assert_eq!(errno(&e), Some(Errno::PERM), "{e}");
    }
}

/// A layer remembers only the devices it leaves out itself. A link to one
/// a lower layer left out finds nothing there: it is refused, or copies
/// what a deeper layer has at that name (ADR-0131).
#[test]
fn a_link_to_a_device_a_lower_layer_left_out_is_refused_or_copies_a_deeper_layer() {
    let deep = Fixture::new();
    fs::write(deep.host("tty"), b"deep").unwrap();
    let lower = Fixture::new();
    let mut t = Tar::new(&lower);
    t.node("sda", EntryType::Block, 8, 0)
        .node("tty", EntryType::Char, 5, 0);
    extract_overlay(
        &lower.root,
        t.bytes().as_slice(),
        &[&deep.root],
        DeviceNodes::Skip,
    )
    .unwrap();
    let lowers = [&lower.root, &deep.root];

    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("sda-link", "sda");
    let e = extract_overlay(&f.root, t.bytes().as_slice(), &lowers, DeviceNodes::Skip).unwrap_err();
    assert!(e.to_string().contains("in no layer below"), "{e}");

    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("tty-link", "tty");
    extract_overlay(&f.root, t.bytes().as_slice(), &lowers, DeviceNodes::Skip).unwrap();
    assert_eq!(fs::read(f.host("tty-link")).unwrap(), b"deep");
}

/// Counts the warnings logged while it is the default.
#[derive(Default)]
struct Warnings(AtomicUsize);

impl tracing::Subscriber for Warnings {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        *m.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// ADR-0128: within the budget, a link to a device left out is left out;
/// past it, the layer remembers no further name, a short one after the
/// first that does not fit included, and warns once: a link to a device it
/// does not remember is linked to what the layer has at that name, else
/// copied from a lower layer that has it, else refused, never left out.
/// The only test here that reaches the warning, so no other can cache it
/// as unwanted.
#[test]
fn past_the_names_budget_a_link_to_a_device_is_linked_copied_or_refused() {
    let warnings = tracing::Dispatch::new(Warnings::default());
    // Long names fill the budget in a few hundred entries.
    let long = format!("{}/", "n".repeat(250)).repeat(16);
    let name = |i: usize| format!("{long}{i:03}");
    let cost = name(0).len() + SKIPPED_NAME_COST;
    let fit = SKIPPED_NAMES_MAX / cost;
    // What the names that fit leave would hold "s" or "y": only a budget
    // closed at the first name past it leaves them unremembered.
    assert!(SKIPPED_NAMES_MAX - fit * cost >= "s".len() + SKIPPED_NAME_COST);

    let f = Fixture::new();
    let (uid, gid) = f.ids();
    let header = |t| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(t);
        h.set_mode(0o644);
        h.set_uid(uid.into());
        h.set_gid(gid.into());
        h.set_mtime(0);
        h.set_size(0);
        h
    };
    let mut b = tar::Builder::new(Vec::new());
    let mut file = header(EntryType::Regular);
    file.set_size(1);
    b.append_data(&mut file, "y", &b"y"[..]).unwrap();
    let mut device = |path: &str| {
        let mut h = header(EntryType::Char);
        h.set_device_major(1).unwrap();
        h.set_device_minor(3).unwrap();
        b.append_data(&mut h, path, &b""[..]).unwrap();
    };
    // Two names past the budget, for one warning.
    for i in 0..fit + 2 {
        device(&name(i));
    }
    device("s");
    device("y");
    b.append_link(&mut header(EntryType::Link), "first", name(0))
        .unwrap();
    b.append_link(&mut header(EntryType::Link), "to-y", "y")
        .unwrap();
    b.append_link(&mut header(EntryType::Link), "to-s", "s")
        .unwrap();
    let tar = b.into_inner().unwrap();

    tracing::dispatcher::with_default(&warnings, || {
        let e = extract_overlay(&f.root, tar.as_slice(), &[], DeviceNodes::Skip).unwrap_err();
        assert!(e.to_string().contains("target s is neither"), "{e}");

        let lower = Fixture::new();
        fs::write(lower.host("s"), b"lower").unwrap();
        // The layer's own "y" comes before a lower layer's.
        fs::write(lower.host("y"), b"lower").unwrap();
        let f = Fixture::new();
        extract_overlay(&f.root, tar.as_slice(), &[&lower.root], DeviceNodes::Skip).unwrap();
        assert!(fs::symlink_metadata(f.host("first")).is_err());
        let ino = |p: &str| fs::symlink_metadata(f.host(p)).unwrap().ino();
        assert_eq!(ino("to-y"), ino("y"));
        assert_eq!(fs::read(f.host("y")).unwrap(), b"y");
        assert_eq!(fs::read(f.host("to-s")).unwrap(), b"lower");
    });
    let count = warnings
        .downcast_ref::<Warnings>()
        .unwrap()
        .0
        .load(Ordering::Relaxed);
    assert_eq!(count, 2, "one warning a layer");
}

fn live_keep() -> IgnoreList {
    let mut keep = IgnoreList::new();
    keep.insert("/ranikust").unwrap();
    keep.insert("/proc").unwrap();
    keep.insert("/etc/mtab").unwrap();
    keep.insert("/var/run").unwrap();
    keep.insert_below("/tmp/apt-key-gpghome").unwrap();
    keep
}

#[test]
fn live_extraction_never_writes_ignored_paths() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.dir("ranikust/", 0o755)
        .file("ranikust/state/blob", 0o644, b"evil")
        .file("proc/1/environ", 0o644, b"evil")
        .file("etc/mtab", 0o644, b"evil")
        .file("etc/hostname", 0o644, b"ok")
        .dir("tmp/apt-key-gpghome/", 0o700)
        .file("tmp/apt-key-gpghome/pubring", 0o600, b"evil")
        .file("var/run/pid", 0o644, b"evil")
        .hardlink("etc/linked", "etc/mtab");
    extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap();
    for gone in [
        "ranikust",
        "proc",
        "etc/mtab",
        "tmp/apt-key-gpghome/pubring",
        "var/run",
        "etc/linked",
    ] {
        assert!(fs::symlink_metadata(f.host(gone)).is_err(), "{gone}");
    }
    assert_eq!(fs::read(f.host("etc/hostname")).unwrap(), b"ok");
    // `PrefixMatchOnly`: the directory itself is written, not its contents.
    assert!(f.host("tmp/apt-key-gpghome").is_dir());
}

#[test]
fn live_extraction_follows_symlinks_to_the_physical_path() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("ranikust/state")).unwrap();
    symlink("/ranikust", f.host("old")).unwrap();
    let mut t = Tar::new(&f);
    t.file("old/state/evil", 0o644, b"x")
        .file("old/new/evil", 0o644, b"x")
        .symlink("lnk", "ranikust/state")
        .file("lnk/evil", 0o644, b"x")
        .dir("d/", 0o755)
        .file("d/ok", 0o644, b"ok")
        .file(".wh.old", 0o644, b"")
        .symlink("d2", "d")
        .file("d2/via", 0o644, b"ok");
    extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap();
    let left: Vec<_> = fs::read_dir(f.host("ranikust/state")).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    assert!(!f.host("ranikust/new").exists());
    assert_eq!(fs::read(f.host("d/ok")).unwrap(), b"ok");
    assert_eq!(fs::read(f.host("d/via")).unwrap(), b"ok");
    // The whiteout removed the symlink `old`, not what it points to.
    assert!(fs::symlink_metadata(f.host("old")).is_err());
    assert!(f.host("ranikust/state").is_dir());
}

#[test]
fn live_whiteouts_leave_ignored_paths_alone() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("var/run")).unwrap();
    fs::write(f.host("var/run/sock"), b"").unwrap();
    fs::write(f.host("var/log"), b"").unwrap();
    fs::create_dir_all(f.host("etc")).unwrap();
    fs::write(f.host("etc/mtab"), b"m").unwrap();
    fs::write(f.host("etc/old"), b"").unwrap();
    fs::create_dir_all(f.host("ranikust")).unwrap();
    fs::write(f.host("gone"), b"").unwrap();
    let mut t = Tar::new(&f);
    t.file(".wh.var", 0o644, b"")
        .file(".wh.ranikust", 0o644, b"")
        .file(".wh.gone", 0o644, b"")
        .file("etc/.wh..wh..opq", 0o644, b"")
        .file("etc/new", 0o644, b"n");
    extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap();
    // kaniko skips a whiteout of a directory holding an ignored path.
    assert!(f.host("var/run/sock").exists());
    assert!(f.host("var/log").exists());
    assert!(f.host("ranikust").is_dir());
    assert!(!f.host("gone").exists());
    // The opaque directory keeps the ignored file and what the layer added.
    assert_eq!(fs::read(f.host("etc/mtab")).unwrap(), b"m");
    assert!(!f.host("etc/old").exists());
    assert!(f.host("etc/new").exists());
}

/// A directory the layer listed, then deleted with the directory around it
/// when a later entry replaced that one, gets no metadata at the end: only
/// what the deletion kept is still there.
#[test]
fn a_replaced_directory_holding_an_ignored_path_leaves_no_pending_metadata() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("var/run")).unwrap();
    fs::write(f.host("var/run/sock"), b"").unwrap();
    let mut t = Tar::new(&f);
    t.dir("var/x/", 0o755)
        .dir("var/x/y/", 0o755)
        .file("var", 0o644, b"");
    extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap();
    assert!(f.host("var/run/sock").exists());
    assert!(!f.host("var/x").exists());
}

/// The same with an ignore-list entry below the deleted directory that is
/// not on disk: the list says something may be kept below it, the walk
/// removed it all the same.
#[test]
fn a_replaced_directory_whose_ignored_path_is_absent_leaves_no_pending_metadata() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("var/run")).unwrap();
    fs::write(f.host("var/run/sock"), b"").unwrap();
    let mut keep = live_keep();
    keep.insert("/var/cache/apt").unwrap();
    let mut t = Tar::new(&f);
    t.dir("var/cache/", 0o755).file("var", 0o644, b"");
    extract_live(&f.root, t.bytes().as_slice(), &keep).unwrap();
    assert!(f.host("var/run/sock").exists());
    assert!(!f.host("var/cache").exists());
}

#[test]
fn live_extraction_keeps_a_directory_holding_an_ignored_path() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("var/run")).unwrap();
    fs::write(f.host("var/run/sock"), b"").unwrap();
    fs::write(f.host("var/other"), b"").unwrap();
    let mut t = Tar::new(&f);
    t.symlink("var", "/elsewhere");
    extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap();
    // As kaniko's FF_KANIKO_PRESERVE_MOUNTED_PATHS: the rest is removed and
    // the entry skipped.
    assert!(f.host("var/run/sock").exists());
    assert!(!f.host("var/other").exists());
    assert!(fs::symlink_metadata(f.host("var")).unwrap().is_dir());
}

/// The same entries as [`entries_on_a_mount_point_are_refused`], skipped
/// instead of refused in a live root.
#[test]
fn live_extraction_skips_entries_on_a_mount_point() {
    let f = Fixture::new();
    fs::write(f.host("file"), b"lower").unwrap();
    fs::create_dir(f.host("dir")).unwrap();
    fs::write(f.host("dir/keep"), b"").unwrap();
    let root = RootPath::open(&f.host)
        .unwrap()
        .with_mnt_id(f.root.mnt_id() + 1);
    let mut t = Tar::new(&f);
    t.file("file", 0o644, b"new")
        .dir("dir/", 0o700)
        .file(".wh.dir", 0o644, b"")
        .file("dir/.wh..wh..opq", 0o644, b"");
    extract_live(&root, t.bytes().as_slice(), &IgnoreList::new()).unwrap();
    assert_eq!(fs::read(f.host("file")).unwrap(), b"lower");
    assert!(f.host("dir/keep").exists());
}

/// Run by the T1 harness as [`extraction_never_writes_below_a_mount`]: a live
/// extraction skips the mounts, whether or not they are on the list.
#[test]
#[ignore = "T1: privileged: needs a bind mount and a tmpfs inside the root (mount namespace)"]
fn live_extraction_skips_real_mounts() {
    let dir = std::env::var_os("RNK_T1_ROOT").expect("RNK_T1_ROOT is set by the T1 harness");
    let marker = fs::symlink_metadata(Path::new(&dir).join(".rnk-t1-root"));
    assert!(marker.is_ok_and(|m| m.is_file()), "no .rnk-t1-root marker");
    let root = RootPath::open(&dir).unwrap();
    let md = fs::metadata(&dir).unwrap();
    let mut t = Tar {
        out: Vec::new(),
        uid: u64::from(md.uid()),
        gid: u64::from(md.gid()),
    };
    t.file("bind/x", 0o644, b"")
        .file("tmpfs/x", 0o644, b"")
        .file(".wh.bind", 0o644, b"")
        .file("tmpfs", 0o644, b"");
    extract_live(&root, t.bytes().as_slice(), &IgnoreList::new()).unwrap();
    assert!(!Path::new(&dir).join("bind/x").exists());
    assert!(!Path::new(&dir).join("tmpfs/x").exists());
    assert!(Path::new(&dir).join("bind").is_dir());
    assert!(Path::new(&dir).join("tmpfs").is_dir());
}

/// A thread swaps the parent directory of every entry with a symlink to the
/// ignored state directory while a live extraction runs: whatever fails,
/// nothing may land in the state directory, nor its directories' metadata
/// change.
#[test]
fn a_racing_swap_never_leads_a_live_extraction_into_an_ignored_path() {
    for _ in 0..20 {
        let f = Fixture::new();
        fs::create_dir_all(f.host("ranikust/state")).unwrap();
        fs::create_dir(f.host("d")).unwrap();
        symlink("/ranikust/state", f.host("swap")).unwrap();
        let mut t = Tar::new(&f);
        t.dir("d/", 0o700);
        for i in 0..200 {
            t.file(&format!("d/f{i}"), 0o644, b"x")
                .dir(&format!("d/s{i}/"), 0o700);
        }
        let bytes = t.bytes();
        let state_mode = mode_of(&f.host("ranikust/state"));
        let host_dir = fs::File::open(&f.host).unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let r = rustix::fs::renameat_with(
                        &host_dir,
                        "d",
                        &host_dir,
                        "swap",
                        rustix::fs::RenameFlags::EXCHANGE,
                    );
                    if r.is_err() {
                        break;
                    }
                }
            });
            // Failing is fine; writing into the state directory is not.
            let _ = extract_live(&f.root, bytes.as_slice(), &live_keep());
            stop.store(true, Ordering::Relaxed);
        });
        let left: Vec<_> = fs::read_dir(f.host("ranikust/state")).unwrap().collect();
        assert!(
            left.is_empty(),
            "{} entries in the state directory",
            left.len()
        );
        assert_eq!(mode_of(&f.host("ranikust/state")), state_mode);
    }
}

/// As kaniko (ADR-0041): no device node or FIFO in a live root, whatever
/// the numbers; the other entries of the layer are written.
#[test]
fn live_extraction_creates_no_device_node_or_fifo() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.node("fifo", EntryType::Fifo, 0, 0)
        .node("null", EntryType::Char, 1, 3)
        .node("wh", EntryType::Char, 0, 0)
        .node("sda", EntryType::Block, 8, 0)
        .file("kept", 0o644, b"ok");
    extract_live(&f.root, t.bytes().as_slice(), &IgnoreList::new()).unwrap();
    for name in ["fifo", "null", "wh", "sda"] {
        assert!(fs::symlink_metadata(f.host(name)).is_err(), "{name}");
    }
    assert_eq!(fs::read(f.host("kept")).unwrap(), b"ok");
}

#[test]
fn a_literal_extraction_keeps_whiteout_names_as_files() {
    let f = Fixture::new();
    fs::write(f.host("x"), b"kept").unwrap();
    fs::create_dir(f.host("d")).unwrap();
    fs::write(f.host("d/y"), b"kept").unwrap();
    let mut t = Tar::new(&f);
    t.file(".wh.x", 0o644, b"a file named like a whiteout")
        .dir("d", 0o755)
        .file("d/.wh..wh..opq", 0o644, b"and an opaque marker");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Literal).unwrap();
    assert_eq!(fs::read(f.host("x")).unwrap(), b"kept");
    assert_eq!(fs::read(f.host("d/y")).unwrap(), b"kept");
    assert_eq!(
        fs::read(f.host(".wh.x")).unwrap(),
        b"a file named like a whiteout"
    );
    assert_eq!(
        fs::read(f.host("d/.wh..wh..opq")).unwrap(),
        b"and an opaque marker"
    );
    // The same tar into a live root.
    let g = Fixture::new();
    fs::write(g.host("x"), b"kept").unwrap();
    extract_live_literal(&g.root, t.bytes().as_slice(), &IgnoreList::new()).unwrap();
    assert_eq!(fs::read(g.host("x")).unwrap(), b"kept");
    assert!(g.host(".wh.x").is_file());
}

/// Red team, ADR-0052 D4 (T1, T4): `/var/run` is no longer ignored, so a
/// layer's `var/run/...` entries are extracted. In a container whose
/// `/var/run -> ../run`, with a secret mounted at `/run/secrets` and the
/// runtime's `/run/.containerenv` (both on the list, as `kaniko::ignore`
/// puts them), entries, hardlinks, whiteouts and an opaque marker aimed at
/// them through `var/run` are skipped at their physical path; links that
/// climb above the root stay in it; a link into `/proc` leads nowhere.
#[test]
fn live_extraction_through_var_run_never_reaches_what_run_keeps() {
    let f = Fixture::new();
    let (outside, before) = f.outside();
    fs::create_dir_all(f.host("run/secrets")).unwrap();
    fs::write(f.host("run/secrets/token"), b"SECRET").unwrap();
    fs::write(f.host("run/.containerenv"), b"engine").unwrap();
    fs::write(f.host("run/old"), b"").unwrap();
    fs::create_dir_all(f.host("var")).unwrap();
    symlink("../run", f.host("var/run")).unwrap();
    fs::create_dir_all(f.host("etc")).unwrap();
    fs::create_dir_all(f.host("proc/self")).unwrap();
    let mut keep = IgnoreList::new();
    keep.insert("/proc").unwrap();
    keep.insert("/run/secrets").unwrap();
    keep.insert("/run/.containerenv").unwrap();
    let mut t = Tar::new(&f);
    t.file("var/run/secrets/token", 0o644, b"EVIL")
        .file("var/run/secrets/new", 0o644, b"EVIL")
        .file("var/run/.containerenv", 0o644, b"EVIL")
        .hardlink("var/run/linked", "var/run/secrets/token")
        .symlink("var/run/secrets2", "secrets")
        .file("var/run/secrets2/via-link", 0o644, b"EVIL")
        .file("var/run/secrets/.wh.token", 0o644, b"")
        .file("var/run/.wh.secrets", 0o644, b"")
        .file("var/run/.wh..containerenv", 0o644, b"")
        .file("var/run/lock", 0o644, b"ok")
        .symlink("var/up", "../../../../../../etc")
        .file("var/up/climbed", 0o644, b"in-root")
        .symlink("var/p", "/proc/self")
        .file("var/p/environ", 0o644, b"EVIL");
    extract_live(&f.root, t.bytes().as_slice(), &keep).unwrap();
    assert_eq!(fs::read(f.host("run/secrets/token")).unwrap(), b"SECRET");
    assert_eq!(
        fs::read_dir(f.host("run/secrets")).unwrap().count(),
        1,
        "nothing added to the mounted secret"
    );
    assert_eq!(fs::read(f.host("run/.containerenv")).unwrap(), b"engine");
    assert!(fs::symlink_metadata(f.host("run/linked")).is_err());
    assert!(f.host("run/old").exists());
    assert_eq!(fs::read(f.host("run/lock")).unwrap(), b"ok");
    assert_eq!(fs::read(f.host("etc/climbed")).unwrap(), b"in-root");
    assert!(!f.host("proc/self/environ").exists());
    assert_eq!(
        snapshot(&outside),
        before,
        "nothing outside the root changed"
    );

    // A link climbing to a name the root lacks (the host's `outside` is a
    // sibling of the root) leads nowhere: the extraction fails, and
    // nothing is written outside.
    let mut t = Tar::new(&f);
    t.symlink("var/up2", "../../../../../../outside")
        .file("var/up2/sentinel", 0o644, b"EVIL");
    assert!(extract_live(&f.root, t.bytes().as_slice(), &keep).is_err());
    assert_eq!(
        snapshot(&outside),
        before,
        "nothing outside the root changed"
    );
}

/// Red team, ADR-0052 D4: an opaque marker below `var/run` while
/// `/var/run -> ../run`. Without the layer's own `var/run/` entry the
/// marker's directory is a symlink, which is never followed: the
/// extraction fails and `/run` is left as it was. With it, the symlink is
/// replaced by the layer's directory and `/run` is not touched either.
#[test]
fn an_opaque_marker_below_a_var_run_link_never_empties_run() {
    for with_dir in [false, true] {
        let f = Fixture::new();
        fs::create_dir_all(f.host("run/secrets")).unwrap();
        fs::write(f.host("run/secrets/token"), b"SECRET").unwrap();
        fs::write(f.host("run/old"), b"o").unwrap();
        fs::create_dir_all(f.host("var")).unwrap();
        symlink("../run", f.host("var/run")).unwrap();
        let mut keep = IgnoreList::new();
        keep.insert("/run/secrets").unwrap();
        let mut t = Tar::new(&f);
        if with_dir {
            t.dir("var/run/", 0o755);
        }
        t.file("var/run/.wh..wh..opq", 0o644, b"")
            .file("var/run/new", 0o644, b"n");
        let r = extract_live(&f.root, t.bytes().as_slice(), &keep);
        assert_eq!(r.is_ok(), with_dir, "{r:?}");
        assert_eq!(fs::read(f.host("run/old")).unwrap(), b"o");
        assert_eq!(fs::read(f.host("run/secrets/token")).unwrap(), b"SECRET");
        assert!(!f.host("run/new").exists());
        let var_run = fs::symlink_metadata(f.host("var/run")).unwrap();
        assert_eq!(var_run.is_dir(), with_dir);
    }
}

/// A hardlink in a live root to a target of the same layer is linked to
/// that target.
#[test]
fn live_extraction_links_a_hardlink_to_a_target_of_the_same_layer() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.dir("d/", 0o755)
        .file("d/f", 0o644, b"x")
        .hardlink("d/l", "d/f");
    extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap();
    let link = fs::metadata(f.host("d/l")).unwrap();
    let target = fs::metadata(f.host("d/f")).unwrap();
    assert_eq!((link.dev(), link.ino()), (target.dev(), target.ino()));
}

/// A hardlink in a live root whose target's directory is missing is
/// reported as missing: a live root has no lower layer to copy it from.
#[test]
fn live_extraction_reports_a_hardlink_whose_target_directory_is_missing() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("l", "nowhere/f");
    let e = extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap_err();
    assert!(
        matches!(&e, Error::Entry { reason, .. }
            if reason.contains("is neither in this layer nor in the root")),
        "{e:?}"
    );
    assert!(fs::symlink_metadata(f.host("l")).is_err());
}

/// Layers each with a header every extraction refuses on its own, after a
/// good first entry.
fn bad_headers(f: &Fixture) -> Vec<(&'static str, Vec<u8>)> {
    let first = || {
        let mut t = Tar::new(f);
        t.file("first", 0o644, b"1");
        t
    };
    let with = |name: &[u8], kind: EntryType, data: &[u8]| {
        let mut t = first();
        let size = u64::try_from(data.len()).unwrap();
        let h = t.header(name, kind, 0o644, size);
        t.push(h, data).file("after", 0o644, b"2").bytes()
    };
    let mut sparse = first();
    let mut h = sparse.header(b"sparse", EntryType::GNUSparse, 0o644, 1);
    let gnu = h.as_gnu_mut().unwrap();
    gnu.set_real_size(1 << 62);
    gnu.sparse[0].set_offset((1 << 62) - 1);
    gnu.sparse[0].set_length(1);
    sparse.push(h, b"x");
    let mut far_uid = first();
    let mut h = far_uid.header(b"far", EntryType::Regular, 0o644, 0);
    h.set_uid(1 << 40);
    far_uid.push(h, b"");
    let mut summed = first().file("summed", 0o644, b"").bytes();
    // A name byte of the second header: its checksum no longer matches.
    summed[1024] ^= 1;
    let mut cut = first().file("cut", 0o644, b"hello").bytes();
    cut.truncate(1024 + 600);
    vec![
        (
            "a name out of the root",
            first().file("../out", 0o644, b"x").bytes(),
        ),
        (
            "a hardlink out of the root",
            first().hardlink("h", "../../x").bytes(),
        ),
        (
            "a hardlink to itself",
            first().hardlink("self", "self").bytes(),
        ),
        (
            "a hardlink whose target is the layer's root",
            first().hardlink("h", "./").bytes(),
        ),
        (
            "a symlink whose header names no target",
            first().symlink("s", "").bytes(),
        ),
        (
            "a file as the layer's root",
            first().file("./", 0o644, b"").bytes(),
        ),
        (
            "a symlink as the layer's root",
            first().symlink("./", "d").bytes(),
        ),
        ("a GNU sparse entry", sparse.bytes()),
        (
            "an unknown entry type",
            with(b"odd", EntryType::new(b'Q'), b""),
        ),
        (
            "a whiteout of `.`",
            first().file("d/.wh..", 0o644, b"").bytes(),
        ),
        (
            "a whiteout naming nothing",
            first().file("d/.wh.", 0o644, b"").bytes(),
        ),
        (
            "a malformed PAX mtime",
            first()
                .pax(&[("mtime", b"12x")])
                .file("m", 0o644, b"")
                .bytes(),
        ),
        (
            "a malformed PAX atime",
            first().pax(&[("atime", b"")]).file("m", 0o644, b"").bytes(),
        ),
        ("a uid out of range", far_uid.bytes()),
        ("a broken checksum", summed),
        ("a stream cut short", cut),
        (
            "a GNU long name over the limit",
            with(b"long", EntryType::GNULongName, &vec![b'n'; (1 << 20) + 1]),
        ),
    ]
}

/// [`check_headers`] refuses, with the very error, every header an
/// extraction that applies whiteouts refuses on its own; that extraction
/// wrote the first entry before it got there.
#[test]
fn check_headers_refuses_what_every_extraction_refuses_by_its_header() {
    let f = Fixture::new();
    for (what, tar) in bad_headers(&f) {
        let checked = check_headers(tar.as_slice()).unwrap_err();
        for wh in [Whiteouts::Apply, Whiteouts::Overlay] {
            let f = Fixture::new();
            let e = extract(&f.root, tar.as_slice(), wh).unwrap_err();
            assert_eq!(e.to_string(), checked.to_string(), "{what}, {wh:?}");
            assert!(f.host("first").exists(), "{what}, {wh:?}");
        }
        let f = Fixture::new();
        let e = extract_live(&f.root, tar.as_slice(), &live_keep()).unwrap_err();
        assert_eq!(e.to_string(), checked.to_string(), "{what}, live");
        assert!(f.host("first").exists(), "{what}, live");
    }
}

/// What only the root can refuse is left to the extraction: a hardlink to
/// a name not in the root, a hardlink or device node as the layer's root
/// (a live root skips the node), and paths the ignore list covers. A good
/// layer passes and is read up to its end-of-archive block, the next one
/// left to the caller.
#[test]
fn check_headers_leaves_what_depends_on_the_root_to_the_extraction() {
    let f = Fixture::new();
    let mut t = Tar::new(&f);
    t.hardlink("h", "missing");
    check_headers(t.bytes().as_slice()).unwrap();
    assert!(extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).is_err());
    let mut node = Tar::new(&f);
    node.node("./", EntryType::Fifo, 0, 0);
    // A live root skips a link to an ignored path.
    let mut link = Tar::new(&f);
    link.hardlink("./", "proc/x");
    for t in [node, link] {
        check_headers(t.bytes().as_slice()).unwrap();
        assert!(extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).is_err());
        extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap();
    }

    let mut t = Tar::new(&f);
    t.dir("d", 0o755)
        .file("d/f", 0o644, b"data")
        .symlink("s", "/etc/passwd")
        .hardlink("l", "d/f")
        .node("c", EntryType::Char, 1, 3)
        .node("p", EntryType::Fifo, 0, 0)
        .file("proc/x", 0o644, b"")
        .file("d/.wh.f", 0o644, b"")
        .file(".wh..wh..opq", 0o644, b"");
    let bytes = t.bytes();
    let mut read = std::io::Cursor::new(&bytes);
    check_headers(&mut read).unwrap();
    assert_eq!(read.position(), u64::try_from(bytes.len() - 512).unwrap());
    extract_live(&f.root, bytes.as_slice(), &live_keep()).unwrap();
}

/// A cleaned directory path of `len` bytes made of names of at most 255
/// bytes, the kernel's `NAME_MAX`.
fn dir_of(len: usize) -> String {
    let mut s = String::new();
    loop {
        let room = len - s.len();
        // One name fills the room, or leaves a slash and a name after it.
        let n = match room {
            0..=255 => room,
            256 => 254,
            _ => 255,
        };
        s.push_str(&"q".repeat(n));
        if s.len() == len {
            return s;
        }
        s.push('/');
    }
}

impl Tar {
    /// A GNU long name (`L`) or long link (`K`) record for the next entry.
    fn gnu_long(&mut self, kind: EntryType, body: &[u8]) -> &mut Self {
        let size = u64::try_from(body.len()).unwrap();
        let h = self.header(b"././@LongLink", kind, 0o644, size);
        self.push(h, body)
    }
}

/// Layers each with, after a good first entry, a value the kernel would
/// refuse (#179).
fn kernel_refused(f: &Fixture) -> Vec<(&'static str, Vec<u8>)> {
    let first = || {
        let mut t = Tar::new(f);
        t.file("first", 0o644, b"1");
        t
    };
    let path = |p: &[u8]| first().pax(&[("path", p)]).file("x", 0o644, b"").bytes();
    let symlink_to = |target: &[u8]| {
        first()
            .pax(&[("linkpath", target)])
            .symlink("s", "x")
            .bytes()
    };
    let hardlink_to = |target: &[u8]| {
        first()
            .pax(&[("linkpath", target)])
            .hardlink("h", "first")
            .bytes()
    };
    let xattr = |name: &str, value: &[u8]| {
        first()
            .pax(&[(format!("SCHILY.xattr.{name}").as_str(), value)])
            .file("x", 0o644, b"")
            .bytes()
    };
    let long = format!("d/{}", "n".repeat(256));
    let deep = format!("{}/f", dir_of(4096));
    vec![
        ("a NUL byte in a PAX path", path(b"d/a\0b")),
        (
            "a NUL byte in a GNU long name",
            first()
                .gnu_long(EntryType::GNULongName, b"d/a\0b\0")
                .file("x", 0o644, b"")
                .bytes(),
        ),
        ("a NUL byte in a whiteout's name", path(b"d/.wh.a\0b")),
        ("a NUL byte in a PAX linkpath", symlink_to(b"t\0u")),
        (
            "a NUL byte in a GNU long link",
            first()
                .gnu_long(EntryType::GNULongLink, b"first\0x\0")
                .hardlink("h", "x")
                .bytes(),
        ),
        ("a name over NAME_MAX", path(long.as_bytes())),
        (
            "a GNU long name over NAME_MAX",
            first()
                .gnu_long(EntryType::GNULongName, long.as_bytes())
                .file("x", 0o644, b"")
                .bytes(),
        ),
        (
            "a whiteout hiding a name over NAME_MAX",
            path(format!("d/.wh.{}", "n".repeat(256)).as_bytes()),
        ),
        (
            "a directory path over PATH_MAX with its NUL",
            path(deep.as_bytes()),
        ),
        (
            "a hardlink target over NAME_MAX",
            hardlink_to(long.as_bytes()),
        ),
        (
            "a hardlink target in a directory over PATH_MAX with its NUL",
            hardlink_to(deep.as_bytes()),
        ),
        (
            "a symlink target over PATH_MAX - 1",
            symlink_to(&[b't'; 4096]),
        ),
        ("an empty PAX linkpath", symlink_to(b"")),
        (
            "an empty GNU long link",
            first()
                .gnu_long(EntryType::GNULongLink, b"\0")
                .symlink("s", "x")
                .bytes(),
        ),
        ("an empty PAX linkpath of a hardlink", hardlink_to(b"")),
        ("an xattr with an empty name", xattr("", b"v")),
        (
            "an xattr name over XATTR_NAME_MAX",
            xattr(&format!("user.{}", "a".repeat(251)), b"v"),
        ),
        ("an xattr name holding a NUL byte", xattr("user.a\0b", b"v")),
        (
            "an xattr value over XATTR_SIZE_MAX",
            xattr("user.big", &vec![b'v'; 65537]),
        ),
        (
            "a user.* xattr on a symlink",
            first()
                .pax(&[("SCHILY.xattr.user.a", b"v")])
                .symlink("s", "first")
                .bytes(),
        ),
        (
            "an xattr with an empty name on a directory",
            first()
                .pax(&[("SCHILY.xattr.", b"v")])
                .dir("d", 0o755)
                .bytes(),
        ),
        (
            "a file capability of no form the kernel takes",
            xattr("security.capability", &[b'x'; 20]),
        ),
        (
            "a file capability of revision 1",
            xattr("security.capability", &capability(0x0100_0000, 12)),
        ),
        (
            "a file capability of revision 2 in 24 bytes",
            xattr("security.capability", &capability(0x0200_0000, 24)),
        ),
        (
            "a file capability with a flag the kernel does not know",
            xattr("security.capability", &capability(0x0200_0002, 20)),
        ),
        (
            "a file capability of revision 3 whose root id is -1",
            xattr("security.capability", &{
                let mut v = capability(0x0300_0000, 24);
                v[20..].fill(0xff);
                v
            }),
        ),
    ]
}

/// A `security.capability` value of `len` bytes whose magic is `magic`.
fn capability(magic: u32, len: usize) -> Vec<u8> {
    let mut v = magic.to_le_bytes().to_vec();
    v.resize(len, 0);
    v
}

/// A POSIX ACL xattr value: `posix_acl_xattr_header`'s version, then each
/// entry's tag, permissions and id, little-endian.
fn posix_acl(version: u32, entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut v = version.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        v.extend(tag.to_le_bytes());
        v.extend(perm.to_le_bytes());
        v.extend(id.to_le_bytes());
    }
    v
}

/// The POSIX ACL tags, and the id of an entry that names no one.
const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;
const ACL_UNDEFINED: u32 = u32::MAX;

/// The owner, the group and others, which every ACL holds.
const ACL_MINIMAL: [(u16, u16, u32); 3] = [
    (ACL_USER_OBJ, 6, ACL_UNDEFINED),
    (ACL_GROUP_OBJ, 4, ACL_UNDEFINED),
    (ACL_OTHER, 4, ACL_UNDEFINED),
];

/// POSIX ACLs, each with its xattr, whether it is set on a directory, and
/// whether the kernel's VFS takes it there: refused by its form
/// (`posix_acl_from_xattr`), by its entries (`posix_acl_valid`) or as a
/// default ACL on a file (`set_posix_acl`); taken, an empty value or one
/// with no entry (which remove it, on a file a default one too), named
/// entries with a mask, a named user twice and a mask with no named entry
/// (`posix_acl_valid` checks neither). Named entries name the caller's own
/// ids, which every user namespace it runs in maps.
fn acl_cases() -> Vec<(&'static str, &'static str, Vec<u8>, bool, bool)> {
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let (u, g, m, o) = (ACL_USER_OBJ, ACL_GROUP_OBJ, ACL_MASK, ACL_OTHER);
    let none = ACL_UNDEFINED;
    let named = |user: u32, group: u32| {
        posix_acl(
            2,
            &[
                (u, 6, none),
                (ACL_USER, 4, user),
                (g, 4, none),
                (ACL_GROUP, 4, group),
                (m, 4, none),
                (o, 4, none),
            ],
        )
    };
    let mut odd = posix_acl(2, &ACL_MINIMAL);
    odd.push(0);
    let access = "system.posix_acl_access";
    let default = "system.posix_acl_default";
    vec![
        (
            "version 3",
            access,
            posix_acl(3, &ACL_MINIMAL),
            false,
            false,
        ),
        (
            "version 3 with no entry",
            access,
            posix_acl(3, &[]),
            false,
            false,
        ),
        ("a size other than 4 + 8n", access, odd, false, false),
        ("shorter than its header", access, vec![2, 0], false, false),
        (
            "an unknown tag",
            access,
            posix_acl(2, &[(0x40, 4, none)]),
            false,
            false,
        ),
        (
            "no ACL_OTHER entry",
            access,
            posix_acl(2, &[(u, 6, none)]),
            false,
            false,
        ),
        (
            "an ACL_USER id of -1",
            access,
            named(none, gid),
            false,
            false,
        ),
        (
            "an ACL_GROUP id of -1",
            access,
            named(uid, none),
            false,
            false,
        ),
        (
            "a default ACL on a file",
            default,
            posix_acl(2, &ACL_MINIMAL),
            false,
            false,
        ),
        (
            "permissions past rwx",
            access,
            posix_acl(2, &[(u, 0o17, none), (g, 4, none), (o, 4, none)]),
            false,
            false,
        ),
        (
            "a named user and no mask",
            access,
            posix_acl(
                2,
                &[(u, 6, none), (ACL_USER, 4, uid), (g, 4, none), (o, 4, none)],
            ),
            false,
            false,
        ),
        (
            "the group before the owner",
            access,
            posix_acl(2, &[(g, 4, none), (u, 6, none), (o, 4, none)]),
            false,
            false,
        ),
        (
            "two owners",
            access,
            posix_acl(2, &[(u, 6, none), (u, 6, none), (g, 4, none), (o, 4, none)]),
            false,
            false,
        ),
        (
            "a mask before the group",
            access,
            posix_acl(2, &[(u, 6, none), (m, 4, none), (g, 4, none), (o, 4, none)]),
            false,
            false,
        ),
        (
            "an entry after ACL_OTHER",
            access,
            posix_acl(2, &[(u, 6, none), (g, 4, none), (o, 4, none), (o, 4, none)]),
            false,
            false,
        ),
        (
            "a default ACL with no ACL_OTHER entry on a directory",
            default,
            posix_acl(2, &[(u, 6, none), (g, 4, none)]),
            true,
            false,
        ),
        (
            "the owner, the group and others",
            access,
            posix_acl(2, &ACL_MINIMAL),
            false,
            true,
        ),
        (
            "named entries with a mask",
            access,
            named(uid, gid),
            false,
            true,
        ),
        (
            "a named user twice",
            access,
            posix_acl(
                2,
                &[
                    (u, 6, none),
                    (ACL_USER, 4, uid),
                    (ACL_USER, 4, uid),
                    (g, 4, none),
                    (m, 4, none),
                    (o, 4, none),
                ],
            ),
            false,
            true,
        ),
        (
            "a mask with no named entry",
            access,
            posix_acl(2, &[(u, 6, none), (g, 4, none), (m, 4, none), (o, 4, none)]),
            false,
            true,
        ),
        ("an empty value", access, Vec::new(), false, true),
        ("no entry", access, posix_acl(2, &[]), false, true),
        (
            "a default ACL on a directory",
            default,
            posix_acl(2, &ACL_MINIMAL),
            true,
            true,
        ),
        (
            "an empty default ACL on a file",
            default,
            Vec::new(),
            false,
            true,
        ),
        (
            "a default ACL with no entry on a file",
            default,
            posix_acl(2, &[]),
            false,
            true,
        ),
    ]
}

/// A layer of a file `first`, then `x`, a directory if `dir`, with the
/// xattr `name` set to `value`.
fn acl_layer(f: &Fixture, name: &str, value: &[u8], dir: bool) -> Vec<u8> {
    let mut t = Tar::new(f);
    t.file("first", 0o644, b"1")
        .pax(&[(format!("SCHILY.xattr.{name}").as_str(), value)]);
    if dir {
        t.dir("x", 0o755);
    } else {
        t.file("x", 0o644, b"");
    }
    t.bytes()
}

/// An empty POSIX ACL removes the one an entry already has, as setting it
/// does since Linux 6.2, on a kernel that refuses the value too.
#[test]
fn an_empty_posix_acl_removes_the_one_there() {
    let f = Fixture::new();
    let d = f.host.join("d");
    fs::create_dir(&d).unwrap();
    let flags = rustix::fs::XattrFlags::empty();
    let minimal = posix_acl(2, &ACL_MINIMAL);
    let named = posix_acl(
        2,
        &[
            (ACL_USER_OBJ, 7, ACL_UNDEFINED),
            (ACL_USER, 5, rustix::process::getuid().as_raw()),
            (ACL_GROUP_OBJ, 5, ACL_UNDEFINED),
            (ACL_MASK, 5, ACL_UNDEFINED),
            (ACL_OTHER, 5, ACL_UNDEFINED),
        ],
    );
    for (name, value) in [
        ("system.posix_acl_access", &named),
        ("system.posix_acl_default", &minimal),
    ] {
        match rustix::fs::setxattr(&d, name, value, flags) {
            Ok(()) => {}
            Err(Errno::NOTSUP) => {
                eprintln!("skipped: the temporary filesystem takes no POSIX ACLs");
                return;
            }
            Err(e) => panic!("{name}: {e}"),
        }
    }
    let mut t = Tar::new(&f);
    t.pax(&[
        ("SCHILY.xattr.system.posix_acl_access", b""),
        ("SCHILY.xattr.system.posix_acl_default", b""),
    ])
    .dir("d", 0o755)
    // None there to remove: no error either.
    .pax(&[("SCHILY.xattr.system.posix_acl_access", b"")])
    .file("f", 0o644, b"");
    extract(&f.root, t.bytes().as_slice(), Whiteouts::Apply).unwrap();
    for name in ["system.posix_acl_access", "system.posix_acl_default"] {
        let mut buf = [0; 256];
        assert_eq!(
            rustix::fs::getxattr(&d, name, &mut buf),
            Err(Errno::NODATA),
            "{name}"
        );
    }
}

/// Red team (#179, T26): a POSIX ACL the kernel's VFS refuses before any
/// filesystem sees it (`do_set_acl`, `posix_acl_from_xattr`,
/// `posix_acl_valid`), or a default ACL on a file, is refused by the check
/// as by every extraction, with the same error, before anything of the
/// entry is written, so a layer the check passes on `/` never fails
/// part-way for it.
#[test]
fn red_team_a_posix_acl_the_kernel_never_sets_is_refused_by_the_check() {
    let f = Fixture::new();
    for (what, name, value, dir, _) in acl_cases().into_iter().filter(|c| !c.4) {
        let layer = acl_layer(&f, name, &value, dir);
        let checked = check_headers(layer.as_slice()).unwrap_err();
        assert!(
            matches!(&checked, Error::Entry { reason, .. } if reason.contains("POSIX ACL")),
            "{what}: {checked}"
        );
        for (how, r, tree) in every_extraction(&layer) {
            let e = r.unwrap_err();
            assert_eq!(e.to_string(), checked.to_string(), "{what}, {how}");
            assert_eq!(
                tree,
                BTreeSet::from([OsString::from("first")]),
                "{what}, {how}"
            );
        }
    }
}

/// The check takes a POSIX ACL exactly where the kernel does, as measured
/// on a file or directory of the temporary filesystem, and every
/// extraction takes one the kernel takes. Skipped where that filesystem
/// takes no ACLs.
#[test]
fn the_check_takes_a_posix_acl_where_the_kernel_does() {
    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("probe");
    fs::write(&probe, b"").unwrap();
    let minimal = posix_acl(2, &ACL_MINIMAL);
    let flags = rustix::fs::XattrFlags::empty();
    if let Err(Errno::NOTSUP) =
        rustix::fs::setxattr(&probe, "system.posix_acl_access", &minimal, flags)
    {
        eprintln!("skipped: the temporary filesystem takes no POSIX ACLs");
        return;
    }
    let f = Fixture::new();
    for (i, (what, name, value, dir, takes)) in acl_cases().into_iter().enumerate() {
        let p = tmp.path().join(i.to_string());
        if dir {
            fs::create_dir(&p).unwrap();
        } else {
            fs::write(&p, b"").unwrap();
        }
        let kernel = rustix::fs::setxattr(&p, name, &value, flags);
        // Linux before 6.2 refuses an empty value, which 6.2 and later take
        // as a removal: every extraction removes the ACL instead.
        let removal = value.is_empty() && kernel == Err(Errno::INVAL);
        assert!(
            kernel.is_ok() == takes || removal,
            "{what}: the kernel gives {kernel:?}"
        );
        let layer = acl_layer(&f, name, &value, dir);
        let checked = check_headers(layer.as_slice());
        assert_eq!(
            checked.is_ok(),
            takes,
            "{what}: the check gives {checked:?}"
        );
        if takes {
            for (how, r, tree) in every_extraction(&layer) {
                r.unwrap_or_else(|e| panic!("{what}, {how}: {e}"));
                assert!(tree.contains(OsStr::new("x")), "{what}, {how}");
            }
        }
    }
}

/// Every extraction of `tar` into a fresh root, by name: [`extract`] in
/// each form (its overlay form is [`extract_overlay`] making device
/// nodes), [`extract_overlay`] leaving them out, and [`extract_live`]; with
/// the names in the root afterwards (a deeper walk by host path would stop
/// at `PATH_MAX`).
fn every_extraction(tar: &[u8]) -> Vec<(&'static str, Result<()>, BTreeSet<OsString>)> {
    type Run = fn(&RootPath, &[u8]) -> Result<()>;
    let runs: [(&str, Run); 5] = [
        ("Apply", |r, t| extract(r, t, Whiteouts::Apply)),
        ("Overlay", |r, t| extract(r, t, Whiteouts::Overlay)),
        ("Literal", |r, t| extract(r, t, Whiteouts::Literal)),
        ("overlay, Skip", |r, t| {
            extract_overlay(r, t, &[], DeviceNodes::Skip).map(drop)
        }),
        ("live", |r, t| extract_live(r, t, &live_keep())),
    ];
    runs.into_iter()
        .map(|(name, run)| {
            let f = Fixture::new();
            let r = run(&f.root, tar);
            let names = fs::read_dir(&f.host)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            (name, r, names)
        })
        .collect()
}

/// A value the kernel would refuse while the entry is written (#179) is
/// refused from the header: by [`check_headers`], and by every extraction
/// with the same error, before anything of the entry is written.
#[test]
fn values_the_kernel_refuses_are_refused_by_the_check_and_every_extraction() {
    let f = Fixture::new();
    for (what, tar) in kernel_refused(&f) {
        let checked = check_headers(tar.as_slice()).unwrap_err();
        assert!(matches!(checked, Error::Entry { .. }), "{what}: {checked}");
        for (how, r, tree) in every_extraction(&tar) {
            let e = r.unwrap_err();
            assert_eq!(e.to_string(), checked.to_string(), "{what}, {how}");
            assert_eq!(
                tree,
                BTreeSet::from([OsString::from("first")]),
                "{what}, {how}"
            );
        }
    }
}

/// Controls: values at the kernel's limits, or that it never gets, are
/// taken by the check and every extraction: a name of `NAME_MAX` bytes, a
/// symlink target of `PATH_MAX - 1` bytes, a NUL byte in a name `..` folds
/// away, an xattr name of `XATTR_NAME_MAX` bytes, a `trusted.*` xattr (never
/// set) and the xattrs of a hardlink (never set); a whiteout hiding a name
/// of `NAME_MAX` bytes and AUFS metadata with a NUL byte by all but the
/// literal form, which writes their names as files; an entry whose whole
/// path is over `PATH_MAX` in a directory that fits it (the kernel gets the
/// directory, then the name); and by the check, a value of `XATTR_SIZE_MAX`
/// bytes, xattrs of any namespace or none (a FUSE filesystem may take any
/// name) and file capabilities of each form the kernel takes.
#[test]
fn values_at_the_kernels_limits_or_that_it_never_gets_are_taken() {
    let f = Fixture::new();
    let name = "n".repeat(255);
    let user = format!("SCHILY.xattr.user.{}", "a".repeat(250));
    let trusted = format!("SCHILY.xattr.trusted.\0{}", "a".repeat(300));
    let mut t = Tar::new(&f);
    t.pax(&[("path", name.as_bytes())])
        .file("x", 0o644, b"")
        .pax(&[("linkpath", &[b't'; 4095])])
        .symlink("s", "x")
        .pax(&[("path", b"a\0/../folded")])
        .file("x", 0o644, b"")
        .pax(&[(user.as_str(), b"v"), (trusted.as_str(), &[b'v'; 70_000])])
        .file("named", 0o644, b"")
        .pax(&[
            ("SCHILY.xattr.com.apple.x", b"v"),
            ("SCHILY.xattr.user.a", b"v"),
        ])
        .hardlink("h", "named");
    let bytes = t.bytes();
    check_headers(bytes.as_slice()).unwrap();
    for (how, r, names) in every_extraction(&bytes) {
        r.unwrap_or_else(|e| panic!("{how}: {e}"));
        for n in [name.as_str(), "s", "folded", "named", "h"] {
            assert!(names.contains(OsStr::new(n)), "{how}: {n}");
        }
    }

    let mut t = Tar::new(&f);
    t.file("d/x", 0o644, b"")
        .pax(&[("path", format!("d/.wh.{name}").as_bytes())])
        .file("x", 0o644, b"")
        .pax(&[("path", format!("d/.wh..wh.\0{name}").as_bytes())])
        .file("x", 0o644, b"");
    let bytes = t.bytes();
    check_headers(bytes.as_slice()).unwrap();
    for (how, r, _) in every_extraction(&bytes) {
        match how {
            "Literal" => assert!(matches!(r, Err(Error::Entry { .. })), "{r:?}"),
            _ => r.unwrap_or_else(|e| panic!("{how}: {e}")),
        }
    }

    // A live root's limit is lower: [`a_live_root_takes_a_directory_path_to_what_proc_shows_of_it`].
    let dir = dir_of(4095);
    let mut t = Tar::new(&f);
    t.pax(&[("path", format!("{dir}/{name}").as_bytes())])
        .file("x", 0o644, b"deep")
        .pax(&[("linkpath", format!("{dir}/{name}").as_bytes())])
        .hardlink("h", "x");
    let bytes = t.bytes();
    check_headers_within(bytes.as_slice(), PATH_MAX).unwrap();
    for (how, r, _) in every_extraction(&bytes) {
        if how != "live" {
            r.unwrap_or_else(|e| panic!("{how}: {e}"));
        }
    }

    // A value's limit is the kernel's, whatever the filesystem takes.
    let xattr = |name: &str, value: &[u8]| {
        let mut t = Tar::new(&f);
        t.pax(&[(format!("SCHILY.xattr.{name}").as_str(), value)])
            .file("x", 0o644, b"");
        check_headers(t.bytes().as_slice())
    };
    xattr("user.big", &[b'v'; 65536]).unwrap();
    xattr("user.big", &[b'v'; 65537]).unwrap_err();
    // Which names a filesystem takes (ext4 takes `gnu.*`), and who may set a
    // file capability, are left to the extraction.
    for name in [
        "gnu.x",
        "btrfs.compression",
        "os2.x",
        "osx.x",
        "com.apple.x",
        "user.",
    ] {
        xattr(name, b"zstd").unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    for value in [
        capability(0x0200_0000, 20),
        capability(0x0200_0001, 20),
        capability(0x0300_0000, 24),
        capability(0x0300_0001, 24),
        Vec::new(),
    ] {
        xattr("security.capability", &value).unwrap_or_else(|e| panic!("{value:?}: {e}"));
    }
}

/// A symlink that puts an entry's directory past what `/proc/self/fd`
/// shows of it in a live root: the entry is refused before a directory it
/// lacks is made, and one a byte shorter is taken; in a directory made
/// there, it is refused as well, and so is a whiteout, an opaque one or a
/// hardlink target there, before anything is removed or linked.
#[test]
fn a_live_root_refuses_a_directory_a_symlink_puts_past_what_proc_shows() {
    let f = Fixture::new();
    let host = fs::canonicalize(&f.host).unwrap();
    let deep = dir_of(live_dir_max(host.as_os_str().as_bytes()) - 2);
    let mut t = Tar::new(&f);
    t.file("first", 0o644, b"1")
        .pax(&[("path", deep.as_bytes())])
        .dir("d", 0o755)
        .pax(&[("linkpath", deep.as_bytes())])
        .symlink("s", "d")
        // `deep/a` fits, `deep/aa` is a byte past.
        .file("s/a/x", 0o644, b"in")
        .file("s/aa/x", 0o644, b"past");
    let e = extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap_err();
    assert!(
        matches!(&e, Error::Entry { path, reason } if path == Path::new("s/aa/x")
            && reason.ends_with(DIR_PAST_PATH_MAX)),
        "{e}"
    );
    // Below the root's fd: the host's path to them is past `PATH_MAX`.
    let root = fs::File::open(&f.host).unwrap();
    let at = |rel: &str| {
        rustix::fs::statat(&root, Path::new(&deep).join(rel), AtFlags::SYMLINK_NOFOLLOW)
    };
    at("a/x").unwrap();
    assert_eq!(at("aa").unwrap_err(), Errno::NOENT);

    // A directory the symlink leads to that already lies past it.
    let mut t = Tar::new(&f);
    t.dir("s/aaaa", 0o755).file("s/aaaa/x", 0o644, b"past");
    let e = extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap_err();
    assert!(
        matches!(&e, Error::Entry { path, reason } if path == Path::new("s/aaaa/x")
            && reason.ends_with(DIR_PAST_PATH_MAX)),
        "{e}"
    );
    assert_eq!(at("aaaa/x").unwrap_err(), Errno::NOENT);

    // What the whiteouts would remove, below the root's fd.
    rustix::fs::openat(
        &root,
        Path::new(&deep).join("aaaa/y"),
        rustix::fs::OFlags::CREATE | rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o644),
    )
    .unwrap();
    let mut wh = Tar::new(&f);
    wh.file("s/aaaa/.wh.y", 0o644, b"");
    let mut opaque = Tar::new(&f);
    opaque.file("s/aaaa/.wh..wh..opq", 0o644, b"");
    let mut link = Tar::new(&f);
    link.hardlink("h", "s/aaaa/x");
    for (t, raw) in [
        (wh, "s/aaaa/.wh.y"),
        (opaque, "s/aaaa/.wh..wh..opq"),
        (link, "h"),
    ] {
        let e = extract_live(&f.root, t.bytes().as_slice(), &live_keep()).unwrap_err();
        assert!(
            matches!(&e, Error::Entry { path, reason } if path == Path::new(raw)
                && reason.ends_with(DIR_PAST_PATH_MAX)),
            "{raw}: {e}"
        );
    }
    at("aaaa/y").unwrap();
    assert_eq!(
        fs::symlink_metadata(f.host.join("h")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

/// A live root reads where a directory lies through `/proc/self/fd`, whose
/// link holds at most `PATH_MAX` bytes with the root's own path: it takes a
/// directory path up to that and refuses one past it before anything of
/// the entry is written, on `/` past 4094 bytes, as [`check_headers`]
/// does; [`extract`] and [`extract_overlay`] take 4095.
#[test]
fn a_live_root_takes_a_directory_path_to_what_proc_shows_of_it() {
    assert_eq!(live_dir_max(b"/"), 4094);
    assert_eq!(live_dir_max(b"/r"), 4092);
    let layer = |f: &Fixture, dir: &str| {
        let mut t = Tar::new(f);
        t.file("first", 0o644, b"1")
            .pax(&[("path", format!("{dir}/x").as_bytes())])
            .file("x", 0o644, b"deep")
            // Its target's directory is read back through `/proc/self/fd`.
            .pax(&[("linkpath", format!("{dir}/x").as_bytes())])
            .hardlink("h", "x");
        t.bytes()
    };

    let f = Fixture::new();
    let host = fs::canonicalize(&f.host).unwrap();
    let max = live_dir_max(host.as_os_str().as_bytes());
    extract_live(&f.root, layer(&f, &dir_of(max)).as_slice(), &live_keep()).unwrap();
    assert!(f.host.join("h").exists());
    let f = Fixture::new();
    let host = fs::canonicalize(&f.host).unwrap();
    let past = dir_of(live_dir_max(host.as_os_str().as_bytes()) + 1);
    let e = extract_live(&f.root, layer(&f, &past).as_slice(), &live_keep()).unwrap_err();
    assert!(
        matches!(&e, Error::Entry { path, .. } if *path == Path::new(&past).join("x")),
        "{e}"
    );
    let tree: BTreeSet<OsString> = fs::read_dir(&f.host)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(tree, BTreeSet::from([OsString::from("first")]));

    let f = Fixture::new();
    check_headers(layer(&f, &dir_of(4094)).as_slice()).unwrap();
    let past = layer(&f, &dir_of(4095));
    let checked = check_headers(past.as_slice()).unwrap_err();
    // The same reason, for an entry of another name.
    assert_eq!(entry_reason(&checked), entry_reason(&e));
    assert!(
        entry_reason(&checked).ends_with(DIR_PAST_PATH_MAX),
        "{checked}"
    );
    for (how, r, _) in every_extraction(&past) {
        match how {
            "live" => assert!(matches!(r, Err(Error::Entry { .. })), "{r:?}"),
            _ => r.unwrap_or_else(|e| panic!("{how}: {e}")),
        }
    }
}

/// The reason of an [`Error::Entry`].
fn entry_reason(e: &Error) -> &str {
    let Error::Entry { reason, .. } = e else {
        panic!("{e}");
    };
    reason
}

/// A device node or FIFO's name and xattrs are refused by the extraction
/// that makes it, before it or its parents are made, and left to it by the
/// check: a live root makes none, and [`DeviceNodes::Skip`] no device.
#[test]
fn a_nodes_name_and_xattrs_are_refused_where_it_is_made() {
    let f = Fixture::new();
    let long = format!("d/{}", "n".repeat(256));
    let deep = format!("{}/p", dir_of(4096));
    let cases: [(&str, &str, &[u8]); 5] = [
        ("SCHILY.xattr.", "d/p", b"v"),
        ("SCHILY.xattr.user.a", "d/p", b"v"),
        ("path", &long, b""),
        ("path", &deep, b""),
        ("path", "d/a\0b", b""),
    ];
    for (key, name, value) in cases {
        // A record of its own, or the name the node takes.
        let record: (&str, &[u8]) = if key == "path" {
            (key, name.as_bytes())
        } else {
            (key, value)
        };
        let mut t = Tar::new(&f);
        t.pax(&[record])
            .node("d/p", EntryType::Fifo, 0, 0)
            .pax(&[record])
            .node("d/c", EntryType::Char, 1, 3);
        let bytes = t.bytes();
        check_headers(bytes.as_slice()).unwrap();
        let first = Path::new(if key == "path" { name } else { "d/p" });
        for (how, r, tree) in every_extraction(&bytes) {
            match how {
                "live" => r.unwrap_or_else(|e| panic!("{key} {name:?}, {how}: {e}")),
                _ => {
                    let e = r.unwrap_err();
                    assert!(
                        matches!(&e, Error::Entry { path, .. } if path == first),
                        "{key} {name:?}, {how}: {e}"
                    );
                    assert!(tree.is_empty(), "{key} {name:?}, {how}: {tree:?}");
                }
            }
        }
        // Left out, the device's name and xattrs never reach the kernel.
        let mut t = Tar::new(&f);
        t.pax(&[record]).node("d/c", EntryType::Char, 1, 3);
        let f = Fixture::new();
        extract_overlay(&f.root, t.bytes().as_slice(), &[], DeviceNodes::Skip).unwrap();
    }
}

/// One entry of a generated layer: a kind, a name, a link target, and for
/// some kinds a value (an mtime, size or uid record, an id). GNU long name
/// and long link records, and PAX records, apply to the entry after them.
type Gen = (u8, usize, usize, usize);

const GEN_NAMES: &[&str] = &[
    "a",
    "d",
    "d/b",
    "../x",
    "./",
    "/abs",
    "d/.wh.b",
    "d/.wh.",
    "d/.wh..",
    ".wh..wh..opq",
    "d/.wh..wh..opq",
    ".wh..wh.plnk.1",
    "proc/x",
];
const GEN_TARGETS: &[&str] = &["a", "d/b", "../x", "missing", "./", "d", "proc/x", ""];
const GEN_TIMES: &[&[u8]] = &[b"1", b"1x", b"-1.5", b"", b"1.000000001"];
const GEN_SIZES: &[&[u8]] = &[b"0", b"1", b"512", b"1024", b"x", b""];

/// The names GNU long name records and PAX `path` records carry: those
/// above, and values the kernel takes or refuses (#179): a NUL byte, one
/// that `..` folds away, a name of 255 and of 256 bytes, a whiteout hiding
/// one, and an entry in a directory of 4095 and of 4096 bytes.
fn gen_record_names() -> Vec<Vec<u8>> {
    let (n255, n256) = ("n".repeat(255), "n".repeat(256));
    let mut names: Vec<Vec<u8>> = GEN_NAMES.iter().map(|n| n.as_bytes().to_vec()).collect();
    names.extend([
        b"d/a\0b".to_vec(),
        b"a\0/../b".to_vec(),
        format!("d/{n255}").into_bytes(),
        format!("d/{n256}").into_bytes(),
        format!("d/.wh.{n255}").into_bytes(),
        format!("d/.wh.{n256}").into_bytes(),
        format!("{}/f", dir_of(4095)).into_bytes(),
        format!("{}/f", dir_of(4096)).into_bytes(),
    ]);
    names
}

/// The link targets GNU long link records and PAX `linkpath` records
/// carry: those above, a NUL byte, a name of 256 bytes, an entry in a
/// directory of 4095 bytes (past what a live root at `/` reads back) and
/// of 4096, and targets of 4095 and of 4096 bytes.
fn gen_record_targets() -> Vec<Vec<u8>> {
    let mut targets: Vec<Vec<u8>> = GEN_TARGETS.iter().map(|t| t.as_bytes().to_vec()).collect();
    targets.extend([
        b"t\0u".to_vec(),
        format!("d/{}", "n".repeat(256)).into_bytes(),
        format!("{}/f", dir_of(4095)).into_bytes(),
        format!("{}/f", dir_of(4096)).into_bytes(),
        vec![b't'; 4095],
        vec![b't'; 4096],
    ]);
    targets
}

/// The xattrs a PAX record sets: one the kernel takes, names empty, of 255
/// and 256 bytes or holding a NUL byte, a value over 64 KiB, a `trusted.*`
/// one (never set), a `user.overlay.*` one that only its escaped form puts
/// over 255 bytes, names of a namespace or none a filesystem may not take,
/// a file capability of no form, and POSIX ACLs the kernel takes, refuses
/// by their form or entries, or takes only on a directory (a default one).
/// Not a name that is only a namespace
/// (`user.`): the filesystem refuses it with an `EINVAL` that
/// [`is_kernel_refusal`] cannot tell from the kernel's.
fn gen_xattr(i: usize) -> (String, Vec<u8>) {
    let v = b"v".to_vec();
    let xattrs = [
        ("user.a".to_owned(), v.clone()),
        (String::new(), v.clone()),
        (format!("user.{}", "a".repeat(250)), v.clone()),
        (format!("user.{}", "a".repeat(251)), v.clone()),
        ("com.apple.x".to_owned(), v.clone()),
        ("user.a\0b".to_owned(), v.clone()),
        ("user.big".to_owned(), vec![b'v'; 65537]),
        (format!("trusted.\0{}", "a".repeat(300)), vec![b'v'; 70_000]),
        (format!("user.overlay.{}", "o".repeat(240)), v.clone()),
        ("gnu.x".to_owned(), v),
        ("security.capability".to_owned(), vec![b'x'; 20]),
        (
            "system.posix_acl_access".to_owned(),
            posix_acl(2, &ACL_MINIMAL),
        ),
        ("system.posix_acl_access".to_owned(), vec![2, 0]),
        (
            "system.posix_acl_access".to_owned(),
            posix_acl(2, &ACL_MINIMAL[..1]),
        ),
        (
            "system.posix_acl_default".to_owned(),
            posix_acl(2, &ACL_MINIMAL),
        ),
    ];
    let n = xattrs.len();
    xattrs.into_iter().nth(i % n).unwrap()
}

/// An error the kernel gives `tar`'s extraction for a header value
/// [`check_headers`] refuses first (#179): a NUL byte (`EINVAL`), a name
/// or symlink target too long (`ENAMETOOLONG` making a link, or for a path
/// past the limits itself, not one looked up through a long symlink), an
/// xattr it refuses by its name or size, a file capability's form, or a
/// `user.*` one on a symlink (`EPERM`, unless the entry has a `security.*`
/// xattr, which a test without privilege is refused with `EPERM` too,
/// matched by its name in the layer, which a live root reports only when
/// no symlink lies on its way). Not a name the
/// filesystem does not take (`EOPNOTSUPP`), nor where a live root's
/// directory lies, read through `/proc/self/fd`, which depends on where
/// the root is.
fn is_kernel_refusal(e: &Error, tar: &[u8]) -> bool {
    let Error::Io { op, path, .. } = e else {
        return false;
    };
    match (*op, errno(e)) {
        ("readlink /proc/self/fd", _) => false,
        ("setxattr", Some(Errno::PERM)) => !security_xattr_paths(tar).contains(&cleaned(path)),
        ("setxattr", Some(Errno::RANGE | Errno::TOOBIG)) | (_, Some(Errno::INVAL)) => true,
        ("symlink" | "link", Some(Errno::NAMETOOLONG)) => true,
        (_, Some(Errno::NAMETOOLONG)) => {
            let path = cleaned(path);
            // An entry's directory, or its target's, is opened by its path.
            let dir = if *op == "open" {
                Some(path.as_path())
            } else {
                path.parent()
            };
            path.iter().any(|c| c.len() > NAME_MAX)
                || dir.is_some_and(|d| d.as_os_str().len() > PATH_MAX)
        }
        _ => false,
    }
}

/// `path` as the extraction names an entry ([`path::clean`]).
fn cleaned(path: &Path) -> PathBuf {
    crate::path::clean(path).expect("an extraction names entries inside the root")
}

/// The names of the entries of `tar` with a `security.*` xattr, as far as
/// it reads.
fn security_xattr_paths(tar: &[u8]) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    let mut archive = tar::Archive::new(tar);
    let Ok(entries) = archive.entries() else {
        return paths;
    };
    for entry in entries {
        let Ok(mut entry) = entry else {
            break;
        };
        let path = cleaned(Path::new(OsStr::from_bytes(&entry.path_bytes())));
        if let Ok(Some(pax)) = entry.pax_extensions()
            && pax
                .flatten()
                .any(|r| r.key_bytes().starts_with(b"SCHILY.xattr.security."))
        {
            paths.insert(path);
        }
    }
    paths
}

fn gen_layer(f: &Fixture, entries: &[Gen]) -> Vec<u8> {
    let (names, targets) = (gen_record_names(), gen_record_targets());
    let mut t = Tar::new(f);
    for &(kind, n, l, value) in entries {
        let name = GEN_NAMES[n % GEN_NAMES.len()];
        let target = GEN_TARGETS[l % GEN_TARGETS.len()];
        let (long_name, long_target) = (&names[n % names.len()], &targets[l % targets.len()]);
        match kind % 18 {
            0 => t.file(name, 0o644, b"x"),
            1 => t.dir(name, 0o755),
            2 => t.symlink(name, target),
            3 => t.hardlink(name, target),
            4 => t.node(name, EntryType::Fifo, 0, 0),
            5 => t.node(name, EntryType::Char, 1, 3),
            6 => {
                let h = t.header(name.as_bytes(), EntryType::new(b'Q'), 0o644, 0);
                t.push(h, b"")
            }
            7 => t.pax(&[("mtime", GEN_TIMES[value % GEN_TIMES.len()])]),
            8 => {
                let mut h = t.header(name.as_bytes(), EntryType::Regular, 0o644, 0);
                h.set_uid(if value % 2 == 0 { 1 << 40 } else { t.uid });
                t.push(h, b"")
            }
            9 => {
                let h = t.header(b"g", EntryType::XGlobalHeader, 0o644, 0);
                t.push(h, b"")
            }
            10 => {
                let h = t.header(name.as_bytes(), EntryType::GNUSparse, 0o644, 0);
                t.push(h, b"")
            }
            11 => t.gnu_long(EntryType::GNULongName, long_name),
            12 => t.gnu_long(EntryType::GNULongLink, long_target),
            13 => t.pax(&[("path", long_name)]),
            14 => t.pax(&[("linkpath", long_target)]),
            15 => t.pax(&[("size", GEN_SIZES[value % GEN_SIZES.len()])]),
            16 => {
                let uid = match value % 4 {
                    0 => t.uid.to_string(),
                    1 => "x".to_owned(),
                    2 => (1u64 << 32).to_string(),
                    _ => String::new(),
                };
                t.pax(&[("uid", uid.as_bytes())])
            }
            17 => {
                let (name, value) = gen_xattr(value);
                t.pax(&[(format!("SCHILY.xattr.{name}").as_str(), &value)])
            }
            _ => unreachable!("a kind modulo 18"),
        };
    }
    t.bytes()
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Red team (ADR-0123): on generated layers of hostile names, links,
    /// whiteouts, entry types, GNU long names and links, PAX paths, link
    /// paths, xattrs, sizes, times and ids, the check refuses only layers
    /// that every extraction applying whiteouts refuses too, so the spool
    /// pass on `/` never fails a layer the extraction takes; and on a layer
    /// it passes, no extraction fails on the framing, the `tar-header`
    /// limit or a value the kernel refuses (#179). The check as
    /// [`extract`] reads, directory paths to 4095 bytes;
    /// [`check_headers`], for `/`, refuses the same and else only a
    /// directory path past 4094 bytes ([`on_slash_disagreement`]).
    #[test]
    fn red_team_check_headers_refuses_only_what_an_extraction_refuses(
        entries in proptest::collection::vec(
            (0u8..18, 0usize..32, 0usize..32, 0usize..60),
            0..8,
        ),
    ) {
        let bytes = gen_layer(&Fixture::new(), &entries);
        let checked = check_headers_within(bytes.as_slice(), PATH_MAX);
        if let Some(why) = on_slash_disagreement(&checked, &check_headers(bytes.as_slice())) {
            proptest::prop_assert!(false, "{why}");
        }
        for (how, r, _) in applying_extractions(&bytes) {
            if let Some(why) = disagreement(&bytes, &checked, &r) {
                proptest::prop_assert!(false, "{how}: {why}");
            }
        }
    }
}

/// The extractions [`check_headers`] reads as: those of [`every_extraction`]
/// that apply whiteouts.
fn applying_extractions(tar: &[u8]) -> Vec<(&'static str, Result<()>, BTreeSet<OsString>)> {
    every_extraction(tar)
        .into_iter()
        .filter(|(how, ..)| *how != "Literal")
        .collect()
}

/// How the check's result `checked` on `tar` disagrees with an
/// extraction's `r`: it refused a layer the extraction takes, or passed
/// one whose framing, `tar-header` limit or header values
/// ([`is_kernel_refusal`]) the extraction then refuses.
fn disagreement(tar: &[u8], checked: &Result<()>, r: &Result<()>) -> Option<String> {
    match (checked, r) {
        (Err(c), Ok(())) => Some(format!("checked {c}, but extracted")),
        (Ok(()), Err(e @ (Error::Tar(_) | Error::ExtensionHeader { .. }))) => {
            Some(format!("checked, but the framing failed: {e}"))
        }
        (Ok(()), Err(e)) if is_kernel_refusal(e, tar) => {
            Some(format!("checked, but the kernel refused a value: {e}"))
        }
        _ => None,
    }
}

/// How [`check_headers`]'s result `on_slash` disagrees with `checked`, the
/// check's as [`extract`] reads: it passed a layer the check refuses, or
/// refused one otherwise than the check did and not for a directory path
/// past 4094 bytes, which only a live root at `/` refuses.
fn on_slash_disagreement(checked: &Result<()>, on_slash: &Result<()>) -> Option<String> {
    match (checked, on_slash) {
        (Err(c), Ok(())) => Some(format!("checked {c}, but passed on /")),
        (Err(c), Err(s)) if c.to_string() == s.to_string() => None,
        (_, Err(Error::Entry { reason, .. })) if reason.ends_with(DIR_PAST_PATH_MAX) => None,
        (_, Err(s)) => Some(format!("checked {checked:?}, but refused on / with {s}")),
        (Ok(()), Ok(())) => None,
    }
}

/// A layer with one of each header [`check_headers`] reads (PAX records,
/// GNU long names and links, links, a FIFO, whiteouts, a PAX `size`, a
/// global header), and the offset of every header block in it.
fn mutation_base(f: &Fixture) -> (Vec<u8>, Vec<usize>) {
    let mut t = Tar::new(f);
    let mut at = Vec::new();
    let long = |t: &mut Tar, at: &mut Vec<usize>, kind: EntryType, body: &[u8]| {
        at.push(t.out.len());
        let size = u64::try_from(body.len()).unwrap();
        let h = t.header(b"././@LongLink", kind, 0o644, size);
        t.push(h, body);
    };
    at.push(t.out.len());
    t.pax(&[("SCHILY.xattr.user.a", b"v"), ("mtime", b"1700000000.5")]);
    at.push(t.out.len());
    t.dir("d", 0o755);
    at.push(t.out.len());
    t.file("d/f", 0o644, b"hello world");
    at.push(t.out.len());
    t.symlink("d/s", "f");
    at.push(t.out.len());
    t.hardlink("d/h", "d/f");
    at.push(t.out.len());
    t.node("p", EntryType::Fifo, 0, 0);
    let mut name = b"d/".to_vec();
    name.extend([b'l'; 150]);
    long(&mut t, &mut at, EntryType::GNULongName, &name);
    at.push(t.out.len());
    t.file("x", 0o644, b"long");
    long(&mut t, &mut at, EntryType::GNULongLink, b"d/f");
    at.push(t.out.len());
    t.hardlink("d/k", "x");
    at.push(t.out.len());
    t.file("d/.wh.gone", 0o644, b"");
    at.push(t.out.len());
    t.file("d/.wh..wh..opq", 0o644, b"");
    at.push(t.out.len());
    t.pax(&[("path", b"d/pp"), ("linkpath", b"f")]);
    at.push(t.out.len());
    t.symlink("n", "t");
    at.push(t.out.len());
    t.pax(&[("size", b"5")]);
    at.push(t.out.len());
    let h = t.header(b"sz", EntryType::Regular, 0o644, 0);
    t.push(h, b"abcde");
    at.push(t.out.len());
    let h = t.header(b"g", EntryType::XGlobalHeader, 0o644, 17);
    t.push(h, b"17 comment=hello\n");
    at.push(t.out.len());
    t.file("proc/x", 0o644, b"");
    at.push(t.out.len());
    t.file("last", 0o644, b"z");
    (t.bytes(), at)
}

/// Members to splice into the base of the proptest below: each value of
/// [`kernel_refused`], and values at the kernel's limits or that it never
/// gets (#179); with the offset of every header block in each. Made once.
fn kernel_members() -> &'static [(Vec<u8>, Vec<usize>)] {
    static MEMBERS: std::sync::OnceLock<Vec<(Vec<u8>, Vec<usize>)>> = std::sync::OnceLock::new();
    MEMBERS.get_or_init(|| make_kernel_members(&Fixture::new()))
}

fn make_kernel_members(f: &Fixture) -> Vec<(Vec<u8>, Vec<usize>)> {
    let mut members: Vec<Vec<u8>> = kernel_refused(f)
        .into_iter()
        // Without the good first entry (a header and a data block) and the
        // end blocks.
        .map(|(_, layer)| layer[1024..layer.len() - 1024].to_vec())
        .collect();
    let name = "n".repeat(255);
    let mut t = Tar::new(f);
    t.pax(&[("path", format!("d/{name}").as_bytes())])
        .file("x", 0o644, b"")
        .pax(&[("linkpath", &[b't'; 4095])])
        .symlink("s", "x")
        .pax(&[
            (
                format!("SCHILY.xattr.user.{}", "a".repeat(250)).as_str(),
                b"v",
            ),
            (format!("SCHILY.xattr.trusted.\0{name}").as_str(), b"v"),
        ])
        .file("named", 0o644, b"")
        .pax(&[("path", format!("d/.wh.{name}").as_bytes())])
        .file("x", 0o644, b"");
    members.push(t.out.clone());
    members
        .into_iter()
        .map(|m| {
            let mut at = Vec::new();
            let mut pos = 0;
            while pos < m.len() {
                at.push(pos);
                let mut h = Header::new_old();
                h.as_mut_bytes().copy_from_slice(&m[pos..pos + 512]);
                let size = usize::try_from(h.size().unwrap()).unwrap();
                pos += 512 + size.div_ceil(512) * 512;
            }
            (m, at)
        })
        .collect()
}

/// Header bytes worth changing: the first and last of each field.
const FIELD_BYTES: &[usize] = &[
    0, 1, 99, 100, 107, 108, 116, 124, 125, 134, 135, 136, 147, 156, 157, 158, 256, 257, 262, 263,
    329, 337, 345, 400, 499,
];
/// Values worth writing there: NUL, octal digits, a base-256 marker, path
/// and entry type characters.
const VALUES: &[u8] = &[
    0, b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'9', 0x80, 0xff, b'/', b'.', b' ', b'L',
    b'K', b'x', b'g', b'S', b'V', b'Q',
];

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(1024))]

    /// Red team (ADR-0123): a valid layer, half the time with a member of
    /// [`kernel_members`] spliced in between two of its members, with one
    /// to four bytes changed, in a header (its checksum recomputed, three
    /// times in four) or anywhere in the stream: the check refuses only
    /// what every extraction that applies whiteouts refuses too, and on
    /// what it passes no extraction fails on the framing, the `tar-header`
    /// limit or a value the kernel refuses (#179); as in the proptest
    /// above.
    #[test]
    fn red_team_check_headers_agrees_with_every_extraction_on_mutated_headers(
        splice in proptest::option::of((0usize..64, 0usize..64)),
        changes in proptest::collection::vec(
            (proptest::bool::ANY, 0usize..64, 0usize..4096, 0usize..256, 0u8..4),
            1..5,
        ),
    ) {
        let f = Fixture::new();
        let (mut bytes, mut headers) = mutation_base(&f);
        if let Some((which, between)) = splice {
            let members = kernel_members();
            let (member, at) = &members[which % members.len()];
            // Before a header no extension header precedes, or before the
            // end blocks.
            let mut starts: Vec<usize> = headers
                .iter()
                .enumerate()
                .filter(|&(i, _)| i == 0 || !b"xLK".contains(&bytes[headers[i - 1] + 156]))
                .map(|(_, &h)| h)
                .collect();
            starts.push(bytes.len() - 1024);
            let pos = starts[between % starts.len()];
            bytes.splice(pos..pos, member.iter().copied());
            for h in &mut headers {
                if *h >= pos {
                    *h += member.len();
                }
            }
            headers.extend(at.iter().map(|a| a + pos));
        }
        for (in_header, which, offset, value, fix) in changes {
            let pos = if in_header {
                let field = if offset % 2 == 0 {
                    FIELD_BYTES[offset / 2 % FIELD_BYTES.len()]
                } else {
                    offset % 512
                };
                headers[which % headers.len()] + field
            } else {
                offset * 7 % bytes.len()
            };
            bytes[pos] = if value < 128 {
                VALUES[value % VALUES.len()]
            } else {
                u8::try_from(value).unwrap()
            };
            let block = pos - pos % 512;
            if fix != 0 && headers.contains(&block) {
                let mut h = Header::new_old();
                h.as_mut_bytes().copy_from_slice(&bytes[block..block + 512]);
                h.set_cksum();
                bytes[block..block + 512].copy_from_slice(h.as_bytes());
            }
        }
        let checked = check_headers_within(bytes.as_slice(), PATH_MAX);
        if let Some(why) = on_slash_disagreement(&checked, &check_headers(bytes.as_slice())) {
            proptest::prop_assert!(false, "{why}");
        }
        for (how, r, _) in applying_extractions(&bytes) {
            if let Some(why) = disagreement(&bytes, &checked, &r) {
                proptest::prop_assert!(false, "{how}: {why}");
            }
        }
    }
}

/// The unmutated base of the proptest above passes the check and every
/// extraction, so its mutations start from a layer that is taken.
#[test]
fn the_mutation_base_is_a_layer_every_extraction_takes() {
    let (bytes, headers) = mutation_base(&Fixture::new());
    assert_eq!(headers.len(), 19);
    check_headers(bytes.as_slice()).unwrap();
    for wh in [Whiteouts::Apply, Whiteouts::Overlay] {
        let f = Fixture::new();
        extract(&f.root, bytes.as_slice(), wh).unwrap_or_else(|e| panic!("{wh:?}: {e}"));
    }
    let f = Fixture::new();
    extract_live(&f.root, bytes.as_slice(), &live_keep()).unwrap();
    assert!(f.host("d/pp").is_symlink());
    assert_eq!(fs::read(f.host("sz")).unwrap(), b"abcde");
}

/// Layers, after a good first entry, each with one encoding of a value at
/// or past the kernel's limits, and whether it is taken.
fn red_team_encodings(f: &Fixture) -> Vec<(&'static str, Vec<u8>, bool)> {
    let first = || {
        let mut t = Tar::new(f);
        t.file("first", 0o644, b"1");
        t
    };
    let pax_file = |records: &[(&str, &[u8])]| first().pax(records).file("x", 0o644, b"").bytes();
    let gnu_file = |body: &[u8]| {
        first()
            .gnu_long(EntryType::GNULongName, body)
            .file("x", 0o644, b"")
            .bytes()
    };
    let symlink_to = |t: &mut Tar| t.symlink("s", "x").bytes();
    let (long, l) = (EntryType::GNULongName, EntryType::GNULongLink);
    let n256 = "n".repeat(256);
    let euro = "€".repeat(85);
    let mut cut = format!("d/{euro}").into_bytes();
    cut.push("€".as_bytes()[0]);
    let xattr = |name: String| pax_file(&[(format!("SCHILY.xattr.{name}").as_str(), b"v")]);
    vec![
        (
            "a NUL at the end of a PAX path",
            pax_file(&[("path", b"d/x\0")]),
            false,
        ),
        ("a PAX path of a NUL", pax_file(&[("path", b"\0")]), false),
        (
            "a GNU long name ending in its own NUL",
            gnu_file(b"d/x\0"),
            true,
        ),
        (
            "a GNU long name ending in two NULs",
            gnu_file(b"d/x\0\0"),
            false,
        ),
        (
            "a GNU long name for an entry whose PAX path holds a NUL",
            symlink_to(first().gnu_long(long, b"d/x\0").pax(&[("path", b"d/a\0b")])),
            true,
        ),
        (
            "a GNU long name holding a NUL for an entry whose PAX path is fine",
            symlink_to(first().pax(&[("path", b"d/x")]).gnu_long(long, b"d/a\0b\0")),
            false,
        ),
        (
            "the first of two PAX paths, fine",
            pax_file(&[("path", b"d/x"), ("path", b"d/a\0b")]),
            true,
        ),
        (
            "the first of two PAX paths, holding a NUL",
            pax_file(&[("path", b"d/a\0b"), ("path", b"d/x")]),
            false,
        ),
        (
            "a NUL at the end of a PAX linkpath",
            symlink_to(first().pax(&[("linkpath", b"t\0")])),
            false,
        ),
        (
            "a GNU long link ending in its own NUL",
            symlink_to(first().gnu_long(l, b"t\0")),
            true,
        ),
        (
            "a GNU long link ending in two NULs",
            symlink_to(first().gnu_long(l, b"t\0\0")),
            false,
        ),
        (
            "a GNU long link for a link whose PAX linkpath is empty",
            symlink_to(first().gnu_long(l, b"t\0").pax(&[("linkpath", b"")])),
            true,
        ),
        (
            "an empty PAX linkpath for a symlink whose ustar target is set",
            first()
                .pax(&[("linkpath", b"")])
                .symlink("s", "first")
                .bytes(),
            false,
        ),
        (
            "a symlink target with a component over NAME_MAX",
            symlink_to(first().pax(&[("linkpath", format!("{n256}/t").as_bytes())])),
            true,
        ),
        (
            "a name of 255 bytes of 3-byte characters",
            pax_file(&[("path", format!("d/{euro}").as_bytes())]),
            true,
        ),
        (
            "a name of 3-byte characters and one more byte",
            pax_file(&[("path", format!("d/{euro}a").as_bytes())]),
            false,
        ),
        (
            "a name of 256 bytes cut inside a character",
            pax_file(&[("path", &cut)]),
            false,
        ),
        (
            "a whiteout hiding 255 bytes of 3-byte characters",
            pax_file(&[("path", format!("d/.wh.{euro}").as_bytes())]),
            true,
        ),
        (
            "a GNU long name of a whiteout hiding a name over NAME_MAX",
            gnu_file(format!("d/.wh.{n256}").as_bytes()),
            false,
        ),
        (
            "an xattr name of 255 bytes of 2-byte characters",
            xattr(format!("user.{}", "é".repeat(125))),
            true,
        ),
        (
            "an xattr name of 2-byte characters and one more byte",
            xattr(format!("user.{}a", "é".repeat(125))),
            false,
        ),
        (
            "a PAX path over PATH_MAX of `./` before a short name",
            pax_file(&[("path", format!("{}d/x", "./".repeat(3000)).as_bytes())]),
            true,
        ),
        (
            "a PAX path over PATH_MAX of slashes between two names",
            pax_file(&[("path", format!("d{}x", "/".repeat(5000)).as_bytes())]),
            true,
        ),
        (
            "a name over NAME_MAX that `..` folds away",
            pax_file(&[("path", format!("{n256}/../x").as_bytes())]),
            true,
        ),
        (
            "a name over NAME_MAX before a trailing `/.`",
            pax_file(&[("path", format!("d/{n256}/.").as_bytes())]),
            false,
        ),
        (
            "a hardlink target with a name over NAME_MAX that `..` folds away",
            first()
                .pax(&[("linkpath", format!("{n256}/../first").as_bytes())])
                .hardlink("h", "x")
                .bytes(),
            true,
        ),
        (
            "a hardlink target with a NUL that `..` folds away",
            first()
                .pax(&[("linkpath", b"a\0/../first")])
                .hardlink("h", "x")
                .bytes(),
            true,
        ),
    ]
}

/// Red team (#179, T26): a value at or past the kernel's limits in each
/// encoding (a NUL inside or at the end of a PAX value or past a GNU
/// record's own, a GNU record and a PAX record for one entry, two PAX
/// records of one key, multi-byte UTF-8 at 255 bytes, `./`, `//` and `..`
/// that clean it): the check and every extraction that applies whiteouts
/// read the same value, and take it or refuse it with the same error
/// before anything of the entry is written.
#[test]
fn red_team_each_encoding_of_a_value_at_the_kernels_limits_is_judged_alike() {
    let f = Fixture::new();
    for (what, tar, taken) in red_team_encodings(&f) {
        let checked = check_headers(tar.as_slice());
        assert_eq!(checked.is_ok(), taken, "{what}: {checked:?}");
        for (how, r, tree) in applying_extractions(&tar) {
            match (&checked, &r) {
                (Ok(()), Ok(())) => {}
                (Err(c), Err(e)) => {
                    assert!(matches!(c, Error::Entry { .. }), "{what}: {c}");
                    assert_eq!(e.to_string(), c.to_string(), "{what}, {how}");
                    assert_eq!(
                        tree,
                        BTreeSet::from([OsString::from("first")]),
                        "{what}, {how}"
                    );
                }
                _ => panic!("{what}, {how}: checked {checked:?}, extracted {r:?}"),
            }
        }
    }
}

/// A directory path of `len` bytes as [`dir_of`] makes it, its names of
/// 3-byte characters but for their last bytes.
fn utf8_dir_of(len: usize) -> String {
    dir_of(len)
        .split('/')
        .map(|n| format!("{}{}", "€".repeat(n.len() / 3), "q".repeat(n.len() % 3)))
        .collect::<Vec<_>>()
        .join("/")
}

/// Red team (#179): a directory path of multi-byte names, its raw name
/// padded with `./` and `//` far past `PATH_MAX`, is held to the limit by
/// its cleaned bytes: a live root takes it at what `/proc/self/fd` shows
/// and refuses it a byte past with the check's error, before anything of
/// it is written; [`check_headers`] takes 4094 bytes and refuses 4095,
/// which [`extract`] and [`extract_overlay`] take.
#[test]
fn red_team_a_padded_directory_path_is_held_to_its_limit_by_its_cleaned_bytes() {
    let raw = |len: usize| {
        let dir = utf8_dir_of(len).replace('/', "//");
        format!("{}{dir}/x", "./".repeat(1000))
    };
    let layer = |f: &Fixture, len: usize| {
        let mut t = Tar::new(f);
        t.file("first", 0o644, b"1")
            .pax(&[("path", raw(len).as_bytes())])
            .file("x", 0o644, b"deep");
        t.bytes()
    };
    let f = Fixture::new();
    let host = fs::canonicalize(&f.host).unwrap();
    let max = live_dir_max(host.as_os_str().as_bytes());
    assert!(raw(max).len() > PATH_MAX + 1000);
    let at_max = layer(&f, max);
    check_headers_within(at_max.as_slice(), max).unwrap();
    extract_live(&f.root, at_max.as_slice(), &live_keep()).unwrap();
    let root = fs::File::open(&f.host).unwrap();
    let at = Path::new(&utf8_dir_of(max)).join("x");
    rustix::fs::statat(&root, &at, AtFlags::SYMLINK_NOFOLLOW).unwrap();

    let f = Fixture::new();
    let past = layer(&f, max + 1);
    let checked = check_headers_within(past.as_slice(), max).unwrap_err();
    let e = extract_live(&f.root, past.as_slice(), &live_keep()).unwrap_err();
    assert_eq!(e.to_string(), checked.to_string());
    let tree: BTreeSet<OsString> = fs::read_dir(&f.host)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(tree, BTreeSet::from([OsString::from("first")]));

    check_headers(layer(&f, 4094).as_slice()).unwrap();
    let past = layer(&f, 4095);
    let checked = check_headers(past.as_slice()).unwrap_err();
    assert!(
        entry_reason(&checked).ends_with(DIR_PAST_PATH_MAX),
        "{checked}"
    );
    for (how, r, _) in every_extraction(&past) {
        if how != "live" {
            r.unwrap_or_else(|e| panic!("{how}: {e}"));
        }
    }
}

/// The `security.capability` values the kernel is asked to set, against
/// [`is_file_capability`]: each revision in its own size and in another,
/// the effective flag, and revision 3 with root ids mapped and not.
fn capability_forms() -> Vec<(&'static str, Vec<u8>)> {
    let v3 = |magic: u32, root: u32| {
        let mut v = capability(magic, 24);
        v[20..].copy_from_slice(&root.to_le_bytes());
        v
    };
    vec![
        ("empty", Vec::new()),
        ("v2", capability(0x0200_0000, 20)),
        ("v2 effective", capability(0x0200_0001, 20)),
        ("v2 unknown flag", capability(0x0200_0002, 20)),
        ("v2 in 24 bytes", capability(0x0200_0000, 24)),
        ("v1 in 12 bytes", capability(0x0100_0000, 12)),
        ("v3 in 20 bytes", capability(0x0300_0000, 20)),
        ("v3 root 0", v3(0x0300_0000, 0)),
        ("v3 effective root 0", v3(0x0300_0001, 0)),
        ("v3 root 1000", v3(0x0300_0000, 1000)),
        ("v3 root 70000", v3(0x0300_0000, 70_000)),
        ("v3 root -2", v3(0x0300_0000, u32::MAX - 1)),
        ("v3 root -1", v3(0x0300_0000, u32::MAX)),
    ]
}

/// Whether [`check_headers`] takes a file whose `security.capability` is
/// `value`.
fn check_takes_capability(value: &[u8]) -> bool {
    let mut t = Tar::new(&Fixture::new());
    t.pax(&[("SCHILY.xattr.security.capability", value)])
        .file("x", 0o755, b"");
    check_headers(t.bytes().as_slice()).is_ok()
}

/// Writes `measured: <line>` to stderr past libtest's capture, for the
/// lane's log.
fn measured(line: &str) {
    use std::io::Write as _;
    // The line is for the log only; the assertions decide.
    let _ = writeln!(std::io::stderr().lock(), "measured: {line}");
}

/// What `fd` holds as `security.capability` read back: its size, revision
/// and, for revision 3, its root id; or why not.
fn capability_read_back(fd: &fs::File) -> String {
    let mut buf = [0u8; 64];
    match rustix::fs::fgetxattr(fd, c"security.capability", &mut buf) {
        Ok(n) => {
            let magic = u32::from_le_bytes(buf[..4].try_into().unwrap_or_default());
            match n {
                24 => format!(
                    "{n}B {magic:#010x} root {}",
                    u32::from_le_bytes(buf[20..24].try_into().unwrap())
                ),
                _ => format!("{n}B {magic:#010x}"),
            }
        }
        Err(e) => format!("none ({e})"),
    }
}

/// One file per [`capability_forms`] form in `dir`, owned by `owner`.
fn capability_files(dir: &Path, owner: Option<u32>) -> Vec<fs::File> {
    (0..capability_forms().len())
        .map(|i| {
            let p = dir.join(format!("cap{i}"));
            let f = fs::File::create(&p).unwrap();
            if let Some(id) = owner {
                std::os::unix::fs::fchown(&f, Some(id), Some(id)).unwrap();
            }
            f
        })
        .collect()
}

/// The errno (0: set) of `fsetxattr(security.capability)` with each of
/// `values` on `files`.
fn set_capabilities(files: &[fs::File], values: &[Vec<u8>]) -> Vec<i32> {
    files
        .iter()
        .zip(values)
        .map(|(f, v)| {
            match rustix::fs::fsetxattr(
                f,
                c"security.capability",
                v,
                rustix::fs::XattrFlags::empty(),
            ) {
                Ok(()) => 0,
                Err(e) => e.raw_os_error(),
            }
        })
        .collect()
}

/// [`set_capabilities`] from a child in a new user namespace whose ids 0 to
/// 65535 are `outside` on: the child unshares before its `exec`, waits
/// for this thread to write its maps, sets each value, writes the errnos
/// back, then runs `/bin/true`.
fn set_capabilities_in_a_user_namespace(
    files: Vec<fs::File>,
    values: Vec<Vec<u8>>,
    outside: u32,
) -> Vec<i32> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::process::CommandExt as _;
    const MAX: usize = 16;
    let n = files.len();
    assert!(n <= MAX && values.len() == n);
    let (mut pid_r, pid_w) = std::io::pipe().unwrap();
    let (go_r, mut go_w) = std::io::pipe().unwrap();
    let (mut res_r, res_w) = std::io::pipe().unwrap();
    let maps = std::thread::spawn(move || -> std::io::Result<()> {
        let mut pid = [0; 4];
        pid_r.read_exact(&mut pid)?;
        let pid = i32::from_le_bytes(pid);
        let map = format!("0 {outside} 65536\n");
        fs::write(format!("/proc/{pid}/uid_map"), &map)?;
        fs::write(format!("/proc/{pid}/gid_map"), &map)?;
        go_w.write_all(&[1])
    });
    let mut cmd = std::process::Command::new("/bin/true");
    let mut errnos = [0i32; MAX];
    // SAFETY: the closure runs in the forked child, single-threaded, before
    // its `exec`, and only makes system calls on what it owns, allocating
    // nothing.
    unsafe {
        cmd.pre_exec(move || {
            rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWUSER)?;
            let pid = rustix::process::getpid().as_raw_nonzero().get();
            rustix::io::write(&pid_w, &pid.to_le_bytes())?;
            let mut go = [0; 1];
            if rustix::io::read(&go_r, &mut go)? != 1 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            for (i, (f, v)) in files.iter().zip(&values).enumerate() {
                let r = rustix::fs::fsetxattr(
                    f,
                    c"security.capability",
                    v,
                    rustix::fs::XattrFlags::empty(),
                );
                errnos[i] = r.map_or_else(|e| e.raw_os_error(), |()| 0);
            }
            let mut out = [0u8; 4 * MAX];
            for (chunk, e) in out.as_chunks_mut::<4>().0.iter_mut().zip(errnos) {
                *chunk = e.to_le_bytes();
            }
            rustix::io::write(&res_w, &out)?;
            Ok(())
        });
    }
    let status = cmd.status();
    // Closes this process's ends the closure held, so a child that failed
    // early ends the map writer's read.
    drop(cmd);
    let maps = maps.join().unwrap();
    assert!(status.as_ref().is_ok_and(|s| s.success()), "{status:?}");
    maps.unwrap();
    let mut out = [0u8; 4 * MAX];
    res_r.read_exact(&mut out).unwrap();
    out.as_chunks::<4>()
        .0
        .iter()
        .take(n)
        .map(|c| i32::from_le_bytes(*c))
        .collect()
}

/// The `measured:` line of one form: whether the check takes it, and per
/// context the errno (0: set) and what was read back.
fn capability_row(what: &str, check: bool, cells: &[(&str, i32, String)]) -> String {
    let mut line = format!(
        "security.capability {what:<20} check {:<6}",
        if check { "takes" } else { "refuses" }
    );
    for (ctx, errno, back) in cells {
        let r = match *errno {
            0 => "set".to_owned(),
            e => Errno::from_raw_os_error(e).to_string(),
        };
        line.push_str(&format!(" | {ctx}: {r}, read back {back}"));
    }
    line
}

/// Red team (#179, T26), T2: the kernel's own verdict on each form of
/// `security.capability` matches [`check_headers`]'s. As root of the
/// initial user namespace (kaniko mode on `/`), it sets exactly the forms
/// the check takes. In a user namespace on a filesystem it does not own,
/// it refuses every form the check refuses, and besides a revision 3 whose
/// root id the namespace does not map (here 70000 and -2), which the check
/// takes: only -1 is unmapped everywhere. Each form's row is a `measured:`
/// line.
#[test]
#[ignore = "T2: sets file capabilities as root of the initial user namespace and of a new one"]
fn red_team_the_kernel_sets_the_file_capabilities_the_check_takes() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    const OUTSIDE: u32 = 200_000;
    let forms = capability_forms();
    let values: Vec<Vec<u8>> = forms.iter().map(|(_, v)| v.clone()).collect();
    let tmp = tempfile::tempdir().unwrap();
    let fs_type = rustix::fs::statfs(tmp.path()).unwrap().f_type;
    measured(&format!(
        "security.capability: kernel {}, filesystem type {fs_type:#x}",
        rustix::system::uname().release().to_string_lossy()
    ));
    let init_dir = tmp.path().join("init");
    fs::create_dir(&init_dir).unwrap();
    let init_files = capability_files(&init_dir, None);
    let init = set_capabilities(&init_files, &values);
    let ns_dir = tmp.path().join("ns");
    fs::create_dir(&ns_dir).unwrap();
    let ns_files = capability_files(&ns_dir, Some(OUTSIDE));
    let reopen: Vec<fs::File> = ns_files.iter().map(|f| f.try_clone().unwrap()).collect();
    let ns = set_capabilities_in_a_user_namespace(ns_files, values.clone(), OUTSIDE);

    let mut takes_unmapped = Vec::new();
    for (i, (what, value)) in forms.iter().enumerate() {
        let check = check_takes_capability(value);
        assert_eq!(
            check,
            value.is_empty() || is_file_capability(value),
            "{what}"
        );
        measured(&capability_row(
            what,
            check,
            &[
                ("root", init[i], capability_read_back(&init_files[i])),
                ("user namespace", ns[i], capability_read_back(&reopen[i])),
            ],
        ));
        assert_eq!(init[i] == 0, check, "{what}: as root, errno {}", init[i]);
        if !check {
            assert_ne!(
                ns[i], 0,
                "{what}: the check refuses what a user namespace sets"
            );
        } else if ns[i] != 0 {
            takes_unmapped.push(*what);
        }
    }
    assert_eq!(takes_unmapped, ["v3 root 70000", "v3 root -2"]);
}

/// Red team (#179, T26), T1: as root of a rootless Podman container, on its
/// tmpfs (a filesystem its user namespace owns), the kernel refuses every
/// form of `security.capability` [`check_headers`] refuses, and sets every
/// other but a revision 3 whose root id the namespace does not map (70000
/// and -2; the container maps 0 to 65535). Each form's row is a `measured:`
/// line.
#[test]
#[ignore = "T1: default: sets file capabilities as root of a rootless Podman container"]
fn red_team_root_of_a_user_namespace_sets_the_file_capabilities_the_check_takes() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of the container"
    );
    let forms = capability_forms();
    let values: Vec<Vec<u8>> = forms.iter().map(|(_, v)| v.clone()).collect();
    let tmp = tempfile::tempdir().unwrap();
    let fs_type = rustix::fs::statfs(tmp.path()).unwrap().f_type;
    measured(&format!(
        "security.capability: kernel {}, filesystem type {fs_type:#x}",
        rustix::system::uname().release().to_string_lossy()
    ));
    let files = capability_files(tmp.path(), None);
    let ns = set_capabilities(&files, &values);
    let mut takes_unmapped = Vec::new();
    for (i, (what, value)) in forms.iter().enumerate() {
        let check = check_takes_capability(value);
        measured(&capability_row(
            what,
            check,
            &[("container root", ns[i], capability_read_back(&files[i]))],
        ));
        if !check {
            assert_ne!(
                ns[i], 0,
                "{what}: the check refuses what the container's root sets"
            );
        } else if ns[i] != 0 {
            takes_unmapped.push(*what);
        }
    }
    assert_eq!(takes_unmapped, ["v3 root 70000", "v3 root -2"]);
}
