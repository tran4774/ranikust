use std::io::Read;
use std::os::unix::fs::FileExt;

use rustix::fs::AtFlags;

use super::*;

fn exe() -> File {
    File::open("/proc/self/exe").expect("open /proc/self/exe")
}

fn bytes_of(fd: BorrowedFd<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();
    File::open(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .expect("read the copy");
    bytes
}

fn binary_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    exe().read_to_end(&mut bytes).expect("read the binary");
    bytes
}

fn dev_null() -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        "/dev/null",
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// ADR-0101: the ways go in order; a way whose copy cannot be made, or
/// whose copy the kernel refuses to run, gives way to the next, and the
/// first that runs ends the search. Each failure is kept with its way, and
/// the copy that runs is told the failures before it.
#[test]
fn the_ways_are_tried_in_order_until_one_runs() {
    // No memfd (`vm.memfd_noexec=2`): the `O_TMPFILE` copy runs, told why.
    let mut made = Vec::new();
    let mut ran = Vec::new();
    let found = try_in_order(
        &SealedCopy::ALL,
        |way| {
            made.push(way);
            match way {
                SealedCopy::Memfd => Err(Errno::PERM.into()),
                SealedCopy::Tmpfile => dev_null(),
            }
        },
        |way, _copy, failed| {
            ran.push((way, failed.iter().map(|(w, _)| *w).collect::<Vec<_>>()));
            Ok(marker(way, BinaryMount::Writable, failed))
        },
        || Ok::<(), Errno>(()),
    );
    let Search::Ran(marker) = found else {
        panic!("tmpfile runs: {found:?}");
    };
    assert_eq!(made, SealedCopy::ALL);
    assert_eq!(ran, [(SealedCopy::Tmpfile, vec![SealedCopy::Memfd])]);
    let (way, binary, passed_over) = parse_marker(&marker).expect("a marker");
    assert_eq!(way, SealedCopy::Tmpfile);
    assert_eq!(binary, BinaryMount::Writable);
    assert!(passed_over.starts_with("memfd: "), "{passed_over}");

    // A memfd the kernel refuses to run: the same.
    let mut ran = Vec::new();
    let found = try_in_order(
        &SealedCopy::ALL,
        |_| dev_null(),
        |way, _copy, failed| {
            ran.push((way, failed.len()));
            match way {
                SealedCopy::Memfd => Err(Errno::ACCESS.into()),
                SealedCopy::Tmpfile => Ok(way),
            }
        },
        || Ok::<(), Errno>(()),
    );
    assert!(
        matches!(found, Search::Ran(SealedCopy::Tmpfile)),
        "{found:?}"
    );
    assert_eq!(ran, [(SealedCopy::Memfd, 0), (SealedCopy::Tmpfile, 1)]);

    // The memfd runs: the `O_TMPFILE` copy is never made.
    let mut made = Vec::new();
    let found = try_in_order(
        &SealedCopy::ALL,
        |way| {
            made.push(way);
            dev_null()
        },
        |way, _copy, _failed| Ok(way),
        || Ok::<(), Errno>(()),
    );
    assert!(matches!(found, Search::Ran(SealedCopy::Memfd)), "{found:?}");
    assert_eq!(made, [SealedCopy::Memfd]);

    // None runs: every failure, in order; none asked, none tried.
    let Search::NoneRan(failed) = try_in_order(
        &SealedCopy::ALL,
        |_| dev_null(),
        |_, _, _| -> io::Result<()> { Err(Errno::NOEXEC.into()) },
        || Ok::<(), Errno>(()),
    ) else {
        panic!("nothing runs");
    };
    let ways: Vec<_> = failed.iter().map(|(way, _)| *way).collect();
    assert_eq!(ways, SealedCopy::ALL);
    assert!(
        failed
            .iter()
            .all(|(_, e)| e.raw_os_error() == Some(libc::ENOEXEC))
    );
    let mut asked = false;
    let none = try_in_order(
        &[],
        |_| {
            asked = true;
            dev_null()
        },
        |_, _, _| -> io::Result<()> { Ok(()) },
        || Ok::<(), Errno>(()),
    );
    assert!(matches!(none, Search::NoneRan(failed) if failed.is_empty()));
    assert!(!asked);
}

/// ADR-0101: once a copy did not run and the core-file limit could not be
/// set back to 0, no other way is tried.
#[test]
fn a_failed_recovery_stops_the_search() {
    let mut made = Vec::new();
    let stopped = try_in_order(
        &SealedCopy::ALL,
        |way| {
            made.push(way);
            dev_null()
        },
        |_, _, _| -> io::Result<()> { Err(Errno::NOEXEC.into()) },
        || Err(Errno::PERM),
    );
    assert!(
        matches!(stopped, Search::Stopped(Errno::PERM)),
        "{stopped:?}"
    );
    assert_eq!(made, [SealedCopy::Memfd]);
}

/// ADR-0101: the copy gets this process's environment with one marker,
/// whatever marker was inherited, so it never makes a copy of its own.
#[test]
fn the_copy_gets_the_environment_with_one_marker() {
    let marker = "tmpfile ro; memfd: memfd_create: Permission denied (os error 13)";
    // The sample is what the seal builds when `vm.memfd_noexec=2` refuses the memfd.
    let failed = [(SealedCopy::Memfd, context("memfd_create")(Errno::ACCESS))];
    assert_eq!(
        super::marker(SealedCopy::Tmpfile, BinaryMount::ReadOnly, &failed),
        marker
    );
    let vars = [
        ("PATH", "/bin"),
        (MARKER, "memfd"),
        ("HOME", "/root"),
        (MARKER, "overlay"),
    ]
    .map(|(k, v)| (OsString::from(k), OsString::from(v)));
    let env = copy_environment(vars, marker).expect("an environment");
    let env: Vec<_> = env.iter().map(|s| s.to_str().expect("UTF-8")).collect();
    assert_eq!(
        env,
        [
            "PATH=/bin",
            "HOME=/root",
            "RANIKUST_SEALED_COPY=tmpfile ro; memfd: memfd_create: Permission denied (os error 13)",
        ]
    );
    assert_eq!(
        parse_marker(marker),
        Some((
            SealedCopy::Tmpfile,
            BinaryMount::ReadOnly,
            "memfd: memfd_create: Permission denied (os error 13)"
        ))
    );
    assert_eq!(
        parse_marker("memfd root"),
        Some((SealedCopy::Memfd, BinaryMount::Root, ""))
    );
    // A way, and the binary's mount, must both be named.
    for refused in [
        "memfd",
        "memfd; x",
        "overlay root; x",
        "memfd rx",
        "memfd unknown:x",
        // No error number: a marker set from outside only warns.
        "memfd unknown:0",
        "memfd unknown:-1",
        "memfd unknown:4096",
    ] {
        assert_eq!(parse_marker(refused), None, "{refused}");
    }
    for raw in [1, 4095] {
        assert_eq!(
            parse_marker(&format!("memfd unknown:{raw}")),
            Some((
                SealedCopy::Memfd,
                BinaryMount::Unknown(Errno::from_raw_os_error(raw)),
                ""
            )),
            "{raw}"
        );
    }
}

/// ADR-0101: the marker carries every answer about the binary's mount to
/// the copy, an error's number included.
#[test]
fn the_marker_carries_the_binarys_mount() {
    for binary in [
        BinaryMount::Root,
        BinaryMount::ReadOnly,
        BinaryMount::Writable,
        BinaryMount::Exposed,
        BinaryMount::Linked,
        BinaryMount::NoMountId,
        BinaryMount::Unplaced,
        BinaryMount::Unknown(Errno::NOSYS),
    ] {
        for failed in [vec![], vec![(SealedCopy::Memfd, io::Error::other("x; y"))]] {
            let value = marker(SealedCopy::Tmpfile, binary, &failed);
            let parsed = parse_marker(&value);
            assert_eq!(
                parsed.map(|(way, b, _)| (way, b)),
                Some((SealedCopy::Tmpfile, binary)),
                "{value}"
            );
        }
    }
}

/// ADR-0101: the binary's mount against `/`'s: the same mount is `/`'s
/// whatever its flags, another one read-only or writable.
#[test]
fn the_binarys_mount_is_told_against_the_root() {
    assert_eq!(BinaryMount::of(7, 7, false), BinaryMount::Root);
    assert_eq!(BinaryMount::of(7, 7, true), BinaryMount::Root);
    assert_eq!(BinaryMount::of(8, 7, true), BinaryMount::ReadOnly);
    assert_eq!(BinaryMount::of(8, 7, false), BinaryMount::Writable);
    // A zero proves nothing: the mount cannot be told, and kaniko mode on
    // `/` refuses.
    assert_eq!(BinaryMount::of(0, 0, true), BinaryMount::NoMountId);
    assert_eq!(BinaryMount::of(0, 7, true), BinaryMount::NoMountId);
    assert_eq!(BinaryMount::of(8, 0, false), BinaryMount::NoMountId);
}

/// The binary's mount (id 10) and the mounts of a container, as
/// `/proc/self/mountinfo` gives them; `extra` adds lines.
fn mounts(own: &str, extra: &[&str]) -> Vec<MountInfo> {
    let mut text = String::from(
        "1 0 0:50 / / rw,relatime - overlay overlay rw,lowerdir=/l\n\
         2 1 8:1 /var/lib/docker/containers/c/hosts /etc/hosts rw,relatime - ext4 /dev/sda1 rw\n",
    );
    text.push_str(&format!("10 1 {own} - ext4 /dev/sda1 rw\n"));
    for line in extra {
        text.push_str(&format!("{line} - ext4 /dev/sda1 rw\n"));
    }
    rnk_rootfs::parse_mountinfo(text.as_bytes()).expect("sample mountinfo")
}

/// ADR-0101: a writable mount of the binary's file system whose root
/// contains the file reaches it, whichever path the container gives each.
#[test]
fn a_writable_mount_containing_the_binary_reaches_it() {
    let bin = Path::new("/ranikust/bin/ranikust");
    let file = "8:1 /home/u/tree/ranikust /ranikust/bin/ranikust ro,relatime";
    let dir = "8:1 /home/u/tree /ranikust/bin ro,relatime";
    let cases: [(&str, &[&str], Option<bool>); 12] = [
        // The file bound :ro, alone, or its tree bound read-write too.
        (file, &[], Some(false)),
        (
            file,
            &["11 1 8:1 /home/u/tree /src rw,relatime"],
            Some(true),
        ),
        (file, &["11 1 8:1 / /host rw,relatime"], Some(true)),
        (file, &["11 1 8:1 /home/u/tree/ranikust /x rw"], Some(true)),
        // The tree bound :ro, the binary below it.
        (dir, &["11 1 8:1 /home/u /home rw"], Some(true)),
        // A sibling, or a name that only starts the same, does not contain it.
        (
            file,
            &[
                "11 1 8:1 /home/u/tree2 /src rw",
                "12 1 8:1 /home/u/tree/sub /s rw",
            ],
            Some(false),
        ),
        // Another file system with the same path.
        (
            file,
            &[
                "11 1 8:1 /home/u/tree /src ro,relatime",
                "12 1 8:2 /home/u/tree /s rw",
            ],
            Some(false),
        ),
        // Read-only by the mount's options.
        (
            file,
            &["11 1 8:1 /home/u/tree /src ro,relatime"],
            Some(false),
        ),
        // Escaped spaces in a root and a mount point.
        (
            "8:1 /home/u/my\\040tree/ranikust /ranikust/bin/ranikust ro",
            &["11 1 8:1 /home/u/my\\040tree /my\\040src rw"],
            Some(true),
        ),
        (
            "8:1 /home/u/my\\040tree /ranikust/my\\040bin ro",
            &["11 1 8:1 /home/u/my\\040tree2 /src rw"],
            Some(false),
        ),
        // Not placed: a path outside its mount, a root that is not a path.
        ("8:1 /home/u/tree /elsewhere ro", &[], None),
        ("8:1 mnt:[4026531840] /ranikust/bin/ranikust ro", &[], None),
    ];
    for (own, extra, want) in cases {
        let path = if own.contains("my\\040bin") {
            Path::new("/ranikust/my bin/ranikust")
        } else {
            bin
        };
        assert_eq!(
            reached_writable(&mounts(own, extra), 10, path),
            want,
            "{own} {extra:?}"
        );
    }
    // A read-only file system, though its mount says rw.
    let text = "10 1 8:1 /home/u/tree/ranikust /ranikust/bin/ranikust ro - ext4 /dev/sda1 rw\n\
                11 1 8:1 /home/u/tree /src rw - ext4 /dev/sda1 ro\n";
    let ro_fs = rnk_rootfs::parse_mountinfo(text.as_bytes()).expect("sample");
    assert_eq!(reached_writable(&ro_fs, 10, bin), Some(false));
    // No line for the binary's mount.
    assert_eq!(reached_writable(&mounts(file, &[]), 99, bin), None);
}

/// ADR-0101: a binary on a root that is not overlayfs (a directory root)
/// is weighed like one on another read-only mount: its links, then the
/// writable mounts that reach it; on an overlay root it builds as it is.
#[test]
fn a_binary_in_a_root_that_is_not_overlayfs_is_weighed() {
    const EXT4: i64 = 0xEF53;
    let never = || -> Result<Option<bool>, Errno> { panic!("the mount table is not read") };
    let fs = |f_type: i64| move || -> Result<i64, Errno> { Ok(f_type) };
    let reach = |reached: Option<bool>| move || -> Result<Option<bool>, Errno> { Ok(reached) };
    let root = BinaryMount::Root;
    // An overlay root: an image's links build, and nothing more is read.
    assert_eq!(verdict(root, fs(OVERLAYFS), 2, never), Ok(root));
    assert_eq!(verdict(root, fs(OVERLAYFS), 1, never), Ok(root));
    // A directory root: other links, a writable mount reaching it, or a
    // mount table that does not place it are refused; nothing reaching it
    // builds.
    assert_eq!(verdict(root, fs(EXT4), 2, never), Ok(BinaryMount::Linked));
    assert_eq!(verdict(root, fs(EXT4), 0, never), Ok(BinaryMount::Unplaced));
    assert_eq!(
        verdict(root, fs(EXT4), 1, reach(Some(true))),
        Ok(BinaryMount::Exposed)
    );
    assert_eq!(verdict(root, fs(EXT4), 1, reach(Some(false))), Ok(root));
    assert_eq!(
        verdict(root, fs(EXT4), 1, reach(None)),
        Ok(BinaryMount::Unplaced)
    );
    // A file system that cannot be told is a failed measurement.
    let failed = || -> Result<i64, Errno> { Err(Errno::NOSYS) };
    assert_eq!(verdict(root, failed, 1, never), Err(Errno::NOSYS));
    // Another read-only mount is weighed whatever its file system; others
    // are as `of` told them.
    let ro = BinaryMount::ReadOnly;
    let no_fs = || -> Result<i64, Errno> { panic!("no file system type is needed") };
    assert_eq!(verdict(ro, no_fs, 2, never), Ok(BinaryMount::Linked));
    assert_eq!(
        verdict(ro, || Ok(OVERLAYFS), 1, reach(Some(true))),
        Ok(BinaryMount::Exposed)
    );
    assert_eq!(verdict(ro, no_fs, 1, reach(Some(false))), Ok(ro));
    for told in [BinaryMount::Writable, BinaryMount::NoMountId] {
        assert_eq!(verdict(told, no_fs, 2, never), Ok(told));
    }
}

/// ADR-0101: on a directory root, the root mount itself does not reach the
/// binary; another writable mount of its file system that contains it does.
#[test]
fn a_directory_root_is_not_its_own_writable_mount() {
    let bin = Path::new("/usr/bin/ranikust");
    let root = "1 0 8:1 /var/lib/c/rootfs / rw,relatime - ext4 /dev/sda1 rw\n";
    let parse = |text: &str| rnk_rootfs::parse_mountinfo(text.as_bytes()).expect("sample");
    assert_eq!(reached_writable(&parse(root), 1, bin), Some(false));
    let shared = format!("{root}2 1 8:1 /var/lib/c /mnt rw,relatime - ext4 /dev/sda1 rw\n");
    assert_eq!(reached_writable(&parse(&shared), 1, bin), Some(true));
    let elsewhere = format!("{root}2 1 8:1 /var/lib/d /mnt rw,relatime - ext4 /dev/sda1 rw\n");
    assert_eq!(reached_writable(&parse(&elsewhere), 1, bin), Some(false));
}

/// ADR-0101: the `O_TMPFILE` copy is never made on FUSE, whose daemon
/// serves the pages the kernel runs; other file systems take it.
#[test]
fn only_fuse_serves_its_own_pages() {
    // statfs(2): FUSE_SUPER_MAGIC; TMPFS_MAGIC, EXT4_SUPER_MAGIC, OVERLAYFS_SUPER_MAGIC.
    assert!(serves_its_own_pages(0x6573_5546));
    for other in [0x0102_1994, 0xEF53, 0x794C_7630] {
        assert!(!serves_its_own_pages(other), "{other:#x}");
    }
}

/// ADR-0101: the mount is measured on the file. Skipped under qemu-user,
/// whose statx leaves the mount id at 0 (`just test-aarch64`).
#[test]
fn the_binarys_mount_is_measured_on_the_file() {
    let open_dir = |path: &str| {
        rustix::fs::open(
            path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .expect(path)
    };
    let mount = |fd: OwnedFd| placed(fd.as_fd()).map(|(mount, _)| mount);
    assert_eq!(mount(open_dir("/")), Ok(BinaryMount::Root));
    assert_eq!(mount(open_dir("/proc")), Ok(BinaryMount::Writable));
    // The kernel's internal mount, whose id Linux 7.0 gives as 0: never `/`.
    let memfd = rustix::fs::memfd_create("rnk-test", rustix::fs::MemfdFlags::CLOEXEC)
        .expect("memfd_create");
    assert_ne!(mount(memfd), Ok(BinaryMount::Root));
    // `/proc` is not weighed further: writable is refused as it is.
    assert_eq!(mount_of(open_dir("/proc").as_fd()), BinaryMount::Writable);
}

/// ADR-0101: a marker whose copy does not check is not believed: the
/// binary's mount is measured on the file this process runs from.
#[test]
fn an_unchecked_marker_is_not_believed_about_the_binary() {
    let actual = mount_of(exe().as_fd());
    let lie = if actual == BinaryMount::Writable {
        BinaryMount::ReadOnly
    } else {
        BinaryMount::Writable
    };
    let value = OsString::from(format!("memfd {}", lie.token()));
    let Sealing::Unsealed { why, binary } = copy_run(&value) else {
        panic!("the test binary passes for a memfd copy");
    };
    assert!(why.contains("is no such copy"), "{why}");
    assert_eq!(binary, actual);
    let Sealing::Unsealed { why, binary } = copy_run(OsStr::new("memfd")) else {
        panic!("a marker with no mount passes");
    };
    assert!(why.contains("names no way"), "{why}");
    assert_eq!(binary, actual);
}

/// ADR-0101: the memfd copy holds the binary's bytes and its seals refuse
/// every write and change of size, through its own descriptor or a new
/// one; nothing can take a seal off.
#[test]
fn a_memfd_copy_is_the_binary_sealed_against_writes() {
    let copy = memfd_copy(&exe()).expect("memfd copy");
    check(SealedCopy::Memfd, copy.as_fd()).expect("sealed");
    assert_eq!(bytes_of(copy.as_fd()), binary_bytes());
    let reopened = File::options()
        .write(true)
        .open(format!("/proc/self/fd/{}", copy.as_raw_fd()))
        .expect("a new descriptor");
    let own = File::from(copy);
    for f in [&own, &reopened] {
        let e = f.write_at(b"x", 0).expect_err("a write");
        assert_eq!(e.raw_os_error(), Some(libc::EPERM), "{e}");
        let len = f.metadata().expect("stat").len();
        for size in [0, len + 1] {
            let e = f.set_len(size).expect_err("a resize");
            assert_eq!(e.raw_os_error(), Some(libc::EPERM), "{e}");
        }
    }
    let e = rustix::fs::fcntl_add_seals(&own, SealFlags::EXEC).expect_err("a seal added");
    assert_eq!(e, Errno::PERM);
}

/// ADR-0101: the `O_TMPFILE` copy holds the binary's bytes, is open
/// read-only, and can never get a name.
#[test]
fn a_tmpfile_copy_is_the_binary_with_no_name_open_read_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let copy = tmpfile_copy(&exe(), dir.path()).expect("tmpfile copy");
    check(SealedCopy::Tmpfile, copy.as_fd()).expect("no name");
    assert_eq!(bytes_of(copy.as_fd()), binary_bytes());
    let mode = rustix::fs::fcntl_getfl(&copy).expect("F_GETFL");
    assert_eq!(mode & OFlags::RWMODE, OFlags::RDONLY);
    let link = rustix::fs::linkat(
        rustix::fs::CWD,
        format!("/proc/self/fd/{}", copy.as_raw_fd()).as_str(),
        rustix::fs::CWD,
        dir.path().join("named"),
        AtFlags::SYMLINK_FOLLOW,
    );
    assert_eq!(link.err(), Some(Errno::NOENT));
    assert_eq!(std::fs::read_dir(dir.path()).expect("ls").count(), 0);
}

/// ADR-0101: the ways share one descriptor of the binary, and a way tried
/// after another that read it still copies all of it.
#[test]
fn a_way_after_another_copies_the_whole_binary() {
    let exe = exe();
    let memfd = memfd_copy(&exe).expect("memfd copy");
    let dir = tempfile::tempdir().expect("tempdir");
    let tmpfile = tmpfile_copy(&exe, dir.path()).expect("tmpfile copy after it");
    assert_eq!(bytes_of(memfd.as_fd()), binary_bytes());
    assert_eq!(bytes_of(tmpfile.as_fd()), binary_bytes());
}

/// Set in the child run of
/// `a_copy_an_interpreter_must_reopen_is_refused_before_the_exec`.
const EXEC_CHILD: &str = "RNK_TEST_SEAL_EXEC_CHILD";

/// ADR-0101: a copy that an interpreter must open again after the exec (a
/// `#!` script here; binfmt_misc without its `O` flag, as an emulator, the
/// same) is refused by the kernel before the exec, with `ENOENT`, since
/// the copy's descriptor is close-on-exec: the process goes on, and the
/// next way is tried. Run in a child copy of this test binary, which a
/// regression would replace with the interpreter.
#[test]
fn a_copy_an_interpreter_must_reopen_is_refused_before_the_exec() {
    if std::env::var_os(EXEC_CHILD).is_some() {
        let script = memfd_with(b"#!/bin/sh\necho the script ran\n");
        let e = execute(&script, "memfd root").expect_err("refused");
        println!("refused before the exec: {}", e.raw_os_error().unwrap_or(0));
        return;
    }
    let name = "seal::tests::a_copy_an_interpreter_must_reopen_is_refused_before_the_exec";
    let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(EXEC_CHILD, "1")
        .output()
        .expect("run the child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let want = format!("refused before the exec: {}", libc::ENOENT);
    assert!(
        out.status.success() && stdout.contains(&want),
        "child: {:?}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn memfd_with(bytes: &[u8]) -> OwnedFd {
    use rustix::fs::MemfdFlags;
    // Executable at `vm.memfd_noexec=1`, as `memfd_copy`'s.
    let fd = match rustix::fs::memfd_create("rnk-test", MemfdFlags::CLOEXEC | MemfdFlags::EXEC) {
        Err(Errno::INVAL) => rustix::fs::memfd_create("rnk-test", MemfdFlags::CLOEXEC),
        made => made,
    }
    .expect("memfd_create");
    let mut file = File::from(fd);
    io::Write::write_all(&mut file, bytes).expect("write the memfd");
    file.into()
}

/// ADR-0101: the installed binary passes for no copy, so a marker set
/// from outside only makes the run warn.
#[test]
fn the_binary_as_installed_passes_for_no_copy() {
    let exe = exe();
    for way in SealedCopy::ALL {
        assert!(check(way, exe.as_fd()).is_err(), "{way}");
    }
    assert_eq!(SealedCopy::from_name("memfd"), Some(SealedCopy::Memfd));
    assert_eq!(SealedCopy::from_name("overlay"), None);
}
