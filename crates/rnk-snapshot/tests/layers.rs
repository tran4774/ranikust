//! Base-layer extraction into the store, unprivileged (T0): the tar
//! headers carry the test user's ids, and whiteouts (`mknod c 0 0`) need no
//! privilege on Linux 5.8 and later.

use std::fs;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::sync::Arc;

use rnk_oci::Digest;
use rnk_snapshot::{Error, MountApi, Overlay, OverlayCaps, Snapshotter};
use rnk_store::{BuildPaths, Store};
use sha2::{Digest as _, Sha256};
use tar::{EntryType, Header};

struct Tar {
    out: Vec<u8>,
}

impl Tar {
    fn new() -> Self {
        Self { out: Vec::new() }
    }

    fn entry(&mut self, name: &str, t: EntryType, mode: u32, data: &[u8]) -> &mut Self {
        let mut h = Header::new_gnu();
        h.set_path(name).unwrap();
        h.set_entry_type(t);
        h.set_mode(mode);
        h.set_uid(u64::from(rustix::process::geteuid().as_raw()));
        h.set_gid(u64::from(rustix::process::getegid().as_raw()));
        h.set_mtime(1_000_000);
        h.set_size(u64::try_from(data.len()).unwrap());
        h.set_cksum();
        self.out.extend_from_slice(h.as_bytes());
        self.out.extend_from_slice(data);
        self.out
            .extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
        self
    }

    fn finish(&mut self) -> (Vec<u8>, Digest) {
        self.out.extend(std::iter::repeat_n(0, 1024));
        let hash: [u8; 32] = Sha256::digest(&self.out).into();
        (self.out.clone(), Digest::from_sha256_bytes(&hash))
    }
}

fn snapshotter() -> (tempfile::TempDir, Overlay) {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let overlay = Overlay::new(
        Arc::new(store),
        OverlayCaps {
            api: MountApi::LowerdirPlus,
        },
    )
    .unwrap();
    (dir, overlay)
}

#[test]
fn a_layer_is_extracted_once_in_the_overlay_form() {
    let (_dir, s) = snapshotter();
    let (tar, id) = Tar::new()
        .entry("etc/", EntryType::Directory, 0o755, b"")
        .entry("etc/os-release", EntryType::Regular, 0o644, b"ID=test\n")
        .entry("etc/.wh.gone", EntryType::Regular, 0o644, b"")
        .finish();
    let l = s.base_layer(&id, tar.as_slice(), &[]).unwrap();
    assert_eq!(l.diff_id(), &id);
    assert!(
        l.path()
            .ends_with(format!("layers/sha256/{}", id.encoded()))
    );
    assert_eq!(
        fs::read(l.path().join("etc/os-release")).unwrap(),
        b"ID=test\n"
    );
    let wh = fs::symlink_metadata(l.path().join("etc/gone")).unwrap();
    assert!(wh.file_type().is_char_device());
    assert_eq!(wh.rdev(), 0);
    // Published: a second extraction reads nothing and gives the same.
    let again = s.base_layer(&id, &b"not a tar"[..], &[]).unwrap();
    assert_eq!(again, l);
}

fn has_inherit_mark(dir: &std::path::Path) -> bool {
    let mut buf = [0u8; 8];
    rustix::fs::getxattr(dir, "user.ranikust.inherit-root", &mut buf).is_ok()
}

#[test]
fn a_layer_without_a_root_entry_is_marked_to_inherit_the_root() {
    let (_dir, s) = snapshotter();
    let (tar, id) = Tar::new()
        .entry("f", EntryType::Regular, 0o644, b"")
        .finish();
    let l = s.base_layer(&id, tar.as_slice(), &[]).unwrap();
    assert!(has_inherit_mark(l.path()));

    // 0750, not an image's usual 0555: publishing renames the directory to
    // another parent, which needs write permission on it unless root.
    let (tar, id) = Tar::new()
        .entry("./", EntryType::Directory, 0o750, b"")
        .finish();
    let l = s.base_layer(&id, tar.as_slice(), &[]).unwrap();
    assert!(!has_inherit_mark(l.path()));
    assert_eq!(fs::metadata(l.path()).unwrap().mode() & 0o7777, 0o750);
}

#[test]
fn a_stage_is_not_released_while_a_stage_built_on_it_lives() {
    use rnk_snapshot::{Base, StageRootfs};
    let (_dir, s) = snapshotter();
    let parent = s.prepare(Base::Scratch).unwrap();
    let parent_dir = parent.mount_point().parent().unwrap().to_owned();
    let child = s.prepare(Base::Stage(&parent)).unwrap();
    let e = parent.release().unwrap_err();
    assert!(matches!(e, Error::State(_)), "{e}");
    // The refused parent's directories stay, for the child's lowers.
    assert!(parent_dir.exists());
    child.release().unwrap();
}

#[test]
fn a_state_directory_with_option_syntax_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("x,lowerdir=");
    let store = Store::open(&BuildPaths::new(dir.path(), &state)).unwrap();
    let e = Overlay::new(
        Arc::new(store),
        OverlayCaps {
            api: MountApi::Mount2,
        },
    )
    .unwrap_err();
    assert!(matches!(e, Error::UnsafePath { .. }), "{e}");
}

#[test]
fn a_tar_that_does_not_hash_to_the_diff_id_is_not_published() {
    let (_dir, s) = snapshotter();
    let (tar, _) = Tar::new()
        .entry("f", EntryType::Regular, 0o644, b"x")
        .finish();
    let wrong = Digest::from_sha256_bytes(&[7; 32]);
    let e = s.base_layer(&wrong, tar.as_slice(), &[]).unwrap_err();
    assert!(e.to_string().contains("diff ID"), "{e}");
    assert!(matches!(e, Error::Store(_)), "{e}");
    assert!(
        s.store()
            .get(&rnk_store::EntryKey::Layer(wrong))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_gzip_blob_is_decoded_and_checked_against_the_tar() {
    let (_dir, s) = snapshotter();
    let (tar, id) = Tar::new()
        .entry("bin/", EntryType::Directory, 0o755, b"")
        .finish();
    let mut gz = Vec::new();
    {
        use std::io::Write as _;
        let mut w = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
        w.write_all(&tar).unwrap();
        w.finish().unwrap();
    }
    let l = s.base_layer(&id, gz.as_slice(), &[]).unwrap();
    assert!(l.path().join("bin").is_dir());
}

#[test]
fn a_hardlink_into_a_lower_layer_is_copied_from_it() {
    let (_dir, s) = snapshotter();
    let (t1, id1) = Tar::new()
        .entry("usr/", EntryType::Directory, 0o755, b"")
        .entry("usr/a", EntryType::Regular, 0o644, b"lower")
        .finish();
    let l1 = s.base_layer(&id1, t1.as_slice(), &[]).unwrap();
    let mut t = Tar::new();
    let mut h = Header::new_gnu();
    h.set_path("usr/b").unwrap();
    h.set_link_name("usr/a").unwrap();
    h.set_entry_type(EntryType::Link);
    h.set_mode(0o644);
    h.set_uid(u64::from(rustix::process::geteuid().as_raw()));
    h.set_gid(u64::from(rustix::process::getegid().as_raw()));
    h.set_mtime(1_000_000);
    h.set_size(0);
    h.set_cksum();
    t.out.extend_from_slice(h.as_bytes());
    let (t2, id2) = t.finish();
    let l2 = s
        .base_layer(&id2, t2.as_slice(), std::slice::from_ref(&l1))
        .unwrap();
    assert_eq!(fs::read(l2.path().join("usr/b")).unwrap(), b"lower");
}

#[test]
fn layers_are_keyed_by_chain_id() {
    // As BuildKit and containerd (`cache/blobs.go`): a layer's extraction
    // depends on the stack below it (a hardlink copied from a lower), so
    // one diff ID on two stacks is extracted twice; a bottom layer's chain
    // ID is its diff ID.
    let (_dir, s) = snapshotter();
    let (ta, ida) = Tar::new()
        .entry("a", EntryType::Regular, 0o644, b"a")
        .finish();
    let (tb, idb) = Tar::new()
        .entry("b", EntryType::Regular, 0o644, b"b")
        .finish();
    let (tc, idc) = Tar::new()
        .entry("c", EntryType::Regular, 0o644, b"c")
        .finish();
    let a = s.base_layer(&ida, ta.as_slice(), &[]).unwrap();
    assert!(
        a.path().ends_with(ida.to_string().replace(':', "/")),
        "{a:?}"
    );
    let b = s.base_layer(&idb, tb.as_slice(), &[]).unwrap();
    let c_on_a = s
        .base_layer(&idc, tc.as_slice(), std::slice::from_ref(&a))
        .unwrap();
    let c_on_b = s
        .base_layer(&idc, tc.as_slice(), std::slice::from_ref(&b))
        .unwrap();
    assert_ne!(c_on_a.path(), c_on_b.path());
    assert_eq!(c_on_a.diff_id(), &idc);
    // ChainID(c on a) = sha256("<diff a> <diff c>") (OCI image spec).
    let chain: [u8; 32] = Sha256::digest(format!("{ida} {idc}")).into();
    let chain = Digest::from_sha256_bytes(&chain);
    assert!(
        c_on_a.path().ends_with(chain.to_string().replace(':', "/")),
        "{c_on_a:?}"
    );
    // The same stack again reuses the extraction.
    let again = s
        .base_layer(&idc, &b"not a tar"[..], std::slice::from_ref(&a))
        .unwrap();
    assert_eq!(again.path(), c_on_a.path());
}
