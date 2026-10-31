//! Red-team tests of kaniko mode's keep area (T0, unprivileged): hostile
//! `COPY --from=<image>` layers unpacked beside the root
//! (`Snapshotter::prepare_source`), hostile stage roots kept
//! (`StageRootfs::keep`) and copied back for a stage built `FROM` them.
//!
//! Every test plants the same things around the root: `outside/secret`
//! beside it (a host file the build must never reach) and `state/blob`
//! inside it, on the ignore list (as the state directory is on `/`). An
//! attack may fail; it must never write, delete or copy either, nor leave a
//! kept directory behind once its stage is released (threat model T1, T4).

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use rnk_oci::Digest;
use rnk_rootfs::{IgnoreList, RootPath};
use rnk_snapshot::{Base, Kaniko, KanikoLayer, Keep, Snapshotter, StageRootfs, WalkOptions};
use sha2::{Digest as _, Sha256};
use tar::{EntryType, Header};

const HOST: &[u8] = b"HOST-SECRET";
const STATE: &[u8] = b"STATE-SECRET";

struct Fx {
    _tmp: tempfile::TempDir,
    /// The temp directory, holding `root`, `kept`, `outside`, `blobs`.
    top: PathBuf,
    root: PathBuf,
    outside: PathBuf,
    kaniko: Kaniko,
}

impl Fx {
    /// A root other than `/`, its keep area beside it.
    fn new() -> Self {
        Self::shown(None)
    }

    /// As [`Fx::new`], the snapshotter told its root is `shown` (`/` makes
    /// it behave as on `/`: the first base goes over the root).
    fn shown(shown: Option<&str>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let top = fs::canonicalize(tmp.path()).unwrap();
        let root = top.join("root");
        let outside = top.join("outside");
        fs::create_dir_all(root.join("state")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("state/blob"), STATE).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), HOST).unwrap();
        fs::create_dir(top.join("kept")).unwrap();
        fs::create_dir(top.join("blobs")).unwrap();
        let mut ignore = IgnoreList::new();
        ignore.insert("/state").unwrap();
        let kaniko = Kaniko::new(
            RootPath::open(&root).unwrap(),
            shown.map_or_else(|| root.clone(), PathBuf::from),
            ignore,
            WalkOptions::default(),
            RootPath::open(top.join("kept")).unwrap(),
        );
        Self {
            _tmp: tmp,
            top,
            root,
            outside,
            kaniko,
        }
    }

    /// Nothing outside the root changed, the ignored state is intact, and
    /// no kept copy holds a secret.
    fn check(&self, what: &str) {
        assert_eq!(
            names(&self.top),
            ["blobs", "kept", "outside", "root"],
            "{what}: the temp directory"
        );
        assert_eq!(names(&self.outside), ["secret"], "{what}: outside");
        assert_eq!(
            fs::read(self.outside.join("secret")).unwrap(),
            HOST,
            "{what}"
        );
        assert_eq!(names(&self.root.join("state")), ["blob"], "{what}: state");
        assert_eq!(
            fs::read(self.root.join("state/blob")).unwrap(),
            STATE,
            "{what}"
        );
        for f in files_below(&self.top.join("kept")) {
            let data = fs::read(&f).unwrap();
            assert!(
                data != HOST && data != STATE,
                "{what}: a kept copy holds a secret at {}",
                f.display()
            );
        }
    }

    fn kept_entries(&self) -> Vec<String> {
        names(&self.top.join("kept"))
    }

    /// A layer blob of `tar`, as the store would hand it over.
    fn layer(&self, name: &str, tar: &RawTar) -> KanikoLayer {
        let mut out = tar.0.clone();
        out.extend(std::iter::repeat_n(0, 1024));
        let hash: [u8; 32] = Sha256::digest(&out).into();
        let blob = self.top.join("blobs").join(name);
        fs::write(&blob, &out).unwrap();
        self.kaniko
            .base_layer(&Digest::from_sha256_bytes(&hash), &blob, &[])
            .unwrap()
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

/// Every regular file below `dir`, never following a symlink.
fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd {
            let e = e.unwrap();
            let t = e.file_type().unwrap();
            if t.is_dir() {
                todo.push(e.path());
            } else if t.is_file() {
                out.push(e.path());
            }
        }
    }
    out
}

/// A tar written header by header, names and link names taken as bytes:
/// `..`, absolute names and anything else the `tar` crate refuses.
#[derive(Default)]
struct RawTar(Vec<u8>);

impl RawTar {
    fn entry(&mut self, name: &str, t: EntryType, link: &str, mode: u32, data: &[u8]) -> &mut Self {
        let mut h = Header::new_gnu();
        {
            let old = h.as_old_mut();
            old.name[..name.len()].copy_from_slice(name.as_bytes());
            old.linkname[..link.len()].copy_from_slice(link.as_bytes());
        }
        h.set_entry_type(t);
        h.set_mode(mode);
        h.set_uid(u64::from(rustix::process::geteuid().as_raw()));
        h.set_gid(u64::from(rustix::process::getegid().as_raw()));
        h.set_mtime(1_000_000);
        h.set_size(u64::try_from(data.len()).unwrap());
        let dev = if matches!(t, EntryType::Char | EntryType::Block) {
            (1, 3)
        } else {
            (0, 0)
        };
        h.set_device_major(dev.0).unwrap();
        h.set_device_minor(dev.1).unwrap();
        h.set_cksum();
        self.0.extend_from_slice(h.as_bytes());
        self.0.extend_from_slice(data);
        self.0
            .extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
        self
    }

    fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
        self.entry(name, EntryType::Regular, "", 0o644, data)
    }

    fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.entry(name, EntryType::Symlink, target, 0o777, b"")
    }

    fn hardlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.entry(name, EntryType::Link, target, 0o644, b"")
    }
}

/// Runs `f` on another thread, failing when it takes longer than 20 s: a
/// FIFO opened for reading would block forever.
fn within_time<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(20))
        .expect("blocked: a FIFO or device was opened")
}

#[test]
fn hostile_copy_from_images_stay_in_their_kept_directory() {
    let f = Fx::new();
    // A stage holds the shared root meanwhile, as when its COPY --from
    // runs; its files must not change either.
    let holder = f.kaniko.prepare(Base::Scratch).unwrap();
    fs::write(f.root.join("holder-file"), b"h").unwrap();
    let out = f.outside.to_str().unwrap();
    let cases: Vec<(&str, RawTar)> = vec![
        ("dot-dot names", {
            let mut t = RawTar::default();
            t.file("../outside/pwn", b"PWN")
                .file("../../pwn", b"PWN")
                .file("a/../../outside/pwn", b"PWN");
            t
        }),
        ("an absolute name", {
            let mut t = RawTar::default();
            t.file("/abs-pwn", b"PWN");
            t
        }),
        ("a file through an absolute host link", {
            let mut t = RawTar::default();
            t.symlink("l", out).file("l/pwn", b"PWN");
            t
        }),
        ("a file through a relative link climbing out", {
            let mut t = RawTar::default();
            t.symlink("l", "../../outside").file("l/pwn", b"PWN");
            t
        }),
        ("a file through a link to /", {
            let mut t = RawTar::default();
            t.symlink("l", "/").file("l/holder-file", b"PWN");
            t
        }),
        ("a chain of links", {
            let mut t = RawTar::default();
            t.symlink("a", "b")
                .symlink("b", "c/../../../outside")
                .file("a/pwn", b"PWN");
            t
        }),
        ("a link loop", {
            let mut t = RawTar::default();
            t.symlink("x", "y").symlink("y", "x").file("x/pwn", b"PWN");
            t
        }),
        ("a hardlink climbing out", {
            let mut t = RawTar::default();
            t.hardlink("h", "../outside/secret");
            t
        }),
        ("a hardlink through a host link", {
            let mut t = RawTar::default();
            t.symlink("l", out).hardlink("h", "l/secret");
            t
        }),
        ("a hardlink to the shared root's state", {
            let mut t = RawTar::default();
            t.hardlink("h", "../root/state/blob");
            t
        }),
        ("a whiteout through a host link", {
            let mut t = RawTar::default();
            t.symlink("l", out).file("l/.wh.secret", b"");
            t
        }),
        ("whiteouts climbing out", {
            let mut t = RawTar::default();
            t.file("../outside/.wh.secret", b"")
                .file(".wh...", b"")
                .file("../.wh..wh..opq", b"");
            t
        }),
        ("an opaque marker through a host link", {
            let mut t = RawTar::default();
            t.symlink("l", out).file("l/.wh..wh..opq", b"");
            t
        }),
        ("a directory replaced by a host link", {
            let mut t = RawTar::default();
            t.entry("d", EntryType::Directory, "", 0o755, b"")
                .symlink("d", out)
                .file("d/pwn", b"PWN");
            t
        }),
        ("a device node", {
            let mut t = RawTar::default();
            t.entry("c", EntryType::Char, "", 0o666, b"");
            t
        }),
        ("a FIFO", {
            let mut t = RawTar::default();
            t.entry("p", EntryType::Fifo, "", 0o644, b"");
            t
        }),
        ("a setuid file", {
            let mut t = RawTar::default();
            t.entry("suid", EntryType::Regular, "", 0o4755, b"x");
            t
        }),
    ];
    for (i, (what, tar)) in cases.into_iter().enumerate() {
        let layer = f.layer(&format!("l{i}"), &tar);
        match f.kaniko.prepare_source(&[layer]) {
            Ok(mut source) => {
                let view = source.view().unwrap();
                // Whatever landed is inside the kept copy: `/` is the copy.
                assert!(view.statx("state").is_err(), "{what}");
                // Device nodes and FIFOs are left out, as on the root
                // (ADR-0041); a setuid file keeps its bits, owned by the
                // test user.
                assert!(view.statx("c").is_err(), "{what}: a device node");
                assert!(view.statx("p").is_err(), "{what}: a FIFO");
                if let Ok(st) = view.statx("suid") {
                    assert_eq!(st.mode & 0o7777, 0o4755, "{what}");
                }
                if let Ok(st) = view.statx("pwn") {
                    assert_eq!(st.file_type(), rnk_rootfs::FileType::Regular, "{what}");
                }
                f.check(what);
                source.release().unwrap();
            }
            Err(e) => {
                eprintln!("{what}: refused: {e}");
                f.check(what);
            }
        }
        assert_eq!(
            f.kept_entries(),
            Vec::<String>::new(),
            "{what}: left behind"
        );
        assert_eq!(names(&f.root), ["holder-file", "state"], "{what}: the root");
        assert_eq!(
            fs::read(f.root.join("holder-file")).unwrap(),
            b"h",
            "{what}"
        );
    }
    holder.release().unwrap();
}

/// A stage root planted with links out of it, to the ignored state and to
/// itself, special files and odd metadata; `f.root` is held by `stage`.
fn plant_hostile(f: &Fx) {
    use std::os::unix::fs::symlink;
    let r = &f.root;
    symlink(&f.outside, r.join("host")).unwrap();
    symlink("../../outside", r.join("climb")).unwrap();
    symlink("/state", r.join("to-state")).unwrap();
    symlink("../state/blob", r.join("to-blob")).unwrap();
    symlink("/", r.join("to-root")).unwrap();
    symlink("loop", r.join("loop")).unwrap();
    fs::create_dir_all(r.join("app/dist")).unwrap();
    fs::write(r.join("app/dist/main.js"), b"js").unwrap();
    symlink("../../state", r.join("app/dist/state")).unwrap();
    fs::hard_link(r.join("app/dist/main.js"), r.join("app/main-link.js")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        r.join("app/fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o640),
        0,
    )
    .unwrap();
    let sock = std::os::unix::net::UnixListener::bind(r.join("app/sock")).unwrap();
    drop(sock);
    fs::write(r.join("app/suid"), b"#!/bin/sh\n").unwrap();
    fs::set_permissions(r.join("app/suid"), fs::Permissions::from_mode(0o4755)).unwrap();
    fs::create_dir(r.join("app/sgid")).unwrap();
    fs::set_permissions(r.join("app/sgid"), fs::Permissions::from_mode(0o2775)).unwrap();
    fs::create_dir(r.join("app/sticky")).unwrap();
    fs::set_permissions(r.join("app/sticky"), fs::Permissions::from_mode(0o1777)).unwrap();
    fs::create_dir(r.join("app/private")).unwrap();
    fs::set_permissions(r.join("app/private"), fs::Permissions::from_mode(0o700)).unwrap();
    // An xattr, when the filesystem takes user ones.
    let _ = rustix::fs::setxattr(
        r.join("app/dist/main.js"),
        "user.rnk",
        b"v",
        rustix::fs::XattrFlags::empty(),
    );
}

#[test]
fn hostile_paths_given_to_keep_never_copy_a_secret() {
    let f = Fx::new();
    let mut stage = f.kaniko.prepare(Base::Scratch).unwrap();
    plant_hostile(&f);
    // The builder hands `keep` physical paths (its link closure); these are
    // what a careless caller could hand it. Each keep replaces the last.
    let cases = [
        "host/secret",
        "climb/secret",
        "../outside/secret",
        "/../../outside/secret",
        "to-state",
        "to-blob",
        "state",
        "state/blob",
        "/state/../state/blob",
        "loop/x",
        "app/fifo",
        "app/sock",
        "app",
    ];
    for p in cases {
        let r = within_time({
            let k = Keep::Paths(vec![PathBuf::from(p)]);
            // `keep` needs the stage; move it in and back out.
            let mut s = stage;
            move || {
                let r = s.keep(&k).map_err(|e| e.to_string());
                (s, r)
            }
        });
        stage = r.0;
        if let Err(e) = &r.1 {
            eprintln!("{p}: refused: {e}");
        }
        f.check(p);
    }
    stage.release().unwrap();
    assert_eq!(f.kept_entries(), Vec::<String>::new());
}

/// Finding: `Keep::Paths` checks the ignore list on the path as given, then
/// resolves it following links: a path through a link (`to-root ->` `/`,
/// `app/dist/state ->` `../../state`) copies the ignored state into the kept
/// copy. The builder only hands over physical paths (`link_closure`), so a
/// Dockerfile cannot reach this (`tests/keep.rs` of `rnk-builder`); the
/// snapshotter does not enforce it.
#[test]
#[ignore = "red-team finding: Keep::Paths through a link copies an ignored path"]
fn keep_paths_through_a_link_never_copy_the_ignored_state() {
    let f = Fx::new();
    let mut stage = f.kaniko.prepare(Base::Scratch).unwrap();
    plant_hostile(&f);
    for p in ["to-root/state/blob", "app/dist/state/blob", "to-root/state"] {
        if let Err(e) = stage.keep(&Keep::Paths(vec![PathBuf::from(p)])) {
            eprintln!("{p}: refused: {e}");
        }
        f.check(p);
    }
    stage.release().unwrap();
}

#[test]
fn the_link_paths_the_builder_keeps_never_copy_a_secret() {
    // What `SourceRoot::keep` hands over for a source through links: each
    // link on the way, then the physical entry. A link into the ignored
    // state is kept as the link; the state itself never is.
    let f = Fx::new();
    let mut stage = f.kaniko.prepare(Base::Scratch).unwrap();
    plant_hostile(&f);
    let paths = [
        "to-state",
        "state/blob",
        "to-blob",
        "app/dist/state",
        "host",
    ]
    .map(PathBuf::from)
    .to_vec();
    match stage.keep(&Keep::Paths(paths)) {
        Ok(()) => {
            let view = stage.view().unwrap();
            assert!(view.statx("state").is_err(), "the state was kept");
        }
        Err(e) => eprintln!("refused: {e}"),
    }
    f.check("link closure");
    stage.release().unwrap();
}

#[test]
fn keeping_special_files_never_blocks_and_never_escapes() {
    let f = Fx::new();
    let mut stage = f.kaniko.prepare(Base::Scratch).unwrap();
    plant_hostile(&f);
    let (stage, r) = within_time(move || {
        let r = stage.keep(&Keep::All).map_err(|e| e.to_string());
        (stage, r)
    });
    let mut stage = stage;
    r.unwrap();
    f.check("keep all");
    let view = stage.view().unwrap();
    assert!(view.statx("state").is_err(), "the ignored state was kept");
    let fifo = view.statx("app/fifo").unwrap();
    assert_eq!(fifo.file_type(), rnk_rootfs::FileType::Fifo);
    assert_eq!(view.read_link("host").unwrap(), f.outside, "kept as a link");
    stage.release().unwrap();
}

/// What a test compares of an entry: type, mode, owner, size, mtime, link
/// target, content, xattr, and which other names share its inode.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    kind: String,
    mode: u32,
    owner: (u32, u32),
    mtime: (i64, i64),
    target: Option<PathBuf>,
    data: Option<Vec<u8>>,
    xattr: Option<Vec<u8>>,
    links: Vec<String>,
}

/// The entries below `dir` (relative names), `skip` left out.
fn tree(dir: &Path, skip: &[&str]) -> Vec<(String, Entry)> {
    let mut out = Vec::new();
    let mut by_ino: std::collections::BTreeMap<u64, Vec<String>> = Default::default();
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        for e in fs::read_dir(dir.join(&rel)).unwrap() {
            let e = e.unwrap();
            let name = rel.join(e.file_name());
            let shown = name.to_string_lossy().into_owned();
            if skip.contains(&shown.as_str()) {
                continue;
            }
            let full = dir.join(&name);
            let m = fs::symlink_metadata(&full).unwrap();
            let t = m.file_type();
            let kind = if t.is_dir() {
                todo.push(name.clone());
                "dir"
            } else if t.is_symlink() {
                "link"
            } else if t.is_fifo() {
                "fifo"
            } else if t.is_socket() {
                "socket"
            } else {
                "file"
            };
            if t.is_file() && m.nlink() > 1 {
                by_ino.entry(m.ino()).or_default().push(shown.clone());
            }
            let mut xattr = vec![0; 16];
            let xattr = rustix::fs::lgetxattr(&full, "user.rnk", &mut xattr)
                .ok()
                .map(|n| xattr[..n].to_vec());
            out.push((
                shown,
                Entry {
                    kind: kind.to_owned(),
                    mode: m.mode() & 0o7777,
                    owner: (m.uid(), m.gid()),
                    // A directory's mtime moves with what is written into
                    // it; the copy sets it last, so it is compared too.
                    mtime: (m.mtime(), m.mtime_nsec()),
                    target: t.is_symlink().then(|| fs::read_link(&full).unwrap()),
                    data: t.is_file().then(|| {
                        let mut v = Vec::new();
                        fs::File::open(&full).unwrap().read_to_end(&mut v).unwrap();
                        v
                    }),
                    xattr,
                    links: Vec::new(),
                },
            ));
        }
    }
    for (name, e) in &mut out {
        if let Some(group) = by_ino.values().find(|g| g.contains(name)) {
            e.links = group.clone();
            e.links.sort();
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn a_stage_built_from_a_kept_hostile_stage_sees_it_as_it_was() {
    for shown in [None, Some("/")] {
        let f = Fx::shown(shown);
        let mut a = f.kaniko.prepare(Base::Scratch).unwrap();
        plant_hostile(&f);
        // The socket is no part of a layer; it is compared on its own.
        let before = tree(&f.root, &["state", "state/blob", "app/sock"]);
        a.keep(&Keep::All).unwrap();
        // Another stage takes the root, plants links to where the copy will
        // write, then gives it up.
        let other = f.kaniko.prepare(Base::Scratch).unwrap();
        std::os::unix::fs::symlink(&f.outside, f.root.join("app")).unwrap();
        other.release().unwrap();

        let child = f.kaniko.prepare(Base::Stage(&a)).unwrap();
        f.check("copy back");
        let after = tree(&f.root, &["state", "state/blob", "app/sock"]);
        assert_eq!(
            before.iter().map(|e| &e.0).collect::<Vec<_>>(),
            after.iter().map(|e| &e.0).collect::<Vec<_>>(),
            "{shown:?}: the names"
        );
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(b, a, "{shown:?}: {} differs after the copy back", b.0);
        }
        let sock = fs::symlink_metadata(f.root.join("app/sock")).unwrap();
        assert!(
            sock.file_type().is_socket() || sock.file_type().is_file(),
            "{shown:?}: the socket became {:?}",
            sock.file_type()
        );
        child.release().unwrap();
        a.release().unwrap();
        assert_eq!(f.kept_entries(), Vec::<String>::new());
    }
}

#[test]
fn a_deep_tree_is_kept_and_deleted_or_refused_cleanly() {
    // Two descriptors per level while copying: the soft limit raised to
    // the hard one, as `ranikust` raises it, so that the other tests of
    // this process keep theirs.
    let lim = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    if let Some(max) = lim.maximum {
        let raised = rustix::process::Rlimit {
            current: Some(max),
            maximum: Some(max),
        };
        let _ = rustix::process::setrlimit(rustix::process::Resource::Nofile, raised);
    }
    let f = Fx::new();
    let mut stage = f.kaniko.prepare(Base::Scratch).unwrap();
    // Longer than PATH_MAX by name: 520 levels of 8 bytes. Kept short of
    // thousands of levels: the ignore list hashes every ancestor of every
    // entry, which grows with the cube of the depth (3,000 levels: 11 s in
    // a release build, a T15 note of the red-team report).
    let depth = 520;
    let name = "abcdefg";
    let mut dir = rustix::fs::open(
        &f.root,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    for _ in 0..depth {
        rustix::fs::mkdirat(&dir, name, rustix::fs::Mode::from_raw_mode(0o755)).unwrap();
        dir = rustix::fs::openat(
            &dir,
            name,
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
    }
    drop(dir);
    match stage.keep(&Keep::All) {
        Ok(()) => {
            // Walked down by descriptor: too long a path to name.
            let kept = f.top.join("kept").join(&f.kept_entries()[0]);
            let mut dir = rustix::fs::open(
                &kept,
                rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            for level in 0..depth {
                dir = rustix::fs::openat(
                    &dir,
                    name,
                    rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY,
                    rustix::fs::Mode::empty(),
                )
                .unwrap_or_else(|e| panic!("level {level} of the copy: {e}"));
            }
        }
        Err(e) => {
            eprintln!("refused: {e}");
            assert_eq!(f.kept_entries(), Vec::<String>::new(), "partial copy left");
        }
    }
    f.check("deep tree");
    stage.release().unwrap();
    assert_eq!(f.kept_entries(), Vec::<String>::new(), "kept copy left");
}
