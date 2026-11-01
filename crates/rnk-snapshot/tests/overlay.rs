//! Overlay stage roots with real mounts: tier T2, as root in the test VM
//! only (`just test-vm unit`), never on a host. Each test unmounts what it
//! mounted; the VM is thrown away afterwards anyway.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::FileTypeExt as _;
use std::path::Path;
use std::sync::Arc;

use rnk_oci::Digest;
use rnk_rootfs::OpenHow;
use rnk_snapshot::{
    Base, LayerDir, MountApi, Overlay, OverlayCaps, Snapshotter, StageRootfs, probe,
};
use rnk_store::{BuildPaths, Store};
use sha2::{Digest as _, Sha256};
use tar::{EntryType, Header};

/// A tar layer of root-owned entries.
fn layer(entries: &[(&str, EntryType, u32, &[u8])]) -> (Vec<u8>, Digest) {
    let mut out = Vec::new();
    for (name, t, mode, data) in entries {
        let mut h = Header::new_gnu();
        h.set_path(name).unwrap();
        h.set_entry_type(*t);
        h.set_mode(*mode);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(1_000_000);
        h.set_size(u64::try_from(data.len()).unwrap());
        h.set_cksum();
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(data);
        out.extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
    }
    out.extend(std::iter::repeat_n(0, 1024));
    let hash: [u8; 32] = Sha256::digest(&out).into();
    (out, Digest::from_sha256_bytes(&hash))
}

struct Build {
    _dir: tempfile::TempDir,
    overlay: Overlay,
    base: Vec<LayerDir>,
}

/// A store in a fresh directory and a base image of two layers: `/` 0555
/// with `etc/os-release` and `bin/old`, then `etc/hostname`.
fn build(api: MountApi) -> Build {
    let dir = tempfile::Builder::new()
        .prefix("rnk-snap")
        .tempdir_in("/var/tmp")
        .unwrap();
    let state = dir.path().join("state");
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let overlay = Overlay::new(Arc::new(store), OverlayCaps { api }).unwrap();
    let (t1, id1) = layer(&[
        ("./", EntryType::Directory, 0o555, b""),
        ("etc/", EntryType::Directory, 0o755, b""),
        ("etc/os-release", EntryType::Regular, 0o644, b"ID=t2\n"),
        ("bin/", EntryType::Directory, 0o755, b""),
        ("bin/old", EntryType::Regular, 0o755, b"old"),
    ]);
    let (t2, id2) = layer(&[("etc/hostname", EntryType::Regular, 0o644, b"h\n")]);
    let l1 = overlay.base_layer_from(&id1, t1.as_slice(), &[]).unwrap();
    let l2 = overlay
        .base_layer_from(&id2, t2.as_slice(), std::slice::from_ref(&l1))
        .unwrap();
    Build {
        _dir: dir,
        overlay,
        base: vec![l1, l2],
    }
}

fn write(root: &rnk_rootfs::RootPath, p: &str, body: &[u8]) {
    let fd = root
        .open_at(p, &OpenHow::write().create(0o644).truncate())
        .unwrap();
    fs::File::from(fd).write_all(body).unwrap();
}

fn read(root: &rnk_rootfs::RootPath, p: &str) -> Vec<u8> {
    use std::io::Read as _;
    let mut out = Vec::new();
    fs::File::from(root.open_regular(p).unwrap())
        .read_to_end(&mut out)
        .unwrap();
    out
}

/// The non-directory entries of a diff, with whether each is a whiteout.
/// Directories are left out: whether a copied-up parent counts as changed
/// depends on its times, which the exporter's T0 tests cover.
fn files(d: &rnk_snapshot::LayerDiff) -> Vec<(String, bool)> {
    d.entries()
        .unwrap()
        .into_iter()
        .filter(|e| !e.kind.is_dir())
        .map(|e| (e.path.to_string(), e.kind == rnk_layer::EntryKind::Whiteout))
        .collect()
}

fn layers_per_instruction(api: MountApi) {
    let b = build(api);
    let mut stage = b.overlay.prepare(Base::Image(&b.base)).unwrap();

    stage.begin_layer().unwrap();
    {
        let root = stage.root().unwrap();
        // The image's `/` stays 0555: the upper root took the lower's.
        assert_eq!(root.statx("").unwrap().permissions(), 0o555);
        assert_eq!(read(root, "etc/hostname"), b"h\n");
        write(root, "etc/new", b"new");
        root.remove_file("bin/old").unwrap();
    }
    let d1 = stage.end_layer().unwrap();
    assert_eq!(
        files(&d1),
        [("bin/old".to_owned(), true), ("etc/new".to_owned(), false)]
    );
    let wh = fs::symlink_metadata(d1.upper().join("bin/old")).unwrap();
    assert!(wh.file_type().is_char_device());

    // The next layer sees the first one's diff as its topmost lower.
    stage.begin_layer().unwrap();
    {
        let root = stage.root().unwrap();
        assert_eq!(read(root, "etc/new"), b"new");
        assert!(root.statx("bin/old").is_err());
        write(root, "etc/new", b"changed");
    }
    let d2 = stage.end_layer().unwrap();
    assert_eq!(files(&d2), [("etc/new".to_owned(), false)]);
    assert!(stage.root().is_err());

    // Between layers the root is read-only.
    {
        let view = stage.view().unwrap();
        assert_eq!(read(view, "etc/new"), b"changed");
        let e = view
            .open_at("etc/other", &OpenHow::write().create(0o644))
            .unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc_erofs()), "{e}");
    }

    // A stage on that one sees its files.
    let mut child = b.overlay.prepare(Base::Stage(&stage)).unwrap();
    child.begin_layer().unwrap();
    assert_eq!(read(child.root().unwrap(), "etc/new"), b"changed");
    child.end_layer().unwrap();
    child.release().unwrap();

    let dir = stage.mount_point().parent().unwrap().to_owned();
    stage.release().unwrap();
    assert!(!dir.exists(), "{} left behind", dir.display());
    assert_no_mounts_under(b.overlay.store().state_dir());
}

fn libc_erofs() -> i32 {
    rustix::io::Errno::ROFS.raw_os_error()
}

fn assert_no_mounts_under(dir: &Path) {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let dir = dir.to_string_lossy();
    assert!(
        !info.lines().any(|l| l.contains(dir.as_ref())),
        "mounts left under {dir}:\n{info}"
    );
}

#[test]
#[ignore = "T2c: sysadmin: mounts overlays"]
fn overlay_probe_mounts_and_writes_through() {
    let dir = tempfile::Builder::new()
        .prefix("rnk-probe")
        .tempdir_in("/var/tmp")
        .unwrap();
    let caps = probe(dir.path()).unwrap();
    eprintln!("overlay probe: {caps:?}");
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        0,
        "probe left entries"
    );
    assert_no_mounts_under(dir.path());
}

#[test]
#[ignore = "T2c: sysadmin: mounts overlays"]
fn a_stage_root_gets_one_layer_per_instruction_with_the_probed_api() {
    let dir = tempfile::Builder::new()
        .prefix("rnk-probe")
        .tempdir_in("/var/tmp")
        .unwrap();
    let caps = probe(dir.path()).unwrap();
    layers_per_instruction(caps.api);
}

#[test]
#[ignore = "T2c: sysadmin: mounts overlays"]
fn a_stage_root_gets_one_layer_per_instruction_with_mount2() {
    layers_per_instruction(MountApi::Mount2);
}

#[test]
#[ignore = "T2c: sysadmin: mounts overlays"]
fn mount2_takes_many_lowers_from_a_deep_state_directory() {
    // With absolute paths, about 90 lowers fit one page; with relative
    // names from a thread of its own (containerd's way, Q-151) the stack is
    // bounded by overlay's 500 instead. A deep state directory makes the
    // absolute paths long on purpose.
    const LAYERS: usize = 400;
    let dir = tempfile::Builder::new()
        .prefix("rnk-snap")
        .tempdir_in("/var/tmp")
        .unwrap();
    let state = dir
        .path()
        .join("a-rather-long-state-directory-name/".repeat(4));
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let overlay = Overlay::new(
        Arc::new(store),
        OverlayCaps {
            api: MountApi::Mount2,
        },
    )
    .unwrap();
    let mut base: Vec<LayerDir> = Vec::new();
    for i in 0..LAYERS {
        let name = format!("f{i}");
        let (t, id) = layer(&[(&name, EntryType::Regular, 0o644, name.as_bytes())]);
        let l = overlay.base_layer_from(&id, t.as_slice(), &base).unwrap();
        base.push(l);
    }
    let cwd = std::env::current_dir().unwrap();
    let mut stage = overlay.prepare(Base::Image(&base)).unwrap();
    stage.begin_layer().unwrap();
    {
        let root = stage.root().unwrap();
        assert_eq!(read(root, "f0"), b"f0");
        assert_eq!(
            read(root, &format!("f{}", LAYERS - 1)),
            format!("f{}", LAYERS - 1).as_bytes()
        );
        write(root, "top", b"t");
    }
    let d = stage.end_layer().unwrap();
    assert_eq!(files(&d), [("top".to_owned(), false)]);
    assert_eq!(read(stage.view().unwrap(), "top"), b"t");
    // The process's working directory never moved, and no links are left.
    assert_eq!(std::env::current_dir().unwrap(), cwd);
    let stage_dir = stage.mount_point().parent().unwrap().to_owned();
    assert!(
        !fs::read_dir(&stage_dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("lowers-")),
        "links left in {}",
        stage_dir.display()
    );
    stage.release().unwrap();
    assert_no_mounts_under(overlay.store().state_dir());
}

#[test]
#[ignore = "T2c: sysadmin: mounts overlays"]
fn a_scratch_stage_mounts_over_an_empty_lower() {
    let b = build(MountApi::Mount2);
    let mut stage = b.overlay.prepare(Base::Scratch).unwrap();
    stage.begin_layer().unwrap();
    write(stage.root().unwrap(), "only", b"x");
    let d = stage.end_layer().unwrap();
    assert_eq!(files(&d), [("only".to_owned(), false)]);
    assert_eq!(read(stage.view().unwrap(), "only"), b"x");
    stage.release().unwrap();
    assert_no_mounts_under(b.overlay.store().state_dir());
}
