//! The exporter on plain directories standing for an upper directory and
//! its lower stack: no mount needed. Whiteouts (`mknod c 0 0`) and
//! `user.*` xattrs need no privilege on Linux 5.8 and later.

use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

use rnk_layer::Compression;
use rustix::fs::{CWD, FileType as RawType, Mode, XattrFlags};

use super::*;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let f = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        for d in ["upper", "l1", "l2"] {
            fs::create_dir(f.p(d)).unwrap();
        }
        f
    }

    fn p(&self, rel: &str) -> std::path::PathBuf {
        self.dir.path().join(rel)
    }

    fn file(&self, rel: &str, body: &[u8]) {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn dir(&self, rel: &str) {
        fs::create_dir_all(self.p(rel)).unwrap();
    }

    fn whiteout(&self, rel: &str) {
        rustix::fs::mknodat(CWD, self.p(rel), RawType::CharacterDevice, Mode::empty(), 0).unwrap();
    }

    fn xattr(&self, rel: &str, name: &str, value: &[u8]) {
        rustix::fs::setxattr(self.p(rel), name, value, XattrFlags::empty()).unwrap();
    }

    /// Copies `rel`'s mode and times from `l1` to `upper`, as a copy-up.
    fn copy_meta(&self, rel: &str, from: &str) {
        let m = fs::metadata(self.p(&format!("{from}/{rel}"))).unwrap();
        let to = self.p(&format!("upper/{rel}"));
        fs::set_permissions(&to, fs::Permissions::from_mode(m.mode())).unwrap();
        let t = |s: i64, ns: i64| rustix::fs::Timespec {
            tv_sec: s,
            tv_nsec: ns,
        };
        rustix::fs::utimensat(
            CWD,
            &to,
            &rustix::fs::Timestamps {
                last_access: t(m.atime(), m.atime_nsec()),
                last_modification: t(m.mtime(), m.mtime_nsec()),
            },
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
    }

    fn diff(&self) -> LayerDiff {
        LayerDiff {
            upper: self.p("upper"),
            lowers: vec![self.p("l1"), self.p("l2")],
        }
    }

    fn entries(&self) -> Vec<(String, EntryKind)> {
        self.diff()
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| (e.path.to_string(), e.kind))
            .collect()
    }
}

fn names(entries: &[(String, EntryKind)]) -> Vec<&str> {
    entries.iter().map(|(p, _)| p.as_str()).collect()
}

#[test]
fn new_files_symlinks_and_dirs_are_kept() {
    let f = Fixture::new();
    f.file("upper/a/b", b"hi");
    symlink("b", f.p("upper/a/link")).unwrap();
    let e = f.entries();
    assert_eq!(names(&e), ["a", "a/b", "a/link"]);
    assert_eq!(e[1].1, EntryKind::File { size: 2 });
    assert_eq!(e[2].1, EntryKind::Symlink(b"b".to_vec()));
}

#[test]
fn a_whiteout_is_written_only_where_a_lower_has_the_name() {
    let f = Fixture::new();
    f.file("l2/etc/old", b"x");
    f.dir("l1/etc");
    f.dir("upper/etc");
    f.whiteout("upper/etc/old");
    f.whiteout("upper/etc/never");
    f.copy_meta("etc", "l1");
    let e = f.entries();
    // `etc` is an unchanged copy-up: only its whiteout is written; the
    // writer adds `etc` back as a parent.
    assert_eq!(names(&e), ["etc/old"]);
    assert_eq!(e[0].1, EntryKind::Whiteout);
}

#[test]
fn a_name_hidden_in_a_higher_lower_needs_no_whiteout() {
    let f = Fixture::new();
    f.file("l2/gone", b"x");
    f.whiteout("l1/gone");
    f.file("l2/opq/inside", b"x");
    f.dir("l1/opq");
    f.xattr("l1/opq", OPAQUE, b"y");
    f.dir("upper/opq");
    f.copy_meta("opq", "l1");
    f.whiteout("upper/gone");
    f.whiteout("upper/opq/inside");
    assert!(f.entries().is_empty(), "{:?}", f.entries());
}

#[test]
fn an_opaque_dir_is_opaque_only_over_a_lower_dir() {
    let f = Fixture::new();
    f.file("l2/d/old", b"x");
    f.dir("upper/d");
    f.xattr("upper/d", OPAQUE, b"y");
    f.dir("upper/fresh");
    f.xattr("upper/fresh", OPAQUE, b"y");
    let e = f.entries();
    assert_eq!(names(&e), ["d", "fresh"]);
    assert_eq!(e[0].1, EntryKind::OpaqueDir);
    assert_eq!(e[1].1, EntryKind::Dir);
}

#[test]
fn a_copied_up_dir_with_changed_metadata_is_written() {
    let f = Fixture::new();
    f.dir("l1/etc");
    f.dir("upper/etc");
    f.copy_meta("etc", "l1");
    fs::set_permissions(f.p("upper/etc"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(names(&f.entries()), ["etc"]);
}

#[test]
fn only_security_capability_reaches_a_layer() {
    // `security.capability` needs CAP_SETFCAP to set; `layer_xattrs`'s own
    // test covers it being kept.
    let f = Fixture::new();
    f.file("upper/f", b"");
    f.xattr("upper/f", "user.overlay.origin", b"x");
    f.xattr("upper/f", "user.overlay.overlay.mine", b"m");
    f.xattr("upper/f", "user.other", b"k");
    let diff = f.diff();
    let e = diff.entries().unwrap();
    assert!(e[0].xattrs.is_empty(), "{:?}", e[0].xattrs);
}

#[test]
fn a_redirect_is_an_error() {
    let f = Fixture::new();
    f.dir("upper/moved");
    f.xattr("upper/moved", REDIRECT, b"/old");
    let e = f.diff().entries().unwrap_err();
    assert!(matches!(e, Error::Export { .. }), "{e}");
}

#[test]
fn whiteouts_sharing_an_inode_are_not_hardlinks_but_files_are() {
    let f = Fixture::new();
    f.file("l1/x", b"");
    f.file("l1/y", b"");
    f.whiteout("upper/x");
    fs::hard_link(f.p("upper/x"), f.p("upper/y")).unwrap();
    f.file("upper/h1", b"same");
    fs::hard_link(f.p("upper/h1"), f.p("upper/h2")).unwrap();
    let e = f.diff().entries().unwrap();
    let by = |p: &str| e.iter().find(|x| x.path.to_string() == p).unwrap();
    assert_eq!(by("x").kind, EntryKind::Whiteout);
    assert_eq!(by("x").inode, None);
    assert_eq!(by("y").inode, None);
    assert!(by("h1").inode.is_some());
    assert_eq!(by("h1").inode, by("h2").inode);
}

#[test]
fn modes_are_raw() {
    let f = Fixture::new();
    f.dir("upper/tmp");
    fs::set_permissions(f.p("upper/tmp"), fs::Permissions::from_mode(0o1777)).unwrap();
    f.file("upper/suid", b"");
    fs::set_permissions(f.p("upper/suid"), fs::Permissions::from_mode(0o4755)).unwrap();
    let e = f.diff().entries().unwrap();
    assert_eq!(e[0].mode & 0o7777, 0o4755);
    assert_eq!(e[1].mode & 0o7777, 0o1777);
}

fn options(threads: usize) -> EncodeOptions {
    EncodeOptions {
        compression: Compression::Gzip,
        level: 6,
        threads: NonZeroUsize::new(threads).unwrap(),
        mtime: Some(0),
    }
}

#[test]
fn export_gives_the_same_bytes_for_any_thread_count_with_parents_from_the_lower() {
    let f = Fixture::new();
    f.dir("l1/root");
    fs::set_permissions(f.p("l1/root"), fs::Permissions::from_mode(0o700)).unwrap();
    f.dir("upper/root");
    f.file("upper/root/.profile", &vec![b'x'; 300_000]);
    f.whiteout("upper/root/never");
    // Made last: adding entries changes the directory's mtime.
    f.copy_meta("root", "l1");
    let mut one = Vec::new();
    let mut many = Vec::new();
    let (d1, counts) = f.diff().export(options(1), &mut one).unwrap();
    let (d8, _) = f.diff().export(options(8), &mut many).unwrap();
    assert_eq!(one, many);
    assert_eq!(d1, d8);
    assert_eq!(
        counts,
        DiffCounts {
            files_added: 1,
            whiteouts: 0,
            opaque_dirs: 0,
            bytes: 300_000,
        }
    );
    // The skipped `root` comes back as a parent, with its real 0700.
    let tar = flate2_free_gunzip(&one);
    let mut ar = tar::Archive::new(tar.as_slice());
    let modes: Vec<(String, u32)> = ar
        .entries()
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.path().unwrap().to_string_lossy().into_owned(),
                e.header().mode().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        modes,
        [
            ("root/".to_owned(), 0o700),
            ("root/.profile".to_owned(), 0o666 & !umask())
        ]
    );
}

fn umask() -> u32 {
    let m = rustix::process::umask(Mode::from_raw_mode(0o022));
    rustix::process::umask(m);
    m.as_raw_mode()
}

/// Decodes a layer blob back to its tar with `rnk-layer`'s own decoder.
fn flate2_free_gunzip(blob: &[u8]) -> Vec<u8> {
    use std::io::Read as _;
    let mut out = Vec::new();
    rnk_layer::Decoder::new(blob)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

#[test]
fn a_tree_deeper_than_path_max_is_refused_clearly() {
    let f = Fixture::new();
    // 2,100 levels of `d/`: 4,200 bytes below the layer root, made by fd so
    // no host path gets that long.
    let mut fd = rustix::fs::open(
        f.p("upper"),
        rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        Mode::empty(),
    )
    .unwrap();
    for _ in 0..2_100 {
        rustix::fs::mkdirat(&fd, "d", Mode::from_raw_mode(0o755)).unwrap();
        fd = rustix::fs::openat(
            &fd,
            "d",
            rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
    }
    let e = f.diff().entries().unwrap_err();
    assert!(
        matches!(e, Error::Export { reason, .. } if reason.contains("PATH_MAX")),
        "{e}"
    );
}

#[test]
fn is_empty_tells_an_unchanged_upper() {
    let f = Fixture::new();
    assert!(f.diff().is_empty().unwrap());
    f.file("upper/x", b"");
    assert!(!f.diff().is_empty().unwrap());
}
