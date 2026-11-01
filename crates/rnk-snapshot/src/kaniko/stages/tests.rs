use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use rnk_store::BuildPaths;
use sha2::{Digest as _, Sha256};
use tar::{EntryType, Header};

use super::*;
use crate::kaniko::SnapshotMode;

struct Fixture {
    tmp: tempfile::TempDir,
    store: Store,
    stages: KanikoStages,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let state = fs::canonicalize(tmp.path()).unwrap().join("state");
        let store = Store::open(&BuildPaths::new(tmp.path(), &state)).unwrap();
        let mut ignore = IgnoreList::new();
        ignore.insert("/proc").unwrap();
        let walk = WalkOptions {
            workers: NonZeroUsize::new(2).unwrap(),
            mode: SnapshotMode::Full,
        };
        let stages = KanikoStages::new(&store, ignore, walk).unwrap();
        Self { tmp, store, stages }
    }

    /// A base layer of `files` (path, content) as a blob file; with
    /// `wrong_id`, recorded under a diff ID it does not have.
    fn layer(&self, name: &str, files: &[(&str, &[u8])], wrong_id: bool) -> KanikoLayer {
        let mut out = Vec::new();
        for (path, data) in files {
            let mut h = Header::new_gnu();
            h.set_path(path).unwrap();
            h.set_entry_type(EntryType::Regular);
            h.set_mode(0o644);
            h.set_uid(u64::from(rustix::process::geteuid().as_raw()));
            h.set_gid(u64::from(rustix::process::getegid().as_raw()));
            h.set_mtime(1_000_000);
            h.set_size(u64::try_from(data.len()).unwrap());
            h.set_cksum();
            out.extend_from_slice(h.as_bytes());
            out.extend_from_slice(data);
            out.extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
        }
        out.extend(std::iter::repeat_n(0, 1024));
        let mut hash: [u8; 32] = Sha256::digest(&out).into();
        if wrong_id {
            hash[0] ^= 1;
        }
        let blob = self.tmp.path().join(name);
        fs::write(&blob, &out).unwrap();
        self.stages
            .base_layer(&Digest::from_sha256_bytes(&hash), &blob, &[])
            .unwrap()
    }

    /// The stage directories made so far, by number.
    fn stage_dirs(&self) -> Vec<String> {
        let mut v: Vec<_> = match fs::read_dir(self.stages.stages_dir()) {
            Ok(rd) => rd
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect(),
            Err(_) => Vec::new(),
        };
        v.sort();
        v
    }
}

/// Writes `path` with `data` and `mode` into the stage's open layer.
fn put(s: &KanikoStage, path: &str, data: &[u8], mode: u32) {
    s.root().unwrap();
    let p = s.mount_point().join(path);
    fs::write(&p, data).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
}

fn paths(diff: &WalkDiff) -> Vec<String> {
    diff.changed
        .iter()
        .map(|c| c.path.display().to_string())
        .collect()
}

#[test]
fn a_stage_on_an_image_gets_a_private_directory_of_its_own() {
    let f = Fixture::new();
    let l = f.layer("l", &[("etc/os", b"base")], false);
    let mut s = f.stages.prepare(Base::Image(&[l])).unwrap();
    let rootfs = f
        .store
        .state_dir()
        .join("builds")
        .join(f.store.build_id())
        .join("stages/0/rootfs");
    assert_eq!(s.mount_point(), rootfs);
    assert_eq!(fs::read(rootfs.join("etc/os")).unwrap(), b"base");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(rootfs.parent().unwrap()), 0o700, "the stage directory");
    assert_eq!(mode(f.stages.stages_dir()), 0o700);
    // The root as overlay mode's stage roots: `0755` unless a layer sets it.
    assert_eq!(mode(&rootfs), 0o755);

    s.begin_layer().unwrap();
    put(&s, "new", b"x", 0o644);
    let d = s.end_layer().unwrap();
    assert_eq!(paths(d.diff()), ["new"]);
    let (digests, counts) = d
        .export(
            rnk_layer::EncodeOptions {
                compression: rnk_layer::Compression::None,
                level: 0,
                threads: NonZeroUsize::MIN,
                mtime: None,
            },
            &mut Vec::new(),
        )
        .unwrap();
    assert_eq!(counts.files_added, 1);
    assert!(digests.size > 1024);
    s.release().unwrap();
    assert!(f.stage_dirs().is_empty(), "released: {:?}", f.stage_dirs());
}

#[test]
fn a_stage_built_from_another_copies_it_and_never_shares_an_inode() {
    let f = Fixture::new();
    let l = f.layer("l", &[("etc/os", b"base")], false);
    let mut parent = f.stages.prepare(Base::Image(&[l])).unwrap();
    parent.begin_layer().unwrap();
    put(&parent, "etc/app", b"parent", 0o600);
    parent.end_layer().unwrap();
    parent.done();

    let mut child = f.stages.prepare(Base::Stage(&parent)).unwrap();
    assert_ne!(child.mount_point(), parent.mount_point());
    let (p, c) = (parent.mount_point(), child.mount_point().to_owned());
    assert_eq!(fs::read(c.join("etc/app")).unwrap(), b"parent");
    assert_eq!(
        fs::metadata(c.join("etc/app"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    for f in ["etc/os", "etc/app"] {
        assert_ne!(
            fs::metadata(p.join(f)).unwrap().ino(),
            fs::metadata(c.join(f)).unwrap().ino(),
            "{f} is a copy, never a hardlink (ADR-0017)"
        );
    }
    // The child's baseline is the copy: only what it changes is its layer,
    // and nothing it does reaches the parent.
    child.begin_layer().unwrap();
    put(&child, "etc/os", b"child", 0o644);
    let d = child.end_layer().unwrap();
    assert_eq!(paths(d.diff()), ["etc/os"]);
    assert_eq!(fs::read(p.join("etc/os")).unwrap(), b"base");
    child.release().unwrap();
    parent.release().unwrap();
    assert!(f.stage_dirs().is_empty());
}

#[test]
fn a_parent_with_a_layer_open_is_refused() {
    let f = Fixture::new();
    let mut parent = f.stages.prepare(Base::Scratch).unwrap();
    parent.begin_layer().unwrap();
    let e = f.stages.prepare(Base::Stage(&parent)).unwrap_err();
    assert!(e.to_string().contains("layer open"), "{e}");
    assert_eq!(f.stage_dirs(), ["0"], "nothing made for the refused one");
}

#[test]
fn a_stage_whose_layers_are_not_needed_is_never_walked() {
    let f = Fixture::new();
    let mut s = f.stages.prepare(Base::Scratch).unwrap();
    s.set_layers_needed(false);
    s.begin_layer().unwrap();
    put(&s, "f", b"x", 0o644);
    let d = s.end_layer().unwrap();
    assert!(d.diff().changed.is_empty());
    assert!(s.walker.is_none(), "no walker made");
    // A reader still sees the root.
    assert!(s.view().unwrap().statx("f").is_ok());
}

#[test]
fn a_done_stage_drops_its_walk_and_stays_readable() {
    let f = Fixture::new();
    let mut s = f.stages.prepare(Base::Scratch).unwrap();
    s.begin_layer().unwrap();
    put(&s, "f", b"x", 0o644);
    s.end_layer().unwrap();
    assert!(s.walker.is_some());
    s.done();
    assert!(s.walker.is_none());
    assert!(s.view().unwrap().statx("f").is_ok());
}

#[test]
fn a_layer_that_fails_its_check_leaves_no_stage_directory() {
    let f = Fixture::new();
    let good = f.layer("good", &[("a", b"1")], false);
    let bad = f.layer("bad", &[("b", b"2")], true);
    let e = f.stages.prepare(Base::Image(&[good, bad])).unwrap_err();
    assert!(matches!(e, Error::DiffIdMismatch { .. }), "{e}");
    assert!(f.stage_dirs().is_empty(), "{:?}", f.stage_dirs());
}

#[test]
fn stages_side_by_side_get_their_own_directories() {
    let f = Fixture::new();
    let l = f.layer("l", &[("f", b"base")], false);
    let roots: Vec<_> = (0..3)
        .map(|_| {
            f.stages
                .prepare(Base::Image(std::slice::from_ref(&l)))
                .unwrap()
        })
        .collect();
    assert_eq!(f.stage_dirs(), ["0", "1", "2"]);
    let mut points: Vec<_> = roots.iter().map(|r| r.mount_point().to_owned()).collect();
    points.dedup();
    assert_eq!(points.len(), 3);
    for r in roots {
        r.release().unwrap();
    }
}

#[test]
fn an_anonymous_store_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    fs::create_dir(&state).unwrap();
    let store = Store::open_anonymous(&state).unwrap();
    let walk = WalkOptions {
        workers: NonZeroUsize::MIN,
        mode: SnapshotMode::Full,
    };
    let e = KanikoStages::new(&store, IgnoreList::new(), walk).unwrap_err();
    assert!(e.to_string().contains("shared root"), "{e}");
}

#[test]
fn a_name_planted_at_the_next_stage_directory_is_refused_never_followed() {
    let f = Fixture::new();
    let outside = f.tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    // `stages/0`, the next stage's name, as a link out and as a directory.
    std::os::unix::fs::symlink(&outside, f.stages.stages_dir().join("0")).unwrap();
    let e = f.stages.prepare(Base::Scratch).unwrap_err();
    assert!(e.to_string().contains("exists"), "{e}");
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    fs::create_dir(f.stages.stages_dir().join("1")).unwrap();
    assert!(f.stages.prepare(Base::Scratch).is_err());
    // The next free name works.
    let s = f.stages.prepare(Base::Scratch).unwrap();
    assert!(s.mount_point().ends_with("stages/2/rootfs"));
}

#[test]
fn a_child_copies_links_as_links_hardlinks_and_modes_and_leaves_nodes_out() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let outside = f.tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"host").unwrap();
    let mut parent = f.stages.prepare(Base::Scratch).unwrap();
    parent.begin_layer().unwrap();
    let p = parent.mount_point().to_owned();
    symlink("/", p.join("etc")).unwrap();
    symlink("../../..", p.join("x")).unwrap();
    symlink(&outside, p.join("abs")).unwrap();
    put(&parent, "suid", b"s", 0o4755);
    fs::hard_link(p.join("suid"), p.join("suid2")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        p.join("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    let _sock = std::os::unix::net::UnixListener::bind(p.join("sock")).unwrap();
    // A device node where this process may make one (root, the VM).
    let dev = rustix::fs::mknodat(
        rustix::fs::CWD,
        p.join("null"),
        rustix::fs::FileType::CharacterDevice,
        rustix::fs::Mode::from_raw_mode(0o666),
        rustix::fs::makedev(1, 3),
    )
    .is_ok();
    parent.end_layer().unwrap();
    parent.done();

    let child = f.stages.prepare(Base::Stage(&parent)).unwrap();
    let c = child.mount_point();
    for (link, target) in [("etc", "/"), ("x", "../../..")] {
        assert_eq!(fs::read_link(c.join(link)).unwrap(), Path::new(target));
    }
    assert_eq!(fs::read_link(c.join("abs")).unwrap(), outside);
    assert_eq!(
        fs::read(outside.join("secret")).unwrap(),
        b"host",
        "nothing written through a link"
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    let m = fs::metadata(c.join("suid")).unwrap();
    assert_eq!(m.permissions().mode() & 0o7777, 0o4755);
    assert_eq!(m.ino(), fs::metadata(c.join("suid2")).unwrap().ino());
    assert_ne!(m.ino(), fs::metadata(p.join("suid")).unwrap().ino());
    for node in ["fifo", "sock", "null"] {
        assert!(
            fs::symlink_metadata(c.join(node)).is_err(),
            "{node} left out (device made: {dev})"
        );
    }
    child.release().unwrap();
    parent.release().unwrap();
    assert_eq!(fs::read(outside.join("secret")).unwrap(), b"host");
}

/// A mount a T2 test made below its temporary directory, detached on drop
/// (panics included) before the directory is removed. If it cannot be
/// detached the process aborts, so nothing is removed through it.
struct Detach(PathBuf);

impl Drop for Detach {
    fn drop(&mut self) {
        if let Err(e) = rustix::mount::unmount(&self.0, rustix::mount::UnmountFlags::DETACH) {
            eprintln!(
                "cannot detach {}: {e}; aborting to remove nothing",
                self.0.display()
            );
            std::process::abort();
        }
    }
}

/// T2 (root, the VM only): a mount below a stage root is never crossed by
/// the release, which reports it instead of deleting what the mount
/// shows. The whole test runs on a thread of its own in a new mount
/// namespace, all mounts private there, so nothing it mounts can reach
/// another namespace; the stage is made and released in that namespace.
#[test]
#[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
fn release_never_crosses_a_mount_below_a_stage_root() {
    std::thread::spawn(|| {
        use rustix::thread::UnshareFlags;
        // SAFETY: this thread shares nothing that the unshared filesystem
        // attributes or mount namespace would break; it ends with the test.
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
            .expect("unshare(CLONE_FS | CLONE_NEWNS)");
        rustix::mount::mount_change(
            "/",
            rustix::mount::MountPropagationFlags::PRIVATE
                | rustix::mount::MountPropagationFlags::REC,
        )
        .expect("make every mount private");
        let f = Fixture::new();
        let s = f.stages.prepare(Base::Scratch).unwrap();
        let target = s.mount_point().join("mnt");
        fs::create_dir(&target).unwrap();
        let source = f.tmp.path().join("volume");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("keep"), b"kept").unwrap();
        rustix::mount::mount_bind(&source, &target).expect("bind mount");
        // Dropped before `f`: the bind is detached before the directory is
        // removed, on a panic too.
        let bind = Detach(target);
        let r = s.release();
        let still = fs::read(source.join("keep"));
        drop(bind);
        assert_eq!(still.unwrap(), b"kept", "the volume's file stays");
        assert!(r.is_err(), "a mount below the root is reported");
    })
    .join()
    .unwrap();
}

/// T2 (root, the VM only): device nodes, FIFOs and sockets a parent's `RUN`
/// made are not made again in a child built `FROM` it (ADR-0041, ADR-0059);
/// its files still are.
#[test]
#[ignore = "T2: mknod as real root; run in the test VM only"]
fn a_child_never_gets_the_device_nodes_a_parent_made() {
    let f = Fixture::new();
    let mut parent = f.stages.prepare(Base::Scratch).unwrap();
    parent.begin_layer().unwrap();
    let p = parent.mount_point().to_owned();
    for (name, kind, dev) in [
        ("null", rustix::fs::FileType::CharacterDevice, (1, 3)),
        ("sda", rustix::fs::FileType::BlockDevice, (8, 0)),
        ("fifo", rustix::fs::FileType::Fifo, (0, 0)),
    ] {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            p.join(name),
            kind,
            rustix::fs::Mode::from_raw_mode(0o666),
            rustix::fs::makedev(dev.0, dev.1),
        )
        .expect("mknod as root");
    }
    let _sock = std::os::unix::net::UnixListener::bind(p.join("sock")).unwrap();
    put(&parent, "file", b"kept", 0o644);
    parent.end_layer().unwrap();
    parent.done();
    let child = f.stages.prepare(Base::Stage(&parent)).unwrap();
    let c = child.mount_point();
    for node in ["null", "sda", "fifo", "sock"] {
        assert!(fs::symlink_metadata(c.join(node)).is_err(), "{node} made");
    }
    assert_eq!(fs::read(c.join("file")).unwrap(), b"kept");
    child.release().unwrap();
    parent.release().unwrap();
}

// Red team (T1, T3, T4, T15, ADR-0017, ADR-0041, ADR-0059): hostile parents
// and layers against per-stage directories. Each test fails when a defence
// gives way.

/// Puts `files` straight into a stage that walks nothing, as a parent's
/// `RUN` leaves them, and closes its layer.
fn unwalked(f: &Fixture, write: impl FnOnce(&Path)) -> KanikoStage {
    let mut s = f.stages.prepare(Base::Scratch).unwrap();
    s.set_layers_needed(false);
    s.begin_layer().unwrap();
    write(s.mount_point());
    s.end_layer().unwrap();
    s.done();
    s
}

/// A tar entry with a raw name and link name, which `tar::Header::set_path`
/// would refuse (`..`).
fn raw_entry(out: &mut Vec<u8>, name: &[u8], kind: EntryType, link: &[u8], data: &[u8]) {
    let mut h = Header::new_old();
    {
        let old = h.as_old_mut();
        old.name[..name.len()].copy_from_slice(name);
        old.linkname[..link.len()].copy_from_slice(link);
    }
    h.set_entry_type(kind);
    h.set_mode(if kind == EntryType::Directory {
        0o777
    } else {
        0o644
    });
    h.set_uid(u64::from(rustix::process::geteuid().as_raw()));
    h.set_gid(u64::from(rustix::process::getegid().as_raw()));
    h.set_mtime(1_000_000);
    h.set_size(u64::try_from(data.len()).unwrap());
    h.set_cksum();
    out.extend_from_slice(h.as_bytes());
    out.extend_from_slice(data);
    out.extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
}

fn raw_layer(f: &Fixture, name: &str, out: &mut Vec<u8>) -> KanikoLayer {
    out.extend(std::iter::repeat_n(0, 1024));
    let hash: [u8; 32] = Sha256::digest(&*out).into();
    let blob = f.tmp.path().join(name);
    fs::write(&blob, &*out).unwrap();
    f.stages
        .base_layer(&Digest::from_sha256_bytes(&hash), &blob, &[])
        .unwrap()
}

/// Every path below `dir`, symlinks not followed, sorted.
fn listing(dir: &Path) -> Vec<(PathBuf, u64)> {
    fn walk(d: &Path, out: &mut Vec<(PathBuf, u64)>) {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let m = fs::symlink_metadata(e.path()).unwrap();
            out.push((e.path(), m.ino()));
            if m.is_dir() {
                walk(&e.path(), out);
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, &mut v);
    v.sort();
    v
}

/// A base layer whose entries climb out of the stage root (`..`, a
/// sibling's root, absolute names), hardlink to a sibling's file, write
/// through a symlink to `/` and set `./` to `0777`: nothing lands outside
/// `rootfs/`, the sibling is untouched, and the stage directory stays
/// `0700`.
#[test]
fn red_team_a_hostile_base_layer_stays_in_its_stage_root() {
    let f = Fixture::new();
    let sibling = unwalked(&f, |p| fs::write(p.join("f"), b"sibling").unwrap());
    let before = listing(f.stages.stages_dir());
    let mut tar = Vec::new();
    raw_entry(&mut tar, b"./", EntryType::Directory, b"", b"");
    raw_entry(&mut tar, b"../up", EntryType::Regular, b"", b"x");
    raw_entry(
        &mut tar,
        b"../../0/rootfs/planted",
        EntryType::Regular,
        b"",
        b"x",
    );
    raw_entry(&mut tar, b"a/../../../up2", EntryType::Regular, b"", b"x");
    raw_entry(&mut tar, b"hl", EntryType::Link, b"../../0/rootfs/f", b"");
    raw_entry(&mut tar, b"etc", EntryType::Symlink, b"/", b"");
    raw_entry(&mut tar, b"sib", EntryType::Symlink, b"../../0/rootfs", b"");
    raw_entry(&mut tar, b"etc/thru-root", EntryType::Regular, b"", b"x");
    raw_entry(&mut tar, b"sib/thru-sibling", EntryType::Regular, b"", b"x");
    let l = raw_layer(&f, "hostile", &mut tar);
    // Refused or kept inside: either way nothing outside changes.
    let r = f.stages.prepare(Base::Image(&[l]));
    let after: Vec<_> = listing(f.stages.stages_dir())
        .into_iter()
        .filter(|(p, _)| !p.starts_with(f.stages.stages_dir().join("1/rootfs")))
        .collect();
    let mut want = before.clone();
    if r.is_ok() {
        let one = f.stages.stages_dir().join("1");
        let rootfs = one.join("rootfs");
        let ino = |p: &Path| fs::symlink_metadata(p).unwrap().ino();
        want.push((one.clone(), ino(&one)));
        want.push((rootfs.clone(), ino(&rootfs)));
        want.sort();
    }
    assert_eq!(after, want, "only rootfs/ changed ({:?})", r.as_ref().err());
    assert_eq!(
        fs::read(sibling.mount_point().join("f")).unwrap(),
        b"sibling"
    );
    assert!(!sibling.mount_point().join("planted").exists());
    assert!(!sibling.mount_point().join("thru-sibling").exists());
    assert!(
        !f.tmp
            .path()
            .join("state/builds")
            .join(f.store.build_id())
            .join("up")
            .exists()
    );
    if let Ok(s) = r {
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = s.mount_point().parent().unwrap().to_owned();
        assert_eq!(mode(&dir), 0o700, "the stage directory, whatever ./ says");
        if let Ok(m) = fs::symlink_metadata(s.mount_point().join("hl")) {
            assert_ne!(
                m.ino(),
                fs::metadata(sibling.mount_point().join("f")).unwrap().ino(),
                "no hardlink into a sibling"
            );
        }
        s.release().unwrap();
    }
    sibling.release().unwrap();
    assert!(f.stage_dirs().is_empty(), "{:?}", f.stage_dirs());
}

/// A parent holding a hardlink to a setuid file outside every stage (as a
/// host root would): the child gets a copy, a new inode, and nothing the
/// child or the releases do reaches the outside file; symlinks into a
/// sibling's root and up into the stages directory are deleted as links.
#[test]
fn red_team_a_parent_hardlink_to_an_outside_setuid_file_is_copied_never_linked() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let outside = f.tmp.path().join("outside-suid");
    fs::write(&outside, b"host binary").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o4755)).unwrap();
    let sibling = unwalked(&f, |p| fs::write(p.join("keep"), b"sibling").unwrap());
    let sib_root = sibling.mount_point().to_owned();
    let parent = unwalked(&f, |p| {
        fs::hard_link(&outside, p.join("su")).unwrap();
        symlink(&sib_root, p.join("sib-abs")).unwrap();
        symlink("../../0/rootfs", p.join("sib-rel")).unwrap();
        symlink("../..", p.join("stages")).unwrap();
        fs::create_dir(p.join("d")).unwrap();
        symlink("../../../0/rootfs/keep", p.join("d/keep")).unwrap();
    });
    assert_eq!(fs::metadata(&outside).unwrap().nlink(), 2);
    let child = f.stages.prepare(Base::Stage(&parent)).unwrap();
    let c = child.mount_point().to_owned();
    let m = fs::metadata(c.join("su")).unwrap();
    assert_ne!(m.ino(), fs::metadata(&outside).unwrap().ino(), "a copy");
    assert_eq!(m.permissions().mode() & 0o7777, 0o4755);
    assert_eq!(fs::metadata(&outside).unwrap().nlink(), 2, "no new link");
    fs::write(c.join("su"), b"child rewrote it").unwrap();
    assert_eq!(fs::read(&outside).unwrap(), b"host binary");
    child.release().unwrap();
    parent.release().unwrap();
    let m = fs::metadata(&outside).unwrap();
    assert_eq!(m.nlink(), 1, "the parent's name unlinked");
    assert_eq!(m.permissions().mode() & 0o7777, 0o4755, "never chmodded");
    assert_eq!(fs::read(&outside).unwrap(), b"host binary");
    assert_eq!(fs::read(sib_root.join("keep")).unwrap(), b"sibling");
    sibling.release().unwrap();
    assert!(f.stage_dirs().is_empty());
}

/// Names with newlines, raw bytes, leading dashes and the longest a name
/// may be are copied as they are and released.
#[test]
fn red_team_names_with_newlines_and_raw_bytes_copy_and_release() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    let long = vec![b'n'; 255];
    let names: Vec<&[u8]> = vec![
        b"a\nb",
        b"\n",
        b"..\n..",
        b"\xff\xfe",
        b"-rf",
        b" ",
        b"*",
        &long,
    ];
    let parent = {
        let f = Fixture::new();
        let names = names.clone();
        let p = unwalked(&f, move |p| {
            for n in &names {
                let d = p.join(OsStr::from_bytes(n));
                fs::create_dir(&d).unwrap();
                fs::write(d.join(OsStr::from_bytes(n)), n).unwrap();
            }
        });
        (f, p)
    };
    let (f, parent) = parent;
    let mut child = f.stages.prepare(Base::Stage(&parent)).unwrap();
    for n in &names {
        let p = child
            .mount_point()
            .join(OsStr::from_bytes(n))
            .join(OsStr::from_bytes(n));
        assert_eq!(fs::read(&p).unwrap(), *n, "{p:?}");
    }
    // The child walks them and exports a layer with them.
    child.begin_layer().unwrap();
    fs::write(child.mount_point().join("a\nb/new\nline"), b"x").unwrap();
    let d = child.end_layer().unwrap();
    // The new file, and its directory, whose mtime moved.
    assert_eq!(paths(d.diff()), ["a\nb", "a\nb/new\nline"]);
    child.release().unwrap();
    parent.release().unwrap();
    assert!(f.stage_dirs().is_empty());
}

/// A 16 TiB sparse file in a parent (`truncate -s 16T`, a few bytes of
/// data) costs the child its data, not its size.
#[test]
fn red_team_a_16t_sparse_file_costs_its_data() {
    use std::io::{Seek as _, SeekFrom, Write as _};
    // ext4 caps a file at 16 TiB less a block.
    let size: u64 = (1 << 44) - 4096;
    let f = Fixture::new();
    let parent = unwalked(&f, |p| {
        let mut file = fs::File::create(p.join("sparse")).unwrap();
        file.write_all(b"head").unwrap();
        file.seek(SeekFrom::Start(size / 3)).unwrap();
        file.write_all(b"third").unwrap();
        file.set_len(size).unwrap();
    });
    let t0 = std::time::Instant::now();
    let child = f.stages.prepare(Base::Stage(&parent)).unwrap();
    let took = t0.elapsed();
    let m = fs::metadata(child.mount_point().join("sparse")).unwrap();
    assert_eq!(m.len(), size);
    assert!(
        m.blocks() * 512 < 1 << 20,
        "{} bytes allocated",
        m.blocks() * 512
    );
    assert!(took < std::time::Duration::from_secs(30), "took {took:?}");
    let file = fs::File::open(child.mount_point().join("sparse")).unwrap();
    let mut buf = [0_u8; 5];
    rustix::io::pread(&file, &mut buf, size / 3).unwrap();
    assert_eq!(&buf, b"third");
    child.release().unwrap();
    parent.release().unwrap();
}

/// Children built from one parent at once (independent stages, `--jobs`)
/// get distinct directories and inodes, none shared with the parent or
/// with each other.
#[test]
fn red_team_children_copied_at_once_share_no_inode() {
    let f = Fixture::new();
    let parent = unwalked(&f, |p| {
        for i in 0..200 {
            fs::create_dir_all(p.join(format!("d{}", i % 10))).unwrap();
            fs::write(p.join(format!("d{}/f{i}", i % 10)), i.to_string()).unwrap();
        }
    });
    let children: Vec<KanikoStage> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..6)
            .map(|_| s.spawn(|| f.stages.prepare(Base::Stage(&parent)).unwrap()))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut seen = std::collections::HashSet::new();
    for (p, ino) in listing(parent.mount_point()) {
        assert!(seen.insert(ino), "{p:?}");
    }
    for c in &children {
        let l = listing(c.mount_point());
        assert_eq!(l.len(), 210);
        for (p, ino) in l {
            assert!(seen.insert(ino), "{p:?} shares an inode");
        }
    }
    for c in children {
        c.release().unwrap();
    }
    parent.release().unwrap();
    assert!(f.stage_dirs().is_empty());
}

/// A directory of the parent swapped for a symlink to an outside directory
/// while children copy it (a process left behind, which the sweep should
/// rule out): every copy fails or holds the real directory, nothing of the
/// outside is copied or changed, and a failed copy leaves no directory.
#[test]
fn red_team_a_swap_in_the_parent_during_the_copy_never_leaves_it() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let outside = f.tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), b"host").unwrap();
    let parent = unwalked(&f, |p| {
        fs::create_dir_all(p.join("a/b")).unwrap();
        for i in 0..50 {
            fs::write(p.join(format!("a/b/f{i}")), b"in").unwrap();
        }
    });
    let base = parent.mount_point().join("a");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (mut ok, mut failed) = (0, 0);
    std::thread::scope(|s| {
        s.spawn(|| {
            let (real, parked, link) = (base.join("b"), base.join("b.p"), base.join("b.l"));
            symlink(&outside, &link).unwrap();
            while !stop.load(Ordering::Relaxed) {
                if fs::rename(&real, &parked).is_ok() {
                    let _ = fs::rename(&link, &real);
                    let _ = fs::rename(&real, &link);
                    let _ = fs::rename(&parked, &real);
                }
            }
        });
        for _ in 0..150 {
            match f.stages.prepare(Base::Stage(&parent)) {
                Ok(c) => {
                    ok += 1;
                    for (p, _) in listing(c.mount_point()) {
                        assert_ne!(p.file_name().unwrap(), "sentinel", "{p:?}");
                    }
                    c.release().unwrap();
                }
                Err(_) => failed += 1,
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"host");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    eprintln!("copies: {ok} whole, {failed} failed");
    assert_eq!(
        f.stage_dirs(),
        ["0"],
        "only the parent ({ok} ok, {failed} failed)"
    );
    parent.release().unwrap();
}

/// A chain of `depth` directories `dddddddd` below `p`, made by
/// descriptors, with a file `leaf` holding `deep` at its end.
fn chain(p: &Path, depth: usize) {
    let mut fd = rustix::fs::open(p, rustix::fs::OFlags::PATH, rustix::fs::Mode::empty()).unwrap();
    for _ in 0..depth {
        rustix::fs::mkdirat(&fd, "dddddddd", rustix::fs::Mode::from_raw_mode(0o755)).unwrap();
        fd = rustix::fs::openat(
            &fd,
            "dddddddd",
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
    }
    let leaf = rustix::fs::openat(
        &fd,
        "leaf",
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE,
        rustix::fs::Mode::from_raw_mode(0o644),
    )
    .unwrap();
    rustix::io::write(&leaf, b"deep").unwrap();
}

/// A tree deeper than `PATH_MAX` in a parent is copied and released: the
/// copy and the deletion go by descriptors, never by full paths.
#[test]
fn red_team_a_tree_deeper_than_path_max_copies_and_releases() {
    let f = Fixture::new();
    let depth = 600; // "dddddddd/" * 600: 5400 bytes of path
    let parent = unwalked(&f, |p| chain(p, depth));
    let child = f.stages.prepare(Base::Stage(&parent));
    let r = child.as_ref().map(|_| ()).map_err(ToString::to_string);
    if let Ok(c) = child {
        // The leaf, reached by descriptors.
        let mut fd = rustix::fs::open(
            c.mount_point(),
            rustix::fs::OFlags::PATH,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        for _ in 0..depth {
            fd = rustix::fs::openat(
                &fd,
                "dddddddd",
                rustix::fs::OFlags::PATH | rustix::fs::OFlags::DIRECTORY,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
        }
        let leaf = rustix::fs::openat(
            &fd,
            "leaf",
            rustix::fs::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let mut buf = [0_u8; 4];
        rustix::io::read(&leaf, &mut buf).unwrap();
        assert_eq!(&buf, b"deep");
        c.release().unwrap();
    }
    parent.release().unwrap();
    assert!(f.stage_dirs().is_empty(), "{:?}", f.stage_dirs());
    r.unwrap();
}

/// The largest xattr value Linux allows (64 KiB) and many xattrs on one
/// file are copied whole, or the copy fails: never cut short.
#[test]
fn red_team_huge_xattrs_are_copied_whole() {
    let f = Fixture::new();
    let big: Vec<u8> = (0..65_536_u32).map(|i| (i % 253) as u8).collect();
    let mut set = true;
    let parent = unwalked(&f, |p| {
        let file = p.join("x");
        fs::write(&file, b"x").unwrap();
        if rustix::fs::setxattr(&file, "user.big", &big, rustix::fs::XattrFlags::empty()).is_err() {
            set = false;
        }
    });
    if !set {
        // A filesystem without user xattrs this large: nothing to try.
        eprintln!("skipped: user.big refused here");
        parent.release().unwrap();
        return;
    }
    let child = f.stages.prepare(Base::Stage(&parent)).unwrap();
    let mut buf = vec![0_u8; 70_000];
    let n = rustix::fs::getxattr(child.mount_point().join("x"), "user.big", &mut buf).unwrap();
    assert_eq!(&buf[..n], &big[..]);
    child.release().unwrap();
    parent.release().unwrap();
}

/// The copy of a parent and the child's baseline walk grow with a deep
/// chain's depth no faster than its square: a `RUN` makes a chain of
/// 20,000 directories in seconds. A red-team finding: each entry's
/// `IgnoreList::covers` used to hash every ancestor's full path, so both
/// grew with its cube (3000 levels: 17 s in release); it now looks only at
/// prefixes as long as the longest literal entry.
#[test]
fn red_team_a_deep_chain_costs_no_more_than_the_square_of_its_depth() {
    let time = |depth: usize| {
        let f = Fixture::new();
        let parent = unwalked(&f, |p| chain(p, depth));
        let t0 = std::time::Instant::now();
        let mut child = f.stages.prepare(Base::Stage(&parent)).unwrap();
        let copy = t0.elapsed();
        let t0 = std::time::Instant::now();
        child.begin_layer().unwrap();
        let walk = t0.elapsed();
        eprintln!("depth {depth}: copy {copy:?}, baseline walk {walk:?}");
        child.end_layer().unwrap();
        child.release().unwrap();
        parent.release().unwrap();
        (copy.as_secs_f64(), walk.as_secs_f64())
    };
    let (c1, w1) = time(300);
    let (c4, w4) = time(1200);
    // Square: 16x; cube: 64x. 32x leaves room for noise.
    assert!(c4 < 32.0 * c1.max(0.01), "copy: {c1:.3} s -> {c4:.3} s");
    assert!(w4 < 32.0 * w1.max(0.01), "walk: {w1:.3} s -> {w4:.3} s");
}
