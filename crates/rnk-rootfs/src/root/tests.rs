use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use rustix::io::FdFlags;

use super::*;
use crate::FileTime;
use crate::test_util::{Fixture, errno};

#[test]
fn open_gives_cloexec_path_fd_and_mount_id() {
    let f = Fixture::new();
    let flags = rustix::io::fcntl_getfd(f.root.fd()).unwrap();
    assert!(flags.contains(FdFlags::CLOEXEC));
    let fl = rustix::fs::fcntl_getfl(f.root.fd()).unwrap();
    assert!(fl.contains(OFlags::PATH));
    assert_ne!(f.root.mnt_id(), 0);
}

#[test]
fn open_refuses_symlinked_host_path_and_missing_dir() {
    let f = Fixture::new();
    let link = f.host.with_file_name("link");
    symlink(&f.host, &link).unwrap();
    let e = RootPath::open(&link).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::LOOP), "{e}");
    let e = RootPath::open(f.host("missing")).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT), "{e}");
    assert!(matches!(
        e,
        Error::Io {
            op: "open root",
            ..
        }
    ));
}

/// `pin_with_dir` pins an entry and the directory it was found in, both by
/// descriptor, and resolves a final symlink from the link's own directory,
/// clamped to the root: the directory returned is the target's, whatever
/// the link's spelling (ADR-0076).
#[test]
fn an_entry_is_pinned_with_the_directory_it_is_in_through_its_symlinks() {
    let f = Fixture::new();
    fs::create_dir(f.host("a")).unwrap();
    fs::write(f.host("a/f"), b"x").unwrap();
    symlink("a/f", f.host("rel")).unwrap();
    symlink("/a/f", f.host("abs")).unwrap();
    symlink("../../a/f", f.host("climb")).unwrap();
    symlink("rel", f.host("chain")).unwrap();
    let file = fs::metadata(f.host("a/f")).unwrap();
    let dir = fs::metadata(f.host("a")).unwrap();
    for name in ["a/f", "rel", "abs", "climb", "chain"] {
        let (_, st, d) = f.root.pin_with_dir(Path::new(name)).unwrap();
        assert_eq!(st.ino, file.ino(), "{name}");
        assert_eq!(d.ino, dir.ino(), "{name}");
    }
    symlink("y", f.host("x")).unwrap();
    symlink("x", f.host("y")).unwrap();
    let e = f.root.pin_with_dir(Path::new("x")).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::LOOP), "{e}");
}

/// A context file is judged by the directory it is in (ADR-0076): the
/// owner's own hardlink is read, a file the owner could not read by its
/// mode bits is refused. The foreign-hardlink case needs another account:
/// `rnk-context`'s T2c test.
#[test]
fn a_context_file_its_directory_owner_could_not_read_is_refused() {
    let f = Fixture::new();
    fs::create_dir(f.host("sub")).unwrap();
    fs::write(f.host("sub/df"), b"FROM scratch\n").unwrap();
    fs::hard_link(f.host("sub/df"), f.host("sub/df-link")).unwrap();
    fs::write(f.host("sub/locked"), b"no\n").unwrap();
    fs::set_permissions(f.host("sub/locked"), fs::Permissions::from_mode(0o000)).unwrap();
    assert!(f.root.open_context_file("sub/df").is_ok());
    assert!(f.root.open_context_file("sub/df-link").is_ok());
    // As root the directory's owner reads anything; else the rule refuses
    // before the kernel's own check would.
    if f.ids().0 != 0 {
        let e = f.root.open_context_file("sub/locked").unwrap_err();
        assert!(e.to_string().contains("could not read"), "{e}");
    }
}

#[test]
fn open_at_create_write_read() {
    let f = Fixture::new();
    let fd = f
        .root
        .open_at("/a.txt", &OpenHow::write().create_new(0o644))
        .unwrap();
    fs::File::from(fd).write_all(b"hello").unwrap();
    let e = f
        .root
        .open_at("a.txt", &OpenHow::write().create_new(0o644))
        .unwrap_err();
    assert_eq!(errno(&e), Some(Errno::EXIST));
    let mut s = String::new();
    fs::File::from(f.root.open_at("./a.txt", &OpenHow::read()).unwrap())
        .read_to_string(&mut s)
        .unwrap();
    assert_eq!(s, "hello");
    assert_eq!(fs::read(f.host("a.txt")).unwrap(), b"hello");

    let fd = f.root.open_at("a.txt", &OpenHow::write().append()).unwrap();
    fs::File::from(fd).write_all(b"!").unwrap();
    assert_eq!(fs::read(f.host("a.txt")).unwrap(), b"hello!");
    let fd = f
        .root
        .open_at("a.txt", &OpenHow::read_write().truncate())
        .unwrap();
    drop(fd);
    assert_eq!(fs::read(f.host("a.txt")).unwrap(), b"");
    let fd = f
        .root
        .open_at("b.txt", &OpenHow::write().create(0o600))
        .unwrap();
    let fl = rustix::io::fcntl_getfd(&fd).unwrap();
    assert!(fl.contains(FdFlags::CLOEXEC));
}

#[test]
fn open_at_no_follow_and_path_handles() {
    let f = Fixture::new();
    fs::write(f.host("t"), b"x").unwrap();
    symlink("t", f.host("l")).unwrap();
    assert!(f.root.open_at("l", &OpenHow::read()).is_ok());
    let e = f
        .root
        .open_at("l", &OpenHow::read().no_follow())
        .unwrap_err();
    assert_eq!(errno(&e), Some(Errno::LOOP));
    let fd = f.root.open_at("l", &OpenHow::path().no_follow()).unwrap();
    let st = meta::statx_at(fd.as_fd(), OsStr::new(""), Path::new("l")).unwrap();
    assert_eq!(st.file_type(), FileType::Symlink);
    let e = f
        .root
        .open_at("t", &OpenHow::read().directory())
        .unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOTDIR));
    // A final symlink is not followed by create_new.
    let e = f
        .root
        .open_at("l", &OpenHow::write().create_new(0o600))
        .unwrap_err();
    assert_eq!(errno(&e), Some(Errno::EXIST));
}

#[test]
fn create_dir_and_create_dir_all() {
    let f = Fixture::new();
    f.root.create_dir("d", 0o755).unwrap();
    let e = f.root.create_dir("d", 0o755).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::EXIST));
    let e = f.root.create_dir("x/y", 0o755).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT));

    f.root.create_dir_all("/a/b/c", 0o755).unwrap();
    assert!(f.host("a/b/c").is_dir());
    f.root.create_dir_all("a/b/c", 0o755).unwrap();
    f.root.create_dir_all("a/./b/../b/d", 0o755).unwrap();
    assert!(f.host("a/b/d").is_dir());
    f.root.create_dir_all("/", 0o755).unwrap();

    // Through a symlink inside the root.
    symlink("/a/b", f.host("ab")).unwrap();
    f.root.create_dir_all("ab/e/f", 0o755).unwrap();
    assert!(f.host("a/b/e/f").is_dir());

    // A file in the way.
    fs::write(f.host("file"), b"").unwrap();
    let e = f.root.create_dir_all("file/x", 0o755).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOTDIR), "{e}");
    // A dangling symlink in the way.
    symlink("/nowhere", f.host("dangling")).unwrap();
    let e = f.root.create_dir_all("dangling/x", 0o755).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT), "{e}");
    assert!(!f.host("nowhere").exists());
}

#[test]
fn symlink_read_link_hard_link() {
    let f = Fixture::new();
    f.root.symlink("/etc/passwd", "p").unwrap();
    assert_eq!(f.root.read_link("p").unwrap(), Path::new("/etc/passwd"));
    assert_eq!(
        fs::read_link(f.host("p")).unwrap(),
        Path::new("/etc/passwd")
    );
    let e = f.root.symlink("x", "p").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::EXIST));

    fs::write(f.host("t"), b"x").unwrap();
    f.root.hard_link("/t", "h").unwrap();
    let (a, b) = (
        fs::metadata(f.host("t")).unwrap(),
        fs::metadata(f.host("h")).unwrap(),
    );
    assert_eq!(a.ino(), b.ino());
    assert_eq!(a.nlink(), 2);

    // A hard link to a symlink links the symlink, not its target.
    f.root.hard_link("p", "p2").unwrap();
    assert_eq!(f.root.read_link("p2").unwrap(), Path::new("/etc/passwd"));
    let e = f.root.read_link("t").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::INVAL));
}

#[test]
fn rename_and_remove() {
    let f = Fixture::new();
    fs::create_dir(f.host("d")).unwrap();
    fs::write(f.host("d/a"), b"x").unwrap();
    f.root.rename("d/a", "/b").unwrap();
    assert!(f.host("b").is_file());
    let e = f.root.remove_file("d").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::ISDIR));
    f.root.remove_file("b").unwrap();
    fs::write(f.host("d/c"), b"x").unwrap();
    let e = f.root.remove_dir("d").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOTEMPTY));
    f.root.remove_file("d/c").unwrap();
    f.root.remove_dir("d").unwrap();
    assert!(!f.host("d").exists());

    // A symlink is removed, not followed.
    fs::write(f.host("t"), b"x").unwrap();
    symlink("t", f.host("l")).unwrap();
    f.root.remove_file("l").unwrap();
    assert!(f.host("t").exists());
}

#[test]
fn invalid_paths() {
    let f = Fixture::new();
    for p in ["/", "", ".", "a/.."] {
        let e = f.root.remove_file(p).unwrap_err();
        assert!(matches!(e, Error::InvalidPath { .. }), "{p:?}: {e}");
    }
    let e = f.root.open_at("a\0b", &OpenHow::read()).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::INVAL));
}

#[test]
fn open_regular_reads_files_and_refuses_the_rest() {
    let f = Fixture::new();
    fs::write(f.host("t"), b"content").unwrap();
    symlink("/t", f.host("l")).unwrap();
    rustix::fs::mkfifoat(rustix::fs::CWD, f.host("fifo"), Mode::from_raw_mode(0o644)).unwrap();
    fs::create_dir(f.host("d")).unwrap();
    let mut s = String::new();
    let fd = f.root.open_regular("l").unwrap();
    assert!(
        rustix::io::fcntl_getfd(&fd)
            .unwrap()
            .contains(FdFlags::CLOEXEC)
    );
    fs::File::from(fd).read_to_string(&mut s).unwrap();
    assert_eq!(s, "content");
    for p in ["fifo", "d", "/"] {
        let e = f.root.open_regular(p).unwrap_err();
        assert!(matches!(e, Error::InvalidPath { .. }), "{p}: {e}");
    }
    let e = f.root.open_regular("missing").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT));
}

/// Paths reach the kernel unnormalized: `file/.` and `file/` are not `file`.
#[test]
fn trailing_dot_or_slash_after_a_file_is_enotdir() {
    let f = Fixture::new();
    fs::write(f.host("file"), b"x").unwrap();
    fs::create_dir(f.host("dir")).unwrap();
    for p in ["file/.", "file/", "/file/..", "file/./"] {
        let e = f.root.open_at(p, &OpenHow::read()).unwrap_err();
        assert_eq!(errno(&e), Some(Errno::NOTDIR), "{p}");
    }
    for p in ["dir/.", "dir/", "/dir/./"] {
        assert!(f.root.open_at(p, &OpenHow::read()).is_ok(), "{p}");
    }
}

#[test]
fn statx_does_not_follow_and_carries_mount_id() {
    let f = Fixture::new();
    fs::write(f.host("t"), b"abc").unwrap();
    symlink("t", f.host("l")).unwrap();
    let st = f.root.statx("t").unwrap();
    assert_eq!(st.file_type(), FileType::Regular);
    assert_eq!(st.size, 3);
    assert_eq!(st.mnt_id, f.root.mnt_id());
    assert!(!st.mount_root);
    let md = fs::symlink_metadata(f.host("t")).unwrap();
    assert_eq!((st.ino, st.uid, st.gid), (md.ino(), md.uid(), md.gid()));
    let st = f.root.statx("/l").unwrap();
    assert_eq!(st.file_type(), FileType::Symlink);
    let st = f.root.statx("/").unwrap();
    assert_eq!(st.file_type(), FileType::Directory);
    let e = f.root.statx("missing").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT));
}

/// A physical path is reached with no symlink on the way: one in a parent
/// is `ELOOP` where `statx` follows it, a last one is the link itself, and
/// mounts are crossed.
#[test]
fn statx_physical_refuses_a_symlink_in_a_parent() {
    let f = Fixture::new();
    fs::create_dir(f.host("d")).unwrap();
    fs::write(f.host("d/t"), b"abc").unwrap();
    symlink("d", f.host("l")).unwrap();
    assert_eq!(f.root.statx("/l/t").unwrap().size, 3);
    let e = f.root.statx_physical("/l/t").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::LOOP), "{e}");
    let st = f.root.statx_physical("/d/t").unwrap();
    assert_eq!((st.size, st.mnt_id), (3, f.root.mnt_id()));
    let st = f.root.statx_physical("/l").unwrap();
    assert_eq!(st.file_type(), FileType::Symlink);
    let host = RootPath::open("/").unwrap();
    // No mount id against `/`'s: qemu-user's statx gives 0 for both (`just test-aarch64`).
    let proc = host.statx_physical("/proc").unwrap();
    assert!(proc.mount_root);
    let version = host.statx_physical("/proc/version").unwrap();
    assert!(!version.mount_root && version.mnt_id == proc.mnt_id);
    // A magic link in a parent is not followed either.
    let magic = format!("/proc/{}/root/proc", std::process::id());
    let e = host.statx_physical(&magic).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::LOOP), "{magic}: {e}");
    // The kernel prints no `..`: `RESOLVE_IN_ROOT` would clamp one, not refuse it.
    for p in ["/../d/t", "/d/../d/t", "d/.."] {
        let e = f.root.statx_physical(p).unwrap_err();
        assert!(matches!(e, Error::InvalidPath { .. }), "{p}: {e}");
    }
    let e = f.root.statx_physical("missing").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT));
}

/// Read-only on the host's `/`: `/proc` is another mount below it, which a
/// root that stays on its mount does not enter.
#[test]
fn stay_on_mount_refuses_to_resolve_into_another_mount() {
    let host = RootPath::open("/").unwrap();
    assert!(host.statx("/proc/self").is_ok());
    let staying = RootPath::open("/").unwrap().stay_on_mount();
    let e = staying.statx("/proc/self").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::XDEV), "{e}");
    // The mount point itself is an entry of the root's own mount.
    assert!(staying.statx("/proc").is_ok());
    let f = Fixture::new();
    fs::create_dir(f.host("d")).unwrap();
    let staying = RootPath::open(&f.host).unwrap().stay_on_mount();
    assert!(staying.statx("/d").is_ok());
    let sub = staying.sub_root("/d").unwrap();
    assert!(sub.read_names("/").unwrap().is_empty());
}

#[test]
fn pin_refuses_a_symlink_in_a_parent_and_reads_through_the_pin() {
    let f = Fixture::new();
    fs::create_dir(f.host("d")).unwrap();
    fs::write(f.host("d/f"), b"abc").unwrap();
    fs::create_dir(f.host("other")).unwrap();
    fs::write(f.host("other/f"), b"zzz").unwrap();
    symlink("other", f.host("l")).unwrap();
    symlink("d/f", f.host("s")).unwrap();

    let e = f.root.pin("l/f").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::LOOP), "{e}");
    for p in ["../d/f", "other/../d/f", "d/.."] {
        let e = f.root.pin(p).unwrap_err();
        assert!(matches!(e, Error::InvalidPath { .. }), "{p}: {e}");
    }

    let pinned = f.root.pin("d/f").unwrap();
    assert_eq!(pinned.stat().file_type(), FileType::Regular);
    // The name now leads elsewhere; the pin still reads the inode it holds.
    fs::rename(f.host("d"), f.host("gone")).unwrap();
    symlink("other", f.host("d")).unwrap();
    let mut content = String::new();
    std::fs::File::from(pinned.open_regular().unwrap())
        .read_to_string(&mut content)
        .unwrap();
    assert_eq!(content, "abc");
    assert!(pinned.read_link().is_err());

    let link = f.root.pin("s").unwrap();
    assert_eq!(link.stat().file_type(), FileType::Symlink);
    assert_eq!(link.read_link().unwrap(), Path::new("d/f"));
    assert!(link.open_regular().is_err(), "a symlink is never opened");
    assert!(link.xattrs().unwrap().is_empty());
}

#[test]
fn pin_never_opens_a_fifo() {
    let f = Fixture::new();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        f.host("p"),
        rustix::fs::FileType::Fifo,
        Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    let pinned = f.root.pin("p").unwrap();
    assert_eq!(pinned.stat().file_type(), FileType::Fifo);
    assert!(pinned.open_regular().is_err());
    // Reading xattrs goes through the magic link without opening the FIFO.
    pinned.xattrs().unwrap();
}

#[test]
fn read_names_lists_entries_without_following_symlinks() {
    let f = Fixture::new();
    assert!(f.root.read_names("/").unwrap().is_empty());
    fs::create_dir(f.host("d")).unwrap();
    fs::write(f.host("d/a"), b"").unwrap();
    symlink("/", f.host("l")).unwrap();
    let mut names = f.root.read_names("/").unwrap();
    names.sort();
    assert_eq!(names, [OsString::from("d"), OsString::from("l")]);
    assert_eq!(f.root.read_names("d").unwrap(), [OsString::from("a")]);
    // `l` resolves to the root itself, never to the host's `/`.
    let mut through_link = f.root.read_names("l").unwrap();
    through_link.sort();
    assert_eq!(through_link, names);
    let e = f.root.read_names("d/a").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOTDIR), "{e}");
}

#[test]
fn set_meta_raw_modes_times_owner() {
    let f = Fixture::new();
    fs::write(f.host("f"), b"x").unwrap();
    fs::create_dir(f.host("tmp")).unwrap();
    let md = fs::metadata(f.host("f")).unwrap();
    let mtime = FileTime {
        secs: 1_000_000_000,
        nanos: 5,
    };
    f.root
        .set_meta(
            "f",
            &Meta {
                uid: Some(md.uid()),
                gid: Some(md.gid()),
                mode: Some(0o104_755),
                atime: Some(FileTime::from_secs(7)),
                mtime: Some(mtime),
                xattrs: vec![],
            },
        )
        .unwrap();
    let st = f.root.statx("f").unwrap();
    assert_eq!(st.permissions(), 0o4755);
    assert_eq!(st.mtime, mtime);
    assert_eq!(st.atime, FileTime::from_secs(7));
    f.root
        .set_meta(
            "/tmp",
            &Meta {
                mode: Some(0o1777),
                ..Meta::default()
            },
        )
        .unwrap();
    assert_eq!(f.root.statx("tmp").unwrap().permissions(), 0o1777);
    // Only atime: mtime stays.
    f.root
        .set_meta(
            "f",
            &Meta {
                atime: Some(FileTime::from_secs(9)),
                ..Meta::default()
            },
        )
        .unwrap();
    let st = f.root.statx("f").unwrap();
    assert_eq!((st.atime, st.mtime), (FileTime::from_secs(9), mtime));
    // The root itself.
    f.root
        .set_meta(
            "/",
            &Meta {
                mode: Some(0o750),
                ..Meta::default()
            },
        )
        .unwrap();
    assert_eq!(
        fs::metadata(&f.host).unwrap().permissions().mode() & 0o7777,
        0o750
    );
}

#[test]
fn set_meta_on_symlink_never_touches_target() {
    let f = Fixture::new();
    fs::write(f.host("t"), b"x").unwrap();
    fs::set_permissions(f.host("t"), fs::Permissions::from_mode(0o644)).unwrap();
    symlink("t", f.host("l")).unwrap();
    let before = fs::metadata(f.host("t")).unwrap();
    let md = fs::symlink_metadata(f.host("l")).unwrap();
    f.root
        .set_meta(
            "l",
            &Meta {
                uid: Some(md.uid()),
                gid: Some(md.gid()),
                mode: Some(0o600),
                mtime: Some(FileTime::from_secs(42)),
                ..Meta::default()
            },
        )
        .unwrap();
    let after = fs::metadata(f.host("t")).unwrap();
    assert_eq!(after.permissions().mode(), before.permissions().mode());
    assert_eq!(after.mtime(), before.mtime());
    assert_eq!(f.root.statx("l").unwrap().mtime, FileTime::from_secs(42));
}

#[test]
fn set_meta_xattrs() {
    let f = Fixture::new();
    fs::write(f.host("f"), b"x").unwrap();
    fs::create_dir(f.host("d")).unwrap();
    symlink("f", f.host("l")).unwrap();
    let m = Meta {
        xattrs: vec![(OsString::from("user.rnk"), b"v".to_vec())],
        ..Meta::default()
    };
    f.root.set_meta("f", &m).unwrap();
    f.root.set_meta("d", &m).unwrap();
    let mut buf = vec![0; 16];
    let n = rustix::fs::getxattr(f.host("f"), "user.rnk", &mut buf).unwrap();
    assert_eq!(&buf[..n], b"v");
    let n = rustix::fs::getxattr(f.host("d"), "user.rnk", &mut buf).unwrap();
    assert_eq!(&buf[..n], b"v");
    // `user.*` is not allowed on a symlink; the target must not get it.
    let m2 = Meta {
        xattrs: vec![(OsString::from("user.other"), b"w".to_vec())],
        ..Meta::default()
    };
    let e = f.root.set_meta("l", &m2).unwrap_err();
    assert_eq!(errno(&e), Some(Errno::PERM), "{e}");
    assert!(rustix::fs::getxattr(f.host("f"), "user.other", &mut buf).is_err());
}

#[test]
fn xattrs_lists_all_without_following_or_opening() {
    let f = Fixture::new();
    fs::write(f.host("f"), b"x").unwrap();
    fs::create_dir(f.host("d")).unwrap();
    symlink("f", f.host("l")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        f.host("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    for (p, n) in [
        ("f", "user.b"),
        ("f", "user.overlay.opaque"),
        ("d", "user.a"),
        ("", "user.r"),
    ] {
        rustix::fs::setxattr(f.host(p), n, b"y", rustix::fs::XattrFlags::empty()).unwrap();
    }
    let names = |p: &str| -> Vec<String> {
        f.root
            .xattrs(p)
            .unwrap()
            .into_iter()
            .map(|(n, _)| n.into_string().unwrap())
            .collect()
    };
    // Sorted, overlay's own names included.
    assert_eq!(names("f"), ["user.b", "user.overlay.opaque"]);
    assert_eq!(names("d"), ["user.a"]);
    assert_eq!(names(""), ["user.r"]);
    // The symlink's own (none), not its target's.
    assert!(names("l").is_empty());
    // A FIFO is read without being opened, which would block.
    assert!(names("fifo").is_empty());
    let e = f.root.xattrs("missing").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOENT), "{e}");
}

#[test]
fn sub_root_is_its_own_root() {
    let f = Fixture::new();
    fs::create_dir_all(f.host("stages/1/rootfs/etc")).unwrap();
    fs::write(f.host("stages/1/rootfs/etc/passwd"), b"inner").unwrap();
    fs::create_dir(f.host("etc")).unwrap();
    fs::write(f.host("etc/passwd"), b"outer").unwrap();
    let sub = f.root.sub_root("stages/1/rootfs").unwrap();
    assert_eq!(sub.mnt_id(), f.root.mnt_id());
    let mut s = String::new();
    fs::File::from(
        sub.open_at("../../../etc/passwd", &OpenHow::read())
            .unwrap(),
    )
    .read_to_string(&mut s)
    .unwrap();
    assert_eq!(s, "inner");
    let e = f.root.sub_root("etc/passwd").unwrap_err();
    assert_eq!(errno(&e), Some(Errno::NOTDIR));
}

#[test]
fn create_sub_root_opens_the_directory_it_made() {
    let f = Fixture::new();
    let sub = f.root.create_sub_root("new", 0o700).unwrap();
    let st = sub.statx("/").unwrap();
    assert_eq!(st.ino, fs::metadata(f.host("new")).unwrap().ino());
    assert_eq!(st.mode & 0o777, 0o700);
    // It refuses any name already taken: a directory, a symlink to one,
    // a file; nothing is followed or opened.
    fs::create_dir(f.host("dir")).unwrap();
    symlink("dir", f.host("link")).unwrap();
    fs::write(f.host("file"), b"").unwrap();
    for name in ["dir", "link", "file", "new"] {
        let e = f.root.create_sub_root(name, 0o700).unwrap_err();
        assert_eq!(errno(&e), Some(rustix::io::Errno::EXIST), "{name}: {e}");
    }
    assert!(fs::read_dir(f.host("dir")).unwrap().next().is_none());
}

/// ADR-0061: the directory moves between two roots' descriptors and comes
/// back as the same root, contents and inode unchanged.
#[test]
fn move_sub_root_moves_the_directory_it_was_given() {
    let f = Fixture::new();
    let from = f.root.create_sub_root("p", 0o700).unwrap();
    let to = f.root.create_sub_root("c", 0o700).unwrap();
    let moved = from.create_sub_root("rootfs", 0o755).unwrap();
    fs::write(f.host("p/rootfs/f"), b"data").unwrap();
    let ino = moved.statx("/").unwrap().ino;
    let root = from.move_sub_root("rootfs", &moved, &to, "rootfs").unwrap();
    assert_eq!(root.statx("/").unwrap().ino, ino);
    assert_eq!(root.mnt_id(), to.mnt_id());
    assert!(fs::symlink_metadata(f.host("p/rootfs")).is_err());
    assert_eq!(fs::read(f.host("c/rootfs/f")).unwrap(), b"data");
}

/// ADR-0061: a name that no longer holds the root to move (swapped for
/// another directory or a symlink) is refused and nothing moves; a name
/// taken at the destination is never replaced; only plain names are taken.
#[test]
fn move_sub_root_refuses_a_swapped_name_and_never_replaces() {
    let f = Fixture::new();
    let from = f.root.create_sub_root("p", 0o700).unwrap();
    let to = f.root.create_sub_root("c", 0o700).unwrap();
    let moved = from.create_sub_root("rootfs", 0o755).unwrap();
    fs::rename(f.host("p/rootfs"), f.host("p/away")).unwrap();
    fs::create_dir(f.host("p/rootfs")).unwrap();
    let e = from
        .move_sub_root("rootfs", &moved, &to, "rootfs")
        .unwrap_err();
    assert!(e.to_string().contains("no longer holds"), "{e}");
    fs::remove_dir(f.host("p/rootfs")).unwrap();
    symlink("away", f.host("p/rootfs")).unwrap();
    let e = from
        .move_sub_root("rootfs", &moved, &to, "rootfs")
        .unwrap_err();
    assert!(e.to_string().contains("no longer holds"), "{e}");
    assert!(fs::read_dir(f.host("c")).unwrap().next().is_none());

    fs::write(f.host("c/taken"), b"keep").unwrap();
    let e = from
        .move_sub_root("away", &moved, &to, "taken")
        .unwrap_err();
    assert_eq!(errno(&e), Some(Errno::EXIST), "{e}");
    assert_eq!(fs::read(f.host("c/taken")).unwrap(), b"keep");
    for bad in ["a/b", "..", ".", "/away", "", "away/"] {
        let e = from.move_sub_root(bad, &moved, &to, "x").unwrap_err();
        assert!(e.to_string().contains("single plain name"), "{bad}: {e}");
        let e = from.move_sub_root("away", &moved, &to, bad).unwrap_err();
        assert!(e.to_string().contains("single plain name"), "{bad}: {e}");
    }
    assert!(fs::symlink_metadata(f.host("p/away")).unwrap().is_dir());
}

/// ADR-0061: the machine's own `/` is never moved from, into or as the
/// root to move; refused before any rename.
#[test]
fn move_sub_root_refuses_the_machines_own_root() {
    let f = Fixture::new();
    let a = f.root.create_sub_root("a", 0o700).unwrap();
    let slash = RootPath::open("/").unwrap();
    for (from, moved, to) in [(&a, &slash, &a), (&slash, &a, &a), (&a, &a, &slash)] {
        let e = from
            .move_sub_root("no-such-name", moved, to, "x")
            .unwrap_err();
        assert!(e.to_string().contains("own root"), "{e}");
    }
}

#[test]
fn create_sub_root_refuses_another_mount_without_following_it() {
    // The mount check, with the parent claiming another mount id.
    let f = Fixture::new();
    let other = RootPath::open(f.host(""))
        .unwrap()
        .stay_on_mount()
        .with_mnt_id(u64::MAX);
    let e = other.create_sub_root("x", 0o700).unwrap_err();
    assert!(e.to_string().contains("another mount"), "{e}");
}

/// `set_meta_if` changes the inode it read, even when `decide` swaps its
/// name for another directory first; `None` changes nothing.
#[test]
fn set_meta_if_changes_the_entry_it_read_whatever_its_name_holds_then() {
    let f = Fixture::new();
    let dir = |name: &str| {
        fs::create_dir(f.host(name)).unwrap();
        fs::set_permissions(f.host(name), fs::Permissions::from_mode(0o700)).unwrap();
    };
    let state = |name: &str| {
        let md = fs::symlink_metadata(f.host(name)).unwrap();
        (md.ino(), md.mode() & 0o7777, md.ctime(), md.ctime_nsec())
    };
    dir("d");
    let before = state("d");
    let none = f.root.set_meta_if("d", |st| {
        assert_eq!(st.permissions(), 0o700);
        None
    });
    assert!(!none.unwrap());
    assert_eq!(state("d"), before);
    assert!(!f.root.set_meta_if("/", |_| None).unwrap());

    let swapped = f.root.set_meta_if("d", |_| {
        fs::rename(f.host("d"), f.host("d-old")).unwrap();
        dir("d");
        Some(Meta {
            mode: Some(0o750),
            ..Meta::default()
        })
    });
    assert!(swapped.unwrap());
    let (old_ino, old_mode, ..) = state("d-old");
    assert_eq!((old_ino, old_mode), (before.0, 0o750));
    assert_eq!(state("d").1, 0o700);
}

/// A refused group change is an `Io` error of op `chown` with `EPERM`:
/// `rnk-store` tells a filesystem that refuses its group by them.
#[test]
fn a_refused_group_change_is_a_chown_error_with_eperm() {
    let groups = rustix::process::getgroups().unwrap();
    if rustix::process::geteuid().is_root()
        || rustix::process::getegid().is_root()
        || groups.iter().any(|g| g.is_root())
    {
        eprintln!("skipped: the process may take group 0");
        return;
    }
    let f = Fixture::new();
    fs::create_dir(f.host("d")).unwrap();
    let e = f
        .root
        .set_meta_if("d", |_| {
            Some(Meta {
                gid: Some(0),
                ..Meta::default()
            })
        })
        .unwrap_err();
    assert!(
        matches!(e, Error::Io { op: "chown", .. })
            && e.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error()),
        "{e}"
    );
}
