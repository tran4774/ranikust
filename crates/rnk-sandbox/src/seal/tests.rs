use std::io::Read;
use std::os::unix::fs::FileExt;

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
/// first that runs ends the search. Each failure is kept with its way.
#[test]
fn the_ways_are_tried_in_order_until_one_runs() {
    let mut made = Vec::new();
    let mut ran = Vec::new();
    let found = try_in_order(
        &SealedCopy::ALL,
        |way| {
            made.push(way);
            match way {
                SealedCopy::Overlay => Err(Errno::PERM.into()),
                _ => dev_null(),
            }
        },
        |way, _copy| {
            ran.push(way);
            match way {
                SealedCopy::Memfd => Err(Errno::ACCESS.into()),
                _ => Ok(way),
            }
        },
    );
    assert_eq!(found.ok(), Some(SealedCopy::Tmpfile));
    assert_eq!(made, SealedCopy::ALL);
    assert_eq!(ran, [SealedCopy::Memfd, SealedCopy::Tmpfile]);

    // None runs: every failure, in order; none asked, none tried.
    let failed = try_in_order(
        &SealedCopy::ALL,
        |_| dev_null(),
        |_, _| -> io::Result<()> { Err(Errno::NOEXEC.into()) },
    )
    .expect_err("nothing runs");
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
        |_, _| -> io::Result<()> { Ok(()) },
    );
    assert!(none.expect_err("no way").is_empty());
    assert!(!asked);
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

/// ADR-0101: the installed binary passes for no copy, so a marker set
/// from outside only makes the run warn.
#[test]
fn the_binary_as_installed_passes_for_no_copy() {
    let exe = exe();
    for way in [SealedCopy::Overlay, SealedCopy::Memfd, SealedCopy::Tmpfile] {
        assert!(check(way, exe.as_fd()).is_err(), "{way}");
    }
    assert_eq!(SealedCopy::from_name("memfd"), Some(SealedCopy::Memfd));
    assert_eq!(SealedCopy::from_name("bind"), None);
}

/// T2 (the VM only), ADR-0101: the overlay copy is the running binary
/// itself, read-only even for root, on a mount that is in no mount table;
/// the overlay's empty layer is gone once it is made.
#[test]
#[ignore = "T2: mounts a detached overlay as real root; run in the test VM only"]
fn an_overlay_copy_is_the_binary_read_only_and_mounted_nowhere() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let copy = overlay_copy(&exe(), scratch.path()).expect("overlay copy");
    check(SealedCopy::Overlay, copy.as_fd()).expect("read-only overlay");
    assert_eq!(bytes_of(copy.as_fd()), binary_bytes());
    let path = format!("/proc/self/fd/{}", copy.as_raw_fd());
    for flags in [OFlags::WRONLY, OFlags::RDWR] {
        let r = rustix::fs::open(path.as_str(), flags | OFlags::CLOEXEC, Mode::empty());
        assert_eq!(r.err(), Some(Errno::ROFS), "{flags:?}");
    }
    let fdinfo =
        std::fs::read_to_string(format!("/proc/self/fdinfo/{}", copy.as_raw_fd())).expect("fdinfo");
    let mnt_id = fdinfo
        .lines()
        .find_map(|l| l.strip_prefix("mnt_id:"))
        .map(str::trim)
        .expect("mnt_id");
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").expect("mountinfo");
    assert!(
        !mountinfo
            .lines()
            .any(|l| l.split(' ').next() == Some(mnt_id)),
        "mount {mnt_id} is attached"
    );
    assert_eq!(std::fs::read_dir(scratch.path()).expect("ls").count(), 0);
}
