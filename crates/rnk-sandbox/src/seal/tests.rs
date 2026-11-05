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
        BinaryMount::NotWeighed,
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

/// The binary's identity in the sample tables.
const BIN: FileId = FileId {
    dev_major: 8,
    dev_minor: 1,
    ino: 77,
};

/// `root_of` for sample tables: the mounts `files` names are files (mount
/// id, inode on the binary's device), every other one a directory.
fn roots(files: &[(u64, u64)]) -> impl FnMut(&MountInfo) -> Option<MountRoot> + '_ {
    move |m| {
        Some(match files.iter().find(|(id, _)| *id == m.mount_id) {
            Some(&(_, ino)) => MountRoot::File(FileId { ino, ..BIN }),
            None => MountRoot::Directory,
        })
    }
}

/// The sample `/etc/hosts` of [`mounts`], mount 2: another file.
const HOSTS: (u64, u64) = (2, 5);

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
            reached_writable(&mounts(own, extra), 10, Some(path), BIN, roots(&[HOSTS])),
            want,
            "{own} {extra:?}"
        );
    }
    // A read-only file system, though its mount says rw.
    let text = "10 1 8:1 /home/u/tree/ranikust /ranikust/bin/ranikust ro - ext4 /dev/sda1 rw\n\
                11 1 8:1 /home/u/tree /src rw - ext4 /dev/sda1 ro\n";
    let ro_fs = rnk_rootfs::parse_mountinfo(text.as_bytes()).expect("sample");
    assert_eq!(
        reached_writable(&ro_fs, 10, Some(bin), BIN, roots(&[])),
        Some(false)
    );
    // No line for the binary's mount.
    assert_eq!(
        reached_writable(&mounts(file, &[]), 99, Some(bin), BIN, roots(&[HOSTS])),
        None
    );
}

/// ADR-0101: a binary on a root that is not an overlay (a directory root)
/// is weighed like one on another read-only mount: its links, then the
/// writable mounts that reach it; on an overlay root it builds as it is,
/// and on a root the mount table does not type it is unplaced.
#[test]
fn a_binary_in_a_root_that_is_not_an_overlay_is_weighed() {
    let never = || -> Result<Option<bool>, Errno> { panic!("the mount table is not read") };
    let copies = |up: bool| move || -> Result<Option<bool>, Errno> { Ok(Some(up)) };
    let reach = |reached: Option<bool>| move || -> Result<Option<bool>, Errno> { Ok(reached) };
    let root = BinaryMount::Root;
    // An overlay root: an image's links build, and nothing more is read.
    assert_eq!(verdict(root, copies(true), 2, Some(never)), Ok(root));
    assert_eq!(verdict(root, copies(true), 1, Some(never)), Ok(root));
    // A directory root: other links, a writable mount reaching it, or a
    // mount table that does not place it are refused; nothing reaching it
    // builds.
    assert_eq!(
        verdict(root, copies(false), 2, Some(never)),
        Ok(BinaryMount::Linked)
    );
    assert_eq!(
        verdict(root, copies(false), 0, Some(never)),
        Ok(BinaryMount::Unplaced)
    );
    assert_eq!(
        verdict(root, copies(false), 1, Some(reach(Some(true)))),
        Ok(BinaryMount::Exposed)
    );
    assert_eq!(
        verdict(root, copies(false), 1, Some(reach(Some(false)))),
        Ok(root)
    );
    assert_eq!(
        verdict(root, copies(false), 1, Some(reach(None))),
        Ok(BinaryMount::Unplaced)
    );
    // A FUSE root the mount table does not type, whatever its links.
    let untyped = || -> Result<Option<bool>, Errno> { Ok(None) };
    assert_eq!(
        verdict(root, untyped, 2, Some(never)),
        Ok(BinaryMount::Unplaced)
    );
    // A file system that cannot be told is a failed measurement.
    let failed = || -> Result<Option<bool>, Errno> { Err(Errno::NOSYS) };
    assert_eq!(verdict(root, failed, 1, Some(never)), Err(Errno::NOSYS));
    // Another read-only mount is weighed whatever its file system; others
    // are as `of` told them.
    let ro = BinaryMount::ReadOnly;
    let no_fs = || -> Result<Option<bool>, Errno> { panic!("no file system type is needed") };
    assert_eq!(verdict(ro, no_fs, 2, Some(never)), Ok(BinaryMount::Linked));
    assert_eq!(
        verdict(ro, copies(true), 1, Some(reach(Some(true)))),
        Ok(BinaryMount::Exposed)
    );
    assert_eq!(verdict(ro, no_fs, 1, Some(reach(Some(false)))), Ok(ro));
    for told in [BinaryMount::Writable, BinaryMount::NoMountId] {
        assert_eq!(verdict(told, no_fs, 2, Some(never)), Ok(told));
    }
}

/// ADR-0101 (2026-11-05): a build that cannot be kaniko mode on a
/// container's `/` walks no mount: a binary the walk would weigh is not
/// weighed, one told without it is told as before.
#[test]
fn a_build_that_cannot_be_kaniko_on_slash_walks_no_mount() {
    type Reach = fn() -> Result<Option<bool>, Errno>;
    let unwalked: Option<Reach> = None;
    let copies = |up: bool| move || -> Result<Option<bool>, Errno> { Ok(Some(up)) };
    let (root, ro) = (BinaryMount::Root, BinaryMount::ReadOnly);
    let not_weighed = Ok(BinaryMount::NotWeighed);
    assert_eq!(verdict(root, copies(false), 1, unwalked), not_weighed);
    assert_eq!(verdict(ro, copies(false), 1, unwalked), not_weighed);
    assert_eq!(verdict(root, copies(true), 1, unwalked), Ok(root));
    assert_eq!(
        verdict(ro, copies(false), 2, unwalked),
        Ok(BinaryMount::Linked)
    );
    assert_eq!(
        verdict(root, copies(false), 0, unwalked),
        Ok(BinaryMount::Unplaced)
    );
    let writable = BinaryMount::Writable;
    assert_eq!(verdict(writable, copies(false), 1, unwalked), Ok(writable));
}

/// ADR-0101: a fuse-overlayfs root (rootless podman's) copies up as
/// overlayfs does, told by its type in the mount table; another FUSE root
/// does not, nor an ext4 one, for which no table is read; a FUSE root the
/// table does not list, or a table that does not parse, leaves it untyped.
#[test]
fn a_fuse_overlayfs_root_copies_up_as_overlayfs_does() {
    const EXT4: rustix::fs::FsWord = 0xEF53;
    let never = || -> Result<Option<String>, Errno> { panic!("the mount table is not read") };
    let table = |text: &str| {
        let t = root_type(text.as_bytes(), 1);
        move || -> Result<Option<String>, Errno> { Ok(t) }
    };
    let podman = "1 0 0:45 / / rw,nodev,relatime - fuse.fuse-overlayfs fuse-overlayfs \
                  rw,user_id=0,group_id=0,default_permissions,allow_other\n";
    let sshfs = "1 0 0:46 / / rw,relatime - fuse.sshfs host:/ rw,user_id=0,group_id=0\n";
    assert_eq!(
        root_type(podman.as_bytes(), 1).as_deref(),
        Some(FUSE_OVERLAYFS)
    );
    assert_eq!(root_type(podman.as_bytes(), 2), None);
    assert_eq!(copies_up(FUSE, table(podman)), Ok(Some(true)));
    assert_eq!(copies_up(OVERLAYFS, never), Ok(Some(true)));
    assert_eq!(copies_up(FUSE, table(sshfs)), Ok(Some(false)));
    assert_eq!(copies_up(EXT4, never), Ok(Some(false)));
    // No line for the root's mount, or a table with a line that does not
    // parse, the root's or another's: a partly parsed table is not believed.
    let other = podman.replacen("1 0", "2 0", 1);
    assert_eq!(copies_up(FUSE, table(&other)), Ok(None));
    assert_eq!(copies_up(FUSE, table("1 0 0:45 / /\n")), Ok(None));
    assert_eq!(
        copies_up(FUSE, table(&format!("{podman}2 1 0:46 / /x\n"))),
        Ok(None)
    );
    let failed = || -> Result<Option<String>, Errno> { Err(Errno::NOSYS) };
    assert_eq!(copies_up(FUSE, failed), Err(Errno::NOSYS));
}

/// ADR-0101: on a directory root, the root mount itself does not reach the
/// binary; another writable mount of its file system that contains it does.
#[test]
fn a_directory_root_is_not_its_own_writable_mount() {
    let bin = Path::new("/usr/bin/ranikust");
    let root = "1 0 8:1 /var/lib/c/rootfs / rw,relatime - ext4 /dev/sda1 rw\n";
    let parse = |text: &str| rnk_rootfs::parse_mountinfo(text.as_bytes()).expect("sample");
    let reached = |text: &str| reached_writable(&parse(text), 1, Some(bin), BIN, roots(&[]));
    assert_eq!(reached(root), Some(false));
    let shared = format!("{root}2 1 8:1 /var/lib/c /mnt rw,relatime - ext4 /dev/sda1 rw\n");
    assert_eq!(reached(&shared), Some(true));
    let elsewhere = format!("{root}2 1 8:1 /var/lib/d /mnt rw,relatime - ext4 /dev/sda1 rw\n");
    assert_eq!(reached(&elsewhere), Some(false));
    // The same blind spots as on another read-only mount: a link of the
    // binary bound read-write, its name removed; a removed directory.
    let link = format!("{root}2 1 8:1 /var/lib/c/x//deleted /x rw - ext4 /dev/sda1 rw\n");
    assert_eq!(
        reached_writable(&parse(&link), 1, Some(bin), BIN, roots(&[(2, BIN.ino)])),
        Some(true)
    );
    assert_eq!(reached(&link), None);
}

/// ADR-0101: the `O_TMPFILE` copy is never made directly on FUSE, whose
/// daemon serves the pages the kernel runs; other file systems take it.
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
    assert_eq!(
        mount_of(open_dir("/proc").as_fd(), true),
        BinaryMount::Writable
    );
}

/// ADR-0101: a file in the root is weighed by the root's own file system,
/// measured through its descriptor: as it is on an overlay, by its links
/// and the mount table elsewhere. Skipped under qemu-user, whose statx
/// leaves the mount id at 0 (`just test-aarch64`).
#[test]
fn a_file_in_the_root_is_weighed_by_the_roots_file_system() {
    let root = rustix::fs::open(
        "/",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("/");
    let st = statx_of(rustix::fs::CWD, c"/", AtFlags::empty()).expect("statx /");
    let text = std::fs::read("/proc/self/mountinfo").expect("mountinfo");
    // A table that does not parse places nothing, as in mount_of.
    let mounts = rnk_rootfs::parse_mountinfo(&text).ok();
    let f_type = rustix::fs::statfs("/").expect("statfs /").f_type;
    let copies = copies_up(f_type, || Ok(root_type(&text, st.stx_mnt_id))).expect("read above");
    let expected = match (copies, st.stx_nlink) {
        (Some(true), _) => BinaryMount::Root,
        (None, _) | (Some(false), 0) => BinaryMount::Unplaced,
        (Some(false), 1) => {
            let reach = match (mounts.as_deref(), FileId::of(&st)) {
                (Some(m), Some(binary)) => {
                    reached_from_slash(m, st.stx_mnt_id, Path::new("/"), binary)
                }
                (None, _) | (_, None) => Ok(None),
            };
            match reach {
                Ok(None) => BinaryMount::Unplaced,
                Ok(Some(true)) => BinaryMount::Exposed,
                Ok(Some(false)) => BinaryMount::Root,
                Err(errno) => BinaryMount::Unknown(errno),
            }
        }
        (Some(false), _) => BinaryMount::Linked,
    };
    assert_eq!(mount_of(root.as_fd(), true), expected);
    // With no walk, what the walk would weigh is not weighed.
    let unwalked = match (copies, st.stx_nlink) {
        (Some(false), 1) => BinaryMount::NotWeighed,
        _ => expected,
    };
    assert_eq!(mount_of(root.as_fd(), false), unwalked);
}

/// ADR-0101: a marker whose copy does not check is not believed: the
/// binary's mount is measured on the file this process runs from.
#[test]
fn an_unchecked_marker_is_not_believed_about_the_binary() {
    let actual = mount_of(exe().as_fd(), true);
    let lie = if actual == BinaryMount::Writable {
        BinaryMount::ReadOnly
    } else {
        BinaryMount::Writable
    };
    let value = OsString::from(format!("memfd {}", lie.token()));
    let Sealing::Unsealed { why, binary } = copy_run(&value, true) else {
        panic!("the test binary passes for a memfd copy");
    };
    assert!(why.contains("is no such copy"), "{why}");
    assert_eq!(binary, actual);
    let Sealing::Unsealed { why, binary } = copy_run(OsStr::new("memfd"), true) else {
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

/// Red team (ADR-0101, T21): escaped names in the mount table hide no reach.
#[test]
fn red_team_escaped_names_in_the_mount_table_are_compared_unescaped() {
    let table = |own: &str, other: &str| {
        let text = format!(
            "1 0 0:50 / / rw - overlay overlay rw\n\
             10 1 8:1 {own} - ext4 /dev/sda1 rw\n\
             11 1 8:1 {other} rw - ext4 /dev/sda1 rw\n"
        );
        rnk_rootfs::parse_mountinfo(text.as_bytes()).expect("sample mountinfo")
    };
    let path = |bytes: &[u8]| PathBuf::from(OsStr::from_bytes(bytes));
    // (binary's line, path below it, writable line, reached)
    let cases: [(&str, &[u8], &str, bool); 10] = [
        (
            r"/h/t\011ab/rnk /r/rnk ro",
            b"/r/rnk",
            r"/h/t\011ab /src",
            true,
        ),
        (
            r"/h/n\012l/rnk /r/rnk ro",
            b"/r/rnk",
            r"/h/n\012l /src",
            true,
        ),
        (r"/h/n\012l/rnk /r/rnk ro", b"/r/rnk", r"/h/n /src", false),
        (
            r"/h/b\134s/rnk /r/rnk ro",
            b"/r/rnk",
            r"/h/b\134s /src",
            true,
        ),
        (r"/h/b\134s/rnk /r/rnk ro", b"/r/rnk", r"/h/b /src", false),
        // A name holding `\040` itself, not a space, and the other way round.
        (
            r"/h/a\134040b/rnk /r/rnk ro",
            b"/r/rnk",
            r"/h/a\040b /src",
            false,
        ),
        (
            r"/h/a\040b/rnk /r/rnk ro",
            b"/r/rnk",
            r"/h/a\134040b /src",
            false,
        ),
        (
            r"/h/a\134040b/rnk /r/rnk ro",
            b"/r/rnk",
            r"/h/a\134040b /x\012y",
            true,
        ),
        // A directory bound at a point with a newline and a tab.
        (
            r"/h/t /r/b\012i\011n ro",
            b"/r/b\ni\tn/rnk",
            r"/h/t/rnk /x",
            true,
        ),
        (
            r"/h/t /r/b\012i\011n ro",
            b"/r/b\ni\tn/rnk",
            r"/h/t/rnk2 /x",
            false,
        ),
    ];
    for (own, below, other, reached) in cases {
        assert_eq!(
            reached_writable(&table(own, other), 10, Some(&path(below)), BIN, roots(&[])),
            Some(reached),
            "{own} | {other}"
        );
    }
    // A path that only spells the mount point's escape is outside it.
    assert_eq!(
        reached_writable(
            &table(r"/h/t /r/b\012in ro", r"/h/t /x"),
            10,
            Some(&path(br"/r/b\012in/rnk")),
            BIN,
            roots(&[])
        ),
        None
    );
}

/// Red team (ADR-0101, T21): a read-write bind of a hard link of the binary
/// whose name was removed after the bind shows its root as `<path>//deleted`;
/// the link count is 1 again, and the bind still reaches the binary, as its
/// inode at the mount point tells. So does a link bound read-write once the
/// binary's own bound name is removed.
#[test]
fn red_team_a_writable_bind_of_a_removed_link_reaches_the_binary() {
    // As the kernel printed it in rootless podman (Linux 7.0).
    let table = |own: &str, other: &str| {
        let text = format!(
            "1 0 0:50 / / rw - overlay overlay rw\n\
             10 1 0:49 {own} /ranikust/bin/ranikust ro,nosuid,nodev - tmpfs tmpfs rw\n\
             11 1 0:49 {other} /x rw,nosuid,nodev - tmpfs tmpfs rw\n"
        );
        rnk_rootfs::parse_mountinfo(text.as_bytes()).expect("sample")
    };
    let bin = Path::new("/ranikust/bin/ranikust");
    let never = || -> Result<Option<bool>, Errno> { panic!("not read") };
    let weigh =
        |reached: Option<bool>| verdict(BinaryMount::ReadOnly, never, 1, Some(move || Ok(reached)));
    let removed = table("/d/bin/a", "/d/bin/b//deleted");
    assert!(removed[2].root_deleted && removed[2].fs_root == Path::new("/d/bin/b"));
    let reached = reached_writable(&removed, 10, Some(bin), BIN, roots(&[(11, BIN.ino)]));
    assert_eq!(weigh(reached), Ok(BinaryMount::Exposed), "{reached:?}");
    // The binary's own name removed, a link of it bound read-write: its
    // name (`readlink`'s `<path> (deleted)`) leads nowhere.
    let own_removed = table("/d/bin/a//deleted", "/d/bin/b");
    let reached = reached_writable(&own_removed, 10, None, BIN, roots(&[(11, BIN.ino)]));
    assert_eq!(weigh(reached), Ok(BinaryMount::Exposed), "{reached:?}");
    // With no such link, where its one name now is cannot be told.
    let reached = reached_writable(&own_removed, 10, None, BIN, roots(&[(11, 6)]));
    assert_eq!(weigh(reached), Ok(BinaryMount::Unplaced), "{reached:?}");
}

/// ADR-0101: a read-write bind of a file is told by its inode, not its
/// name: a link of the binary renamed after the bind reaches it; the same
/// root holding another file does not.
#[test]
fn a_writable_file_bind_is_told_by_its_inode_not_its_name() {
    let file = "8:1 /home/u/tree/ranikust /ranikust/bin/ranikust ro";
    let bin = Path::new("/ranikust/bin/ranikust");
    let renamed = mounts(file, &["11 1 8:1 /home/u/tree/c /x rw"]);
    assert_eq!(
        reached_writable(&renamed, 10, Some(bin), BIN, roots(&[HOSTS, (11, BIN.ino)])),
        Some(true)
    );
    assert_eq!(
        reached_writable(&renamed, 10, Some(bin), BIN, roots(&[HOSTS, (11, 6)])),
        Some(false)
    );
}

/// ADR-0101: Docker's and kubelet's read-write binds of single files on
/// the binary's file system (`/etc/hosts`, `/dev/termination-log`) do not
/// reach it, even once a file was replaced and its root reads `//deleted`:
/// their inodes are others.
#[test]
fn a_replaced_file_bind_of_another_file_does_not_reach_the_binary() {
    let text = "1 0 0:50 / / rw - overlay overlay rw\n\
                2 1 8:1 /var/lib/docker/containers/c/hosts//deleted /etc/hosts rw - ext4 /dev/sda1 rw\n\
                3 1 8:1 /var/lib/kubelet/pods/p/containers/c/x /dev/termination-log rw - ext4 /dev/sda1 rw\n\
                10 1 8:1 /home/u/tree/ranikust /ranikust/bin/ranikust ro - ext4 /dev/sda1 rw\n";
    let mounts = rnk_rootfs::parse_mountinfo(text.as_bytes()).expect("sample");
    assert!(mounts[1].root_deleted);
    let bin = Path::new("/ranikust/bin/ranikust");
    let reached = reached_writable(&mounts, 10, Some(bin), BIN, roots(&[(2, 5), (3, 6)]));
    let never = || -> Result<Option<bool>, Errno> { panic!("not read") };
    let got = verdict(BinaryMount::ReadOnly, never, 1, Some(move || Ok(reached)));
    assert_eq!(got, Ok(BinaryMount::ReadOnly), "{reached:?}");
}

/// ADR-0101: what cannot be told fails closed: a writable directory of the
/// binary's file system whose name was removed or whose root is no path,
/// a mount point that does not lead to its mount, and a binary whose name
/// no longer leads to it (removed in a directory bound read-only, a link
/// left elsewhere); on another file system, or read-only, none counts. A
/// tree that holds the file is told by its path, before any mount point is
/// reached.
#[test]
fn a_mount_that_cannot_be_told_places_nothing() {
    let dir = "8:1 /home/u/tree /ranikust/bin ro";
    let elsewhere = mounts(dir, &["11 1 8:1 /home/u/other /o rw"]);
    let bin_in_dir = Path::new("/ranikust/bin/ranikust");
    assert_eq!(
        reached_writable(&elsewhere, 10, Some(bin_in_dir), BIN, roots(&[HOSTS])),
        Some(false)
    );
    assert_eq!(
        reached_writable(&elsewhere, 10, None, BIN, roots(&[HOSTS])),
        None
    );
    let file = "8:1 /home/u/tree/ranikust /ranikust/bin/ranikust ro";
    let bin = Path::new("/ranikust/bin/ranikust");
    let removed = "11 1 8:1 /home/u/tree//deleted /src";
    let cases: [(String, Option<bool>); 4] = [
        (format!("{removed} rw"), None),
        ("11 1 8:1 mnt:[4026531840] /n rw".to_owned(), None),
        (format!("{removed} ro"), Some(false)),
        (removed.replace("8:1", "8:2") + " rw", Some(false)),
    ];
    for (line, want) in cases {
        let got = reached_writable(&mounts(file, &[&line]), 10, Some(bin), BIN, roots(&[HOSTS]));
        assert_eq!(got, want, "{line}");
    }
    let unreached = |m: &MountInfo| (m.mount_id != 11).then_some(MountRoot::Directory);
    let other = mounts(file, &["11 1 8:1 /home/u/other /src rw"]);
    assert_eq!(
        reached_writable(&other, 10, Some(bin), BIN, unreached),
        None
    );
    let never = |_: &MountInfo| -> Option<MountRoot> { panic!("no mount point is reached") };
    let tree = mounts(file, &["11 1 8:1 /home/u /home rw"]);
    assert_eq!(
        reached_writable(&tree, 10, Some(bin), BIN, never),
        Some(true)
    );
}

/// ADR-0101: a mount another one covers (at its mount point or above it,
/// and not one it lies on, so mounted over it later) is told hidden from
/// the table alone: its mount point is not looked up and places nothing,
/// nor does the binary's name on its own mount when that one is covered. A
/// mount on it, or one it lies on, does not cover it.
#[test]
fn a_covered_mount_is_not_looked_up() {
    let file = "8:1 /home/u/tree/ranikust /ranikust/bin/ranikust ro";
    let bin = Path::new("/ranikust/bin/ranikust");
    let work = "11 1 8:1 /home/u/other /work rw";
    let cases: [(&[&str], Option<bool>, &[u64]); 5] = [
        (&[work], Some(false), &[2, 11]),
        // Mounted over it later, at its mount point or above it.
        (&[work, "12 1 0:60 / /work rw"], None, &[2]),
        (
            &["11 1 8:1 /home/u/other /work/a rw", "12 1 0:60 / /work rw"],
            None,
            &[2],
        ),
        // A mount on it, or the one it lies on at the same mount point.
        (&[work, "12 11 0:60 / /work/sub rw"], Some(false), &[2, 11]),
        (
            &["11 1 0:60 / /work rw", "12 11 8:1 /home/u/other /work rw"],
            Some(false),
            &[2, 12],
        ),
    ];
    for (extra, want, want_looked) in cases {
        let mut looked = Vec::new();
        let mut files = roots(&[HOSTS]);
        let got = reached_writable(&mounts(file, extra), 10, Some(bin), BIN, |m| {
            looked.push(m.mount_id);
            files(m)
        });
        assert_eq!((got, looked.as_slice()), (want, want_looked), "{extra:?}");
    }
    let seen = |p: &Path| -> Result<Option<Seen>, Errno> {
        let (mount, found) = if p == bin {
            (10, MountRoot::File(BIN))
        } else {
            (
                2,
                MountRoot::File(FileId {
                    ino: HOSTS.1,
                    ..BIN
                }),
            )
        };
        Ok(Some(Seen {
            mount,
            mount_root: true,
            found,
        }))
    };
    let looked_up = |extra: &[&str]| {
        let mut names = Vec::new();
        let got = reached_by(&mounts(file, extra), 10, bin, BIN, |p| {
            names.push(p.to_owned());
            seen(p)
        });
        (got, names)
    };
    let hosts = PathBuf::from("/etc/hosts");
    assert_eq!(
        looked_up(&[]),
        (Ok(Some(false)), vec![bin.to_owned(), hosts.clone()])
    );
    assert_eq!(
        looked_up(&["11 1 0:60 / /ranikust rw"]),
        (Ok(None), vec![hosts])
    );
}

/// ADR-0101: mount ids are reused, so a walk holds only on a table that
/// reads the same once it is done: one that changed meanwhile places
/// nothing, whatever the walk found, and nor does one that does not parse;
/// a failed read keeps its errno.
#[test]
fn a_table_that_changed_during_the_walk_places_nothing() {
    let table = b"1 0 0:50 / / rw - overlay overlay rw\n".to_vec();
    let mut moved = table.clone();
    moved.extend_from_slice(b"2 1 0:51 / /mnt rw - tmpfs tmpfs rw\n");
    let reads = |texts: Vec<Vec<u8>>| {
        let mut texts = texts.into_iter();
        move || texts.next().ok_or(Errno::IO)
    };
    let walk = |found: Result<Option<bool>, Errno>| {
        move |m: &[MountInfo]| {
            assert_eq!(m.len(), 1, "the table first read");
            found
        }
    };
    let stable = || reads(vec![table.clone(), table.clone()]);
    let changed = || reads(vec![table.clone(), moved.clone()]);
    for found in [Ok(Some(true)), Ok(Some(false)), Ok(None), Err(Errno::NOSYS)] {
        assert_eq!(on_a_stable_table(stable(), walk(found)), found);
        assert_eq!(on_a_stable_table(changed(), walk(found)), Ok(None));
    }
    let never = |_: &[MountInfo]| -> Result<Option<bool>, Errno> { panic!("no walk") };
    assert_eq!(
        on_a_stable_table(reads(vec![b"1 0 0:50 / /\n".to_vec()]), never),
        Ok(None)
    );
    assert_eq!(on_a_stable_table(reads(vec![]), never), Err(Errno::IO));
    assert_eq!(
        on_a_stable_table(reads(vec![table.clone()]), walk(Ok(Some(false)))),
        Err(Errno::IO)
    );
}

/// ADR-0101: a mount's root is reached at its mount point through
/// `RootPath` from `/`, and must be that mount's root: `/proc` is a
/// directory; a file inside a mount (`/proc/version`, a file of the
/// temporary directory's mount) is not its root, a line naming another
/// mount there, or a mount point that is missing, is not reached. A file
/// bind's root (T1 `binary-mounts`) has no sample on a host. A file's
/// name, as `readlink` of its descriptor gives it, leads to the file until
/// the name is removed. Skipped under qemu-user, whose statx leaves the
/// mount id at 0 (`just test-aarch64`).
#[test]
fn a_mount_root_is_reached_at_its_mount_point() {
    let text = std::fs::read("/proc/self/mountinfo").expect("mountinfo");
    let lines: Vec<MountInfo> = text
        .split(|b| *b == b'\n')
        // A line of an odd shape elsewhere on this host does not matter here.
        .filter_map(|line| rnk_rootfs::parse_mountinfo(line).ok()?.pop())
        .collect();
    let id =
        |path: &std::ffi::CStr| statx_of(rustix::fs::CWD, path, AtFlags::empty()).expect("statx");
    let proc_id = id(c"/proc").stx_mnt_id;
    let proc = lines
        .iter()
        .find(|m| m.mount_id == proc_id)
        .expect("/proc's line");
    let root = RootPath::open("/").expect("/");
    assert_eq!(root_at(&root, proc), Ok(Some(MountRoot::Directory)));
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f");
    std::fs::write(&file, b"x").expect("write");
    let st = id(&CString::new(file.as_os_str().as_bytes()).expect("a path"));
    let mut at_file = proc.clone();
    at_file.mount_point.clone_from(&file);
    at_file.mount_id = st.stx_mnt_id;
    assert_eq!(root_at(&root, &at_file), Ok(None));
    let mut version = proc.clone();
    version.mount_point = PathBuf::from("/proc/version");
    assert_eq!(root_at(&root, &version), Ok(None));
    let mut another = proc.clone();
    another.mount_id = u64::MAX;
    assert_eq!(root_at(&root, &another), Ok(None));
    let mut missing = proc.clone();
    missing.mount_point = dir.path().join("missing");
    assert_eq!(root_at(&root, &missing), Ok(None));
    let open = File::open(&file).expect("open");
    let name = || {
        let link = format!("/proc/self/fd/{}", open.as_raw_fd());
        PathBuf::from(OsString::from_vec(
            rustix::fs::readlinkat(rustix::fs::CWD, link.as_str(), Vec::new())
                .expect("readlink")
                .into_bytes(),
        ))
    };
    let own = Seen {
        mount: st.stx_mnt_id,
        mount_root: false,
        found: MountRoot::File(FileId::of(&st).expect("an inode")),
    };
    assert_eq!(seen_at(&root, &name()), Ok(Some(own)));
    std::fs::remove_file(&file).expect("remove");
    assert_eq!(seen_at(&root, &name()), Ok(None), "{}", name().display());
}

/// ADR-0101: a path the kernel printed has no symlink in it, so one on the
/// way means the tree changed since: `/proc` reached through a symlink to
/// `/`, or a file through a symlinked directory, is not followed.
#[test]
fn a_symlink_on_the_way_places_nothing() {
    let root = RootPath::open("/").expect("/");
    let dir = tempfile::tempdir().expect("tempdir");
    std::os::unix::fs::symlink("/", dir.path().join("r")).expect("symlink");
    let proc_id = statx_of(rustix::fs::CWD, c"/proc", AtFlags::empty())
        .expect("statx")
        .stx_mnt_id;
    let mut proc = line_at(proc_id, Path::new("/proc"), false);
    assert_eq!(root_at(&root, &proc), Ok(Some(MountRoot::Directory)));
    proc.mount_point = dir.path().join("r/proc");
    assert_eq!(root_at(&root, &proc), Ok(None));
    std::fs::create_dir(dir.path().join("d")).expect("mkdir");
    std::fs::write(dir.path().join("d/f"), b"x").expect("write");
    std::os::unix::fs::symlink("d", dir.path().join("l")).expect("symlink");
    assert!(matches!(
        seen_at(&root, &dir.path().join("d/f")),
        Ok(Some(_))
    ));
    assert_eq!(seen_at(&root, &dir.path().join("l/f")), Ok(None));
}

/// A sample line of mount `id` at `point` on the binary's sample device,
/// its root `/elsewhere`.
/// The root of the mount `m`, reached at its mount point from `root`, as
/// [`reached_from_slash`] reaches it.
fn root_at(root: &RootPath, m: &MountInfo) -> Result<Option<MountRoot>, Errno> {
    Ok(seen_at(root, &m.mount_point)?.and_then(|s| s.root_of(m)))
}

fn line_at(id: u64, point: &Path, read_only: bool) -> MountInfo {
    let mut m = mounts("8:1 /elsewhere /x rw", &[])
        .into_iter()
        .find(|m| m.mount_id == 10)
        .expect("the sample's own line");
    m.mount_id = id;
    m.mount_point = point.to_owned();
    m.read_only = read_only;
    m
}

/// ADR-0101 (T21): the binary's own name, removed while it runs, places
/// nothing: `readlink`'s stale name is not followed into its mount's tree.
#[test]
fn a_removed_name_of_the_binary_places_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (file, link) = (dir.path().join("f"), dir.path().join("g"));
    std::fs::write(&file, b"x").expect("write");
    std::fs::hard_link(&file, &link).expect("link");
    let open = File::open(&file).expect("open");
    std::fs::remove_file(&file).expect("remove");
    let st = statx_of(open.as_fd(), c"", AtFlags::EMPTY_PATH).expect("statx");
    assert_eq!(st.stx_nlink, 1, "the link left");
    let fd = format!("/proc/self/fd/{}", open.as_raw_fd());
    let name = PathBuf::from(OsString::from_vec(
        rustix::fs::readlinkat(rustix::fs::CWD, fd.as_str(), Vec::new())
            .expect("readlink")
            .into_bytes(),
    ));
    // Its own mount alone, at the directory `readlink` names: no mount
    // point is looked up.
    let own = line_at(st.stx_mnt_id, name.parent().expect("a parent"), false);
    let binary = FileId::of(&st).expect("an inode");
    assert_eq!(
        reached_from_slash(&[own], st.stx_mnt_id, &name, binary),
        Ok(None),
        "{}",
        name.display()
    );
}

/// ADR-0101: a lookup that fails otherwise than leading nowhere (a name
/// longer than `NAME_MAX`, 255), at a mount point or at the binary's own
/// name, keeps its errno (`Unknown`), and a path `RootPath` refuses with
/// no errno is `EIO`, unless a writable bind of the binary is found: a
/// reach wins. Only a name that leads to the binary on its own mount
/// places it in a tree. A host has no file bind, so the bind's root is
/// given (T1 `binary-mounts` reaches real ones); every other path is
/// looked up.
#[test]
fn a_failed_lookup_keeps_its_errno_unless_the_binary_is_reached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f");
    std::fs::write(&file, b"x").expect("write");
    let path = CString::new(file.as_os_str().as_bytes()).expect("a path");
    let st = statx_of(rustix::fs::CWD, &path, AtFlags::empty()).expect("statx");
    let binary = FileId::of(&st).expect("an inode");
    let long = dir.path().join("x".repeat(300));
    let root = RootPath::open("/").expect("/");
    assert_eq!(seen_at(&root, &long), Err(Errno::NAMETOOLONG));
    // The binary's own mount is a sample one, so its name places nothing:
    // only the writable mounts below count.
    let own = line_at(u64::MAX, dir.path(), true);
    let mut failing = line_at(u64::MAX - 1, &long, false);
    let mut bind = line_at(u64::MAX - 4, &dir.path().join("bind"), false);
    // Roots apart from the own mount's: no tree holds the binary's path.
    failing.fs_root = PathBuf::from("/failing");
    bind.fs_root = PathBuf::from("/bind");
    // Mounted on the own mount, which they would otherwise cover.
    failing.parent_id = own.mount_id;
    bind.parent_id = own.mount_id;
    let check = |lines: &[&MountInfo], name: &Path| {
        let lines: Vec<MountInfo> = lines.iter().map(|m| (*m).clone()).collect();
        reached_by(&lines, u64::MAX, name, binary, |p| {
            if p == bind.mount_point {
                return Ok(Some(Seen {
                    mount: bind.mount_id,
                    mount_root: true,
                    found: MountRoot::File(binary),
                }));
            }
            seen_at(&root, p)
        })
    };
    assert_eq!(check(&[&own, &failing], &file), Err(Errno::NAMETOOLONG));
    assert_eq!(check(&[&own, &failing, &bind], &file), Ok(Some(true)));
    assert_eq!(check(&[&own], &long), Err(Errno::NAMETOOLONG));
    assert_eq!(check(&[&own, &bind], &long), Ok(Some(true)));
    let up = dir.path().join("..");
    assert_eq!(seen_at(&root, &up), Err(Errno::IO));
    let mut refused = line_at(u64::MAX - 2, &up, false);
    refused.fs_root = PathBuf::from("/refused");
    refused.parent_id = own.mount_id;
    assert_eq!(check(&[&own, &refused], &file), Err(Errno::IO));
    assert_eq!(check(&[&own, &refused, &bind], &file), Ok(Some(true)));
    assert_eq!(check(&[&own], &up), Err(Errno::IO));
    assert_eq!(check(&[&own, &bind], &up), Ok(Some(true)));
    // Its root holds the path the own mount would give the binary.
    let mut tree = line_at(u64::MAX - 3, &dir.path().join("missing"), false);
    tree.parent_id = own.mount_id;
    assert_eq!(check(&[&own, &tree], &file), Ok(None));
}
