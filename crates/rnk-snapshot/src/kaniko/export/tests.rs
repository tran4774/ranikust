use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::PathBuf;

use proptest::prelude::*;
use rnk_layer::Compression;
use rnk_rootfs::{IgnoreList, Whiteouts};

use super::*;
use crate::kaniko::{SnapshotMode, WalkOptions, Walker};

struct Fixture {
    _tmp: tempfile::TempDir,
    host: PathBuf,
    root: RootPath,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let host = fs::canonicalize(tmp.path()).unwrap().join("root");
        fs::create_dir(&host).unwrap();
        let root = RootPath::open(&host).unwrap();
        Self {
            _tmp: tmp,
            host,
            root,
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.host.join(rel)
    }

    fn write(&self, rel: &str, data: &[u8]) {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }
}

fn walker(workers: usize) -> Walker {
    Walker::new(WalkOptions {
        workers: NonZeroUsize::new(workers).unwrap(),
        mode: SnapshotMode::Full,
    })
    .unwrap()
}

fn options() -> EncodeOptions {
    EncodeOptions {
        compression: Compression::None,
        level: 0,
        threads: NonZeroUsize::MIN,
        mtime: None,
    }
}

/// The diff since the baseline after `change` ran on the root.
fn diff_after(f: &Fixture, change: impl FnOnce(&Fixture)) -> WalkDiff {
    let mut w = walker(4);
    w.baseline(&f.root, &IgnoreList::new()).unwrap();
    change(f);
    w.diff(&f.root, &IgnoreList::new()).unwrap()
}

fn layer(f: &Fixture, d: &WalkDiff) -> (Vec<u8>, LayerDigests, DiffCounts) {
    let mut out = Vec::new();
    let (digests, counts) = d.export(&f.root, options(), &mut out).unwrap();
    (out, digests, counts)
}

/// The tar's entries: name, type flag, link target.
fn tar_entries(bytes: &[u8]) -> Vec<(String, u8, Option<String>)> {
    let mut a = tar::Archive::new(bytes);
    a.entries()
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let h = e.header();
            (
                String::from_utf8_lossy(&e.path_bytes()).into_owned(),
                h.entry_type().as_byte(),
                h.link_name()
                    .unwrap()
                    .map(|l| l.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

fn names(bytes: &[u8]) -> Vec<String> {
    tar_entries(bytes).into_iter().map(|e| e.0).collect()
}

#[test]
fn an_unchanged_root_gives_an_empty_layer() {
    let f = Fixture::new();
    f.write("a/b", b"x");
    let d = diff_after(&f, |_| {});
    assert!(d.is_empty());
    let (bytes, _, counts) = layer(&f, &d);
    assert!(names(&bytes).is_empty());
    assert_eq!(bytes.len(), 1024, "an empty tar: two zero blocks");
    assert_eq!(counts, DiffCounts::default());
}

#[test]
fn a_deleted_directory_is_one_whiteout_under_its_parents() {
    let f = Fixture::new();
    f.write("a/b/c/f", b"x");
    f.write("a/b/c/g", b"y");
    f.write("a/b/keep", b"z");
    let d = diff_after(&f, |f| fs::remove_dir_all(f.p("a/b/c")).unwrap());
    let (bytes, _, counts) = layer(&f, &d);
    // `a/b` changed (its mtime); `a` comes back as its parent.
    assert_eq!(names(&bytes), ["a/", "a/b/", "a/b/.wh.c"]);
    assert_eq!((counts.whiteouts, counts.files_added), (1, 1));
}

#[test]
fn a_deep_new_file_brings_its_unchanged_parents() {
    let f = Fixture::new();
    f.write("root/x/y/old", b"o");
    let mode = fs::metadata(f.p("root/x")).unwrap().mode();
    let d = diff_after(&f, |f| f.write("root/x/y/z/new", b"n"));
    let entries = d.entries(&f.root).unwrap();
    let fed: Vec<_> = entries
        .iter()
        .map(|e| String::from_utf8_lossy(e.path.as_bytes()).into_owned())
        .collect();
    assert_eq!(fed, ["root/x/y", "root/x/y/z", "root/x/y/z/new"]);
    let (bytes, _, _) = layer(&f, &d);
    assert_eq!(
        names(&bytes),
        [
            "root/",
            "root/x/",
            "root/x/y/",
            "root/x/y/z/",
            "root/x/y/z/new"
        ]
    );
    let mut a = tar::Archive::new(&bytes[..]);
    let x = a
        .entries()
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.path_bytes().as_ref() == b"root/x/")
        .unwrap();
    assert_eq!(
        x.header().mode().unwrap(),
        mode & 0o7777,
        "read from the root"
    );
}

#[test]
fn a_hardlink_into_a_lower_layer_is_a_regular_file() {
    let f = Fixture::new();
    f.write("etc/passwd", b"root:x:0:0");
    let d = diff_after(&f, |f| fs::hard_link(f.p("etc/passwd"), f.p("x")).unwrap());
    let (bytes, _, _) = layer(&f, &d);
    let entries = tar_entries(&bytes);
    assert_eq!(entries, [("x".to_owned(), b'0', None)]);
    let mut a = tar::Archive::new(&bytes[..]);
    let mut e = a.entries().unwrap().next().unwrap().unwrap();
    let mut content = String::new();
    e.read_to_string(&mut content).unwrap();
    assert_eq!(content, "root:x:0:0");
}

#[test]
fn hardlinks_within_the_layer_are_tar_hardlinks() {
    let f = Fixture::new();
    f.write("old", b"o");
    fs::hard_link(f.p("old"), f.p("old2")).unwrap();
    let d = diff_after(&f, |f| {
        f.write("c", b"c");
        fs::hard_link(f.p("c"), f.p("d")).unwrap();
        // A mode change of a linked inode changes every name of it.
        fs::set_permissions(f.p("old"), fs::Permissions::from_mode(0o600)).unwrap();
    });
    let (bytes, _, _) = layer(&f, &d);
    assert_eq!(
        tar_entries(&bytes),
        [
            ("c".to_owned(), b'0', None),
            ("d".to_owned(), b'1', Some("c".to_owned())),
            ("old".to_owned(), b'0', None),
            ("old2".to_owned(), b'1', Some("old".to_owned())),
        ]
    );
}

#[test]
fn symlinks_and_fifos_are_written_never_followed_or_opened() {
    let f = Fixture::new();
    let d = diff_after(&f, |f| {
        symlink("/etc/passwd", f.p("l")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            f.p("p"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o644),
            0,
        )
        .unwrap();
    });
    let (bytes, _, counts) = layer(&f, &d);
    assert_eq!(
        tar_entries(&bytes),
        [
            ("l".to_owned(), b'2', Some("/etc/passwd".to_owned())),
            ("p".to_owned(), b'6', None),
        ]
    );
    assert_eq!(counts.bytes, 0);
}

#[test]
fn a_file_swapped_since_the_walk_is_refused() {
    let f = Fixture::new();
    let d = diff_after(&f, |f| f.write("f", b"one"));
    // Another inode of the same size: the layer would carry other content
    // than the walk hashed.
    fs::remove_file(f.p("f")).unwrap();
    f.write("f", b"two");
    let e = d.export(&f.root, options(), Vec::new()).unwrap_err();
    assert!(e.to_string().contains("changed between the walk"), "{e}");
}

/// The inode number of a swapped file can come back at once and its size
/// and times can match within one timestamp tick (CI's aarch64 runner did
/// this): the content the walk hashed is checked as the layer is written.
#[test]
fn content_other_than_the_walk_hashed_is_refused() {
    let f = Fixture::new();
    let mut d = diff_after(&f, |f| f.write("f", b"one"));
    let entry = d
        .changed
        .iter_mut()
        .find(|c| c.path == Path::new("f"))
        .expect("f in the diff");
    assert!(entry.meta.content.is_some(), "full mode hashes the content");
    // Same inode, size and times: only the bytes differ from the walk's.
    entry.meta.content = entry.meta.content.map(|h| !h);
    // Raised while the bytes are written, and still the export's own error.
    let e = d.export(&f.root, options(), Vec::new()).unwrap_err();
    assert!(
        matches!(&e, Error::Export { path, .. } if path == Path::new("f")),
        "{e}"
    );
    assert!(e.to_string().contains("changed between the walk"), "{e}");
}

#[test]
fn a_fifo_swapped_in_for_a_file_is_never_opened() {
    let f = Fixture::new();
    let d = diff_after(&f, |f| f.write("f", b"data"));
    fs::remove_file(f.p("f")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        f.p("f"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    // Opening the FIFO for reading would block forever; the check refuses it.
    assert!(d.export(&f.root, options(), Vec::new()).is_err());
}

#[test]
fn a_parent_swapped_for_a_symlink_is_never_followed() {
    let f = Fixture::new();
    f.write("d/f", b"before");
    // Rewritten in place: `d` itself is unchanged, only `d/f` is in the diff.
    let d = diff_after(&f, |f| f.write("d/f", b"walked"));
    assert_eq!(d.changed.len(), 1);
    // `d` now leads, through a symlink, to another file of the same name
    // and size: following it would put other content in the layer.
    fs::rename(f.p("d"), f.p("d0")).unwrap();
    f.write("other/f", b"secret");
    symlink("other", f.p("d")).unwrap();
    for e in [
        d.export(&f.root, options(), Vec::new()).unwrap_err(),
        d.entries(&f.root).unwrap_err(),
    ] {
        // `RESOLVE_NO_SYMLINKS`: ELOOP, nothing opened through the link.
        assert!(
            e.to_string().to_lowercase().contains("symbolic link"),
            "{e}"
        );
    }
}

#[test]
fn names_of_a_reused_inode_are_not_linked() {
    let entry = |p: &[u8], size: u64| Entry {
        path: EntryPath::new(p).unwrap(),
        kind: EntryKind::File { size },
        mode: 0o100_644,
        uid: 0,
        gid: 0,
        mtime: 1,
        xattrs: BTreeMap::new(),
        inode: Some((1, 7)),
    };
    let mut same = vec![entry(b"a", 3), entry(b"b", 3)];
    split_reused_inodes(&mut same, &[Some(5), Some(5)]);
    assert!(same.iter().all(|e| e.inode == Some((1, 7))));
    let mut reused = vec![entry(b"a", 3), entry(b"b", 4), entry(b"c", 3)];
    split_reused_inodes(&mut reused, &[None, None, None]);
    assert!(reused.iter().all(|e| e.inode.is_none()));
    // Same size, mode, owner and second, other content: not one inode.
    let mut other_content = vec![entry(b"a", 3), entry(b"b", 3)];
    split_reused_inodes(&mut other_content, &[Some(5), Some(6)]);
    assert!(other_content.iter().all(|e| e.inode.is_none()));
}

#[test]
fn a_link_target_other_than_the_walk_hashed_is_refused() {
    let f = Fixture::new();
    let mut d = diff_after(&f, |f| symlink("a", f.p("l")).unwrap());
    let entry = d
        .changed
        .iter_mut()
        .find(|c| c.path == Path::new("l"))
        .expect("l in the diff");
    assert!(entry.meta.content.is_some(), "full mode hashes the target");
    entry.meta.content = entry.meta.content.map(|h| !h);
    for e in [
        d.export(&f.root, options(), Vec::new()).unwrap_err(),
        d.entries(&f.root).unwrap_err(),
    ] {
        assert!(e.to_string().contains("changed between the walk"), "{e}");
    }
}

#[test]
fn the_layer_does_not_depend_on_the_walk_worker_count() {
    let f = Fixture::new();
    for i in 0..40 {
        f.write(&format!("d{}/e{}/f{i}", i % 5, i % 3), &[i; 100]);
    }
    let mut one = walker(1);
    let mut eight = walker(8);
    one.baseline(&f.root, &IgnoreList::new()).unwrap();
    eight.baseline(&f.root, &IgnoreList::new()).unwrap();
    for i in 0..40 {
        if i % 4 == 0 {
            f.write(&format!("d{}/e{}/f{i}", i % 5, i % 3), b"changed");
        }
    }
    fs::remove_dir_all(f.p("d1")).unwrap();
    f.write("n/new", b"n");
    let a = one.diff(&f.root, &IgnoreList::new()).unwrap();
    let b = eight.diff(&f.root, &IgnoreList::new()).unwrap();
    assert_eq!(layer(&f, &a).1, layer(&f, &b).1);
}

/// What a tree holds, for comparing a root with a rebuilt one: per path
/// its type and permission bits, size, content or link target, and mtime
/// second. Which names share an inode is left out: a link to a file of an
/// earlier layer is written as a regular file (`ADR-0012`), so the rebuilt
/// tree has the content but not the link.
type Model = BTreeMap<String, (u32, u64, Vec<u8>, i64)>;

fn model(dir: &Path) -> Model {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(dir).unwrap().display().to_string();
            let data = if m.is_file() {
                fs::read(&p).unwrap()
            } else if m.file_type().is_symlink() {
                fs::read_link(&p).unwrap().into_os_string().into_vec()
            } else {
                Vec::new()
            };
            if m.is_dir() {
                todo.push(p.clone());
            }
            let size = if m.is_dir() { 0 } else { m.size() };
            out.insert(rel, (m.mode(), size, data, m.mtime()));
        }
    }
    out
}

/// One change to a test tree.
#[derive(Clone, Debug)]
enum Op {
    Write(usize, u8),
    Mkdir(usize),
    Remove(usize),
    Symlink(usize, usize),
    Link(usize, usize),
    Chmod(usize, u32),
    Fifo(usize),
    Touch(usize, i64),
}

const NAMES: [&str; 8] = ["a", "b", "d/a", "d/b", "d/e/a", "d/e/b", "e", "e/a"];

fn op() -> impl Strategy<Value = Op> {
    let i = 0..NAMES.len();
    prop_oneof![
        3 => (i.clone(), any::<u8>()).prop_map(|(i, b)| Op::Write(i, b)),
        1 => i.clone().prop_map(Op::Mkdir),
        2 => i.clone().prop_map(Op::Remove),
        1 => (i.clone(), i.clone()).prop_map(|(a, b)| Op::Symlink(a, b)),
        1 => (i.clone(), i.clone()).prop_map(|(a, b)| Op::Link(a, b)),
        1 => (i.clone(), prop::sample::select(vec![0o600_u32, 0o640, 0o644, 0o755]))
            .prop_map(|(i, m)| Op::Chmod(i, m)),
        1 => i.clone().prop_map(Op::Fifo),
        1 => (i, 1_000_000_000_i64..1_100_000_000).prop_map(|(i, t)| Op::Touch(i, t)),
    ]
}

/// Applies `op` to the tree at `dir`, skipping what cannot be done there
/// (a parent that is a file, a name already taken).
fn apply_op(dir: &Path, op: &Op) {
    let p = |i: usize| dir.join(NAMES[i]);
    let parents = |q: &Path| fs::create_dir_all(q.parent().unwrap()).is_ok();
    let free = |q: &Path| fs::symlink_metadata(q).is_err();
    // Results are ignored on purpose: an op that does not apply to the
    // current tree is skipped.
    match *op {
        Op::Write(i, b) => {
            let q = p(i);
            if parents(&q) && (free(&q) || fs::symlink_metadata(&q).unwrap().is_file()) {
                let _ = fs::write(&q, vec![b; usize::from(b % 7)]);
            }
        }
        Op::Mkdir(i) => {
            let _ = fs::create_dir_all(p(i));
        }
        Op::Remove(i) => {
            let q = p(i);
            if let Ok(m) = fs::symlink_metadata(&q) {
                let _ = if m.is_dir() {
                    fs::remove_dir_all(&q)
                } else {
                    fs::remove_file(&q)
                };
            }
        }
        Op::Symlink(i, t) => {
            let q = p(i);
            if parents(&q) && free(&q) {
                let _ = symlink(NAMES[t], &q);
            }
        }
        Op::Link(i, t) => {
            let (q, src) = (p(i), p(t));
            if parents(&q) && free(&q) && fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
                let _ = fs::hard_link(&src, &q);
            }
        }
        Op::Chmod(i, m) => {
            let q = p(i);
            if fs::symlink_metadata(&q).is_ok_and(|md| md.is_file()) {
                let _ = fs::set_permissions(&q, fs::Permissions::from_mode(m));
            }
        }
        Op::Fifo(i) => {
            let q = p(i);
            if parents(&q) && free(&q) {
                let _ = rustix::fs::mknodat(
                    rustix::fs::CWD,
                    &q,
                    rustix::fs::FileType::Fifo,
                    rustix::fs::Mode::from_raw_mode(0o644),
                    0,
                );
            }
        }
        Op::Touch(i, t) => {
            let ts = rustix::fs::Timespec {
                tv_sec: t,
                tv_nsec: 0,
            };
            let _ = rustix::fs::utimensat(
                rustix::fs::CWD,
                p(i),
                &rustix::fs::Timestamps {
                    last_access: ts,
                    last_modification: ts,
                },
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// The base layer and the diff's layer, extracted in turn over an
    /// empty directory, rebuild the tree the second walk saw.
    #[test]
    fn applying_the_layers_rebuilds_the_tree(
        base in prop::collection::vec(op(), 0..12),
        change in prop::collection::vec(op(), 0..12),
    ) {
        let f = Fixture::new();
        for o in &base {
            apply_op(&f.host, o);
        }
        let mut w = walker(4);
        let first = w.diff(&f.root, &IgnoreList::new()).unwrap();
        let (layer0, _, _) = layer(&f, &first);
        for o in &change {
            apply_op(&f.host, o);
        }
        let second = w.diff(&f.root, &IgnoreList::new()).unwrap();
        let (layer1, _, _) = layer(&f, &second);

        let out = tempfile::tempdir().unwrap();
        let dst = RootPath::open(fs::canonicalize(out.path()).unwrap()).unwrap();
        rnk_rootfs::extract(&dst, &layer0[..], Whiteouts::Apply).unwrap();
        rnk_rootfs::extract(&dst, &layer1[..], Whiteouts::Apply).unwrap();
        prop_assert_eq!(model(out.path()), model(&f.host));
    }
}
