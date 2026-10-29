use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};
use std::sync::atomic::{AtomicBool, Ordering};

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
#[ignore = "T1: needs a bind mount and a tmpfs inside the root (mount namespace)"]
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
#[ignore = "T1: chown to foreign ids needs a user namespace with an id map"]
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

/// Run by the T2 harness in the VM: device nodes need `CAP_MKNOD` in the
/// initial user namespace.
#[test]
#[ignore = "T2: mknod of a real device needs CAP_MKNOD"]
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
    extract_overlay(&f.root, t.bytes().as_slice(), &[&lower.root]).unwrap();

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
    extract_overlay(&f.root, t.bytes().as_slice(), &[&top.root, &bottom.root]).unwrap();
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
    let e = extract_overlay(&f.root, t.bytes().as_slice(), &[&top.root, &bottom.root]).unwrap_err();
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
        let e = extract_overlay(&f.root, t.bytes().as_slice(), &[&lower.root]).unwrap_err();
        assert!(matches!(e, Error::Entry { .. }), "{target}: {e}");
        assert!(!f.host("h").exists(), "{target}");
    }
    // A symlink target is copied as a symlink, never followed.
    let mut t = Tar::new(&f);
    t.hardlink("h", "file");
    extract_overlay(&f.root, t.bytes().as_slice(), &[&lower.root]).unwrap();
    assert_eq!(fs::read_link(f.host("h")).unwrap(), out.join("sentinel"));
    assert_eq!(snapshot(&out), before);
}
