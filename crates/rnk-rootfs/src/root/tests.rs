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
