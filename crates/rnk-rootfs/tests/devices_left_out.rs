//! Red team of extraction as a user namespace does it (`DeviceNodes::Skip`,
//! ADR-0106, ADR-0128, ADR-0131): hostile layers of devices, links to them,
//! whiteouts, opaque markers, symlinks and overlay's own xattr names, over
//! lower layers holding symlinks to the host (T1, T2, T15, T18). Run as a
//! regular user, where a device node that is not a whiteout cannot be made:
//! an attempt fails the extraction with `EPERM`.
//!
//! A test binary of its own: past the device-name budget the extraction
//! warns, and `rnk-rootfs`'s unit tests count that warning in one test.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, symlink};
use std::path::{Path, PathBuf};

use rnk_rootfs::{DeviceNodes, Error, RootPath, extract_overlay};
use tar::{EntryType, Header};

/// A temp dir holding `root/`, and `outside/` beside it with a sentinel;
/// removed on drop.
struct Dir {
    _tmp: tempfile::TempDir,
    host: PathBuf,
    root: RootPath,
}

impl Dir {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // The temp dir may sit below a symlink, which `RootPath::open` refuses.
        let host = fs::canonicalize(tmp.path()).unwrap().join("root");
        fs::create_dir(&host).unwrap();
        let out = host.with_file_name("outside");
        fs::create_dir_all(out.join("sub")).unwrap();
        fs::write(out.join("sentinel"), b"host").unwrap();
        fs::write(out.join("sub/file"), b"host").unwrap();
        let root = RootPath::open(&host).unwrap();
        Self {
            _tmp: tmp,
            host,
            root,
        }
    }

    fn host(&self, p: &str) -> PathBuf {
        self.host.join(p)
    }

    fn outside(&self) -> PathBuf {
        self.host.with_file_name("outside")
    }

    fn ids(&self) -> (u64, u64) {
        let md = fs::metadata(&self.host).unwrap();
        (md.uid().into(), md.gid().into())
    }
}

/// A tar of raw names, as a hostile layer has them.
struct Tar {
    out: Vec<u8>,
    ids: (u64, u64),
}

impl Tar {
    fn new(d: &Dir) -> Self {
        Self {
            out: Vec::new(),
            ids: d.ids(),
        }
    }

    fn push(&mut self, name: &[u8], t: EntryType, size: usize, edit: impl FnOnce(&mut Header)) {
        let mut h = Header::new_gnu();
        let old = h.as_old_mut();
        old.name = [0; 100];
        old.name[..name.len().min(100)].copy_from_slice(&name[..name.len().min(100)]);
        h.set_entry_type(t);
        h.set_mode(0o755);
        h.set_uid(self.ids.0);
        h.set_gid(self.ids.1);
        h.set_mtime(1_000_000);
        h.set_size(u64::try_from(size).unwrap());
        edit(&mut h);
        h.set_cksum();
        self.out.extend_from_slice(h.as_bytes());
    }

    fn data(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
        self.out
            .extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
    }

    fn entry(&mut self, name: &str, t: EntryType, data: &[u8]) -> &mut Self {
        if name.len() > 100 {
            self.push(b"././@LongLink", EntryType::GNULongName, name.len(), |_| {});
            self.data(name.as_bytes());
        }
        self.push(name.as_bytes(), t, data.len(), |_| {});
        self.data(data);
        self
    }

    fn dir(&mut self, name: &str) -> &mut Self {
        self.entry(name, EntryType::Directory, b"")
    }

    fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
        self.entry(name, EntryType::Regular, data)
    }

    fn link(&mut self, t: EntryType, name: &str, target: &str) -> &mut Self {
        self.push(name.as_bytes(), t, 0, |h| {
            let old = h.as_old_mut();
            old.linkname = [0; 100];
            old.linkname[..target.len()].copy_from_slice(target.as_bytes());
        });
        self
    }

    fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.link(EntryType::Symlink, name, target)
    }

    fn hardlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.link(EntryType::Link, name, target)
    }

    fn node(&mut self, name: &str, t: EntryType, major: u32, minor: u32) -> &mut Self {
        if name.len() > 100 {
            self.push(b"././@LongLink", EntryType::GNULongName, name.len(), |_| {});
            self.data(name.as_bytes());
        }
        self.push(name.as_bytes(), t, 0, |h| {
            h.set_device_major(major).unwrap();
            h.set_device_minor(minor).unwrap();
        });
        self
    }

    /// The xattrs of the next entry, in a PAX header.
    fn xattrs(&mut self, xattrs: &[(&str, &[u8])]) -> &mut Self {
        let mut data = Vec::new();
        for (name, value) in xattrs {
            let body = "SCHILY.xattr.".len() + name.len() + value.len() + 3;
            let mut len = body + 1;
            while len != body + len.to_string().len() {
                len = body + len.to_string().len();
            }
            data.extend_from_slice(format!("{len} SCHILY.xattr.{name}=").as_bytes());
            data.extend_from_slice(value);
            data.push(b'\n');
        }
        self.push(b"pax", EntryType::XHeader, data.len(), |_| {});
        self.data(&data);
        self
    }

    /// Device names past the budget for the names a layer remembers (about
    /// 1 MiB, ADR-0128): from the next device on, none is remembered.
    fn past_the_budget(&mut self) -> &mut Self {
        let long = format!("{}/", "n".repeat(250)).repeat(16);
        for i in 0..300 {
            self.node(&format!("{long}{i:03}"), EntryType::Char, 1, 3);
        }
        self
    }

    fn bytes(&self) -> Vec<u8> {
        let mut v = self.out.clone();
        v.extend(std::iter::repeat_n(0, 1024));
        v
    }
}

/// What may change under `dir`: each path's type and mode, size, mtime,
/// link count and inode.
type Snapshot = BTreeMap<PathBuf, (u32, u64, i64, u64, u64)>;

fn walk(dir: &Path, each: &mut dyn FnMut(&Path, &fs::Metadata)) {
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries {
            let p = e.unwrap().path();
            let md = fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                stack.push(p.clone());
            }
            each(&p, &md);
        }
    }
}

fn snapshot(dir: &Path) -> Snapshot {
    let mut out = BTreeMap::new();
    walk(dir, &mut |p, md| {
        let rel = p.strip_prefix(dir).unwrap().to_owned();
        out.insert(rel, (md.mode(), md.len(), md.mtime(), md.nlink(), md.ino()));
    });
    out
}

/// The inodes below `dir`, as (device, inode).
fn inodes(dir: &Path) -> BTreeSet<(u64, u64)> {
    let mut out = BTreeSet::new();
    walk(dir, &mut |_, md| {
        out.insert((md.dev(), md.ino()));
    });
    out
}

/// The xattrs of `p`, by name.
fn xattrs(p: &Path) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut names = vec![0; 64 << 10];
    let n = rustix::fs::llistxattr(p, &mut names).unwrap();
    names[..n]
        .split(|&b| b == 0)
        .filter(|n| !n.is_empty())
        .map(|n| {
            let mut v = vec![0; 64 << 10];
            let len = rustix::fs::lgetxattr(p, OsStr::from_bytes(n), &mut v).unwrap();
            (n.to_vec(), v[..len].to_vec())
        })
        .collect()
}

/// Every block or character device below `dir` that is not overlay's
/// whiteout (0/0), and every `user.overlay.*` xattr that overlay would read
/// as its own: all but the escaped `user.overlay.overlay.*` and the
/// `opaque=y` of a directory `opaque` allows.
fn overlay_hazards(dir: &Path, opaque: &dyn Fn(&Path) -> bool) -> Vec<String> {
    let mut found = Vec::new();
    walk(dir, &mut |p, md| {
        let rel = p.strip_prefix(dir).unwrap();
        let t = md.file_type();
        if (t.is_char_device() || t.is_block_device()) && md.rdev() != 0 {
            found.push(format!("device {}", rel.display()));
        }
        if t.is_symlink() {
            return;
        }
        for (name, value) in xattrs(p) {
            let Some(rest) = name.strip_prefix(b"user.overlay.") else {
                continue;
            };
            let marker = rest == b"opaque" && value == b"y" && md.is_dir() && opaque(rel);
            if !marker && !rest.starts_with(b"overlay.") {
                found.push(format!("{} {}", rel.display(), name.escape_ascii()));
            }
        }
    });
    found
}

fn is_entry_error(r: &rnk_rootfs::Result<rnk_rootfs::Extracted>) -> bool {
    matches!(r, Err(Error::Entry { .. }))
}

/// Red team: links to a left-out device, however spelled or chained, are left out with it (T18).
#[test]
fn red_team_links_to_a_left_out_device_are_left_out_however_spelled() {
    let lower = Dir::new();
    let names = [
        "dev/null", "dev/sda", "a/l1", "b/c/l2", "l3", "l4", "l5", "l6",
    ];
    for n in names {
        let p = lower.host(n);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"lower").unwrap();
    }
    let before = snapshot(&lower.host);
    let f = Dir::new();
    let mut t = Tar::new(&f);
    t.dir("dev/")
        .node("dev/null", EntryType::Char, 1, 3)
        .node("./dev//sda", EntryType::Block, 8, 0)
        .hardlink("a/l1", "/dev/null")
        .hardlink("b/c/l2", "./a/../a//l1")
        .hardlink("l3", "dev/./sda/")
        .hardlink("l4", "b/c/l2")
        .hardlink("l5", "x/../l4")
        .hardlink("l6", "//l5");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Skip,
    )
    .unwrap();
    for n in names {
        assert!(fs::symlink_metadata(f.host(n)).is_err(), "{n}");
    }
    assert_eq!(snapshot(&lower.host), before);
}

/// Red team: links through symlinks to a left-out device or the host never reach outside (T1, T2).
#[test]
fn red_team_links_through_symlinks_to_left_out_devices_never_reach_outside() {
    let lower = Dir::new();
    let out = lower.outside();
    let out_s = out.to_str().unwrap().to_owned();
    fs::create_dir(lower.host("dev")).unwrap();
    symlink(out.join("sentinel"), lower.host("dev/null")).unwrap();
    symlink(&out, lower.host("hostdir")).unwrap();
    for d in ["via", "abs", "up", "host"] {
        fs::create_dir(lower.host(d)).unwrap();
        fs::write(lower.host(&format!("{d}/null")), b"lower").unwrap();
    }
    let lower_before = snapshot(&lower.host);
    let out_before = snapshot(&out);
    let out_inodes = inodes(&out);

    let layer = |t: &mut Tar| {
        t.dir("dev/")
            .node("dev/null", EntryType::Char, 1, 3)
            .symlink("via", "dev")
            .symlink("abs", "/dev")
            .symlink("up", "../../../../../dev")
            .symlink("host", &out_s);
    };
    for (target, lowers) in [
        ("via/null", true),
        ("abs/null", true),
        ("up/null", true),
        ("host/sentinel", true),
        ("hostdir/sentinel", true),
        ("via/null", false),
        ("host/sentinel", false),
    ] {
        let f = Dir::new();
        let mut t = Tar::new(&f);
        layer(&mut t);
        t.hardlink("x", target);
        let below: &[&RootPath] = if lowers { &[&lower.root] } else { &[] };
        let r = extract_overlay(&f.root, t.bytes().as_slice(), below, DeviceNodes::Skip);
        assert!(is_entry_error(&r), "{target} {lowers}: {r:?}");
        assert!(fs::symlink_metadata(f.host("x")).is_err(), "{target}");
        assert!(inodes(&f.host).is_disjoint(&out_inodes), "{target}");
    }

    // Past the budget the device's name is not remembered: the link copies
    // the lower layer's symlink at that name, never what it points to.
    let f = Dir::new();
    let mut t = Tar::new(&f);
    t.past_the_budget()
        .node("dev/null", EntryType::Char, 1, 3)
        .hardlink("x", "dev/null");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Skip,
    )
    .unwrap();
    assert_eq!(fs::read_link(f.host("x")).unwrap(), out.join("sentinel"));
    assert!(inodes(&f.host).is_disjoint(&out_inodes));
    let f = Dir::new();
    let mut t = Tar::new(&f);
    t.past_the_budget()
        .node("hostdir/sentinel", EntryType::Char, 1, 3)
        .hardlink("y", "hostdir/sentinel");
    let r = extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Skip,
    );
    assert!(is_entry_error(&r), "{r:?}");

    assert_eq!(snapshot(&out), out_before);
    assert_eq!(snapshot(&lower.host), lower_before);
}

/// Red team: whiteouts and opaque markers never carry a left-out device (T18).
#[test]
fn red_team_whiteouts_and_opaque_markers_never_carry_a_left_out_device() {
    let lower = Dir::new();
    let mut t = Tar::new(&lower);
    t.file(".wh.w", b"")
        .dir("o/")
        .file("o/.wh..wh..opq", b"")
        .node("o/dev", EntryType::Char, 1, 3)
        .file("o/f", b"lower");
    extract_overlay(&lower.root, t.bytes().as_slice(), &[], DeviceNodes::Skip).unwrap();
    let lower_before = snapshot(&lower.host);

    for target in ["w", "o/.wh..wh..opq", "o/dev"] {
        let f = Dir::new();
        let mut t = Tar::new(&f);
        t.hardlink("l", target);
        let r = extract_overlay(
            &f.root,
            t.bytes().as_slice(),
            &[&lower.root],
            DeviceNodes::Skip,
        );
        assert!(is_entry_error(&r), "{target}: {r:?}");
        assert!(fs::symlink_metadata(f.host("l")).is_err(), "{target}");
    }

    let f = Dir::new();
    let mut t = Tar::new(&f);
    t.node("dev/null", EntryType::Char, 1, 3)
        .dir("o/")
        .hardlink("o/.wh..wh..opq", "dev/null")
        .node(".wh.k", EntryType::Char, 1, 3)
        .node("d/.wh.e", EntryType::Block, 8, 0)
        .node("x", EntryType::Char, 1, 3)
        .file(".wh.x", b"")
        .file("x", b"replaced")
        .hardlink("y", "x");
    extract_overlay(
        &f.root,
        t.bytes().as_slice(),
        &[&lower.root],
        DeviceNodes::Skip,
    )
    .unwrap();
    assert!(fs::symlink_metadata(f.host("o/.wh..wh..opq")).is_err());
    for w in ["k", "d/e"] {
        let md = fs::symlink_metadata(f.host(w)).unwrap();
        assert!(md.file_type().is_char_device() && md.rdev() == 0, "{w}");
    }
    assert_eq!(fs::read(f.host("y")).unwrap(), b"replaced");
    assert_eq!(
        fs::metadata(f.host("y")).unwrap().ino(),
        fs::metadata(f.host("x")).unwrap().ino()
    );
    assert_eq!(
        overlay_hazards(&f.host, &|p| p == Path::new("o")),
        Vec::<String>::new()
    );
    assert_eq!(
        xattrs(&f.host("o")).get(&b"user.overlay.opaque"[..]),
        Some(&b"y".to_vec())
    );
    assert_eq!(snapshot(&lower.host), lower_before);
}

/// Overlay's own xattr names: those it reads on an upper or lower inode as
/// metadata (Linux 6.8 included), under `user.` (`userxattr`, a user
/// namespace) and `trusted.`.
const OVERLAY_NAMES: [&str; 11] = [
    "opaque",
    "redirect",
    "origin",
    "impure",
    "nlink",
    "upper",
    "metacopy",
    "uuid",
    "protattr",
    "whiteout",
    "whiteouts",
];

/// Red team: overlay's own xattr names in a layer never become overlay metadata (T1, T15).
#[test]
fn red_team_overlay_xattr_names_from_a_layer_never_become_overlay_metadata() {
    let mut records: Vec<(String, Vec<u8>)> = Vec::new();
    for n in OVERLAY_NAMES {
        let value = match n {
            "redirect" => b"/etc".to_vec(),
            "opaque" | "whiteout" | "whiteouts" | "metacopy" => b"y".to_vec(),
            _ => b"x".to_vec(),
        };
        for prefix in ["user.overlay.", "trusted.overlay.", "user.overlay.overlay."] {
            records.push((format!("{prefix}{n}"), value.clone()));
        }
    }
    let records: Vec<(&str, &[u8])> = records
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_slice()))
        .collect();

    let lower = Dir::new();
    fs::write(lower.host("h"), b"lower").unwrap();
    for (name, value) in [
        ("user.overlay.metacopy", &b"y"[..]),
        ("user.overlay.redirect", b"/etc"),
        ("user.overlay.overlay.redirect", b"/kept"),
    ] {
        rustix::fs::setxattr(
            lower.host("h"),
            name,
            value,
            rustix::fs::XattrFlags::empty(),
        )
        .unwrap();
    }
    for devices in [DeviceNodes::Skip, DeviceNodes::Create] {
        let f = Dir::new();
        let mut t = Tar::new(&f);
        t.xattrs(&records)
            .dir("./")
            .xattrs(&records)
            .dir("d/")
            .xattrs(&records)
            .file("d/f", b"f")
            .xattrs(&records)
            .file("g", b"g")
            .dir("o/")
            .file("o/.wh..wh..opq", b"")
            .xattrs(&records)
            .dir("o/")
            .hardlink("hl", "h");
        extract_overlay(&f.root, t.bytes().as_slice(), &[&lower.root], devices).unwrap();
        assert_eq!(
            overlay_hazards(&f.host, &|p| p == Path::new("o")),
            Vec::<String>::new(),
            "{devices:?}"
        );
        // Round trips: escaped once more, to show as given through overlay.
        for p in ["", "d", "d/f", "g", "o"] {
            let x = xattrs(&f.host(p));
            assert_eq!(
                x.get(&b"user.overlay.overlay.redirect"[..]),
                Some(&b"/etc".to_vec()),
                "{devices:?} {p:?}"
            );
            assert_eq!(
                x.get(&b"user.overlay.overlay.overlay.whiteout"[..]),
                Some(&b"y".to_vec()),
                "{devices:?} {p:?}"
            );
        }
        // The control: the marker's opaque survives the later entry.
        assert_eq!(
            xattrs(&f.host("o")).get(&b"user.overlay.opaque"[..]),
            Some(&b"y".to_vec()),
            "{devices:?}"
        );
        let hl = xattrs(&f.host("hl"));
        assert_eq!(
            hl.keys()
                .filter(|k| k.starts_with(b"user."))
                .map(|k| k.escape_ascii().to_string())
                .collect::<Vec<_>>(),
            ["user.overlay.overlay.redirect"],
            "{devices:?}"
        );
    }
}

/// The controls of the checks above: each sees what it looks for.
#[test]
fn red_team_controls_the_checks_see_what_they_look_for() {
    let f = Dir::new();
    let out = f.outside();
    let before = snapshot(&out);
    fs::create_dir(f.host("d")).unwrap();
    fs::write(f.host("d/f"), b"x").unwrap();
    for (p, name, value) in [
        ("d/f", "user.overlay.redirect", &b"/etc"[..]),
        ("d", "user.overlay.opaque", b"y"),
    ] {
        rustix::fs::setxattr(f.host(p), name, value, rustix::fs::XattrFlags::empty()).unwrap();
    }
    assert_eq!(overlay_hazards(&f.host, &|_| false).len(), 2);
    assert_eq!(overlay_hazards(&f.host, &|p| p == Path::new("d")).len(), 1);
    fs::hard_link(out.join("sentinel"), f.host("h")).unwrap();
    assert!(!inodes(&f.host).is_disjoint(&inodes(&out)));
    assert_ne!(snapshot(&out), before);
    // Unprivileged, a device the extraction makes fails it with `mknod`.
    if !rustix::process::geteuid().is_root() {
        let f = Dir::new();
        let mut t = Tar::new(&f);
        t.node("null", EntryType::Char, 1, 3);
        let r = extract_overlay(&f.root, t.bytes().as_slice(), &[], DeviceNodes::Create);
        assert!(matches!(r, Err(Error::Io { op: "mknod", .. })), "{r:?}");
    }
}

/// One generated entry: a kind, a name, a target, an xattr.
type Gen = (u8, usize, usize, usize);

const GEN_NAMES: &[&str] = &[
    "dev/null",
    "dev",
    "a",
    "d",
    "d/b",
    "s",
    "s/x",
    "hostdir",
    "hostdir/x",
    "l",
    ".wh.a",
    "d/.wh.b",
    "d/.wh..wh..opq",
    "/x/../a",
    "q//r",
    "./",
];
const GEN_LINKS: &[&str] = &[
    "dev/null",
    "a",
    "d/b",
    "s/x",
    "hostdir/x",
    "hostdir/sentinel",
    "hostfile",
    "l",
    "missing",
    "q/r",
    "/dev/./null",
    "d",
];
/// Symlink targets: `@` is the host directory outside the root.
const GEN_SYMLINKS: &[&str] = &[
    "@",
    "@/sentinel",
    "/",
    "../../../..",
    "dev",
    "d",
    "/dev/null",
];
const GEN_XATTRS: &[&str] = &[
    "user.overlay.opaque",
    "user.overlay.redirect",
    "user.overlay.whiteout",
    "trusted.overlay.opaque",
    "user.x",
];

fn gen_layer(d: &Dir, entries: &[Gen]) -> Vec<u8> {
    let out = d.outside();
    let out = out.to_str().unwrap();
    let mut t = Tar::new(d);
    for &(kind, name, target, xattr) in entries {
        let name = GEN_NAMES[name % GEN_NAMES.len()];
        match kind % 9 {
            0 => t.file(name, b"upper"),
            1 => t.dir(name),
            2 => t.symlink(
                name,
                &GEN_SYMLINKS[target % GEN_SYMLINKS.len()].replace('@', out),
            ),
            3 => t.hardlink(name, GEN_LINKS[target % GEN_LINKS.len()]),
            4 => t.node(name, EntryType::Char, 1, 3),
            5 => t.node(name, EntryType::Block, 8, 0),
            6 => t.node(name, EntryType::Char, 0, 0),
            7 => t.node(name, EntryType::Fifo, 0, 0),
            8 => t.xattrs(&[(GEN_XATTRS[xattr % GEN_XATTRS.len()], b"y")]),
            _ => unreachable!("a kind modulo 9"),
        };
    }
    t.bytes()
}

/// Two lower layers as the namespace extracts them, over which generated
/// layers go: devices left out, a whiteout, an opaque directory, files at
/// the names links target, and symlinks to the host.
fn gen_lowers() -> (Dir, Dir) {
    let bottom = Dir::new();
    let out = bottom.outside();
    let out = out.to_str().unwrap().to_owned();
    let mut t = Tar::new(&bottom);
    t.dir("dev/")
        .file("dev/null", b"bottom")
        .node("dev/sda", EntryType::Block, 8, 0)
        .file("a", b"bottom")
        .dir("d/")
        .file("d/b", b"bottom")
        .file("l", b"bottom")
        .symlink("hostdir", &out)
        .symlink("hostfile", &format!("{out}/sentinel"))
        .dir("q/")
        .file("q/r", b"bottom");
    extract_overlay(&bottom.root, t.bytes().as_slice(), &[], DeviceNodes::Skip).unwrap();
    let top = Dir::new();
    let mut t = Tar::new(&top);
    t.node("dev/null", EntryType::Char, 1, 3)
        .file(".wh.l", b"")
        .dir("d/")
        .file("d/.wh..wh..opq", b"")
        .symlink("s", &out);
    extract_overlay(
        &top.root,
        t.bytes().as_slice(),
        &[&bottom.root],
        DeviceNodes::Skip,
    )
    .unwrap();
    (top, bottom)
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Red team: generated hostile layers make no device and stay in their root (T1, T2, T18).
    #[test]
    fn red_team_generated_layers_without_devices_stay_in_their_root(
        entries in proptest::collection::vec((0u8..9, 0..16usize, 0..12usize, 0..5usize), 1..10),
    ) {
        let (top, bottom) = gen_lowers();
        let f = Dir::new();
        let tar = gen_layer(&f, &entries);
        let watched = [
            f.outside(),
            top.outside(),
            bottom.outside(),
            top.host.clone(),
            bottom.host.clone(),
        ];
        let before: Vec<Snapshot> = watched.iter().map(|d| snapshot(d)).collect();
        let foreign: BTreeSet<(u64, u64)> = watched.iter().flat_map(|d| inodes(d)).collect();
        let lowers = [&top.root, &bottom.root];
        let r = extract_overlay(&f.root, tar.as_slice(), &lowers, DeviceNodes::Skip);
        proptest::prop_assert!(
            !matches!(&r, Err(Error::Io { op: "mknod", .. })),
            "a device was attempted: {r:?}"
        );
        // An opaque marker lands where `d` leads in the root.
        let hazards = overlay_hazards(&f.host, &|_| true);
        proptest::prop_assert!(hazards.is_empty(), "{hazards:?} ({r:?})");
        proptest::prop_assert!(inodes(&f.host).is_disjoint(&foreign), "{r:?}");
        for (d, b) in watched.iter().zip(&before) {
            proptest::prop_assert_eq!(&snapshot(d), b, "{}", d.display());
        }
    }
}
