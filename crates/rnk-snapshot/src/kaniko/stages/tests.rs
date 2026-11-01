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

/// T2 (root, the VM): a mount below a stage root, made in this namespace
/// after the stage, is never crossed by the release, which fails instead
/// of deleting what the mount shows.
#[test]
fn release_never_crosses_a_mount_below_a_stage_root() {
    if !rustix::process::geteuid().is_root() {
        eprintln!("skipped: needs root (T2)");
        return;
    }
    let f = Fixture::new();
    let s = f.stages.prepare(Base::Scratch).unwrap();
    let target = s.mount_point().join("mnt");
    fs::create_dir(&target).unwrap();
    let source = f.tmp.path().join("volume");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("keep"), b"kept").unwrap();
    if rustix::mount::mount_bind(&source, &target).is_err() {
        eprintln!("skipped: bind mount refused here");
        return;
    }
    let r = s.release();
    let still = fs::read(source.join("keep"));
    let _ = rustix::mount::unmount(&target, rustix::mount::UnmountFlags::DETACH);
    assert_eq!(still.unwrap(), b"kept", "the volume's file stays");
    // The mount point keeps its directory: the release reports it.
    assert!(r.is_err(), "a mount below the root is reported");
}
