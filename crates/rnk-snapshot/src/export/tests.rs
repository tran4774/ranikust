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

    /// Sets `rel`'s modification time, not following a symlink: a file
    /// extracted from a tar has whole seconds (`nanos` 0).
    fn mtime(&self, rel: &str, secs: i64, nanos: i64) {
        let t = rustix::fs::Timespec {
            tv_sec: secs,
            tv_nsec: nanos,
        };
        rustix::fs::utimensat(
            CWD,
            self.p(rel),
            &rustix::fs::Timestamps {
                last_access: t,
                last_modification: t,
            },
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
    }

    /// `rel` in `l1` and in `upper` with these bodies, the same mode and
    /// the modification time `secs`.`nanos`: a copy-up.
    fn pair(&self, rel: &str, lower: &[u8], upper: &[u8], secs: i64, nanos: i64) {
        self.file(&format!("l1/{rel}"), lower);
        self.file(&format!("upper/{rel}"), upper);
        self.mtime(&format!("l1/{rel}"), secs, nanos);
        self.mtime(&format!("upper/{rel}"), secs, nanos);
    }

    /// The diff over `l1`, `l2` as extracted image layers.
    fn diff(&self) -> LayerDiff {
        self.diff_over(&["l1", "l2"], 0)
    }

    /// The diff over `lowers` (topmost first, made if missing), the first
    /// `built` of them diffs of this build.
    fn diff_over(&self, lowers: &[&str], built: usize) -> LayerDiff {
        for l in lowers {
            fs::create_dir_all(self.p(l)).unwrap();
        }
        LayerDiff {
            upper: self.p("upper"),
            lowers: lowers.iter().map(|l| self.p(l)).collect(),
            built,
        }
    }

    fn names_over(&self, lowers: &[&str], built: usize) -> Vec<String> {
        self.diff_over(lowers, built)
            .entries()
            .unwrap()
            .into_iter()
            .map(|e| e.path.to_string())
            .collect()
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
    f.whiteout("upper/gone");
    f.whiteout("upper/opq/inside");
    // After the whiteout inside it, which sets the directory's mtime.
    f.copy_meta("opq", "l1");
    assert!(f.entries().is_empty(), "{:?}", f.entries());
}

#[test]
fn an_opaque_dir_is_written_as_whiteouts_of_what_it_hides() {
    // As BuildKit: no `.wh..wh..opq`, but a walk of the lower directory
    // against the upper one (continuity's double-walking differ).
    let f = Fixture::new();
    f.file("l2/d/old", b"x");
    f.file("l2/d/sub/deep", b"x");
    f.file("l2/d/hidden", b"x");
    f.dir("l1/d");
    f.whiteout("l1/d/hidden");
    f.pair("d/keep", b"same", b"same", 1_000, 0);
    fs::rename(f.p("l1/d/keep"), f.p("l2/d/keep")).unwrap();
    f.file("upper/d/new", b"n");
    f.xattr("upper/d", OPAQUE, b"y");
    f.dir("upper/fresh");
    f.xattr("upper/fresh", OPAQUE, b"y");
    let e = f.entries();
    // `d` keeps its mode and owner: unchanged, so written only as a
    // parent; `d/sub` is one whiteout for the whole directory; `hidden`
    // was already gone below; `keep` is unchanged.
    assert_eq!(names(&e), ["d/new", "d/old", "d/sub", "fresh"]);
    assert_eq!(e[0].1, EntryKind::File { size: 1 });
    assert_eq!(e[1].1, EntryKind::Whiteout);
    assert_eq!(e[2].1, EntryKind::Whiteout);
    assert_eq!(e[3].1, EntryKind::Dir);
}

#[test]
fn below_a_name_the_lowers_show_as_a_non_dir_nothing_is_compared() {
    // `l1/t` is a file hiding `l2/t/x` and `l2/t/y`: the new directory
    // `upper/t` needs no whiteout for `x`, and its `y` is new, not a
    // copy-up of the hidden one.
    let f = Fixture::new();
    f.pair("t", b"file", b"file", 1_000, 0);
    fs::remove_file(f.p("upper/t")).unwrap();
    f.file("l2/t/x", b"x");
    f.file("l2/t/y", b"same");
    f.mtime("l2/t/y", 1_000, 0);
    f.file("upper/t/y", b"same");
    f.mtime("upper/t/y", 1_000, 0);
    f.whiteout("upper/t/x");
    assert_eq!(names(&f.entries()), ["t", "t/y"]);
}

#[test]
fn over_a_diff_of_this_build_equal_nanoseconds_still_compare_contents() {
    // Two `COPY`s of same-size files within one clock tick, or `cp -a`:
    // the stat is identical to the nanosecond, the content is not.
    let f = Fixture::new();
    f.pair("app/f", b"one", b"two", 1_000, 7);
    f.pair("app/g", b"same", b"same", 1_000, 7);
    // `l1` made by this build: compared, `f` kept, `g` dropped.
    assert_eq!(f.names_over(&["l1", "l2"], 1), ["app/f"]);
    // `l1` an image layer: its nanoseconds are trusted, as BuildKit.
    assert!(f.names_over(&["l1", "l2"], 0).is_empty());
}

#[test]
fn a_deep_stack_with_an_opaque_middle_layer_and_whiteouts_below() {
    // l1/d has x; l2/d is opaque with y, and whites out w; l3/d has z and
    // w, both hidden by l2. The new opaque upper/d hides x and y only.
    let f = Fixture::new();
    f.file("l1/d/x", b"x");
    f.file("l2/d/y", b"y");
    f.xattr("l2/d", OPAQUE, b"y");
    f.whiteout("l2/d/w");
    f.file("l3/d/z", b"z");
    f.file("l3/d/w", b"w");
    f.dir("l4/d/deep/er");
    f.dir("upper/d");
    f.xattr("upper/d", OPAQUE, b"y");
    let got = f.names_over(&["l1", "l2", "l3", "l4"], 0);
    assert_eq!(got, ["d/x", "d/y"]);
    // A whiteout below the opaque path, in a middle layer, hides a name of
    // the layers below it even without an opaque upper.
    let g = Fixture::new();
    g.file("l3/e/gone", b"g");
    g.dir("l2/e");
    g.whiteout("l2/e/gone");
    g.dir("upper/e");
    g.whiteout("upper/e/gone");
    assert!(g.names_over(&["l1", "l2", "l3"], 0).is_empty());
}

#[test]
fn a_symlink_in_a_lower_is_never_followed() {
    // `l1/d` is a symlink looping on itself, hiding `l2/d/q`: the opaque
    // upper `d` is written as new, with no whiteout and no ELOOP.
    let f = Fixture::new();
    symlink("d", f.p("l1/d")).unwrap();
    f.file("l2/d/q", b"q");
    f.file("upper/d/new", b"n");
    f.xattr("upper/d", OPAQUE, b"y");
    assert_eq!(f.names_over(&["l1", "l2"], 0), ["d", "d/new"]);
}

#[test]
fn a_redirect_under_an_opaque_dir_is_an_error() {
    let f = Fixture::new();
    f.dir("l1/d");
    f.dir("upper/d/moved");
    f.xattr("upper/d", OPAQUE, b"y");
    f.xattr("upper/d/moved", REDIRECT, b"/old");
    let e = f.diff().entries().unwrap_err();
    assert!(matches!(e, Error::Export { .. }), "{e}");
}

#[test]
fn an_unchanged_copied_up_file_is_dropped() {
    let f = Fixture::new();
    f.pair("etc/f", b"abc", b"abc", 1_000, 0);
    f.pair("etc/ns", b"old", b"new", 1_000, 5);
    assert!(f.entries().is_empty(), "{:?}", f.entries());
}

#[test]
fn equal_stat_with_whole_seconds_compares_contents() {
    let f = Fixture::new();
    f.pair("same", b"abc", b"abc", 1_000, 0);
    f.pair("changed", b"abc", b"xyz", 1_000, 0);
    assert_eq!(names(&f.entries()), ["changed"]);
}

#[test]
fn nanoseconds_decide_when_not_whole_seconds() {
    let f = Fixture::new();
    // Same seconds and nanoseconds: trusted as unchanged, unread.
    f.pair("trusted", b"abc", b"xyz", 1_000, 7);
    // One side truncated, the other not: changed.
    f.pair("half", b"abc", b"abc", 1_000, 0);
    f.mtime("upper/half", 1_000, 7);
    f.pair("other_ns", b"abc", b"abc", 1_000, 7);
    f.mtime("upper/other_ns", 1_000, 8);
    f.pair("other_s", b"abc", b"abc", 1_000, 0);
    f.mtime("upper/other_s", 1_001, 0);
    f.pair("size", b"abc", b"abcd", 1_000, 0);
    assert_eq!(names(&f.entries()), ["half", "other_ns", "other_s", "size"]);
}

#[test]
fn symlinks_compare_targets_and_empty_files_are_equal() {
    let f = Fixture::new();
    symlink("aaa", f.p("l1/s1")).unwrap();
    symlink("aaa", f.p("upper/s1")).unwrap();
    symlink("aaa", f.p("l1/s2")).unwrap();
    symlink("bbb", f.p("upper/s2")).unwrap();
    for s in ["s1", "s2"] {
        f.mtime(&format!("l1/{s}"), 1_000, 0);
        f.mtime(&format!("upper/{s}"), 1_000, 0);
    }
    f.pair("empty", b"", b"", 1_000, 0);
    let e = f.entries();
    assert_eq!(names(&e), ["s2"]);
    assert_eq!(e[0].1, EntryKind::Symlink(b"bbb".to_vec()));
}

#[test]
fn a_changed_type_or_mode_is_kept() {
    let f = Fixture::new();
    f.pair("mode", b"abc", b"abc", 1_000, 0);
    fs::set_permissions(f.p("upper/mode"), fs::Permissions::from_mode(0o600)).unwrap();
    f.mtime("upper/mode", 1_000, 0);
    f.file("l1/t", b"");
    f.mtime("l1/t", 1_000, 0);
    symlink("x", f.p("upper/t")).unwrap();
    f.mtime("upper/t", 1_000, 0);
    assert_eq!(names(&f.entries()), ["mode", "t"]);
}

#[test]
fn a_directory_is_compared_without_its_mtime() {
    // BuildKit's `sameDirent` checks size and times only for non-dirs.
    let f = Fixture::new();
    f.dir("l1/d");
    f.dir("upper/d");
    f.mtime("l1/d", 1_000, 0);
    f.mtime("upper/d", 2_000, 5);
    assert!(f.entries().is_empty(), "{:?}", f.entries());
}

#[test]
fn capabilities_take_part_in_the_comparison() {
    // `security.capability` needs CAP_SETFCAP to set, so compare stats.
    let f = Fixture::new();
    f.file("upper/f", b"abc");
    let st = RootPath::open(f.p("upper")).unwrap().statx("f").unwrap();
    let cap = |v: &[u8]| vec![(OsString::from("security.capability"), v.to_vec())];
    assert!(meta_equal(&st, &st, &cap(b"a"), &cap(b"a")));
    assert!(!meta_equal(&st, &st, &cap(b"a"), &cap(b"b")));
    assert!(!meta_equal(&st, &st, &cap(b"a"), &[]));
}

#[test]
fn unchanged_hardlinks_are_dropped_outside_opaque_dirs() {
    // BuildKit's overlay differ drops them whatever their link count.
    let f = Fixture::new();
    f.pair("h1", b"same", b"same", 1_000, 0);
    fs::hard_link(f.p("upper/h1"), f.p("upper/h2")).unwrap();
    f.file("l1/h2", b"same");
    f.mtime("l1/h2", 1_000, 0);
    assert!(f.entries().is_empty(), "{:?}", f.entries());
}

#[test]
fn under_an_opaque_dir_an_unchanged_hardlink_links_to_a_changed_one() {
    // continuity's `ChangeKindUnmodified`: kept only to link to a changed
    // name of its inode.
    let f = Fixture::new();
    f.dir("l1/d");
    f.file("l1/d/h1", b"aaa");
    f.mtime("l1/d/h1", 1_000, 0);
    f.file("l1/d/h2", b"bbb");
    f.mtime("l1/d/h2", 1_000, 0);
    f.file("l1/d/lone", b"ccc");
    f.mtime("l1/d/lone", 1_000, 0);
    f.file("upper/d/h1", b"aaa");
    f.mtime("upper/d/h1", 1_000, 0);
    fs::hard_link(f.p("upper/d/h1"), f.p("upper/d/h2")).unwrap();
    f.file("upper/d/lone", b"ccc");
    f.mtime("upper/d/lone", 1_000, 0);
    f.xattr("upper/d", OPAQUE, b"y");
    let e = f.diff().entries().unwrap();
    let got: Vec<_> = e.iter().map(|x| x.path.to_string()).collect();
    assert_eq!(got, ["d/h1", "d/h2"]);
    assert!(e[0].inode.is_some());
    assert_eq!(e[0].inode, e[1].inode);
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
            unchanged: 1,
            bytes_compared: 0,
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
