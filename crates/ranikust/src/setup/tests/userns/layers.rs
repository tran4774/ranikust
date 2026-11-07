//! Base layers extracted, published and cleaned up in the build's user
//! namespace, as its root (#77, ADR-0099, ADR-0106).

use std::io::{BufRead as _, BufReader};
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileTypeExt as _;
use std::path::PathBuf;
use std::time::Duration;

use rnk_oci::Digest;
use rnk_snapshot::Overlay;
use sha2::{Digest as _, Sha256};
use tar::{EntryType, Header};

use super::*;

/// The namespace's root, and a user other than root.
const ROOT: (u32, u32) = (0, 0);
const USER: (u32, u32) = (1000, 1000);
/// An owner no map of a test holds: each maps 65537 ids at most.
const FAR: (u32, u32) = (3_000_000_000, 3_000_000_000);

/// Every entry's mtime in the tars.
const MTIME: i64 = 1_000_000;

/// `security.capability` v2: `cap_net_raw` permitted and effective.
const CAP_NET_RAW: [u8; 20] = [
    0x01, 0x00, 0x00, 0x02, 0x00, 0x20, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// A plain tar, entry by entry, each with its own owner.
#[derive(Default)]
struct Tar(Vec<u8>);

impl Tar {
    fn push(
        &mut self,
        name: &str,
        t: EntryType,
        (uid, gid): (u32, u32),
        mode: u32,
        data: &[u8],
        edit: impl FnOnce(&mut Header),
    ) -> &mut Self {
        let mut h = Header::new_gnu();
        let old = h.as_old_mut();
        old.name = [0; 100];
        old.name[..name.len()].copy_from_slice(name.as_bytes());
        h.set_entry_type(t);
        h.set_mode(mode);
        h.set_uid(u64::from(uid));
        h.set_gid(u64::from(gid));
        h.set_mtime(MTIME.unsigned_abs());
        h.set_size(u64::try_from(data.len()).unwrap());
        edit(&mut h);
        h.set_cksum();
        self.0.extend_from_slice(h.as_bytes());
        self.0.extend_from_slice(data);
        self.0
            .extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
        self
    }

    fn dir(&mut self, name: &str, ids: (u32, u32), mode: u32) -> &mut Self {
        self.push(name, EntryType::Directory, ids, mode, b"", |_| {})
    }

    fn file(&mut self, name: &str, ids: (u32, u32), mode: u32, data: &[u8]) -> &mut Self {
        self.push(name, EntryType::Regular, ids, mode, data, |_| {})
    }

    fn node(
        &mut self,
        name: &str,
        t: EntryType,
        ids: (u32, u32),
        mode: u32,
        dev: (u32, u32),
    ) -> &mut Self {
        self.push(name, t, ids, mode, b"", |h| {
            h.set_device_major(dev.0).unwrap();
            h.set_device_minor(dev.1).unwrap();
        })
    }

    fn symlink(&mut self, name: &str, ids: (u32, u32), target: &str) -> &mut Self {
        self.push(name, EntryType::Symlink, ids, 0o777, b"", |h| {
            h.set_link_name(target).unwrap();
        })
    }

    fn hardlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.push(name, EntryType::Link, ROOT, 0o644, b"", |h| {
            h.set_link_name(target).unwrap();
        })
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
        self.push("pax", EntryType::XHeader, ROOT, 0o644, &data, |_| {})
    }

    /// The tar and its diff ID.
    fn finish(&mut self) -> (Vec<u8>, Digest) {
        self.0.extend(std::iter::repeat_n(0, 1024));
        let hash: [u8; 32] = Sha256::digest(&self.0).into();
        (self.0.clone(), Digest::from_sha256_bytes(&hash))
    }
}

/// The bottom layer: a `0555` root of root's, owners other than root, a
/// user's symlink, a hardlink, setuid, setgid and sticky modes, `user.*`
/// and `security.capability` xattrs, a FIFO and two device nodes. Also the
/// length of its part up to `etc/owned`, which ends at an entry.
fn bottom() -> (Vec<u8>, Digest, usize) {
    let mut t = Tar::default();
    t.dir("./", ROOT, 0o555)
        .dir("bin/", ROOT, 0o755)
        .file("bin/su", ROOT, 0o4755, b"su")
        .hardlink("bin/su-again", "bin/su")
        .file("bin/wall", (0, 5), 0o2755, b"wall")
        .dir("etc/", ROOT, 0o755)
        .file("etc/owned", (2, 3), 0o640, b"owned");
    let cut = t.0.len();
    t.xattrs(&[
        ("security.capability", &CAP_NET_RAW),
        ("user.origin", b"ping"),
    ])
    .file("bin/ping", ROOT, 0o755, b"ping")
    .dir("dev/", ROOT, 0o755)
    .node("dev/null", EntryType::Char, ROOT, 0o666, (1, 3))
    .node("dev/loop0", EntryType::Block, (0, 6), 0o660, (7, 0))
    .dir("home/", ROOT, 0o755)
    .dir("home/u/", USER, 0o700)
    .file("home/u/f", USER, 0o600, b"mine")
    .symlink("home/u/link", USER, "f")
    .dir("lib/", ROOT, 0o755)
    .xattrs(&[("user.k", b"v")])
    .file("lib/shared", (2, 3), 0o640, b"shared")
    .dir("opq/", ROOT, 0o755)
    .file("opq/a", ROOT, 0o644, b"a")
    .dir("run/", ROOT, 0o755)
    .node("run/fifo", EntryType::Fifo, ROOT, 0o600, (0, 0))
    .dir("srv/", (0, 5), 0o2775)
    .dir("tmp/", ROOT, 0o1777)
    .file("gone", ROOT, 0o644, b"gone");
    let (tar, id) = t.finish();
    (tar, id, cut)
}

/// The layer above it: a `0555` root of a user's, a whiteout, an opaque
/// directory and a hardlink to a file of the bottom layer.
fn top() -> (Vec<u8>, Digest) {
    Tar::default()
        .dir("./", USER, 0o555)
        .file(".wh.gone", ROOT, 0o644, b"")
        .dir("lib/", ROOT, 0o755)
        .hardlink("lib/link", "lib/shared")
        .dir("opq/", ROOT, 0o755)
        .file("opq/.wh..wh..opq", ROOT, 0o644, b"")
        .file("opq/b", ROOT, 0o644, b"b")
        .finish()
}

/// [`dump`] of the bottom layer as extracted in a user namespace: as
/// overlay as root extracts it ([`DEVICES`] aside).
fn bottom_dump() -> String {
    let cap = CAP_NET_RAW.escape_ascii();
    [
        ". d 0:0 555".to_owned(),
        "bin d 0:0 755".to_owned(),
        format!("bin/ping f 0:0 755 \"ping\" security.capability={cap} user.origin=ping"),
        "bin/su f 0:0 4755 links=2 \"su\"".to_owned(),
        "bin/su-again f 0:0 4755 links=2 \"su\"".to_owned(),
        "bin/wall f 0:5 2755 \"wall\"".to_owned(),
        "dev d 0:0 755".to_owned(),
        "etc d 0:0 755".to_owned(),
        "etc/owned f 2:3 640 \"owned\"".to_owned(),
        "gone f 0:0 644 \"gone\"".to_owned(),
        "home d 0:0 755".to_owned(),
        "home/u d 1000:1000 700".to_owned(),
        "home/u/f f 1000:1000 600 \"mine\"".to_owned(),
        "home/u/link l 1000:1000 777 -> f".to_owned(),
        "lib d 0:0 755".to_owned(),
        "lib/shared f 2:3 640 \"shared\" user.k=v".to_owned(),
        "opq d 0:0 755".to_owned(),
        "opq/a f 0:0 644 \"a\"".to_owned(),
        "run d 0:0 755".to_owned(),
        "run/fifo p 0:0 600".to_owned(),
        "srv d 0:5 2775".to_owned(),
        "tmp d 0:0 1777".to_owned(),
    ]
    .join("\n")
}

/// What overlay as root also makes of the bottom layer: its device nodes.
const DEVICES: [&str; 2] = ["dev/loop0 b 0:6 660 7,0", "dev/null c 0:0 666 1,3"];

/// [`dump`] of the top layer: the whiteout is the 0/0 node, the opaque
/// directory has overlay's xattr, the hardlink a copy of the lower file.
const TOP_DUMP: &str = "\
. d 1000:1000 555
gone c 0:0 0 0,0
lib d 0:0 755
lib/link f 2:3 640 \"shared\" user.k=v
opq d 0:0 755 user.overlay.opaque=y
opq/b f 0:0 644 \"b\"";

/// Each entry below `root` as this process sees it, one sorted line each:
/// path, type, owner, mode, then the links to a file and its contents, a
/// symlink's target, the numbers of a device, an mtime other than the
/// tars' [`MTIME`] and every xattr but `security.selinux`.
fn dump(root: &Path) -> String {
    let mut lines = Vec::new();
    let mut todo = vec![PathBuf::from(".")];
    while let Some(rel) = todo.pop() {
        let p = root.join(&rel);
        let m = fs::symlink_metadata(&p).unwrap();
        let t = m.file_type();
        let kind = match () {
            () if t.is_dir() => "d",
            () if t.is_file() => "f",
            () if t.is_symlink() => "l",
            () if t.is_char_device() => "c",
            () if t.is_block_device() => "b",
            () if t.is_fifo() => "p",
            () => "?",
        };
        let name = rel.strip_prefix(".").unwrap_or(&rel).display().to_string();
        let mut line = format!(
            "{} {kind} {}:{} {:o}",
            if name.is_empty() { "." } else { &name },
            m.uid(),
            m.gid(),
            m.mode() & 0o7777
        );
        if t.is_file() {
            if m.nlink() > 1 {
                line += &format!(" links={}", m.nlink());
            }
            line += &format!(" \"{}\"", fs::read(&p).unwrap().escape_ascii());
        }
        if t.is_symlink() {
            line += &format!(" -> {}", fs::read_link(&p).unwrap().display());
        }
        if t.is_char_device() || t.is_block_device() {
            let dev = m.rdev();
            line += &format!(" {},{}", rustix::fs::major(dev), rustix::fs::minor(dev));
        }
        // A whiteout is the extraction's own node, made when it ran.
        let whiteout = t.is_char_device() && m.rdev() == 0;
        if !whiteout && m.mtime() != MTIME {
            line += &format!(" t={}", m.mtime());
        }
        let mut names = vec![0; 4096];
        let n = rustix::fs::llistxattr(&p, &mut names).unwrap();
        let mut names: Vec<&[u8]> = names[..n]
            .split(|&b| b == 0)
            .filter(|n| !n.is_empty())
            .collect();
        // SELinux labels every inode by policy where it runs; no layer dumped
        // here sets the name.
        names.retain(|&x| x != b"security.selinux");
        names.sort_unstable();
        for x in names {
            let x = std::ffi::OsStr::from_bytes(x);
            let mut value = vec![0; 4096];
            let n = rustix::fs::lgetxattr(&p, x, &mut value).unwrap();
            line += &format!(" {}={}", x.display(), value[..n].escape_ascii());
        }
        lines.push(line);
        if t.is_dir() {
            for e in fs::read_dir(&p).unwrap() {
                todo.push(rel.join(e.unwrap().file_name()));
            }
        }
    }
    lines.sort();
    lines.join("\n")
}

/// Whether `dir` or a directory below it holds an entry named `name`.
fn holds(dir: &Path, name: &str) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries.map(|e| e.unwrap()).any(|e| {
            e.file_name() == name || (e.file_type().unwrap().is_dir() && holds(&e.path(), name))
        })
    })
}

/// In a build with `store`: the bottom and top layers through `overlay`,
/// then a diff ID the bottom layer's tar does not hash to, a layer of an
/// owner no map holds and one with a `security.selinux` xattr, unless
/// `skip_selinux`. What it saw, for [`expected`], then where the two layers
/// are. Closes the store.
fn extract_layers(store: Store, skip_selinux: bool) -> String {
    let capture = rnk_metrics::Capture::new();
    let store = Arc::new(store);
    let overlay = Overlay::new(Arc::clone(&store), CAPS).unwrap();
    let (b, b_id, _) = bottom();
    let (t, t_id) = top();
    let lower = capture
        .run(|| overlay.base_layer_from(&b_id, b.as_slice(), &[]))
        .unwrap();
    let skipped = capture.report()["phases"]["rnk_rootfs.extract"]["nodes_skipped"].clone();
    let upper = overlay
        .base_layer_from(&t_id, t.as_slice(), std::slice::from_ref(&lower))
        .unwrap();
    let wrong = Digest::from_sha256_bytes(&[7; 32]);
    let mismatch = overlay
        .base_layer_from(&wrong, b.as_slice(), &[])
        .map_or_else(|e| e.to_string(), |l| format!("published {l:?}"));
    // The error from `op` on, after the layer's key, which depends on the
    // map.
    let outcome = |(tar, id): (Vec<u8>, Digest), op: &str| {
        overlay
            .base_layer_from(&id, tar.as_slice(), &[])
            .map_or_else(
                |e| {
                    let e = e.to_string();
                    e.split_once(&format!(": {op} "))
                        .map_or(format!("refused: {e}"), |(_, why)| {
                            format!("refused: {op} {why}")
                        })
                },
                |_| "published".to_owned(),
            )
    };
    let far = outcome(
        Tar::default()
            .dir("./", ROOT, 0o755)
            .file("far", FAR, 0o644, b"far")
            .finish(),
        "chown",
    );
    let selinux_outcome = if skip_selinux {
        SELINUX_SKIPPED.to_owned()
    } else {
        outcome(
            Tar::default()
                .dir("./", ROOT, 0o755)
                .xattrs(&[("security.selinux", b"system_u:object_r:bin_t:s0")])
                .file("sel", ROOT, 0o644, b"sel")
                .finish(),
            "setxattr",
        )
    };
    // The refusing call counts, not its errno, which a security module may
    // change.
    let selinux_outcome = if selinux_outcome.starts_with("refused: setxattr ./sel: ") {
        "refused: setxattr ./sel".to_owned()
    } else {
        selinux_outcome
    };
    let published = fs::read_dir(lower.path().parent().unwrap())
        .unwrap()
        .count();
    let tmp = store.state_dir().join("tmp").join(store.build_id());
    let tmp_left = fs::read_dir(tmp).unwrap().count();
    let report = format!(
        "nodes_skipped {skipped}\nnamed by diff ID {}\nmismatch refused {}\nfar owner {far}\n\
         security.selinux {selinux_outcome}\npublished {published}\ntmp {tmp_left}\n-- bottom\n{}\n\
         -- top\n{}\n-- paths\n{}\n{}",
        lower.path().ends_with(b_id.encoded()),
        mismatch.contains("the uncompressed tar has diff ID"),
        dump(lower.path()),
        dump(upper.path()),
        lower.path().display(),
        upper.path().display(),
    );
    drop(overlay);
    match Arc::into_inner(store).map(Store::close) {
        Some(Ok(())) => report,
        Some(Err(e)) => format!("error: close: {e}"),
        None => "error: the store is still shared".to_owned(),
    }
}

/// What [`extract_layers`] reports for the `security.selinux` layer where
/// SELinux runs: it is not extracted there.
const SELINUX_SKIPPED: &str = "skipped: SELinux is enabled here and decides it by policy";

/// Whether SELinux runs here, which decides `security.selinux` by policy
/// rather than by capability: told by its filesystem or the kernel's list
/// of security modules on a host, and, in a container that masks both, by
/// the label procfs gives this process's directory, which only SELinux
/// answers.
fn selinux_enabled() -> bool {
    let mut label = [0u8; 256];
    Path::new("/sys/fs/selinux/enforce").exists()
        || fs::read_to_string("/sys/kernel/security/lsm")
            .is_ok_and(|lsm| lsm.trim().split(',').any(|m| m == "selinux"))
        || matches!(
            rustix::fs::getxattr("/proc/self", "security.selinux", &mut label),
            Ok(_) | Err(rustix::io::Errno::RANGE)
        )
}

/// [`selinux_enabled`], told once per test so that its processes agree,
/// with a note in the lane's log when the `security.selinux` case is
/// skipped.
fn selinux_case_skipped() -> bool {
    let on = selinux_enabled();
    if on {
        measured("the security.selinux case is skipped: SELinux decides it by policy here");
    }
    on
}

/// [`extract_layers`]'s report, its paths left out, for a build in a user
/// namespace (`userns`) or as root. In the namespace, the owner no map
/// holds fails the extraction, which publishes nothing (ADR-0098), and so
/// does a `security.*` xattr other than `security.capability`, which only
/// a process with `CAP_SYS_ADMIN` over the filesystem's namespace sets
/// where no security module manages the name (SELinux decides
/// `security.selinux` by policy, so the case is skipped where it runs:
/// `skip_selinux`).
fn expected(userns: bool, skip_selinux: bool) -> String {
    let mut bottom: Vec<String> = bottom_dump().lines().map(str::to_owned).collect();
    if !userns {
        bottom.extend(DEVICES.map(str::to_owned));
        bottom.sort();
    }
    format!(
        "nodes_skipped {}\nnamed by diff ID {}\nmismatch refused true\nfar owner {}\n\
         security.selinux {}\npublished {}\ntmp 0\n-- bottom\n{}\n-- top\n{TOP_DUMP}",
        if userns { 2 } else { 0 },
        !userns,
        if userns {
            "refused: chown ./far: Invalid argument (os error 22)"
        } else {
            "published"
        },
        match (userns, skip_selinux) {
            (_, true) => SELINUX_SKIPPED,
            (true, false) => "refused: setxattr ./sel",
            (false, false) => "published",
        },
        match (userns, skip_selinux) {
            (true, _) => 2,
            (false, true) => 3,
            (false, false) => 4,
        },
        bottom.join("\n"),
    )
}

/// The base layers of [`extract_layers`], in a build that enters the user
/// namespace with the state directory `state`, as `user` when given, with
/// `skip_selinux` for [`extract_layers`]. The map, the report and the
/// layers' paths, or `refused\t<refusal>`.
fn extract_in_namespace(state: &Path, user: Option<(u32, u32)>, skip_selinux: bool) -> String {
    in_child(user, || match enter_user_namespace(state) {
        Ok(s) => format!(
            "{}\n{}",
            s.userns.map,
            extract_layers(s.store, skip_selinux)
        ),
        Err(r) => format!("refused\t{r}"),
    })
}

/// Splits [`extract_in_namespace`]'s output into the map, the report and
/// the two layers' paths.
fn split(out: &str) -> (&str, &str, [PathBuf; 2]) {
    let (map, rest) = out.split_once('\n').unwrap_or_else(|| panic!("{out}"));
    let (report, paths) = rest
        .split_once("\n-- paths\n")
        .unwrap_or_else(|| panic!("{out}"));
    let (lower, upper) = paths.split_once('\n').unwrap_or_else(|| panic!("{out}"));
    (map, report, [lower.into(), upper.into()])
}

/// A reader of `data` that, once it is read, says `blocked` on `w` and
/// blocks until the process is killed.
struct Blocking<'a> {
    data: &'a [u8],
    w: BorrowedFd<'a>,
}

impl std::io::Read for Blocking<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.data.is_empty() {
            send(self.w, "blocked\n");
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        let n = buf.len().min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// A build in the user namespace killed with `SIGKILL` halfway through the
/// bottom layer publishes nothing; the next build's store removes what it
/// left, as root of the namespace, and extracts the layer whole. The
/// killed build's map, or `None` when the set-up was refused (asserted by
/// the caller).
fn killed_extraction_publishes_nothing(state: &Path, user: Option<(u32, u32)>) -> Option<String> {
    let (tar, diff_id, cut) = bottom();
    let (pid, r) = spawn_child(user, |w| {
        let set_up = match enter_user_namespace(state) {
            Ok(s) => s,
            Err(r) => return format!("refused\t{r}\n"),
        };
        send(
            w,
            &format!("{}\t{}\n", set_up.userns.map, set_up.store.build_id()),
        );
        let overlay = Overlay::new(Arc::new(set_up.store), CAPS).unwrap();
        let r = overlay.base_layer_from(
            &diff_id,
            Blocking {
                data: &tar[..cut],
                w,
            },
            &[],
        );
        format!("error: the extraction ended: {r:?}\n")
    });
    let mut r = BufReader::new(r);
    let mut first = String::new();
    r.read_line(&mut first).unwrap();
    if first.starts_with("refused\t") {
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()).unwrap();
        return None;
    }
    let (map, build) = first
        .trim_end()
        .split_once('\t')
        .unwrap_or_else(|| panic!("{first}"));
    let mut second = String::new();
    let read = r.read_line(&mut second);
    // Killed before any assertion, so that none leaves it blocked for good.
    let killed = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    let waited = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
    read.unwrap();
    killed.unwrap();
    assert_eq!(second, "blocked\n", "{first}");
    let Ok(Some((_, status))) = waited else {
        panic!("waitpid: {waited:?}");
    };
    assert_eq!(
        status.terminating_signal(),
        Some(libc::SIGKILL),
        "{status:?}"
    );
    // Halfway: the entries before the cut are on disk, none after it, and
    // nothing is published.
    let tmp = state.join("tmp").join(build);
    let layers = state.join("layers/v3/sha256");
    let published = || fs::read_dir(&layers).map_or(0, Iterator::count);
    assert!(holds(&tmp, "owned"), "{}", tmp.display());
    assert!(!holds(&tmp, "ping"), "{}", tmp.display());
    assert_eq!(published(), 0);

    let build = build.to_owned();
    let next = in_child(user, || {
        let set_up = match enter_user_namespace(state) {
            Ok(s) => s,
            Err(r) => return format!("refused\t{r}"),
        };
        let left = set_up
            .store
            .state_dir()
            .join("tmp")
            .join(&build)
            .symlink_metadata()
            .is_ok();
        let store = Arc::new(set_up.store);
        let overlay = Overlay::new(Arc::clone(&store), CAPS).unwrap();
        let layer = overlay.base_layer_from(&diff_id, tar.as_slice(), &[]);
        drop(overlay);
        let closed = Arc::into_inner(store).map(Store::close);
        match (layer, closed) {
            (Ok(l), Some(Ok(()))) => format!("left {left}\n{}", dump(l.path())),
            (l, c) => format!("error: {l:?} {c:?}"),
        }
    });
    assert_eq!(next, format!("left false\n{}", bottom_dump()));
    assert!(!tmp.exists());
    assert!(!state.join("builds").join(build).exists());
    assert_eq!(published(), 1);
    Some(map.to_owned())
}

/// T1, as root of a rootless Podman container: base layers extracted as
/// root of the build's user namespace keep the owners, modes (setuid,
/// setgid and sticky included), `user.*` and `security.capability` xattrs,
/// symlinks, hardlinks, mtimes, whiteouts and opaque directories their tars
/// give, as overlay as root extracts them; a `0555` root, root's or a
/// user's, is published by the rename; device nodes are left out and
/// counted; a diff ID the tar does not hash to, an owner the namespace does
/// not map, or a `security.selinux` xattr publishes nothing, the last
/// refused by its `setxattr`; where SELinux runs, which decides it by
/// policy, that case is skipped, with a `measured:` line in the lane's log.
#[test]
#[ignore = "T1: default: root in a rootless Podman container extracts layers in the build's user namespace"]
fn base_layers_extract_in_the_user_namespace_as_overlay_as_root_extracts_them() {
    if !alone_in(
        module_path!(),
        "base_layers_extract_in_the_user_namespace_as_overlay_as_root_extracts_them",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let skip_selinux = selinux_case_skipped();
    let tmp = private_tempdir();
    let out = extract_in_namespace(&tmp.path().join("state"), None, skip_selinux);
    let (map, report, _) = split(&out);
    assert_eq!(map, "multi-range", "{out}");
    assert_eq!(report, expected(true, skip_selinux));
}

/// T1, as root of a rootless Podman container: an extraction in the build's
/// user namespace killed with `SIGKILL` publishes nothing, and the next
/// build cleans up after it.
#[test]
#[ignore = "T1: default: root in a rootless Podman container kills an extraction in the build's user namespace"]
fn a_killed_extraction_in_the_user_namespace_publishes_nothing() {
    if !alone_in(
        module_path!(),
        "a_killed_extraction_in_the_user_namespace_publishes_nothing",
    ) {
        return;
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "run as root of a rootless Podman container (just test-rootless)"
    );
    let tmp = private_tempdir();
    let map = killed_extraction_publishes_nothing(&tmp.path().join("state"), None);
    assert_eq!(map.as_deref(), Some("multi-range"));
}

/// The user who ran sudo, from `SUDO_UID` and `SUDO_GID`, and whether
/// AppArmor restricts unprivileged user namespaces (Ubuntu 24.04).
fn sudo_user() -> ((u32, u32), bool) {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (var("SUDO_UID"), var("SUDO_GID")) else {
        panic!("run through sudo by a regular user, as the T2 unit suite does");
    };
    assert_ne!(uid, 0, "run through sudo by a regular user");
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    ((uid, gid), restricted)
}

/// A gid of no map: neither the user's nor a subordinate one.
const UNMAPPED_GID: u32 = 4242;

/// T2: the same layers as real root, with no user namespace, give the
/// device nodes too and the chain IDs as names, and publish the layers of
/// an owner no map holds and of a `security.selinux` xattr. As the user who
/// ran sudo, with the user's subordinate ids and a setgid state directory
/// of a gid no map holds: where AppArmor restricts unprivileged user
/// namespaces (Ubuntu 24.04) the set-up is refused; elsewhere (22.04) the
/// layers come out as root's, devices aside and those two layers refused,
/// owned outside by the user (the namespace's root) and by subordinate ids
/// (the others), and the user outside the namespace cannot move a `0555`
/// root of a subordinate id. Where SELinux runs, the `security.selinux`
/// case is skipped, with a `measured:` line in the suite's log.
#[test]
#[ignore = "T2: extracts layers as real root and as the sudo user in the build's user namespace"]
fn a_regular_user_s_base_layers_extract_as_overlay_as_root_extracts_them() {
    if !alone_in(
        module_path!(),
        "a_regular_user_s_base_layers_extract_as_overlay_as_root_extracts_them",
    ) {
        return;
    }
    let ((uid, gid), restricted) = sudo_user();
    let skip_selinux = selinux_case_skipped();
    let tmp = private_tempdir();
    let as_root = in_child(None, || {
        match Store::open(&BuildPaths::new(tmp.path(), tmp.path().join("root-state"))) {
            Ok(store) => extract_layers(store, skip_selinux),
            Err(e) => format!("error: {e}"),
        }
    });
    let (report, _) = as_root
        .split_once("\n-- paths\n")
        .unwrap_or_else(|| panic!("{as_root}"));
    assert_eq!(report, expected(false, skip_selinux));

    chown(tmp.path(), Some(uid), Some(gid)).unwrap();
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    chown(&state, Some(uid), Some(UNMAPPED_GID)).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o2700)).unwrap();
    let out = extract_in_namespace(&state, Some((uid, gid)), skip_selinux);
    if restricted {
        assert!(
            out.starts_with("refused\tno user namespace for the build: "),
            "{out}"
        );
        assert!(!state.join(rnk_store::STATE_MARKER).exists(), "{out}");
        return;
    }
    let (map, report, [lower, upper]) = split(&out);
    assert_eq!(map, "newuidmap", "{out}");
    assert_eq!(report, expected(true, skip_selinux));
    // Outside, the owners are the subordinate ids, from the second one on.
    let sub = crate::subids::lookup(uid)
        .unwrap()
        .expect("the VM's user has subordinate ids");
    let host = |ns: u32| sub.uids.start + ns - 1;
    assert_eq!(fs::metadata(&lower).unwrap().uid(), uid);
    assert_eq!(fs::metadata(&upper).unwrap().uid(), host(USER.0));
    assert_eq!(
        fs::metadata(lower.join("etc/owned")).unwrap().uid(),
        host(2)
    );
    // The control: outside the namespace, the user cannot move the `0555`
    // root of a subordinate id (ADR-0099).
    let moved = in_child(Some((uid, gid)), || {
        match fs::rename(&upper, tmp.path().join("moved")) {
            Ok(()) => "moved".to_owned(),
            Err(e) => format!("{:?}", e.kind()),
        }
    });
    assert_eq!(moved, "PermissionDenied");
}

/// T2: as the user who ran sudo, an extraction in the build's user
/// namespace killed with `SIGKILL` publishes nothing, and the next build
/// removes what it left, files of subordinate ids the user cannot remove
/// outside. Where AppArmor restricts unprivileged user namespaces (Ubuntu
/// 24.04), the set-up is refused and nothing is made.
#[test]
#[ignore = "T2: kills an extraction in the sudo user's build user namespace"]
fn a_regular_user_s_killed_extraction_publishes_nothing() {
    if !alone_in(
        module_path!(),
        "a_regular_user_s_killed_extraction_publishes_nothing",
    ) {
        return;
    }
    let (user, restricted) = sudo_user();
    let tmp = private_tempdir();
    chown(tmp.path(), Some(user.0), Some(user.1)).unwrap();
    let state = tmp.path().join("state");
    match killed_extraction_publishes_nothing(&state, Some(user)) {
        None => {
            assert!(
                restricted,
                "refused where user namespaces are not restricted"
            );
            assert!(!state.join(rnk_store::STATE_MARKER).exists());
        }
        Some(map) => {
            assert!(!restricted, "{map}");
            assert_eq!(map, "newuidmap");
        }
    }
}
