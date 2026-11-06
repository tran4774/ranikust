//! A cached layer applied to an open kaniko-mode stage root instead of
//! running its step (`design/kaniko-mode.md#a-cache-hit`): per-stage
//! directories (`KanikoStage::apply_layer`) and the shared root
//! (`KanikoRoot::apply_layer`), also as on `/`, in temporary directories
//! (T0), with the red team of what a `RUN` left in the root before the
//! layer; foreign owners and device nodes as root (T2c); a hostile layer on
//! the `/` of a default Docker container (T2).
//!
//! A cached layer is image content anyone who can push to the cache
//! repository wrote: it is extracted under every rule of a base layer, and
//! the walk that follows reports only what the next step changes.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read as _, Write as _};
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use rnk_layer::{Compression, EncodeOptions};
use rnk_oci::Digest;
use rnk_rootfs::{IgnoreList, RootPath};
use rnk_snapshot::kaniko::{IgnoreSpec, MountInfo, ignore_list, parse_mountinfo};
use rnk_snapshot::{
    Base, Error, ExportLayer, Kaniko, KanikoLayer, KanikoRoot, KanikoStage, KanikoStages, Keep,
    LayerOpen, SnapshotMode, Snapshotter, StageRootfs, WalkDiff, WalkOptions,
};
use rnk_store::{BuildPaths, Store};
use sha2::{Digest as _, Sha256};
use tar::{EntryType, Header};

/// The modification time of every hand-made entry.
const T: u64 = 1_000_000;

/// A hand-made plain tar layer; names are written raw, so `..` and
/// whiteouts (`.wh.<name>`, `.wh..wh..opq`) go in as given.
struct Tar {
    out: Vec<u8>,
    uid: u32,
    gid: u32,
}

impl Tar {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
        }
    }

    /// The entries after this one belong to `uid` and `gid`.
    fn owner(&mut self, uid: u32, gid: u32) -> &mut Self {
        self.uid = uid;
        self.gid = gid;
        self
    }

    fn entry(
        &mut self,
        name: &str,
        kind: EntryType,
        link: &str,
        data: &[u8],
        mode: u32,
        dev: (u32, u32),
    ) -> &mut Self {
        let mut h = Header::new_gnu();
        {
            let old = h.as_old_mut();
            old.name[..name.len()].copy_from_slice(name.as_bytes());
            old.linkname[..link.len()].copy_from_slice(link.as_bytes());
        }
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_uid(u64::from(self.uid));
        h.set_gid(u64::from(self.gid));
        h.set_mtime(T);
        h.set_size(u64::try_from(data.len()).unwrap());
        h.set_device_major(dev.0).unwrap();
        h.set_device_minor(dev.1).unwrap();
        h.set_cksum();
        self.out.extend_from_slice(h.as_bytes());
        self.out.extend_from_slice(data);
        self.out
            .extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
        self
    }

    fn dir(&mut self, name: &str, mode: u32) -> &mut Self {
        self.entry(name, EntryType::Directory, "", b"", mode, (0, 0))
    }

    fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
        self.entry(name, EntryType::Regular, "", data, 0o644, (0, 0))
    }

    fn file_mode(&mut self, name: &str, data: &[u8], mode: u32) -> &mut Self {
        self.entry(name, EntryType::Regular, "", data, mode, (0, 0))
    }

    fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.entry(name, EntryType::Symlink, target, b"", 0o777, (0, 0))
    }

    fn hardlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.entry(name, EntryType::Link, target, b"", 0o644, (0, 0))
    }

    fn node(&mut self, name: &str, kind: EntryType, dev: (u32, u32)) -> &mut Self {
        self.entry(name, kind, "", b"", 0o666, dev)
    }

    /// The tar stream, its end included.
    fn bytes(&self) -> Vec<u8> {
        let mut out = self.out.clone();
        out.extend(std::iter::repeat_n(0, 1024));
        out
    }
}

/// The diff ID of a plain tar.
fn diff_id(tar: &[u8]) -> Digest {
    let hash: [u8; 32] = Sha256::digest(tar).into();
    Digest::from_sha256_bytes(&hash)
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bytes).unwrap();
    gz.finish().unwrap()
}

/// `tar` as a layer whose blob is a file of `dir`, as the store keeps one.
fn blob<S: Snapshotter<Layer = KanikoLayer>>(
    snap: &S,
    dir: &Path,
    name: &str,
    tar: &[u8],
) -> KanikoLayer {
    let path = dir.join(name);
    fs::write(&path, tar).unwrap();
    snap.base_layer(&diff_id(tar), &path, &[]).unwrap()
}

/// A layer's compressed bytes as a registry hands them out: opened anew
/// for each extraction, counted.
#[derive(Debug)]
struct Source {
    bytes: Vec<u8>,
    /// Fails the read at the end of the body: a digest the source found
    /// wrong.
    fail_at_end: bool,
    /// Fails the open: the request refused before its first byte.
    refuse: bool,
    opens: AtomicUsize,
}

impl Source {
    fn new(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            bytes,
            fail_at_end: false,
            refuse: false,
            opens: AtomicUsize::new(0),
        })
    }

    fn failing_at_end(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            fail_at_end: true,
            ..Arc::into_inner(Self::new(bytes)).unwrap()
        })
    }

    fn refusing() -> Arc<Self> {
        Arc::new(Self {
            refuse: true,
            ..Arc::into_inner(Self::new(Vec::new())).unwrap()
        })
    }

    fn opens(&self) -> usize {
        self.opens.load(Ordering::Relaxed)
    }
}

/// Fails every read: the source's verdict at the end of the body.
struct DigestMismatch;

impl std::io::Read for DigestMismatch {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("digest mismatch"))
    }
}

impl LayerOpen for Source {
    fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        if self.refuse {
            return Err(std::io::Error::other("refused before its first byte"));
        }
        let body = std::io::Cursor::new(self.bytes.clone());
        if self.fail_at_end {
            return Ok(Box::new(body.chain(DigestMismatch)));
        }
        Ok(Box::new(body))
    }
}

/// `tar` (plain, or compressed as `bytes`) as a layer streamed from
/// `source`, made once per root it goes to, as the builder makes base
/// layers.
fn streamed<S: Snapshotter<Layer = KanikoLayer>>(
    snap: &S,
    tar: &[u8],
    source: &Arc<Source>,
) -> KanikoLayer {
    snap.streamed_layer(&diff_id(tar), Arc::clone(source) as Arc<dyn LayerOpen>)
        .unwrap()
}

fn walk() -> WalkOptions {
    WalkOptions {
        workers: NonZeroUsize::new(2).unwrap(),
        mode: SnapshotMode::Full,
    }
}

/// The two kinds of kaniko-mode stage root, as these tests drive them.
trait Applies: StageRootfs {
    fn apply(&mut self, layer: &KanikoLayer) -> rnk_snapshot::Result<()>;
    fn walked(diff: &Self::Diff) -> &WalkDiff;
}

impl Applies for KanikoStage {
    fn apply(&mut self, layer: &KanikoLayer) -> rnk_snapshot::Result<()> {
        self.apply_layer(layer)
    }

    fn walked(diff: &Self::Diff) -> &WalkDiff {
        diff.diff()
    }
}

impl Applies for KanikoRoot {
    fn apply(&mut self, layer: &KanikoLayer) -> rnk_snapshot::Result<()> {
        self.apply_layer(layer)
    }

    fn walked(diff: &Self::Diff) -> &WalkDiff {
        diff.diff()
    }
}

/// Per-stage directories (ADR-0059) in a store of a temporary directory,
/// `/proc` and `/ignored` on the stage roots' ignore list.
struct Stages {
    _tmp: tempfile::TempDir,
    top: PathBuf,
    snap: KanikoStages,
}

impl Stages {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let top = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir(top.join("blobs")).unwrap();
        let store = Store::open(&BuildPaths::new(&top, top.join("state"))).unwrap();
        let mut ignore = IgnoreList::new();
        ignore.insert("/proc").unwrap();
        ignore.insert("/ignored").unwrap();
        let snap = KanikoStages::new(&store, ignore, walk()).unwrap();
        Self {
            _tmp: tmp,
            top,
            snap,
        }
    }

    fn blobs(&self) -> PathBuf {
        self.top.join("blobs")
    }

    /// `builds/<id>/stages`, where every stage directory is made.
    fn stages_dir(&self) -> PathBuf {
        let builds = self.top.join("state/builds");
        let id = fs::read_dir(&builds).unwrap().next().unwrap().unwrap();
        id.path().join("stages")
    }
}

/// The shared root in a temporary directory: `state/` inside it and on the
/// ignore list, as the state directory is on `/`; with `slash`, the
/// snapshotter is told its root is `/` and keeps stages and spools in an
/// anonymous store on `state/` (ADR-0056).
struct Shared {
    _tmp: tempfile::TempDir,
    top: PathBuf,
    root: PathBuf,
    snap: Kaniko,
    _store: Option<Arc<Store>>,
}

impl Shared {
    fn new(slash: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let top = fs::canonicalize(tmp.path()).unwrap();
        let root = top.join("root");
        fs::create_dir_all(root.join("state")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(top.join("blobs")).unwrap();
        fs::create_dir(top.join("kept")).unwrap();
        let mut ignore = IgnoreList::new();
        for p in ["/state", "/ranikust", "/proc", "/ignored"] {
            ignore.insert(p).unwrap();
        }
        let (shown, kept, store) = if slash {
            let store = Arc::new(Store::open_anonymous(&root.join("state")).unwrap());
            (PathBuf::from("/"), root.join("state"), Some(store))
        } else {
            (root.clone(), top.join("kept"), None)
        };
        let snap = Kaniko::new(
            RootPath::open(&root).unwrap(),
            shown,
            ignore,
            walk(),
            RootPath::open(&kept).unwrap(),
        );
        if let Some(store) = &store {
            snap.keep_in_anonymous_files(Arc::clone(store)).unwrap();
        }
        Self {
            _tmp: tmp,
            top,
            root,
            snap,
            _store: store,
        }
    }

    fn blobs(&self) -> PathBuf {
        self.top.join("blobs")
    }
}

/// `h`, a host path a test writes into a stage root through, checked to be
/// below `top`, the fixture's temporary directory: never `/`, which is what
/// `mount_point()` gives for a shared root told it is `/`.
fn inside(top: &Path, h: PathBuf) -> PathBuf {
    assert!(
        h != Path::new("/") && h.starts_with(top),
        "{} is not below the test's directory {}",
        h.display(),
        top.display()
    );
    h
}

/// Changed and deleted paths of a diff, as strings.
fn changes(d: &WalkDiff) -> (Vec<String>, Vec<String>) {
    (
        d.changed
            .iter()
            .map(|c| c.path.display().to_string())
            .collect(),
        d.deleted.iter().map(|p| p.display().to_string()).collect(),
    )
}

/// One entry of [`flat`].
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: String,
    /// Type and mode bits.
    mode: u32,
    owner: (u32, u32),
    /// Not a directory's.
    size: u64,
    /// In seconds; not the root's, which no layer carries.
    mtime: i64,
    /// Content, link target or device numbers.
    body: Vec<u8>,
    /// The first path of its hardlink group.
    group: String,
}

/// Every entry of the root at `dir` as an image holds it. `state` and
/// `ranikust`, the ignored state and marker directories of the shared
/// root, are left out.
fn flat(dir: &Path) -> Vec<Entry> {
    let mut all = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(dir).unwrap().to_owned();
            if d == dir && (rel == Path::new("state") || rel == Path::new("ranikust")) {
                continue;
            }
            let md = fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                stack.push(p.clone());
            }
            all.push((p, rel, md));
        }
    }
    all.sort_by(|a, b| a.1.cmp(&b.1));
    let root = fs::symlink_metadata(dir).unwrap();
    let mut out = vec![Entry {
        path: String::new(),
        mode: root.mode(),
        owner: (root.uid(), root.gid()),
        size: 0,
        mtime: 0,
        body: Vec::new(),
        group: String::new(),
    }];
    let mut groups = BTreeMap::new();
    for (p, rel, md) in all {
        let rel = rel.display().to_string();
        let body = if md.file_type().is_symlink() {
            fs::read_link(&p).unwrap().as_os_str().as_bytes().to_vec()
        } else if md.is_file() {
            fs::read(&p).unwrap()
        } else {
            md.rdev().to_le_bytes().to_vec()
        };
        let group = groups
            .entry((md.dev(), md.ino()))
            .or_insert_with(|| rel.clone())
            .clone();
        out.push(Entry {
            path: rel,
            mode: md.mode(),
            owner: (md.uid(), md.gid()),
            size: if md.is_dir() { 0 } else { md.len() },
            mtime: md.mtime(),
            body,
            group,
        });
    }
    out
}

fn set_times(p: &Path, secs: i64) {
    let t = rustix::fs::Timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    let times = rustix::fs::Timestamps {
        last_access: t,
        last_modification: t,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        p,
        &times,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

/// `diff` encoded with `compression`, every time clamped when `epoch` is
/// set: the blob and its diff ID.
fn encode<D: ExportLayer>(
    diff: D,
    compression: Compression,
    epoch: Option<i64>,
) -> (Vec<u8>, Digest) {
    let mut out = Vec::new();
    let (digests, _) = diff
        .export(
            EncodeOptions {
                compression,
                level: 1,
                threads: NonZeroUsize::MIN,
                mtime: epoch,
            },
            &mut out,
        )
        .unwrap();
    (out, Digest::from_sha256_bytes(digests.diff_id.as_bytes()))
}

// The next diff after an applied layer.

/// After a walked step, two cached layers applied, then a step that
/// changes nothing (an empty diff) and one that writes a file and deletes
/// one a layer brought (those, and `app`, which the deletion moves): the
/// walk never reports
/// what the layers brought as new.
fn the_next_diff_holds_only_the_next_steps_changes<S>(
    snap: &S,
    blobs: &Path,
    top: &Path,
    host: impl Fn(&S::Root) -> PathBuf,
) where
    S: Snapshotter<Layer = KanikoLayer>,
    S::Root: Applies,
{
    let base = Tar::new().dir("etc", 0o755).file("etc/os", b"base").bytes();
    let one = Tar::new()
        .dir("app", 0o755)
        .file("app/a", b"one")
        .file("etc/os", b"changed")
        .bytes();
    // `app` in it too: extraction sets its time to `T`, so the deletion
    // below moves it whatever the clock's granularity.
    let two = Tar::new().dir("app", 0o755).file("app/b", b"two").bytes();
    let base = blob(snap, blobs, "base", &base);
    let mut root = snap.prepare(Base::Image(&[base])).unwrap();
    root.set_layers_needed(true);
    root.begin_layer().unwrap();
    let d = root.end_layer().unwrap();
    assert!(
        S::Root::walked(&d).is_empty(),
        "{:?}",
        changes(S::Root::walked(&d))
    );
    drop(d);
    root.apply(&blob(snap, blobs, "one", &one)).unwrap();
    root.apply(&blob(snap, blobs, "two", &two)).unwrap();
    let h = inside(top, host(&root));
    assert_eq!(fs::read(h.join("app/a")).unwrap(), b"one");
    assert_eq!(fs::read(h.join("app/b")).unwrap(), b"two");
    assert_eq!(fs::read(h.join("etc/os")).unwrap(), b"changed");
    assert_eq!(
        fs::metadata(h.join("app")).unwrap().mtime(),
        i64::try_from(T).unwrap()
    );

    root.begin_layer().unwrap();
    let d = root.end_layer().unwrap();
    assert!(
        S::Root::walked(&d).is_empty(),
        "{:?}",
        changes(S::Root::walked(&d))
    );
    drop(d);

    root.begin_layer().unwrap();
    fs::write(h.join("step"), b"x").unwrap();
    // What a cached layer brought is in the new baseline: its deletion shows.
    fs::remove_file(h.join("app/a")).unwrap();
    let d = root.end_layer().unwrap();
    assert_eq!(
        changes(S::Root::walked(&d)),
        (
            vec!["app".to_owned(), "step".to_owned()],
            vec!["app/a".to_owned()]
        )
    );
    drop(d);
    root.release().unwrap();
}

#[test]
fn per_stage_the_next_diff_holds_only_the_next_steps_changes() {
    let f = Stages::new();
    the_next_diff_holds_only_the_next_steps_changes(&f.snap, &f.blobs(), &f.top, |r| {
        r.mount_point().to_owned()
    });
}

#[test]
fn shared_root_the_next_diff_holds_only_the_next_steps_changes() {
    for slash in [false, true] {
        let f = Shared::new(slash);
        let root = f.root.clone();
        the_next_diff_holds_only_the_next_steps_changes(&f.snap, &f.blobs(), &f.top, |_| {
            root.clone()
        });
    }
}

/// A stage whose layers are not needed is never walked, hits or not.
#[test]
fn a_stage_whose_layers_are_not_needed_applies_without_a_walk() {
    let f = Stages::new();
    let one = Tar::new().file("a", b"one").bytes();
    let mut s = f.snap.prepare(Base::Scratch).unwrap();
    s.set_layers_needed(false);
    s.apply_layer(&blob(&f.snap, &f.blobs(), "one", &one))
        .unwrap();
    s.begin_layer().unwrap();
    let d = s.end_layer().unwrap();
    assert!(d.diff().is_empty());
    assert_eq!(d.diff().stats.entries, 0, "never walked");
    assert_eq!(fs::read(s.mount_point().join("a")).unwrap(), b"one");
    s.release().unwrap();
}

// Whiteouts and opaque directories.

/// A whiteout and an opaque directory in a cached layer remove the lower
/// entries, and the next diff, after a walk taken before the layer, does
/// not report them again.
fn whiteouts_remove_lower_entries_once<S>(
    snap: &S,
    blobs: &Path,
    top: &Path,
    host: impl Fn(&S::Root) -> PathBuf,
) where
    S: Snapshotter<Layer = KanikoLayer>,
    S::Root: Applies,
{
    let base = Tar::new()
        .file("keep", b"k")
        .file("gone", b"g")
        .dir("dir", 0o755)
        .file("dir/x", b"x")
        .file("dir/y", b"y")
        .dir("opq", 0o755)
        .file("opq/a", b"a")
        .dir("opq/sub", 0o755)
        .file("opq/sub/b", b"b")
        .bytes();
    let cached = Tar::new()
        .file(".wh.gone", b"")
        .file("dir/.wh.x", b"")
        .dir("opq", 0o755)
        .file("opq/.wh..wh..opq", b"")
        .file("opq/new", b"n")
        .bytes();
    let base = blob(snap, blobs, "base", &base);
    let mut root = snap.prepare(Base::Image(&[base])).unwrap();
    root.set_layers_needed(true);
    root.begin_layer().unwrap();
    drop(root.end_layer().unwrap());
    root.apply(&blob(snap, blobs, "cached", &cached)).unwrap();
    let h = inside(top, host(&root));
    assert!(!h.join("gone").exists());
    assert!(!h.join("dir/x").exists());
    assert_eq!(fs::read(h.join("dir/y")).unwrap(), b"y");
    let mut opq: Vec<_> = fs::read_dir(h.join("opq"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    opq.sort();
    assert_eq!(opq, ["new"], "the opaque directory holds only the layer's");
    assert!(!h.join(".wh.gone").exists() && !h.join("opq/.wh..wh..opq").exists());

    root.begin_layer().unwrap();
    let d = root.end_layer().unwrap();
    assert!(
        S::Root::walked(&d).is_empty(),
        "{:?}",
        changes(S::Root::walked(&d))
    );
    drop(d);
    root.begin_layer().unwrap();
    fs::write(h.join("keep"), b"step").unwrap();
    let d = root.end_layer().unwrap();
    assert_eq!(
        changes(S::Root::walked(&d)),
        (vec!["keep".to_owned()], vec![])
    );
    drop(d);
    root.release().unwrap();
}

#[test]
fn per_stage_whiteouts_in_a_cached_layer_remove_lower_entries_once() {
    let f = Stages::new();
    whiteouts_remove_lower_entries_once(&f.snap, &f.blobs(), &f.top, |r| {
        r.mount_point().to_owned()
    });
}

#[test]
fn shared_root_whiteouts_in_a_cached_layer_remove_lower_entries_once() {
    for slash in [false, true] {
        let f = Shared::new(slash);
        let root = f.root.clone();
        whiteouts_remove_lower_entries_once(&f.snap, &f.blobs(), &f.top, |_| root.clone());
    }
}

// The same root built both ways.

/// A base with files, a hardlink pair, a symlink and nested directories.
fn rich_base() -> Vec<u8> {
    Tar::new()
        .dir("etc", 0o755)
        .file("etc/conf", b"base conf")
        .file("etc/drop", b"dropped")
        .dir("usr", 0o755)
        .dir("usr/bin", 0o755)
        .file_mode("usr/bin/tool", b"tool", 0o755)
        .dir("usr/share", 0o755)
        .dir("usr/share/doc", 0o755)
        .file("usr/share/doc/a", b"a")
        .file("usr/share/doc/b", b"b")
        .dir("var", 0o755)
        .dir("var/cache", 0o755)
        .file("var/cache/x", b"cache")
        .symlink("link", "etc/conf")
        .dir("data", 0o755)
        .file("data/hl1", b"linked")
        .hardlink("data/hl2", "data/hl1")
        .bytes()
}

/// A step as a `RUN` would make it: content, mode, deletions of a file and
/// a tree, a directory replaced by a file, new nested directories, a
/// symlink, a hardlink among new files, an empty private directory; every
/// time it sets is in whole seconds.
fn step_one(top: &Path, h: &Path) {
    inside(top, h.to_owned());
    fs::write(h.join("etc/conf"), b"changed conf").unwrap();
    fs::set_permissions(h.join("usr/bin/tool"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(h.join("etc/drop")).unwrap();
    fs::remove_dir_all(h.join("usr/share/doc")).unwrap();
    fs::remove_dir_all(h.join("var/cache")).unwrap();
    fs::write(h.join("var/cache"), b"now a file").unwrap();
    fs::create_dir_all(h.join("opt/app/bin")).unwrap();
    fs::write(h.join("opt/app/bin/run"), b"run").unwrap();
    fs::set_permissions(h.join("opt/app/bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::hard_link(h.join("opt/app/bin/run"), h.join("opt/app/bin/run2")).unwrap();
    std::os::unix::fs::symlink("bin", h.join("opt/app/current")).unwrap();
    fs::create_dir(h.join("opt/empty")).unwrap();
    fs::set_permissions(h.join("opt/empty"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(h.join("data/hl2")).unwrap();
    for (p, t) in [
        ("etc/conf", 2_000_001),
        ("var/cache", 2_000_002),
        ("opt/app/bin/run", 2_000_003),
        ("opt/app/current", 2_000_004),
        ("opt/empty", 2_000_005),
        ("opt/app/bin", 2_000_006),
        ("opt/app", 2_000_007),
        ("opt", 2_000_008),
        ("etc", 2_000_009),
        ("usr/share", 2_000_010),
        ("var", 2_000_011),
        ("data", 2_000_012),
    ] {
        set_times(&h.join(p), t);
    }
}

/// The step after: new content, a file through a hardlink, a removed link.
fn step_two(top: &Path, h: &Path) {
    inside(top, h.to_owned());
    fs::create_dir(h.join("srv")).unwrap();
    fs::write(h.join("srv/new"), b"two").unwrap();
    fs::write(h.join("opt/app/bin/run"), b"run v2").unwrap();
    fs::remove_file(h.join("link")).unwrap();
    // Brought by the cached layer: its deletion must show in the next diff.
    fs::remove_file(h.join("opt/app/current")).unwrap();
}

/// One stage runs `step_one` and exports its layer (gzip); another, on the
/// same base, applies that layer: their roots are the same, and the same
/// next step then gives both the same layer.
fn built_both_ways<S>(
    snap: &S,
    blobs: &Path,
    top: &Path,
    host: impl Fn(&S::Root) -> PathBuf,
    cached: impl Fn(&S, &[u8], Vec<u8>) -> KanikoLayer,
) where
    S: Snapshotter<Layer = KanikoLayer>,
    S::Root: Applies,
{
    let base_tar = rich_base();
    let base = blob(snap, blobs, "base", &base_tar);

    let mut ran = snap
        .prepare(Base::Image(std::slice::from_ref(&base)))
        .unwrap();
    ran.set_layers_needed(true);
    ran.begin_layer().unwrap();
    step_one(top, &host(&ran));
    let (gz, id) = encode(ran.end_layer().unwrap(), Compression::Gzip, None);
    let ran_root = flat(&inside(top, host(&ran)));
    ran.begin_layer().unwrap();
    step_two(top, &host(&ran));
    let (_, ran_next) = encode(ran.end_layer().unwrap(), Compression::None, Some(1));
    ran.release().unwrap();

    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(gz.as_slice())
        .read_to_end(&mut tar)
        .unwrap();
    assert_eq!(diff_id(&tar), id);
    let layer = cached(snap, &tar, gz);
    let mut hit = snap.prepare(Base::Image(&[base])).unwrap();
    hit.set_layers_needed(true);
    hit.apply(&layer).unwrap();
    assert_eq!(
        flat(&inside(top, host(&hit))),
        ran_root,
        "the root the step left"
    );
    hit.begin_layer().unwrap();
    step_two(top, &host(&hit));
    let (_, hit_next) = encode(hit.end_layer().unwrap(), Compression::None, Some(1));
    assert_eq!(hit_next, ran_next, "the next step's layer");
    hit.release().unwrap();
}

#[test]
fn per_stage_an_applied_layer_gives_the_root_its_step_left() {
    let f = Stages::new();
    let blobs = f.blobs();
    built_both_ways(
        &f.snap,
        &f.blobs(),
        &f.top,
        |r| r.mount_point().to_owned(),
        |snap, tar, gz| {
            let path = blobs.join("cached.gz");
            fs::write(&path, gz).unwrap();
            snap.base_layer(&diff_id(tar), &path, &[]).unwrap()
        },
    );
    // Streamed from its registry as it arrives (ADR-0055).
    built_both_ways(
        &f.snap,
        &f.blobs(),
        &f.top,
        |r| r.mount_point().to_owned(),
        |snap, tar, gz| streamed(snap, tar, &Source::new(gz)),
    );
}

#[test]
fn shared_root_an_applied_layer_gives_the_root_its_step_left() {
    for slash in [false, true] {
        let f = Shared::new(slash);
        let root = f.root.clone();
        built_both_ways(
            &f.snap,
            &f.blobs(),
            &f.top,
            |_| root.clone(),
            |snap, tar, gz| streamed(snap, tar, &Source::new(gz)),
        );
    }
}

// Device nodes and FIFOs (ADR-0041).

/// Device nodes and FIFOs in a cached layer are not made, as in a base
/// layer, and the walk never reports them: the image keeps them through
/// the layer reused as it is.
fn nodes_are_left_out<S>(snap: &S, blobs: &Path, top: &Path, host: impl Fn(&S::Root) -> PathBuf)
where
    S: Snapshotter<Layer = KanikoLayer>,
    S::Root: Applies,
{
    let cached = Tar::new()
        .dir("nodes", 0o755)
        .node("nodes/null", EntryType::Char, (1, 3))
        .node("nodes/loop", EntryType::Block, (7, 0))
        .node("nodes/pipe", EntryType::Fifo, (0, 0))
        .file("nodes/file", b"f")
        .bytes();
    let mut root = snap.prepare(Base::Scratch).unwrap();
    root.set_layers_needed(true);
    root.apply(&blob(snap, blobs, "nodes", &cached)).unwrap();
    let names: Vec<_> = fs::read_dir(inside(top, host(&root)).join("nodes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["file"]);
    root.begin_layer().unwrap();
    let d = root.end_layer().unwrap();
    assert!(
        S::Root::walked(&d).is_empty(),
        "{:?}",
        changes(S::Root::walked(&d))
    );
    drop(d);
    root.release().unwrap();
}

#[test]
fn device_nodes_and_fifos_in_a_cached_layer_are_left_out() {
    let f = Stages::new();
    nodes_are_left_out(&f.snap, &f.blobs(), &f.top, |r| r.mount_point().to_owned());
    for slash in [false, true] {
        let f = Shared::new(slash);
        let root = f.root.clone();
        nodes_are_left_out(&f.snap, &f.blobs(), &f.top, |_| root.clone());
    }
}

// Refusals and failures.

#[test]
fn a_layer_is_applied_only_to_a_closed_usable_root() {
    let f = Stages::new();
    let layer = blob(
        &f.snap,
        &f.blobs(),
        "l",
        &Tar::new().file("a", b"a").bytes(),
    );
    let mut s = f.snap.prepare(Base::Scratch).unwrap();
    s.begin_layer().unwrap();
    assert!(matches!(s.apply_layer(&layer), Err(Error::State(_))));
    assert!(!s.mount_point().join("a").exists());
    drop(s.end_layer().unwrap());
    // A root handed to the only stage built on it (ADR-0061).
    let child = f.snap.prepare_last(&s).unwrap();
    assert!(matches!(s.apply_layer(&layer), Err(Error::State(_))));
    assert!(!child.mount_point().join("a").exists());
    child.release().unwrap();
    s.release().unwrap();

    let f = Shared::new(false);
    let layer = blob(
        &f.snap,
        &f.blobs(),
        "l",
        &Tar::new().file("a", b"a").bytes(),
    );
    let mut r = f.snap.prepare(Base::Scratch).unwrap();
    r.begin_layer().unwrap();
    assert!(matches!(r.apply_layer(&layer), Err(Error::State(_))));
    drop(r.end_layer().unwrap());
    // A stage that kept its root gave it up.
    r.keep(&Keep::All).unwrap();
    assert!(matches!(r.apply_layer(&layer), Err(Error::State(_))));
    assert!(!f.root.join("a").exists());
    r.release().unwrap();
}

/// On the shared root a refused layer changes nothing, not even the view of
/// a kept stage the holder's next step reads; the holder's applied layer
/// ends it, as an instruction does.
#[test]
fn shared_root_a_refused_layer_leaves_the_views_of_kept_stages() {
    let f = Shared::new(true);
    let layer = blob(
        &f.snap,
        &f.blobs(),
        "l",
        &Tar::new().file("a", b"a").bytes(),
    );
    let mut kept = f.snap.prepare(Base::Scratch).unwrap();
    kept.begin_layer().unwrap();
    fs::write(f.root.join("k"), b"kept").unwrap();
    drop(kept.end_layer().unwrap());
    kept.keep(&Keep::All).unwrap();
    let mut now = f.snap.prepare(Base::Scratch).unwrap();
    let state = || fs::read_dir(f.root.join("state")).unwrap().count();
    let before = state();
    let names = kept.view().unwrap().read_names("").unwrap();
    assert!(names.iter().any(|n| n == "k"), "{names:?}");
    let with_view = state();
    assert!(
        with_view > before,
        "the view is a directory of the state directory"
    );
    assert!(matches!(kept.apply_layer(&layer), Err(Error::State(_))));
    assert_eq!(state(), with_view, "the refused layer removed no view");
    now.apply_layer(&layer).unwrap();
    assert_eq!(state(), before, "the applied layer ends the view");
    now.release().unwrap();
    kept.release().unwrap();
}

/// A per-stage layer that fails once it may have reached the root (a wrong
/// diff ID found at its end, which leaves the whole layer in it; a digest
/// its source found wrong, which may leave part of it) leaves a root
/// nothing goes on from: no layer, view, child stage or hand-over. Its
/// release still deletes it.
#[test]
fn per_stage_a_layer_that_fails_part_way_breaks_the_root() {
    let f = Stages::new();
    let tar = Tar::new().file("part", b"p").bytes();
    let path = f.blobs().join("wrong");
    fs::write(&path, &tar).unwrap();
    let wrong_id = f
        .snap
        .base_layer(&Digest::from_sha256_bytes(&[0; 32]), &path, &[])
        .unwrap();
    let mismatch = streamed(&f.snap, &tar, &Source::failing_at_end(tar.clone()));
    let good = blob(
        &f.snap,
        &f.blobs(),
        "good",
        &Tar::new().file("g", b"g").bytes(),
    );
    for (bad, whole) in [(wrong_id, true), (mismatch, false)] {
        let mut s = f.snap.prepare(Base::Scratch).unwrap();
        s.set_layers_needed(true);
        assert!(s.apply_layer(&bad).is_err());
        if whole {
            assert!(
                s.mount_point().join("part").exists(),
                "extracted as it came"
            );
        }
        assert!(matches!(s.begin_layer(), Err(Error::State(_))));
        assert!(matches!(s.apply_layer(&good), Err(Error::State(_))));
        assert!(!s.mount_point().join("g").exists());
        assert!(matches!(s.view(), Err(Error::State(_))));
        assert!(f.snap.prepare(Base::Stage(&s)).is_err());
        assert!(f.snap.prepare_last(&s).is_err());
        s.release().unwrap();
    }
    assert_eq!(fs::read_dir(f.stages_dir()).unwrap().count(), 0);
}

/// On the shared root the break lasts until the root is emptied: the next
/// stage on a base starts whole, with none of the failed layer.
#[test]
fn shared_root_a_layer_that_fails_part_way_breaks_it_until_emptied() {
    let f = Shared::new(false);
    let tar = Tar::new().file("part", b"p").bytes();
    let path = f.blobs().join("wrong");
    fs::write(&path, &tar).unwrap();
    let bad = f
        .snap
        .base_layer(&Digest::from_sha256_bytes(&[0; 32]), &path, &[])
        .unwrap();
    let good = blob(
        &f.snap,
        &f.blobs(),
        "good",
        &Tar::new().file("g", b"g").bytes(),
    );
    let mut r = f.snap.prepare(Base::Scratch).unwrap();
    r.set_layers_needed(true);
    assert!(matches!(
        r.apply_layer(&bad),
        Err(Error::DiffIdMismatch { .. })
    ));
    assert!(f.root.join("part").exists());
    assert!(matches!(r.begin_layer(), Err(Error::State(_))));
    assert!(matches!(r.apply_layer(&good), Err(Error::State(_))));
    assert!(matches!(r.view(), Err(Error::State(_))));
    assert!(matches!(r.keep(&Keep::All), Err(Error::State(_))));
    assert!(f.snap.prepare(Base::Stage(&r)).is_err(), "a squashed child");
    r.release().unwrap();
    let mut next = f.snap.prepare(Base::Image(&[good])).unwrap();
    assert!(!f.root.join("part").exists());
    next.set_layers_needed(true);
    next.begin_layer().unwrap();
    assert!(next.end_layer().unwrap().diff().is_empty());
    next.release().unwrap();
    assert!(f.snap.finish().unwrap());
}

/// A layer that fails before any entry can reach the root leaves it as it
/// was, usable: per stage a source that refuses the request, on `/` a
/// streamed layer whose check fails (verified before extraction, ADR-0055).
#[test]
fn a_layer_that_fails_before_its_first_entry_leaves_the_root_usable() {
    let f = Stages::new();
    let tar = Tar::new().file("a", b"a").bytes();
    let mut s = f.snap.prepare(Base::Scratch).unwrap();
    s.set_layers_needed(true);
    let refused = Source::refusing();
    assert!(s.apply_layer(&streamed(&f.snap, &tar, &refused)).is_err());
    assert_eq!(refused.opens(), 1);
    s.begin_layer().unwrap();
    assert!(s.end_layer().unwrap().diff().is_empty());
    s.apply_layer(&streamed(&f.snap, &tar, &Source::new(tar.clone())))
        .unwrap();
    assert!(s.mount_point().join("a").exists());
    s.release().unwrap();

    let f = Shared::new(true);
    let mut r = f.snap.prepare(Base::Scratch).unwrap();
    r.set_layers_needed(true);
    let unverified = Tar::new().file("unverified", b"x").bytes();
    let e = r
        .apply_layer(&streamed(
            &f.snap,
            &unverified,
            &Source::failing_at_end(unverified.clone()),
        ))
        .unwrap_err();
    assert!(format!("{e:?}").contains("digest mismatch"), "{e:?}");
    assert!(!f.root.join("unverified").exists());
    let wrong_id = Source::new(unverified.clone());
    let mislabelled = f
        .snap
        .streamed_layer(
            &Digest::from_sha256_bytes(&[0; 32]),
            Arc::clone(&wrong_id) as Arc<dyn LayerOpen>,
        )
        .unwrap();
    assert!(matches!(
        r.apply_layer(&mislabelled),
        Err(Error::DiffIdMismatch { .. })
    ));
    assert!(!f.root.join("unverified").exists());
    r.begin_layer().unwrap();
    assert!(r.end_layer().unwrap().diff().is_empty());
    // A good one is downloaded once: the extraction reads the spool.
    let gz = gzip(&tar);
    let source = Source::new(gz);
    r.apply_layer(&streamed(&f.snap, &tar, &source)).unwrap();
    assert_eq!(source.opens(), 1);
    assert_eq!(fs::read(f.root.join("a")).unwrap(), b"a");
    let state: Vec<_> = fs::read_dir(f.root.join("state"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != ".ranikust-building")
        .collect();
    assert!(state.is_empty(), "the spool has no name: {state:?}");
    r.release().unwrap();
}

/// Layers whose digest and diff ID are right but whose tar has, after a good
/// first entry, a header every extraction refuses on its own, and whether it
/// is the `tar-header` limit (`--limit`) that refuses it.
fn bad_headers() -> Vec<(&'static str, Vec<u8>, bool)> {
    let first = || {
        let mut t = Tar::new();
        t.file("first", b"1");
        t
    };
    let entry = |kind, data: &[u8]| {
        first()
            .entry("x", kind, "", data, 0o644, (0, 0))
            .file("after", b"2")
            .bytes()
    };
    let mut summed = first().file("summed", b"").bytes();
    // A name byte of the second header: its checksum no longer matches.
    summed[1024] ^= 1;
    vec![
        (
            "a name out of the root",
            first().file("../out", b"x").bytes(),
            false,
        ),
        (
            "a GNU sparse entry",
            entry(EntryType::GNUSparse, b"x"),
            false,
        ),
        (
            "an unknown entry type",
            entry(EntryType::new(b'Q'), b""),
            false,
        ),
        (
            "a hardlink to itself",
            first().hardlink("self", "self").bytes(),
            false,
        ),
        (
            "a whiteout naming nothing",
            first().file("d/.wh.", b"").bytes(),
            false,
        ),
        (
            "a file as the layer's root",
            first().file("./", b"").bytes(),
            false,
        ),
        (
            "a malformed PAX time",
            entry(EntryType::XHeader, b"12 mtime=1x\n"),
            false,
        ),
        ("a broken checksum", summed, false),
        (
            "a GNU long name over the limit",
            entry(EntryType::GNULongName, &vec![b'n'; (1 << 20) + 1]),
            true,
        ),
    ]
}

/// On `/` a streamed layer's tar headers are checked in the pass that
/// verifies its digest and diff ID (ADR-0123): one with a header every
/// extraction refuses, or one over the `tar-header` limit, fails before its
/// first entry reaches `/`, which is left as it was and usable. Control: the
/// same layer from the store, which is never spooled, reaches `/` before
/// its bad header and breaks it.
#[test]
fn on_slash_a_streamed_layer_with_a_bad_tar_header_changes_nothing() {
    let good = Tar::new().file("good", b"g").bytes();
    for (what, tar, limit) in bad_headers() {
        let f = Shared::new(true);
        let mut r = f.snap.prepare(Base::Scratch).unwrap();
        r.set_layers_needed(true);
        let source = Source::new(gzip(&tar));
        let e = r
            .apply_layer(&streamed(&f.snap, &tar, &source))
            .unwrap_err();
        assert!(matches!(e, Error::Rootfs(_)), "{what}: {e:?}");
        assert_eq!(
            limit,
            matches!(e, Error::Rootfs(rnk_rootfs::Error::ExtensionHeader { .. })),
            "{what}: {e:?}"
        );
        assert_eq!(source.opens(), 1, "{what}");
        assert!(!f.root.join("first").exists(), "{what}: / changed");
        // A body its source finds wrong says so before its tar does.
        let wrong = Source::failing_at_end(gzip(&tar));
        let e = r.apply_layer(&streamed(&f.snap, &tar, &wrong)).unwrap_err();
        assert!(
            format!("{e:?}").contains("digest mismatch"),
            "{what}: {e:?}"
        );
        assert!(!f.root.join("first").exists(), "{what}: / changed");
        r.begin_layer().unwrap();
        assert!(r.end_layer().unwrap().diff().is_empty(), "{what}");
        r.apply_layer(&streamed(&f.snap, &good, &Source::new(gzip(&good))))
            .unwrap();
        assert!(f.root.join("good").exists(), "{what}");
        r.release().unwrap();

        let mut r = f.snap.prepare(Base::Scratch).unwrap();
        let stored = blob(&f.snap, &f.blobs(), "stored", &tar);
        assert!(r.apply_layer(&stored).is_err(), "{what}: the control");
        assert!(f.root.join("first").exists(), "{what}: the control");
        assert!(
            matches!(r.begin_layer(), Err(Error::State(_))),
            "{what}: the control breaks /"
        );
        r.release().unwrap();
    }
}

// Hostile cached layers.

/// What a hostile cached layer aims at in a per-stage root: links into
/// ignored paths and entries and whiteouts through them, which are skipped
/// as in a base layer; names, hardlinks and symlinks that climb out to a
/// sibling stage or `/`, which are refused or kept inside the root. Nothing
/// outside the stage root changes, a sibling stage is untouched, and the
/// ignored paths in the root keep what a `RUN` put there.
#[test]
fn red_team_a_hostile_cached_layer_stays_in_its_stage_root() {
    let f = Stages::new();
    let mut sibling = f.snap.prepare(Base::Scratch).unwrap();
    sibling.set_layers_needed(false);
    sibling.begin_layer().unwrap();
    fs::write(sibling.mount_point().join("f"), b"sibling").unwrap();
    drop(sibling.end_layer().unwrap());
    let n = sibling.mount_point().parent().unwrap().file_name().unwrap();
    let up = format!("../../{}/rootfs", n.to_str().unwrap());
    let mut s = f.snap.prepare(Base::Scratch).unwrap();
    s.set_layers_needed(true);
    let h = inside(&f.top, s.mount_point().to_owned());
    // What a `RUN` left in the ignored paths.
    fs::create_dir_all(h.join("ignored")).unwrap();
    fs::write(h.join("ignored/canary"), b"IGNORED").unwrap();
    fs::create_dir_all(h.join("proc/self")).unwrap();
    let outside = |f: &Stages| {
        let mut v = Vec::new();
        let mut stack = vec![f.top.clone()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p == h || p == f.blobs() {
                    continue;
                }
                let md = fs::symlink_metadata(&p).unwrap();
                if md.is_dir() {
                    stack.push(p.clone());
                }
                v.push((p, md.ino(), md.mode(), md.len()));
            }
        }
        v.sort();
        v
    };
    let before = outside(&f);

    let skipped = Tar::new()
        .symlink("to-ignored", "/ignored")
        .file("to-ignored/planted", b"EVIL")
        .file("to-ignored/.wh.canary", b"")
        .file(".wh.ignored", b"")
        .file("ignored/.wh.canary", b"")
        .file("ignored/planted", b"EVIL")
        .symlink("to-proc", "/proc/self")
        .file("to-proc/environ", b"EVIL")
        .file("proc/self/planted", b"EVIL")
        .file(".wh.proc", b"")
        .hardlink("hl", "ignored/canary")
        .file("fine", b"applied")
        .bytes();
    s.apply_layer(&blob(&f.snap, &f.blobs(), "skipped", &skipped))
        .unwrap();
    assert_eq!(fs::read(h.join("fine")).unwrap(), b"applied");
    assert_eq!(fs::read(h.join("ignored/canary")).unwrap(), b"IGNORED");
    assert_eq!(fs::read_dir(h.join("ignored")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(h.join("proc/self")).unwrap().count(), 0);
    assert!(!h.join("hl").exists(), "a hardlink to an ignored file");
    s.begin_layer().unwrap();
    assert!(s.end_layer().unwrap().diff().is_empty());

    // Each on a stage of its own: refused, or kept in that stage's root.
    for (name, tar) in [
        ("dotdot", Tar::new().file("../up", b"EVIL").bytes()),
        ("deep", Tar::new().file("a/../../../up2", b"EVIL").bytes()),
        (
            "sibling",
            Tar::new().file(&format!("{up}/planted"), b"EVIL").bytes(),
        ),
        (
            "hardlink-up",
            Tar::new().hardlink("hl-up", &format!("{up}/f")).bytes(),
        ),
        (
            "symlink-up",
            Tar::new()
                .symlink("up", &up)
                .file("up/planted", b"EVIL")
                .bytes(),
        ),
        (
            "symlink-slash",
            Tar::new()
                .symlink("abs", "/")
                .file("abs/planted", b"EVIL")
                .bytes(),
        ),
    ] {
        let mut other = f.snap.prepare(Base::Scratch).unwrap();
        let applied = other.apply_layer(&blob(&f.snap, &f.blobs(), name, &tar));
        let hl = other.mount_point().join("hl-up");
        if applied.is_ok() && hl.exists() {
            let theirs = fs::metadata(sibling.mount_point().join("f")).unwrap();
            assert_ne!(fs::metadata(&hl).unwrap().ino(), theirs.ino(), "{name}");
        }
        other.release().unwrap();
        assert_eq!(
            outside(&f),
            before,
            "{name}: nothing outside its root changed"
        );
    }
    assert_eq!(
        fs::read(sibling.mount_point().join("f")).unwrap(),
        b"sibling"
    );
    assert_eq!(fs::read_dir(sibling.mount_point()).unwrap().count(), 1);
    s.release().unwrap();
    sibling.release().unwrap();
}

/// The state directory of a build on `/`, relative to the root.
const STATE: &str = "var/lib/ranikust";

/// A build on the root at `host` as kaniko mode on `/` runs one: the state
/// directory `/var/lib/ranikust` holds an anonymous store, the ignore list
/// is `kaniko::ignore_list`'s for `mounts` and `--ignore-path=/ignored`.
/// `host` is `/` in a container (the T2 test) or a temporary directory.
fn build_on_slash(host: &Path, mounts: &[MountInfo]) -> (Kaniko, Arc<Store>) {
    let state = host.join(STATE);
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let store = Arc::new(Store::open_anonymous(&state).unwrap());
    let ignore = ignore_list(
        &IgnoreSpec {
            root: host,
            state_dir: Some(&state),
            ignore_paths: &[PathBuf::from("/ignored")],
        },
        mounts,
    )
    .unwrap();
    let kaniko = Kaniko::new(
        RootPath::open(host).unwrap(),
        PathBuf::from("/"),
        ignore,
        walk(),
        RootPath::open(&state).unwrap(),
    );
    kaniko.keep_in_anonymous_files(Arc::clone(&store)).unwrap();
    (kaniko, store)
}

/// What the hostile layer must leave alone, below `host`.
fn protected(host: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut v = Vec::new();
    for dir in [STATE, "ignored", "vol", "ranikust"] {
        let mut names: Vec<_> = fs::read_dir(host.join(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for p in names {
            let body = fs::read(&p).unwrap_or_default();
            v.push((p, body));
        }
    }
    v
}

/// The hostile cached layer of the `/` tests: links into the state
/// directory, `/proc`, an ignored path and a volume, entries through them,
/// whiteouts and opaque directories over each, a hardlink to a file of the
/// state directory, and one entry that is fine.
fn hostile_on_slash() -> Vec<u8> {
    Tar::new()
        .symlink("to-state", "/var/lib/ranikust")
        .file("to-state/planted", b"EVIL")
        .file("to-state/.wh.canary", b"")
        .file("var/lib/ranikust/planted", b"EVIL")
        .file("var/lib/ranikust/.wh.canary", b"")
        .file("var/lib/.wh.ranikust", b"")
        .file("var/lib/.wh..wh..opq", b"")
        .hardlink("hl-state", "var/lib/ranikust/canary")
        .symlink("to-proc", "/proc/self")
        .file("to-proc/planted", b"EVIL")
        .file("to-proc/environ", b"EVIL")
        .file("proc/planted", b"EVIL")
        .file(".wh.proc", b"")
        .symlink("to-ignored", "/ignored")
        .file("to-ignored/planted", b"EVIL")
        .file(".wh.ignored", b"")
        .file("ignored/.wh.canary", b"")
        .symlink("to-vol", "/vol")
        .file("to-vol/planted", b"EVIL")
        .file("vol/.wh.canary", b"")
        .file(".wh.vol", b"")
        .file(".wh.ranikust", b"")
        .file("ranikust/.wh..built", b"")
        .file("fine", b"applied")
        .bytes()
}

/// Kaniko mode on `/` (the root at `host`, which holds `vol/`, a mount in
/// `mounts`): a hostile cached layer, streamed as from a registry cache,
/// writes nothing into the state directory, `/proc`, an ignored path, a
/// volume or the marker of `/`, links nothing of them into the root, and
/// the walk after it reports nothing; a layer whose names climb out of the
/// root reaches nothing: streamed, it is refused before `/` changes; from
/// the store, while writing, which breaks `/`.
fn red_team_hostile_layer_on_slash(host: &Path, mounts: &[MountInfo]) {
    fs::create_dir_all(host.join("ignored")).unwrap();
    fs::write(host.join("ignored/canary"), b"IGNORED").unwrap();
    assert!(host.join("vol/canary").is_file(), "the volume's canary");
    let (kaniko, _store) = build_on_slash(host, mounts);
    fs::write(host.join(STATE).join("canary"), b"STATE").unwrap();
    let mut root = kaniko.prepare(Base::Scratch).unwrap();
    root.set_layers_needed(true);
    let before = protected(host);
    assert!(host.join("ranikust/.built").is_file());

    let tar = hostile_on_slash();
    root.apply_layer(&streamed(&kaniko, &tar, &Source::new(gzip(&tar))))
        .unwrap();
    assert_eq!(
        protected(host),
        before,
        "the protected paths after the layer"
    );
    assert_eq!(fs::read(host.join("fine")).unwrap(), b"applied");
    for p in ["proc/planted", "proc/self/planted"] {
        assert!(!host.join(p).exists(), "{p}");
    }
    assert!(
        matches!(
            fs::symlink_metadata(host.join("hl-state")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
        ),
        "a hardlink into the state directory"
    );
    root.begin_layer().unwrap();
    let d = root.end_layer().unwrap();
    assert!(d.diff().is_empty(), "{:?}", changes(d.diff()));
    drop(d);

    let climbs = Tar::new()
        .file("../escape", b"EVIL")
        .file("after", b"EVIL")
        .bytes();
    let e = root
        .apply_layer(&streamed(&kaniko, &climbs, &Source::new(climbs.clone())))
        .unwrap_err();
    assert!(
        matches!(e, Error::Rootfs(rnk_rootfs::Error::Entry { .. })),
        "a name climbing out of the root is refused: {e:?}"
    );
    assert!(!host.join("escape").exists());
    assert!(!host.join("after").exists());
    // Refused by its header in the spool pass, before `/` changed (ADR-0123).
    root.begin_layer().unwrap();
    assert!(root.end_layer().unwrap().diff().is_empty());
    assert_eq!(
        protected(host),
        before,
        "the protected paths after the refusal"
    );

    // From the store it is never spooled: the extraction refuses the entry,
    // and `/` is broken. Its blob is made in the root (a container's `/` may
    // have no `/tmp`), after the walk above, which would see it.
    let blobs = tempfile::tempdir_in(host).unwrap();
    let stored = blob(&kaniko, blobs.path(), "climbs", &climbs);
    let e = root.apply_layer(&stored).unwrap_err();
    assert!(
        matches!(e, Error::Rootfs(rnk_rootfs::Error::Entry { .. })),
        "a name climbing out of the root is refused while writing: {e:?}"
    );
    for p in ["escape", "../escape", "after"] {
        assert!(!host.join(p).exists(), "{p}");
    }
    assert!(
        matches!(root.begin_layer(), Err(Error::State(_))),
        "the refusal while writing breaks /"
    );
    assert_eq!(
        protected(host),
        before,
        "the protected paths after the refusal while writing"
    );
    root.release().unwrap();
    assert!(!kaniko.finish().unwrap(), "/ is left as it is");
}

/// [`red_team_hostile_layer_on_slash`] on a temporary root told it is `/`,
/// its volume declared in a mount table.
#[test]
fn red_team_a_hostile_cached_layer_on_slash_touches_no_ignored_path() {
    let tmp = tempfile::tempdir().unwrap();
    let host = fs::canonicalize(tmp.path()).unwrap().join("root");
    fs::create_dir_all(host.join("vol")).unwrap();
    fs::create_dir_all(host.join("proc/self")).unwrap();
    fs::write(host.join("vol/canary"), b"VOLUME").unwrap();
    let mountinfo = format!(
        "1 0 0:1 / / rw - overlay overlay rw\n\
         2 1 0:3 /vol {} rw - ext4 /dev/x rw\n",
        host.join("vol").display()
    );
    let mounts = parse_mountinfo(mountinfo.as_bytes()).unwrap();
    red_team_hostile_layer_on_slash(&host, &mounts);
    assert_eq!(fs::read_dir(host.join("proc/self")).unwrap().count(), 0);
    let around: Vec<_> = fs::read_dir(host.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(around, ["root"], "nothing beside the root");
}

// Red team: what a `RUN` left in the root before a cached layer lands on it.

/// How long an apply of these small layers may take before it counts as
/// blocked.
const DEADLINE: Duration = Duration::from_secs(30);

/// Runs `f`, an apply. A watchdog waits [`DEADLINE`], then opens each FIFO
/// of `fifos` for reading and writing, which ends an open or a read
/// blocked on one: the test fails instead of hanging.
fn within_deadline<T>(fifos: &[PathBuf], f: impl FnOnce() -> T) -> T {
    let (done, wait) = mpsc::channel::<()>();
    std::thread::scope(|s| {
        let dog = s.spawn(move || {
            if wait.recv_timeout(DEADLINE) != Err(mpsc::RecvTimeoutError::Timeout) {
                return false;
            }
            let nonblock = i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).unwrap();
            for p in fifos {
                drop(
                    fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(nonblock)
                        .open(p),
                );
            }
            true
        });
        let out = f();
        drop(done);
        assert!(!dog.join().unwrap(), "blocked for over {DEADLINE:?}");
        out
    })
}

/// Files watched for being opened, read or written (inotify). An `O_PATH`
/// descriptor, which runs no driver's `open`, is not reported (Linux 6.5).
struct Opens {
    fd: OwnedFd,
    names: HashMap<i32, PathBuf>,
}

impl Opens {
    fn new() -> Self {
        use rustix::fs::inotify::{CreateFlags, init};
        Self {
            fd: init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC).unwrap(),
            names: HashMap::new(),
        }
    }

    fn watch(&mut self, p: &Path) {
        use rustix::fs::inotify::{WatchFlags as W, add_watch};
        use rustix::fs::{Mode, OFlags, open};
        let what = W::OPEN | W::ACCESS | W::MODIFY | W::CLOSE | W::DONT_FOLLOW;
        match add_watch(&self.fd, p, what) {
            Ok(wd) => {
                self.names.insert(wd, p.to_owned());
            }
            // A device cgroup (Docker's default) refuses a watch on a node
            // it denies; it then refuses every open of it too.
            Err(rustix::io::Errno::PERM) => {
                let o = open(
                    p,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                );
                assert_eq!(o.err(), Some(rustix::io::Errno::PERM), "{p:?} is unwatched");
            }
            Err(e) => panic!("watch {p:?}: {e}"),
        }
    }

    /// Fails with what was opened, read or written since the last call.
    fn none_since(&self, what: &str) {
        use rustix::fs::inotify::{ReadFlags as R, Reader};
        let any = R::OPEN | R::ACCESS | R::MODIFY | R::CLOSE_WRITE | R::CLOSE_NOWRITE;
        let mut buf = [MaybeUninit::uninit(); 4096];
        let mut events = Reader::new(&self.fd, &mut buf);
        let mut seen = Vec::new();
        loop {
            match events.next() {
                Ok(e) if e.events().intersects(any) => {
                    seen.push((self.names.get(&e.wd()).cloned(), e.events()));
                }
                Ok(_) => {}
                Err(rustix::io::Errno::AGAIN) => break,
                Err(e) => panic!("inotify: {e}"),
            }
        }
        assert!(seen.is_empty(), "{what} opened a planted node: {seen:?}");
    }
}

/// A special file a `RUN` makes with `mknod`: a FIFO as any user (T0), a
/// device node as root (T2c).
#[derive(Clone, Copy, Debug)]
enum Node {
    Fifo,
    Char(u32, u32),
    Block(u32, u32),
}

impl Node {
    /// Makes it at `p`, watched by `opens`; a FIFO is added to `fifos`.
    fn plant(self, p: PathBuf, opens: &mut Opens, fifos: &mut Vec<PathBuf>) {
        use rustix::fs::{FileType as F, Mode, makedev};
        let (kind, dev) = match self {
            Self::Fifo => (F::Fifo, 0),
            Self::Char(major, minor) => (F::CharacterDevice, makedev(major, minor)),
            Self::Block(major, minor) => (F::BlockDevice, makedev(major, minor)),
        };
        rustix::fs::mknodat(rustix::fs::CWD, &p, kind, Mode::from_raw_mode(0o666), dev).unwrap();
        opens.watch(&p);
        if matches!(self, Self::Fifo) {
            fifos.push(p);
        }
    }
}

/// Special files of `nodes` a `RUN` left where a cached layer then writes:
/// at names it writes as a file and as a directory, at a hardlink's target
/// and a whiteout's, in a directory it replaces with a file and in one it
/// makes opaque. Neither the walk of that `RUN`, nor the layer, nor the
/// walk after it opens, reads or writes one, and nothing blocks; a layer
/// that names one as a directory fails, and breaks the root.
fn planted_nodes_are_never_opened<S>(
    snap: &S,
    blobs: &Path,
    top: &Path,
    nodes: &[Node],
    host: impl Fn(&S::Root) -> PathBuf,
) where
    S: Snapshotter<Layer = KanikoLayer>,
    S::Root: Applies,
{
    let mut opens = Opens::new();
    let mut fifos = Vec::new();
    let mut root = snap.prepare(Base::Scratch).unwrap();
    root.set_layers_needed(true);
    let h = inside(top, host(&root));
    let mut layer = Tar::new();
    root.begin_layer().unwrap();
    for (i, &node) in nodes.iter().enumerate() {
        fs::create_dir(h.join(format!("dir-{i}"))).unwrap();
        fs::create_dir(h.join(format!("opq-{i}"))).unwrap();
        for name in ["as-file", "as-dir", "hl-target", "whited", "dir", "opq"] {
            let at = match name {
                "dir" | "opq" => format!("{name}-{i}/node"),
                _ => format!("{name}-{i}"),
            };
            node.plant(h.join(at), &mut opens, &mut fifos);
        }
        layer
            .file(&format!("as-file-{i}"), b"file")
            .dir(&format!("as-dir-{i}"), 0o755)
            .file(&format!("as-dir-{i}/child"), b"child")
            .hardlink(&format!("hl-{i}"), &format!("hl-target-{i}"))
            .file(&format!(".wh.whited-{i}"), b"")
            .file(&format!("dir-{i}"), b"now a file")
            .dir(&format!("opq-{i}"), 0o755)
            .file(&format!("opq-{i}/.wh..wh..opq"), b"")
            .file(&format!("opq-{i}/new"), b"new");
    }
    drop(root.end_layer().unwrap());
    opens.none_since("the walk of the RUN that planted them");

    let over = blob(snap, blobs, "over-nodes", &layer.bytes());
    within_deadline(&fifos, || root.apply(&over)).unwrap();
    opens.none_since("the cached layer");
    let read = |n: String| fs::read(h.join(n)).unwrap();
    let ino = |n: String| fs::symlink_metadata(h.join(n)).unwrap().ino();
    for i in 0..nodes.len() {
        assert_eq!(read(format!("as-file-{i}")), b"file");
        assert_eq!(read(format!("as-dir-{i}/child")), b"child");
        assert_eq!(read(format!("dir-{i}")), b"now a file");
        assert_eq!(ino(format!("hl-{i}")), ino(format!("hl-target-{i}")));
        assert!(fs::symlink_metadata(h.join(format!("whited-{i}"))).is_err());
        let names: Vec<_> = fs::read_dir(h.join(format!("opq-{i}")))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["new"]);
    }
    root.begin_layer().unwrap();
    let d = root.end_layer().unwrap();
    assert!(
        S::Root::walked(&d).is_empty(),
        "{:?}",
        changes(S::Root::walked(&d))
    );
    drop(d);
    opens.none_since("the walk after the layer");
    root.release().unwrap();

    for (i, &node) in nodes.iter().enumerate() {
        let mut root = snap.prepare(Base::Scratch).unwrap();
        let h = inside(top, host(&root));
        root.begin_layer().unwrap();
        node.plant(h.join("parent"), &mut opens, &mut fifos);
        drop(root.end_layer().unwrap());
        let below = Tar::new().file("parent/x", b"x").bytes();
        let below = blob(snap, blobs, &format!("below-{i}"), &below);
        let r = within_deadline(&fifos, || root.apply(&below));
        assert!(r.is_err(), "{node:?} as a parent directory");
        assert!(
            matches!(root.begin_layer(), Err(Error::State(_))),
            "{node:?} as a parent directory: the root is broken"
        );
        opens.none_since(&format!("{node:?} as a parent directory"));
        root.release().unwrap();
    }
}

/// Red team (T18, ADR-0041): FIFOs a `RUN` planted where a cached layer
/// writes are never opened or blocked on, per stage and on the shared root.
#[test]
fn red_team_fifos_a_run_planted_are_never_opened_by_a_cached_layer() {
    let f = Stages::new();
    planted_nodes_are_never_opened(&f.snap, &f.blobs(), &f.top, &[Node::Fifo], |r| {
        r.mount_point().to_owned()
    });
    for slash in [false, true] {
        let f = Shared::new(slash);
        let root = f.root.clone();
        planted_nodes_are_never_opened(&f.snap, &f.blobs(), &f.top, &[Node::Fifo], |_| {
            root.clone()
        });
    }
}

/// `..` from the root at `h` up to `top`, as the host resolves it.
fn up_to(top: &Path, h: &Path) -> String {
    "../".repeat(h.strip_prefix(top).unwrap().components().count())
}

/// The ways a link in the root at `h` names `outside`, beside the root
/// below `top`: its absolute host path, and `..` from the root. Each with
/// the link's target and the directory it names inside the root: the host
/// path below the root, and `outside` in the root, where `..` stops.
fn ways_out(top: &Path, h: &Path) -> [(&'static str, String, PathBuf); 2] {
    let abs = top.join("outside");
    let in_root = h.join(abs.strip_prefix("/").unwrap());
    [
        ("abs", abs.display().to_string(), in_root),
        (
            "rel",
            format!("{}outside", up_to(top, h)),
            h.join("outside"),
        ),
    ]
}

/// `outside` below `top`, with `secret`, `doomed` and `sub/inner`, its
/// modes and times set.
fn make_outside(top: &Path) -> PathBuf {
    let o = top.join("outside");
    fs::create_dir_all(o.join("sub")).unwrap();
    for (name, body) in [
        ("secret", "SECRET"),
        ("doomed", "DOOMED"),
        ("sub/inner", "INNER"),
    ] {
        fs::write(o.join(name), body).unwrap();
    }
    fs::set_permissions(o.join("secret"), fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&o, fs::Permissions::from_mode(0o750)).unwrap();
    for p in ["sub/inner", "secret", "doomed", "sub", "."] {
        set_times(&o.join(p), 1_234_567);
    }
    o
}

/// One entry of [`tree`]: path, inode, type and mode, mtime, and content
/// or link target.
type Seen = (PathBuf, u64, u32, i64, Vec<u8>);

/// `dir` and every entry below it, as [`Seen`]: what anything that reached
/// it would change.
fn tree(dir: &Path) -> Vec<Seen> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(p) = stack.pop() {
        let md = fs::symlink_metadata(&p).unwrap();
        let body = if md.is_dir() {
            for e in fs::read_dir(&p).unwrap() {
                stack.push(e.unwrap().path());
            }
            Vec::new()
        } else if md.file_type().is_symlink() {
            fs::read_link(&p).unwrap().as_os_str().as_bytes().to_vec()
        } else {
            fs::read(&p).unwrap()
        };
        out.push((p, md.ino(), md.mode(), md.mtime(), body));
    }
    out.sort();
    out
}

/// Every entry below `top` but the root at `h` and the blobs: path, inode,
/// type and mode, and size.
fn beside(top: &Path, h: &Path) -> Vec<(PathBuf, u64, u32, u64)> {
    let mut v = Vec::new();
    let mut stack = vec![top.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p == h || p == top.join("blobs") {
                continue;
            }
            let md = fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                stack.push(p.clone());
            }
            v.push((p, md.ino(), md.mode(), md.len()));
        }
    }
    v.sort();
    v
}

/// `secret`, `doomed` and `sub/inner` in `dir`, a decoy for `outside`
/// inside the root.
fn decoy(dir: &Path) {
    fs::create_dir_all(dir.join("sub")).unwrap();
    for name in ["secret", "doomed", "sub/inner"] {
        fs::write(dir.join(name), b"DECOY").unwrap();
    }
}

/// What a `RUN` leaves in the root at `h` to lead a later layer out: for
/// each way out, symlinks to `outside` and to its `secret` at each name the
/// layer uses; a hardlink to `secret`; a symlink loop; directories holding
/// symlinks out. With `decoys`, a decoy stands where each way out leads
/// inside the root.
fn plant_links(top: &Path, h: &Path, decoys: bool) {
    use std::os::unix::fs::symlink;
    for d in ["holder", "opq"] {
        fs::create_dir(h.join(d)).unwrap();
    }
    for (way, target, in_root) in ways_out(top, h) {
        for name in [
            "as-dir",
            "parent",
            "hl-parent",
            "wh-parent",
            "opq",
            "whited",
        ] {
            symlink(&target, h.join(format!("{way}-{name}"))).unwrap();
        }
        for name in ["as-file", "hl-target"] {
            symlink(format!("{target}/secret"), h.join(format!("{way}-{name}"))).unwrap();
        }
        for d in ["holder", "opq"] {
            symlink(&target, h.join(d).join(way)).unwrap();
        }
        if decoys {
            decoy(&in_root);
        }
    }
    fs::hard_link(top.join("outside/secret"), h.join("hl-out")).unwrap();
    symlink("loop-b", h.join("loop-a")).unwrap();
    symlink("loop-a", h.join("loop-b")).unwrap();
}

/// A layer that uses every link [`plant_links`] left: writes over them as a
/// file and as a directory, writes and links through them, links them,
/// whites out through them and them, empties the directories holding
/// links out.
fn through_links() -> Vec<u8> {
    let mut t = Tar::new();
    for way in ["abs", "rel"] {
        t.file_mode(&format!("{way}-as-file"), b"EVIL", 0o4755)
            .dir(&format!("{way}-as-dir"), 0o777)
            .file(&format!("{way}-as-dir/planted"), b"EVIL")
            .file(&format!("{way}-parent/planted"), b"EVIL")
            .hardlink(&format!("{way}-hl"), &format!("{way}-hl-parent/secret"))
            .hardlink(&format!("{way}-hl-sym"), &format!("{way}-hl-target"))
            .file(&format!("{way}-wh-parent/.wh.doomed"), b"")
            .file(&format!(".wh.{way}-whited"), b"");
    }
    t.file_mode("hl-out", b"EVIL", 0o4755)
        .file(".wh.holder", b"")
        .dir("opq", 0o755)
        .file("opq/.wh..wh..opq", b"")
        .file("loop-a", b"no loop");
    t.bytes()
}

/// Links a `RUN` left in the root lead a cached layer nowhere outside it:
/// symlinks are followed only inside the root (onto decoys), a symlink or a
/// hardlink the layer writes over is replaced, never written through, and
/// deletions never follow a link. Without decoys, each use on a root of its
/// own is refused or applied inside the root, a loop included, without
/// blocking; a refusal breaks the root.
fn planted_links_never_lead_out<S>(
    snap: &S,
    blobs: &Path,
    top: &Path,
    host: impl Fn(&S::Root) -> PathBuf,
) where
    S: Snapshotter<Layer = KanikoLayer>,
    S::Root: Applies,
{
    let outside = make_outside(top);
    let seen = tree(&outside);
    let mut root = snap.prepare(Base::Scratch).unwrap();
    root.set_layers_needed(true);
    let h = inside(top, host(&root));
    root.begin_layer().unwrap();
    plant_links(top, &h, true);
    drop(root.end_layer().unwrap());
    let around = beside(top, &h);
    let layer = blob(snap, blobs, "through-links", &through_links());
    within_deadline(&[], || root.apply(&layer)).unwrap();
    assert_eq!(tree(&outside), seen, "outside the root");
    assert_eq!(beside(top, &h), around, "beside the root");
    let md = |p: &str| fs::symlink_metadata(h.join(p)).unwrap();
    for (way, _, decoy) in ways_out(top, &h) {
        let at = |n: &str| format!("{way}-{n}");
        assert!(md(&at("as-file")).is_file(), "{way}");
        assert_eq!(fs::read(h.join(at("as-file"))).unwrap(), b"EVIL");
        assert!(md(&at("as-dir")).is_dir(), "{way}");
        assert_eq!(fs::read(h.join(at("as-dir/planted"))).unwrap(), b"EVIL");
        assert_eq!(fs::read(decoy.join("planted")).unwrap(), b"EVIL", "{way}");
        assert_eq!(
            md(&at("hl")).ino(),
            fs::metadata(decoy.join("secret")).unwrap().ino(),
            "{way}: linked to the decoy"
        );
        assert!(md(&at("hl-sym")).file_type().is_symlink(), "{way}");
        assert!(fs::symlink_metadata(decoy.join("doomed")).is_err(), "{way}");
        assert!(fs::symlink_metadata(h.join(at("whited"))).is_err(), "{way}");
    }
    assert_eq!(fs::read(h.join("hl-out")).unwrap(), b"EVIL");
    assert!(fs::symlink_metadata(h.join("holder")).is_err());
    assert_eq!(fs::read_dir(h.join("opq")).unwrap().count(), 0);
    assert_eq!(fs::read(h.join("loop-a")).unwrap(), b"no loop");
    root.begin_layer().unwrap();
    let d = root.end_layer().unwrap();
    assert!(
        S::Root::walked(&d).is_empty(),
        "{:?}",
        changes(S::Root::walked(&d))
    );
    drop(d);
    root.release().unwrap();

    let mut uses = Vec::new();
    for way in ["abs", "rel"] {
        let t = |p: &str| format!("{way}-{p}");
        uses.extend([
            (
                t("parent"),
                Tar::new().file(&t("parent/planted"), b"E").bytes(),
            ),
            (
                t("hl"),
                Tar::new().hardlink("hl", &t("hl-parent/secret")).bytes(),
            ),
            (
                t("wh"),
                Tar::new().file(&t("wh-parent/.wh.doomed"), b"").bytes(),
            ),
            (
                t("opq"),
                Tar::new().file(&t("opq/.wh..wh..opq"), b"").bytes(),
            ),
            (t("opq-dir"), {
                let mut opq = Tar::new();
                opq.dir(&t("opq"), 0o777).file(&t("opq/.wh..wh..opq"), b"");
                opq.bytes()
            }),
        ]);
    }
    uses.extend([
        (
            "loop-parent".to_owned(),
            Tar::new().file("loop-a/x", b"E").bytes(),
        ),
        (
            "loop-hl".to_owned(),
            Tar::new().hardlink("hl", "loop-a/x").bytes(),
        ),
        (
            "loop-wh".to_owned(),
            Tar::new().file("loop-a/.wh.x", b"").bytes(),
        ),
        (
            "loop-opq".to_owned(),
            Tar::new().file("loop-a/.wh..wh..opq", b"").bytes(),
        ),
    ]);
    for (name, tar) in uses {
        let mut root = snap.prepare(Base::Scratch).unwrap();
        let h = inside(top, host(&root));
        root.begin_layer().unwrap();
        plant_links(top, &h, false);
        drop(root.end_layer().unwrap());
        let around = beside(top, &h);
        let layer = blob(snap, blobs, &name, &tar);
        let r = within_deadline(&[], || root.apply(&layer));
        assert_eq!(tree(&outside), seen, "{name}: outside the root");
        assert_eq!(beside(top, &h), around, "{name}: beside the root");
        if r.is_err() {
            assert!(
                matches!(root.begin_layer(), Err(Error::State(_))),
                "{name}: the root is broken"
            );
        }
        root.release().unwrap();
    }
    assert_eq!(tree(&outside), seen, "outside the root, the roots released");
}

/// Red team (T1, T2): symlinks, a hardlink and a loop a `RUN` planted lead
/// a cached layer nowhere outside a stage root.
#[test]
fn red_team_links_a_run_planted_never_lead_a_cached_layer_out() {
    let f = Stages::new();
    planted_links_never_lead_out(&f.snap, &f.blobs(), &f.top, |r| r.mount_point().to_owned());
    for slash in [false, true] {
        let f = Shared::new(slash);
        let root = f.root.clone();
        planted_links_never_lead_out(&f.snap, &f.blobs(), &f.top, |_| root.clone());
        let seen = tree(&f.top.join("outside"));
        f.snap.finish().unwrap();
        assert_eq!(tree(&f.top.join("outside")), seen, "emptied at the end");
    }
}

/// A process a `RUN` left behind swaps `race`, a directory of the root,
/// with `swap`, a symlink out of it (by each way out in turn), over and
/// over, while a cached layer links below `race`, whites out in it, makes
/// it opaque, fills it and sets its mode and times; decoys stand where the
/// ways out lead inside the root. A long layer first, then short ones,
/// which may end before a swap breaks them, so the modes and times set at
/// the end race too. Nothing outside the root changes, whatever the
/// outcome; a failure breaks the root. Returns the swaps made and the
/// short applies that succeeded.
fn a_swapped_directory_keeps_the_layer_in<S>(
    snap: &S,
    blobs: &Path,
    top: &Path,
    host: impl Fn(&S::Root) -> PathBuf,
) -> (u64, usize)
where
    S: Snapshotter<Layer = KanikoLayer>,
    S::Root: Applies,
{
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    let outside = make_outside(top);
    let seen = tree(&outside);
    let mut t = Tar::new();
    t.hardlink("hl-old", "race/old")
        .file("race/.wh.doomed", b"")
        .dir("race", 0o777)
        .file("race/.wh..wh..opq", b"");
    for i in 0..1500 {
        t.file(&format!("race/f{i}"), b"EVIL");
        if i % 100 == 0 {
            t.dir(&format!("race/d{i}"), 0o777)
                .file(&format!("race/d{i}/g"), b"EVIL");
        }
    }
    let long = t.bytes();
    let short = Tar::new()
        .dir("race", 0o777)
        .file("race/f", b"EVIL")
        .bytes();
    let (mut swaps, mut done) = (0, 0);
    for round in 0..66 {
        let tar = if round < 6 { &long } else { &short };
        let mut root = snap.prepare(Base::Scratch).unwrap();
        root.set_layers_needed(true);
        let h = inside(top, host(&root));
        let ways = ways_out(top, &h);
        root.begin_layer().unwrap();
        for (_, _, in_root) in &ways {
            decoy(in_root);
            fs::write(in_root.join("old"), b"DECOY").unwrap();
        }
        fs::create_dir(h.join("race")).unwrap();
        for n in ["old", "doomed"] {
            fs::write(h.join("race").join(n), b"RUN").unwrap();
        }
        std::os::unix::fs::symlink(&ways[round % 2].1, h.join("swap")).unwrap();
        drop(root.end_layer().unwrap());
        let layer = blob(snap, blobs, &format!("race-{round}"), tar);
        let (race, swap) = (h.join("race"), h.join("swap"));
        let stop = AtomicBool::new(false);
        let (r, n) = std::thread::scope(|s| {
            let racer = s.spawn(|| {
                let mut n = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    if renameat_with(CWD, &race, CWD, &swap, RenameFlags::EXCHANGE).is_ok() {
                        n += 1;
                    }
                }
                n
            });
            let r = within_deadline(&[], || root.apply(&layer));
            stop.store(true, Ordering::Relaxed);
            (r, racer.join().unwrap())
        });
        swaps += n;
        assert_eq!(tree(&outside), seen, "round {round}: outside the root");
        if r.is_err() {
            assert!(
                matches!(root.begin_layer(), Err(Error::State(_))),
                "round {round}: the root is broken"
            );
        } else if round >= 6 {
            done += 1;
        }
        root.release().unwrap();
    }
    assert!(swaps > 0, "the racer never swapped");
    (swaps, done)
}

/// Red team (T3): a directory swapped for a symlink out of the root while a
/// cached layer lands in it keeps the layer in the root, per stage and on
/// the shared root.
#[test]
fn red_team_a_directory_swapped_for_a_link_out_keeps_a_cached_layer_in() {
    let f = Stages::new();
    let per_stage = a_swapped_directory_keeps_the_layer_in(&f.snap, &f.blobs(), &f.top, |r| {
        r.mount_point().to_owned()
    });
    println!("per stage: {per_stage:?} (swaps, short applies done)");
    for slash in [false, true] {
        let f = Shared::new(slash);
        let root = f.root.clone();
        let shared =
            a_swapped_directory_keeps_the_layer_in(&f.snap, &f.blobs(), &f.top, |_| root.clone());
        println!("shared root, slash {slash}: {shared:?} (swaps, short applies done)");
    }
}

/// A tar with `first`, an extension header of `size` bytes, then `after`.
fn oversized_header(size: usize) -> Vec<u8> {
    let mut t = Tar::new();
    t.file("first", b"1")
        .entry(
            "pax",
            EntryType::XHeader,
            "",
            &vec![b'a'; size],
            0o644,
            (0, 0),
        )
        .file("after", b"2");
    t.bytes()
}

/// Red team (T15, ADR-0079): a cached layer whose tar declares an extension
/// header over the `tar-header` limit is refused at that header, as a base
/// layer is. Per stage from the store, it breaks the root its first entry
/// reached; on `/` streamed, the spool pass refuses it before `/` changes
/// (ADR-0123).
#[test]
fn red_team_an_oversized_tar_header_in_a_cached_layer_is_refused_at_that_header() {
    let tar = oversized_header(2 << 20);
    let gz = gzip(&tar);
    let is_limit =
        |e: &Error| matches!(e, Error::Rootfs(rnk_rootfs::Error::ExtensionHeader { .. }));

    let f = Stages::new();
    let path = f.blobs().join("header.gz");
    fs::write(&path, &gz).unwrap();
    let layer = f.snap.base_layer(&diff_id(&tar), &path, &[]).unwrap();
    let mut s = f.snap.prepare(Base::Scratch).unwrap();
    let e = s.apply_layer(&layer).unwrap_err();
    assert!(is_limit(&e), "{e:?}");
    assert!(s.mount_point().join("first").exists());
    assert!(!s.mount_point().join("after").exists());
    assert!(matches!(s.begin_layer(), Err(Error::State(_))));
    s.release().unwrap();

    let f = Shared::new(true);
    let mut r = f.snap.prepare(Base::Scratch).unwrap();
    r.set_layers_needed(true);
    let e = r
        .apply_layer(&streamed(&f.snap, &tar, &Source::new(gz)))
        .unwrap_err();
    assert!(is_limit(&e), "{e:?}");
    assert!(!f.root.join("first").exists());
    r.begin_layer().unwrap();
    assert!(r.end_layer().unwrap().diff().is_empty());
    r.release().unwrap();
}

/// Red team (T1, T15): on `/`, a hostile cached layer from the store whose
/// diff ID is wrong (a blob's is checked at its end) and one streamed whose
/// last entry the root refuses (a hardlink to nothing) both fail once
/// entries may have landed: neither touches an ignored path, each leaves `/`
/// broken, and a new base empties `/` of the first, the ignore list kept.
/// One streamed with an oversized header is refused before `/` changes
/// (ADR-0123).
#[test]
fn red_team_a_bad_cached_layer_on_slash_touches_no_ignored_path() {
    let tmp = tempfile::tempdir().unwrap();
    let top = fs::canonicalize(tmp.path()).unwrap();
    let host = top.join("root");
    fs::create_dir_all(host.join("vol")).unwrap();
    fs::create_dir_all(host.join("proc/self")).unwrap();
    fs::create_dir_all(host.join("ignored")).unwrap();
    fs::write(host.join("vol/canary"), b"VOLUME").unwrap();
    fs::write(host.join("ignored/canary"), b"IGNORED").unwrap();
    let mountinfo = format!(
        "1 0 0:1 / / rw - overlay overlay rw\n\
         2 1 0:3 /vol {} rw - ext4 /dev/x rw\n",
        host.join("vol").display()
    );
    let mounts = parse_mountinfo(mountinfo.as_bytes()).unwrap();
    let (kaniko, _store) = build_on_slash(&host, &mounts);
    fs::write(host.join(STATE).join("canary"), b"STATE").unwrap();
    let mut root = kaniko.prepare(Base::Scratch).unwrap();
    root.set_layers_needed(true);
    let before = protected(&host);

    let tar = hostile_on_slash();
    let path = top.join("hostile.gz");
    fs::write(&path, gzip(&tar)).unwrap();
    let wrong = kaniko
        .base_layer(&Digest::from_sha256_bytes(&[0; 32]), &path, &[])
        .unwrap();
    assert!(matches!(
        root.apply_layer(&wrong),
        Err(Error::DiffIdMismatch { .. })
    ));
    assert_eq!(protected(&host), before, "after the wrong diff ID");
    assert!(matches!(root.begin_layer(), Err(Error::State(_))));
    assert!(matches!(root.keep(&Keep::All), Err(Error::State(_))));
    root.release().unwrap();

    let mut root = kaniko.prepare(Base::Scratch).unwrap();
    assert!(!host.join("fine").exists(), "emptied by the new base");
    assert_eq!(protected(&host), before, "after the new base");
    let cut = oversized_header(2 << 20);
    let e = root
        .apply_layer(&streamed(&kaniko, &cut, &Source::new(gzip(&cut))))
        .unwrap_err();
    assert!(
        matches!(e, Error::Rootfs(rnk_rootfs::Error::ExtensionHeader { .. })),
        "{e:?}"
    );
    assert_eq!(protected(&host), before, "after the oversized header");
    assert!(!host.join("first").exists(), "refused before / changed");
    let mut dangling = tar.clone();
    dangling.truncate(dangling.len() - 1024);
    dangling.extend(Tar::new().hardlink("dangling", "missing").bytes());
    let e = root
        .apply_layer(&streamed(&kaniko, &dangling, &Source::new(gzip(&dangling))))
        .unwrap_err();
    assert!(
        matches!(e, Error::Rootfs(rnk_rootfs::Error::Entry { .. })),
        "{e:?}"
    );
    assert_eq!(fs::read(host.join("fine")).unwrap(), b"applied");
    assert_eq!(protected(&host), before, "after the hardlink to nothing");
    assert!(matches!(root.begin_layer(), Err(Error::State(_))));
    root.release().unwrap();
    assert!(!kaniko.finish().unwrap(), "/ is left as it is");
    assert_eq!(fs::read_dir(host.join("proc/self")).unwrap().count(), 0);
    let mut around: Vec<_> = fs::read_dir(&top)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    around.sort();
    assert_eq!(around, ["hostile.gz", "root"], "nothing beside the root");
}

// Root: foreign owners and device nodes (T2c), a default container's `/` (T2).

/// A cached layer as root, with foreign owners, a setuid file and device
/// nodes, gives the root what the same layer gives as a base layer, owners and
/// modes kept and no device node made where root could make one, and the
/// next diff is empty.
fn owners_and_nodes_as_a_base_layer<S>(
    snap: &S,
    blobs: &Path,
    top: &Path,
    host: impl Fn(&S::Root) -> PathBuf,
) where
    S: Snapshotter<Layer = KanikoLayer>,
    S::Root: Applies,
{
    let tar = Tar::new()
        .owner(1234, 5678)
        .dir("owned", 0o750)
        .file_mode("owned/setuid", b"s", 0o4755)
        .owner(0, 0)
        .dir("nodes", 0o755)
        .node("nodes/null", EntryType::Char, (1, 3))
        .node("nodes/loop", EntryType::Block, (7, 0))
        .node("nodes/pipe", EntryType::Fifo, (0, 0))
        .owner(4321, 4321)
        .file("nodes/file", b"f")
        .bytes();
    let layer = blob(snap, blobs, "owners", &tar);
    let based = snap
        .prepare(Base::Image(std::slice::from_ref(&layer)))
        .unwrap();
    let as_base = flat(&inside(top, host(&based)));
    based.release().unwrap();
    let mut root = snap.prepare(Base::Scratch).unwrap();
    root.set_layers_needed(true);
    root.apply(&layer).unwrap();
    let h = inside(top, host(&root));
    let applied = flat(&h);
    assert_eq!(applied, as_base, "as the same layer as a base layer");
    let md = fs::symlink_metadata(h.join("owned/setuid")).unwrap();
    assert_eq!(
        (md.uid(), md.gid(), md.mode() & 0o7777),
        (1234, 5678, 0o4755)
    );
    let names: Vec<_> = fs::read_dir(h.join("nodes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["file"], "no device node or FIFO made");
    root.begin_layer().unwrap();
    let d = root.end_layer().unwrap();
    assert!(
        S::Root::walked(&d).is_empty(),
        "{:?}",
        changes(S::Root::walked(&d))
    );
    drop(d);
    root.release().unwrap();
}

#[test]
#[ignore = "T2c: default: chown to other owners and device nodes root could make"]
fn a_cached_layer_with_foreign_owners_and_device_nodes_applies_as_a_base_layer() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let f = Stages::new();
    owners_and_nodes_as_a_base_layer(&f.snap, &f.blobs(), &f.top, |r| r.mount_point().to_owned());
    let f = Shared::new(false);
    let root = f.root.clone();
    owners_and_nodes_as_a_base_layer(&f.snap, &f.blobs(), &f.top, |_| root.clone());
}

/// Red team (T18): device nodes a `RUN` planted with `mknod` (`CAP_MKNOD`
/// is a Docker default) where a cached layer writes are never opened, per
/// stage and on the shared root: `null`, `zero`, a loop block device and a
/// FIFO.
#[test]
#[ignore = "T2c: default: device nodes a RUN planted with mknod where a cached layer writes"]
fn red_team_device_nodes_a_run_planted_are_never_opened_by_a_cached_layer() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let nodes = [
        Node::Char(1, 3),
        Node::Char(1, 5),
        Node::Block(7, 0),
        Node::Fifo,
    ];
    let f = Stages::new();
    planted_nodes_are_never_opened(&f.snap, &f.blobs(), &f.top, &nodes, |r| {
        r.mount_point().to_owned()
    });
    for slash in [false, true] {
        let f = Shared::new(slash);
        let root = f.root.clone();
        planted_nodes_are_never_opened(&f.snap, &f.blobs(), &f.top, &nodes, |_| root.clone());
    }
}

/// The environment variable that tells the inner test it was started by
/// [`red_team_a_hostile_cached_layer_in_a_default_container_on_slash`].
const INNER_ENV: &str = "RNK_T2_APPLY_ON_SLASH";
/// The image the inner test runs in: the `bin`, `lib`, `lib64` and `sbin`
/// symlinks into `/usr`, which the container binds from the VM.
const IMAGE: &str = "rnk-t2-apply:latest";
const INNER: &str = "inner_a_hostile_cached_layer_on_a_containers_slash";

/// T2 (the VM, ADR-0063 rule 1: a volume of the VM is in reach): kaniko
/// mode on the `/` of a default `docker run` (Docker's capabilities,
/// seccomp and AppArmor profiles) applies a hostile cached layer; the
/// volume bound from the VM keeps exactly its canary, seen from outside.
#[test]
#[ignore = "T2: kaniko mode on the / of a default Docker container, with a volume of the VM"]
fn red_team_a_hostile_cached_layer_in_a_default_container_on_slash() {
    use std::process::{Command, Stdio};
    let have = Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    if !have.success() {
        let mut image = tar::Builder::new(Vec::new());
        for l in ["bin", "lib", "lib64", "sbin"] {
            let mut h = Header::new_gnu();
            h.set_entry_type(EntryType::Symlink);
            h.set_mode(0o777);
            h.set_size(0);
            image.append_link(&mut h, l, format!("usr/{l}")).unwrap();
        }
        let image = image.into_inner().unwrap();
        let mut import = Command::new("docker")
            .args(["import", "-", IMAGE])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        import.stdin.take().unwrap().write_all(&image).unwrap();
        assert!(import.wait().unwrap().success(), "docker import of {IMAGE}");
    }
    let vol = tempfile::tempdir().unwrap();
    fs::write(vol.path().join("canary"), b"VOLUME").unwrap();
    let exe = std::env::current_exe().unwrap();
    let out = Command::new("docker")
        .args(["run", "--rm", "--pull=never", "--network=none"])
        .arg("-v")
        .arg(format!("{}:/t/apply-test:ro", exe.display()))
        .args(["-v", "/usr:/usr:ro"])
        .arg("-v")
        .arg(format!("{}:/vol", vol.path().display()))
        .args(["-e", &format!("{INNER_ENV}=1"), IMAGE, "/t/apply-test"])
        .args([
            "--ignored",
            "--exact",
            "--nocapture",
            "--test-threads=1",
            INNER,
        ])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    println!("{text}");
    assert!(out.status.success(), "the inner test failed: {text}");
    assert!(
        text.contains("1 passed"),
        "the inner test did not run: {text}"
    );
    let names: Vec<_> = fs::read_dir(vol.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["canary"], "nothing added to the volume");
    assert_eq!(fs::read(vol.path().join("canary")).unwrap(), b"VOLUME");
}

/// Inner: [`red_team_hostile_layer_on_slash`] on the container's own `/`.
/// Does nothing unless its T2 test started it, and refuses a `/` that is
/// not a container's.
#[test]
#[ignore = "inner: started by red_team_a_hostile_cached_layer_in_a_default_container_on_slash"]
fn inner_a_hostile_cached_layer_on_a_containers_slash() {
    if std::env::var_os(INNER_ENV).is_none() {
        println!("not started by its T2 test: nothing to do");
        return;
    }
    assert!(
        Path::new("/.dockerenv").exists(),
        "kaniko mode on / only in a throwaway container"
    );
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let mounts = rnk_snapshot::kaniko::read_mountinfo().unwrap();
    assert!(
        mounts.iter().any(|m| m.mount_point == Path::new("/vol")),
        "the volume is a mount"
    );
    red_team_hostile_layer_on_slash(Path::new("/"), &mounts);
}
