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
            Ok(marker(way, failed))
        },
        || Ok::<(), Errno>(()),
    );
    let Search::Ran(marker) = found else {
        panic!("tmpfile runs: {found:?}");
    };
    assert_eq!(made, SealedCopy::ALL);
    assert_eq!(ran, [(SealedCopy::Tmpfile, vec![SealedCopy::Memfd])]);
    let (way, passed_over) = parse_marker(&marker).expect("a marker");
    assert_eq!(way, SealedCopy::Tmpfile);
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
    let marker = "tmpfile; memfd: memfd_create: Permission denied (os error 13)";
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
            "RANIKUST_SEALED_COPY=tmpfile; memfd: memfd_create: Permission denied (os error 13)",
        ]
    );
    assert_eq!(
        parse_marker(marker),
        Some((
            SealedCopy::Tmpfile,
            "memfd: memfd_create: Permission denied (os error 13)"
        ))
    );
    assert_eq!(parse_marker("memfd"), Some((SealedCopy::Memfd, "")));
    assert_eq!(parse_marker("overlay; x"), None);
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
    for way in SealedCopy::ALL {
        assert!(check(way, exe.as_fd()).is_err(), "{way}");
    }
    assert_eq!(SealedCopy::from_name("memfd"), Some(SealedCopy::Memfd));
    assert_eq!(SealedCopy::from_name("overlay"), None);
}
