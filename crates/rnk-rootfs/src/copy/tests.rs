use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::meta::FileTime;
use crate::test_util::{Fixture, errno, snapshot, tree};

fn mode_of(p: &Path) -> u32 {
    fs::symlink_metadata(p).unwrap().mode() & 0o7777
}

fn set_mode(p: &Path, mode: u32) {
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

fn set_mtime(f: &Fixture, p: &str, secs: i64) {
    let m = Meta {
        mtime: Some(FileTime::from_secs(secs)),
        ..Meta::default()
    };
    f.root.set_meta(p, &m).unwrap();
}

fn xattr(p: &Path, name: &str) -> Option<Vec<u8>> {
    let mut buf = vec![0; 256];
    let n = rustix::fs::lgetxattr(p, name, &mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// A source tree with every kind of entry `copy` handles unprivileged.
fn source() -> Fixture {
    let s = Fixture::new();
    fs::create_dir_all(s.host("a/ro")).unwrap();
    fs::write(s.host("a/f"), b"content").unwrap();
    set_mode(&s.host("a/f"), 0o4750);
    fs::hard_link(s.host("a/f"), s.host("a/hl")).unwrap();
    fs::write(s.host("a/ro/inside"), b"ro").unwrap();
    symlink("../f", s.host("a/ro/up")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        s.host("a/fifo"),
        RawFileType::Fifo,
        Mode::from_raw_mode(0o640),
        0,
    )
    .unwrap();
    set_mode(&s.host("a/fifo"), 0o640);
    set_mode(&s.host("a/ro/inside"), 0o444);
    for p in ["a/f", "a/ro/inside", "a/fifo"] {
        set_mtime(&s, p, 1_000);
    }
    set_mtime(&s, "a/ro", 2_000);
    set_mode(&s.host("a/ro"), 0o555);
    set_mtime(&s, "a", 3_000);
    set_mode(&s.host("a"), 0o750);
    s
}

#[test]
fn a_directory_copies_its_contents_with_raw_modes_times_and_links() {
    let s = source();
    let d = Fixture::new();
    copy(&s.root, "a", &d.root, "/app/", &CopyOpts::default()).unwrap();

    let app = d.root.statx("app").unwrap();
    // Created for the contents: 0755, the source's owner and times.
    assert_eq!(app.permissions(), 0o755);
    assert_eq!(app.mtime.secs, 3_000);
    assert_eq!(fs::read(d.host("app/f")).unwrap(), b"content");
    assert_eq!(mode_of(&d.host("app/f")), 0o4750);
    assert_eq!(d.root.statx("app/f").unwrap().mtime.secs, 1_000);
    // The hardlink stays one inode with two names.
    let f = fs::metadata(d.host("app/f")).unwrap();
    assert_eq!(f.nlink(), 2);
    assert_eq!(fs::metadata(d.host("app/hl")).unwrap().ino(), f.ino());
    assert_eq!(mode_of(&d.host("app/ro")), 0o555);
    assert_eq!(d.root.statx("app/ro").unwrap().mtime.secs, 2_000);
    assert_eq!(mode_of(&d.host("app/ro/inside")), 0o444);
    assert_eq!(
        fs::read_link(d.host("app/ro/up")).unwrap(),
        Path::new("../f")
    );
    let fifo = fs::symlink_metadata(d.host("app/fifo")).unwrap();
    assert!(fifo.file_type().is_fifo());
    assert_eq!(mode_of(&d.host("app/fifo")), 0o640);
    let (uid, gid) = d.ids();
    assert_eq!((app.uid, app.gid), (uid, gid));
}

#[test]
fn chown_and_chmod_apply_to_every_entry_but_symlink_modes() {
    let s = source();
    let d = Fixture::new();
    let (uid, gid) = d.ids();
    let o = CopyOpts {
        owner: Some(Owner { uid, gid }),
        chmod: Some(Chmod::Octal(0o750)),
        skip_special: false,
    };
    copy(&s.root, "a", &d.root, "deep/er/app", &o).unwrap();
    for p in ["deep/er/app/f", "deep/er/app/ro", "deep/er/app/fifo"] {
        assert_eq!(mode_of(&d.host(p)), 0o750, "{p}");
    }
    // Created parents and the created destination take an octal mode too,
    // as BuildKit's `MkdirAll` (Q-120).
    for p in ["deep", "deep/er", "deep/er/app"] {
        assert_eq!(mode_of(&d.host(p)), 0o750, "{p}");
    }
    assert!(
        fs::symlink_metadata(d.host("deep/er/app/ro/up"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn a_file_goes_to_exactly_its_destination() {
    let s = source();
    let d = Fixture::new();
    copy(&s.root, "a/f", &d.root, "x/y/renamed", &CopyOpts::default()).unwrap();
    assert_eq!(fs::read(d.host("x/y/renamed")).unwrap(), b"content");
    assert_eq!(mode_of(&d.host("x")), 0o755);
    // A hardlinked source alone is a plain file.
    assert_eq!(fs::metadata(d.host("x/y/renamed")).unwrap().nlink(), 1);

    // An existing file is replaced; an existing directory is an error.
    fs::write(d.host("old"), b"old").unwrap();
    copy(&s.root, "a/f", &d.root, "old", &CopyOpts::default()).unwrap();
    assert_eq!(fs::read(d.host("old")).unwrap(), b"content");
    fs::create_dir(d.host("dir")).unwrap();
    let e = copy(&s.root, "a/f", &d.root, "dir", &CopyOpts::default()).unwrap_err();
    assert!(matches!(e, Error::InvalidPath { .. }), "{e}");
    let e = copy(&s.root, "a/f", &d.root, "/", &CopyOpts::default()).unwrap_err();
    assert!(matches!(e, Error::InvalidPath { .. }), "{e}");
    let e = copy(&s.root, "a/f", &d.root, "../../x", &CopyOpts::default()).unwrap_err();
    assert!(matches!(e, Error::InvalidPath { .. }), "{e}");
}

#[test]
fn an_existing_destination_directory_is_merged_and_keeps_its_metadata() {
    let s = source();
    let d = Fixture::new();
    fs::create_dir_all(d.host("app/a_dir_in_the_way")).unwrap();
    fs::create_dir(d.host("app/ro")).unwrap();
    fs::write(d.host("app/ro/kept"), b"").unwrap();
    fs::create_dir(d.host("app/f")).unwrap();
    set_mode(&d.host("app"), 0o711);
    // A directory where the source has a file: refused.
    let e = copy(&s.root, "a", &d.root, "app", &CopyOpts::default()).unwrap_err();
    assert!(matches!(e, Error::InvalidPath { .. }), "{e}");
    fs::remove_dir(d.host("app/f")).unwrap();
    fs::write(d.host("app/hl"), b"a file where the source has a file").unwrap();
    copy(&s.root, "a", &d.root, "app", &CopyOpts::default()).unwrap();
    assert_eq!(mode_of(&d.host("app")), 0o711);
    assert!(d.host("app/ro/kept").exists());
    assert!(d.host("app/a_dir_in_the_way").is_dir());
    assert_eq!(fs::read(d.host("app/hl")).unwrap(), b"content");
}

#[test]
fn the_root_of_the_source_copies_everything() {
    let s = source();
    let d = Fixture::new();
    copy(&s.root, "", &d.root, "", &CopyOpts::default()).unwrap();
    assert_eq!(tree(&s.host), tree(&d.host));
}

#[test]
fn a_symlinked_source_is_followed_inside_its_root_only() {
    let s = source();
    let (out, before) = s.outside();
    // Absolute and climbing targets resolve inside the source root.
    symlink("/a/f", s.host("abs")).unwrap();
    symlink("../../../../a/f", s.host("climb")).unwrap();
    symlink(out.join("sentinel"), s.host("host")).unwrap();
    let d = Fixture::new();
    copy(&s.root, "abs", &d.root, "abs", &CopyOpts::default()).unwrap();
    copy(&s.root, "climb", &d.root, "climb", &CopyOpts::default()).unwrap();
    assert_eq!(fs::read(d.host("abs")).unwrap(), b"content");
    assert_eq!(fs::read(d.host("climb")).unwrap(), b"content");
    let e = copy(&s.root, "host", &d.root, "host", &CopyOpts::default()).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT), "{e}");
    assert!(!d.host("host").exists());
    assert_eq!(snapshot(&out), before);
}

#[test]
fn symlinks_inside_a_copied_directory_are_never_followed() {
    let s = Fixture::new();
    let (out, before) = s.outside();
    fs::create_dir(s.host("a")).unwrap();
    symlink(&out, s.host("a/outdir")).unwrap();
    symlink(out.join("sentinel"), s.host("a/outfile")).unwrap();
    let d = Fixture::new();
    copy(&s.root, "a", &d.root, "app", &CopyOpts::default()).unwrap();
    assert!(
        fs::symlink_metadata(d.host("app/outdir"))
            .unwrap()
            .is_symlink()
    );
    assert!(
        fs::symlink_metadata(d.host("app/outfile"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        tree(&d.host),
        ["app", "app/outdir", "app/outfile"]
            .iter()
            .map(PathBuf::from)
            .collect()
    );
    assert_eq!(snapshot(&out), before);
}

#[test]
fn destination_symlinks_stay_inside_the_destination_root() {
    let s = source();
    let d = Fixture::new();
    let (out, before) = d.outside();
    // A destination directory that is a symlink to the host's outside dir:
    // followed inside the root, where that path does not exist.
    symlink(&out, d.host("app")).unwrap();
    let e = copy(&s.root, "a", &d.root, "app", &CopyOpts::default()).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT), "{e}");
    let e = copy(&s.root, "a/f", &d.root, "app/f", &CopyOpts::default()).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT), "{e}");
    // The last name is replaced, never written through.
    symlink(out.join("sentinel"), d.host("file")).unwrap();
    copy(&s.root, "a/f", &d.root, "file", &CopyOpts::default()).unwrap();
    assert!(fs::symlink_metadata(d.host("file")).unwrap().is_file());
    // A symlink to a directory inside the root is followed for the
    // destination of a directory's contents.
    fs::create_dir(d.host("real")).unwrap();
    symlink("/real", d.host("via")).unwrap();
    copy(&s.root, "a/ro", &d.root, "via", &CopyOpts::default()).unwrap();
    assert!(d.host("real/inside").is_file());
    assert_eq!(snapshot(&out), before);
}

#[test]
fn sockets_become_empty_files() {
    // BuildKit's `copyDevice` clears the socket bit: an empty regular file
    // with the socket's mode (Q-120).
    let s = Fixture::new();
    fs::create_dir(s.host("a")).unwrap();
    let _l = std::os::unix::net::UnixListener::bind(s.host("a/sock")).unwrap();
    set_mode(&s.host("a/sock"), 0o640);
    fs::write(s.host("a/f"), b"").unwrap();
    let d = Fixture::new();
    copy(&s.root, "a", &d.root, "app", &CopyOpts::default()).unwrap();
    assert!(d.host("app/f").exists());
    let m = fs::symlink_metadata(d.host("app/sock")).unwrap();
    assert!(m.file_type().is_file());
    assert_eq!((m.len(), m.mode() & 0o7777), (0, 0o640));
}

#[test]
fn xattrs_are_copied_but_overlays_own() {
    // BuildKit copies every xattr (Q-120); overlay's private ones never.
    let s = Fixture::new();
    fs::create_dir(s.host("d")).unwrap();
    fs::write(s.host("d/f"), b"").unwrap();
    for (p, name) in [
        ("d/f", "user.app"),
        ("d", "user.dir"),
        ("d/f", "user.overlay.origin"),
        ("d/f", "user.overlay.overlay.kept"),
    ] {
        rustix::fs::setxattr(s.host(p), name, b"v", rustix::fs::XattrFlags::empty()).unwrap();
    }
    let d = Fixture::new();
    copy(&s.root, "", &d.root, "out", &CopyOpts::default()).unwrap();
    assert_eq!(xattr(&d.host("out/d/f"), "user.app"), Some(b"v".to_vec()));
    assert_eq!(xattr(&d.host("out/d"), "user.dir"), Some(b"v".to_vec()));
    assert_eq!(xattr(&d.host("out/d/f"), "user.overlay.origin"), None);
    assert_eq!(
        xattr(&d.host("out/d/f"), "user.overlay.overlay.kept"),
        Some(b"v".to_vec())
    );
}

#[test]
fn a_symbolic_chmod_changes_each_entrys_own_mode() {
    let s = Fixture::new();
    fs::create_dir(s.host("d")).unwrap();
    fs::write(s.host("d/f"), b"").unwrap();
    fs::write(s.host("d/run"), b"").unwrap();
    set_mode(&s.host("d/f"), 0o640);
    set_mode(&s.host("d/run"), 0o700);
    set_mode(&s.host("d"), 0o700);
    let d = Fixture::new();
    let o = CopyOpts {
        chmod: Some(Chmod::parse("go+rX").unwrap()),
        ..CopyOpts::default()
    };
    copy(&s.root, "", &d.root, "new/out", &o).unwrap();
    assert_eq!(mode_of(&d.host("new/out/d/f")), 0o644);
    assert_eq!(mode_of(&d.host("new/out/d/run")), 0o755);
    assert_eq!(mode_of(&d.host("new/out/d")), 0o755);
    // Created parents: 0755 for a symbolic mode.
    assert_eq!(mode_of(&d.host("new")), 0o755);
}

#[test]
fn entries_on_another_mount_are_refused() {
    let s = source();
    let d = Fixture::new();
    fs::create_dir(d.host("app")).unwrap();
    let id = d.root.mnt_id();
    let fake = RootPath::open(&d.host).unwrap().with_mnt_id(id + 1);
    let e = copy(&s.root, "a", &fake, "app", &CopyOpts::default()).unwrap_err();
    assert!(
        matches!(e, Error::InvalidPath { reason, .. } if reason.contains("mount")),
        "{e}"
    );
}

#[test]
fn concurrent_swaps_never_leave_either_root() {
    let s = source();
    let d = Fixture::new();
    let (sout, sbefore) = s.outside();
    let (dout, dbefore) = d.outside();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Swap a source directory and a destination directory for symlinks
        // to the outside directories, over and over.
        let swap = |base: PathBuf, dir: &'static str, out: PathBuf| {
            let stop = &stop;
            move || {
                let real = base.join(dir);
                let parked = base.join(format!("{dir}.parked"));
                let link = base.join(format!("{dir}.link"));
                let _ = symlink(&out, &link);
                while !stop.load(Ordering::Relaxed) {
                    if fs::rename(&real, &parked).is_ok() {
                        let _ = fs::rename(&link, &real);
                        let _ = fs::rename(&real, &link);
                        let _ = fs::rename(&parked, &real);
                    }
                }
            }
        };
        scope.spawn(swap(s.host.clone(), "a", sout.clone()));
        scope.spawn(swap(d.host.clone(), "app", dout.clone()));
        for _ in 0..200 {
            let _ = fs::create_dir(d.host("app"));
            // Errors are fine; leaving a root is not.
            let _ = copy(&s.root, "a", &d.root, "app", &CopyOpts::default());
        }
        stop.store(true, Ordering::Relaxed);
    });
    assert_eq!(snapshot(&sout), sbefore);
    assert_eq!(snapshot(&dout), dbefore);
    // Nothing from the outside directories was copied as content.
    for p in tree(&d.host) {
        assert_ne!(p.file_name().unwrap(), "sentinel", "{}", p.display());
    }
}

/// Run by the T2 harness in the VM: device nodes need `CAP_MKNOD`, foreign
/// owners root.
#[test]
#[ignore = "T2: mknod of a real device and chown to other users need root"]
fn device_nodes_and_foreign_owners_are_copied() {
    let s = Fixture::new();
    fs::create_dir(s.host("a")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        s.host("a/null"),
        RawFileType::CharacterDevice,
        Mode::from_raw_mode(0o666),
        rustix::fs::makedev(1, 3),
    )
    .unwrap();
    fs::write(s.host("a/owned"), b"").unwrap();
    rustix::fs::chown(
        s.host("a/owned"),
        Some(rustix::fs::Uid::from_raw(1234)),
        Some(rustix::fs::Gid::from_raw(5678)),
    )
    .unwrap();
    let d = Fixture::new();
    copy(&s.root, "a", &d.root, "app", &CopyOpts::default()).unwrap();
    let null = fs::symlink_metadata(d.host("app/null")).unwrap();
    assert!(null.file_type().is_char_device());
    assert_eq!(null.rdev(), rustix::fs::makedev(1, 3));
    let owned = d.root.statx("app/owned").unwrap();
    assert_eq!((owned.uid, owned.gid), (1234, 5678));
    // --chown 0:0 over a foreign owner.
    let o = CopyOpts {
        owner: Some(Owner { uid: 0, gid: 0 }),
        chmod: None,
        skip_special: false,
    };
    copy(&s.root, "a", &d.root, "root", &o).unwrap();
    let owned = d.root.statx("root/owned").unwrap();
    assert_eq!((owned.uid, owned.gid), (0, 0));
}

/// Without privileges, the same copies fail cleanly instead.
#[test]
fn foreign_owners_fail_cleanly_unprivileged() {
    let s = source();
    let d = Fixture::new();
    if d.ids().0 == 0 {
        return; // Running as root: the T2 test covers this.
    }
    let o = CopyOpts {
        owner: Some(Owner { uid: 0, gid: 0 }),
        chmod: None,
        skip_special: false,
    };
    let e = copy(&s.root, "a/f", &d.root, "f", &o).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::PERM), "{e}");
}

#[test]
fn a_destination_inside_the_source_is_refused() {
    // One root as both source and destination, as kaniko mode on a live
    // root copies (`COPY . /workspace/sub`): without the check this recurses
    // until the fds or the disk run out.
    for (from, to) in [
        ("", "sub"),
        ("a", "a/b/c"),
        ("a", "a"),
        ("a", "a/new/deeper"),
    ] {
        let s = source();
        let e = copy(&s.root, from, &s.root, to, &CopyOpts::default()).unwrap_err();
        assert!(
            matches!(e, Error::InvalidPath { reason, .. } if reason.contains("inside")),
            "{from} -> {to}: {e}"
        );
    }
    // Next to the source, not inside it: fine.
    let s = source();
    copy(&s.root, "a/ro", &s.root, "a/ro2", &CopyOpts::default()).unwrap();
    assert!(s.host("a/ro2/inside").is_file());
}

#[test]
fn a_filter_leaves_out_refused_entries_and_whole_directories() {
    let s = source();
    let d = Fixture::new();
    let keep = |p: &Path, is_dir: bool| -> bool {
        // Refuses a file, and a directory with what is below it.
        p != Path::new("a/hl") && !(is_dir && p == Path::new("a/ro"))
    };
    copy_filtered(&s.root, "a", &d.root, "/app", &CopyOpts::default(), &keep).unwrap();
    assert_eq!(fs::read(d.host("app/f")).unwrap(), b"content");
    assert!(fs::symlink_metadata(d.host("app/hl")).is_err());
    assert!(fs::symlink_metadata(d.host("app/ro")).is_err());
    assert!(fs::symlink_metadata(d.host("app/fifo")).is_ok());
}

#[test]
fn skip_special_leaves_fifos_out_and_copies_the_rest() {
    let s = source();
    let d = Fixture::new();
    let o = CopyOpts {
        skip_special: true,
        ..CopyOpts::default()
    };
    copy_filtered(&s.root, "a", &d.root, "out", &o, &|_, _| true).unwrap();
    assert!(
        fs::symlink_metadata(d.host("out/fifo")).is_err(),
        "no FIFO made"
    );
    assert_eq!(fs::read(d.host("out/f")).unwrap(), b"content");
    assert_eq!(
        fs::metadata(d.host("out/f")).unwrap().ino(),
        fs::metadata(d.host("out/hl")).unwrap().ino(),
        "hardlinks inside the tree kept"
    );
    // A FIFO named as the source itself is left out too.
    copy(&s.root, "a/fifo", &d.root, "lone", &o).unwrap();
    assert!(fs::symlink_metadata(d.host("lone")).is_err());
    // Without the switch it is made again, never opened.
    copy(&s.root, "a/fifo", &d.root, "made", &CopyOpts::default()).unwrap();
    assert!(
        fs::symlink_metadata(d.host("made"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
}

#[test]
fn a_sparse_file_is_copied_without_filling_its_holes() {
    use std::io::{Seek as _, SeekFrom, Write as _};
    let s = Fixture::new();
    let d = Fixture::new();
    let big = 1_u64 << 33; // 8 GiB, nearly all holes
    {
        let mut f = fs::File::create(s.host("sparse")).unwrap();
        f.write_all(b"head").unwrap();
        f.seek(SeekFrom::Start(big / 2)).unwrap();
        f.write_all(b"middle").unwrap();
        f.set_len(big).unwrap();
    }
    copy(&s.root, "sparse", &d.root, "copy", &CopyOpts::default()).unwrap();
    let m = fs::metadata(d.host("copy")).unwrap();
    assert_eq!(m.len(), big, "the size kept");
    assert!(
        m.blocks() * 512 < 64 << 20,
        "holes stay holes: {} bytes allocated",
        m.blocks() * 512
    );
    let f = fs::File::open(d.host("copy")).unwrap();
    let mut buf = [0_u8; 6];
    rustix::io::pread(&f, &mut buf[..4], 0).unwrap();
    assert_eq!(&buf[..4], b"head");
    rustix::io::pread(&f, &mut buf, big / 2).unwrap();
    assert_eq!(&buf, b"middle");
    rustix::io::pread(&f, &mut buf[..4], big - 4).unwrap();
    assert_eq!(&buf[..4], &[0; 4]);
}

#[test]
fn an_empty_file_and_a_dense_file_copy_exactly() {
    let s = Fixture::new();
    let d = Fixture::new();
    fs::write(s.host("empty"), b"").unwrap();
    let dense: Vec<u8> = (0..300_000_u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    fs::write(s.host("dense"), &dense).unwrap();
    copy(&s.root, "empty", &d.root, "e", &CopyOpts::default()).unwrap();
    copy(&s.root, "dense", &d.root, "d", &CopyOpts::default()).unwrap();
    assert_eq!(fs::read(d.host("e")).unwrap(), b"");
    assert_eq!(fs::read(d.host("d")).unwrap(), dense);
}

#[test]
fn a_file_holding_more_than_its_size_is_refused_never_cut_short() {
    // procfs shows size 0 for a file with content: the case of a source
    // whose size understates it, or one appended to while copied.
    let src = fs::File::open("/proc/self/status").unwrap();
    assert_eq!(src.metadata().unwrap().len(), 0);
    let d = Fixture::new();
    let out = fs::File::create(d.host("out")).unwrap();
    let e = copy_data(src.as_fd(), out.as_fd(), Path::new("status")).unwrap_err();
    assert!(e.to_string().contains("changed while it was copied"), "{e}");
}

// Red team (T3, T15, ADR-0059): a source changed while `copy_data` copies
// it, as a process left behind in a parent stage would change it.

/// A sparse source of many data ranges, truncated to nothing while it is
/// copied: the copy fails, or holds what the source held at one moment
/// (all of it, or nothing). A copy of the old size with the ranges after
/// the cut turned into zeros is a file cut short without an error.
#[test]
fn red_team_a_source_truncated_while_copied_is_refused_never_zero_filled() {
    use std::io::{Seek as _, SeekFrom, Write as _};
    const STEP: u64 = 1 << 20;
    const RANGES: u64 = 256;
    let s = Fixture::new();
    let d = Fixture::new();
    let mut silent = 0;
    for attempt in 0..200_u64 {
        let src_path = s.host("src");
        {
            let mut f = fs::File::create(&src_path).unwrap();
            for i in 0..RANGES {
                f.seek(SeekFrom::Start(i * STEP)).unwrap();
                f.write_all(&[0xab; 4096]).unwrap();
            }
            f.set_len(RANGES * STEP).unwrap();
        }
        let src = fs::File::open(&src_path).unwrap();
        let out_path = d.host("out");
        let _ = fs::remove_file(&out_path);
        let out = fs::File::create(&out_path).unwrap();
        let cutter = fs::OpenOptions::new().write(true).open(&src_path).unwrap();
        let r = std::thread::scope(|scope| {
            scope.spawn(|| {
                // A varying delay, so that the cut lands at a different
                // range each attempt.
                for _ in 0..(attempt * 997) % 50_000 {
                    std::hint::spin_loop();
                }
                cutter.set_len(0).unwrap();
            });
            copy_data(src.as_fd(), out.as_fd(), Path::new("src"))
        });
        if let Ok(n) = r {
            let got = fs::read(&out_path).unwrap();
            assert_eq!(n, u64::try_from(got.len()).unwrap());
            let present = (0..RANGES)
                .filter(|i| got.get(usize::try_from(i * STEP).unwrap()) == Some(&0xab))
                .count();
            if n != 0 && present != usize::try_from(RANGES).unwrap() {
                silent += 1;
                eprintln!("attempt {attempt}: copied {n} bytes, {present} of {RANGES} ranges, Ok");
            }
        }
    }
    assert_eq!(silent, 0, "copies cut short without an error");
}
