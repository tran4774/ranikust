use std::ffi::OsStr;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;

use super::{Fence, OutputDir, Reached, fault, reach, trusted_link};
use crate::error::Error;
use crate::root::RootPath;

/// A device node such as `/dev/null` is never replaced by an output, and
/// a directory of `/proc` or `/sys` never takes one.
#[test]
fn kernel_names_and_filesystems_never_take_an_output() {
    let dev = OutputDir::open(Path::new("/dev"), None, None).unwrap();
    let e = dev
        .write_atomically(OsStr::new("null"), 0o644, |_| Ok::<(), Error>(()))
        .unwrap_err();
    assert!(invalid(&e).contains("device node"), "{e}");
    for dir in ["/proc", "/sys"] {
        let out = OutputDir::open(Path::new(dir), None, None).unwrap();
        let e = out
            .write_atomically(OsStr::new("x.tar"), 0o644, |_| Ok::<(), Error>(()))
            .unwrap_err();
        assert!(invalid(&e).contains("kernel interfaces"), "{dir}: {e}");
    }
}

/// A temporary directory only its owner may write: `reach` follows no
/// symlink in a directory its group or others may write, and a umask of
/// `002` would give a new one `0775`.
fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn invalid(e: &Error) -> &'static str {
    match e {
        Error::InvalidPath { reason, .. } => reason,
        other => panic!("expected InvalidPath, got {other}"),
    }
}

fn mkfifo(p: &Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        p,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
}

#[test]
fn missing_directories_are_made_below_the_last_one_reached() {
    let dir = private_dir();
    let out = OutputDir::open(&dir.path().join("a/b"), Some(0o700), None).unwrap();
    assert_eq!(out.path(), dir.path().join("a/b"));
    let mode = fs::metadata(dir.path().join("a"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    // Without `create`, a missing directory is an error; `check` lets it be.
    let e = OutputDir::open(&dir.path().join("c"), None, None).unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(rustix::io::Errno::NOENT.raw_os_error())
    );
    OutputDir::check(&dir.path().join("c/d"), None).unwrap();
}

#[test]
fn a_symlink_on_the_way_is_refused_and_nothing_is_made_through_it() {
    let dir = private_dir();
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    symlink(&elsewhere, dir.path().join("out")).unwrap();
    let e = OutputDir::open(&dir.path().join("out/sub"), Some(0o755), None).unwrap_err();
    assert!(invalid(&e).contains("symlink"), "{e}");
    let e = OutputDir::check(&dir.path().join("out/sub"), None).unwrap_err();
    assert!(invalid(&e).contains("symlink"), "{e}");
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    // A dangling one is not made either.
    symlink(dir.path().join("made"), dir.path().join("dangling")).unwrap();
    let e = OutputDir::open(&dir.path().join("dangling/x"), Some(0o755), None).unwrap_err();
    assert!(invalid(&e).contains("symlink"), "{e}");
    assert!(!dir.path().join("made").exists());
}

#[test]
fn the_build_context_is_never_an_output_directory() {
    let dir = private_dir();
    let ctx = dir.path().join("ctx");
    fs::create_dir_all(ctx.join("sub")).unwrap();
    let fence = Fence::of(&RootPath::open(&ctx).unwrap()).unwrap();
    // Even a path that comes out again with `..`: an output path does not
    // pass through the context at all.
    for p in [
        ctx.clone(),
        ctx.join("out"),
        ctx.join("sub/out"),
        ctx.join("sub/.."),
        ctx.join(".."),
    ] {
        let e = OutputDir::open(&p, Some(0o755), Some(&fence)).unwrap_err();
        assert!(
            invalid(&e).contains("build context"),
            "{}: {e}",
            p.display()
        );
        let e = OutputDir::check(&p, Some(&fence)).unwrap_err();
        assert!(
            invalid(&e).contains("build context"),
            "{}: {e}",
            p.display()
        );
    }
    assert!(!ctx.join("out").exists());
    // Beside the context is fine.
    OutputDir::open(&dir.path().join("out"), Some(0o755), Some(&fence)).unwrap();
}

#[test]
fn writing_in_place_never_follows_or_writes_through_another_name() {
    let dir = private_dir();
    let out = OutputDir::open(dir.path(), None, None).unwrap();
    let victim = dir.path().join("victim");
    fs::write(&victim, b"keep me").unwrap();

    symlink(&victim, dir.path().join("link")).unwrap();
    let e = out
        .write_in_place(OsStr::new("link"), 0o644, b"x")
        .unwrap_err();
    assert!(invalid(&e).contains("symlink"), "{e}");
    symlink(dir.path().join("created"), dir.path().join("dangling")).unwrap();
    let e = out
        .write_in_place(OsStr::new("dangling"), 0o644, b"x")
        .unwrap_err();
    assert!(invalid(&e).contains("symlink"), "{e}");
    assert!(!dir.path().join("created").exists());

    fs::hard_link(&victim, dir.path().join("hard")).unwrap();
    let e = out
        .write_in_place(OsStr::new("hard"), 0o644, b"x")
        .unwrap_err();
    assert!(invalid(&e).contains("hard links"), "{e}");
    assert_eq!(fs::read(&victim).unwrap(), b"keep me");

    // A FIFO is never opened, so it never blocks.
    mkfifo(&dir.path().join("fifo"));
    let e = out
        .write_in_place(OsStr::new("fifo"), 0o644, b"x")
        .unwrap_err();
    assert!(invalid(&e).contains("regular"), "{e}");

    // A file of its own is truncated and rewritten; a missing one is made.
    fs::write(dir.path().join("digest"), b"a much longer old text").unwrap();
    out.write_in_place(OsStr::new("digest"), 0o644, b"sha256:1")
        .unwrap();
    assert_eq!(fs::read(dir.path().join("digest")).unwrap(), b"sha256:1");
    out.write_in_place(OsStr::new("new"), 0o600, b"n").unwrap();
    let mode = fs::metadata(dir.path().join("new"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0);
}

#[test]
fn an_atomic_write_replaces_the_name_and_leaves_nothing_on_error() {
    let dir = private_dir();
    let out = OutputDir::open(dir.path(), None, None).unwrap();
    let victim = dir.path().join("victim");
    fs::write(&victim, b"keep me").unwrap();
    let write = |name: &str| {
        out.write_atomically(OsStr::new(name), 0o644, |f| {
            f.write_all(b"tar").map_err(|source| Error::Io {
                op: "write",
                path: name.into(),
                source,
            })
        })
    };
    // Anything but a regular file at the name is refused, not replaced.
    symlink(&victim, dir.path().join("link.tar")).unwrap();
    mkfifo(&dir.path().join("fifo.tar"));
    fs::create_dir(dir.path().join("dir.tar")).unwrap();
    for (name, what) in [
        ("link.tar", "symlink"),
        ("fifo.tar", "FIFO"),
        ("dir.tar", "directory"),
    ] {
        let e = write(name).unwrap_err();
        assert!(invalid(&e).contains(what), "{name}: {e}");
    }
    assert!(
        fs::symlink_metadata(dir.path().join("link.tar"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&victim).unwrap(), b"keep me");
    // A regular file is replaced; a missing one made.
    fs::write(dir.path().join("image.tar"), b"old").unwrap();
    write("image.tar").unwrap();
    assert_eq!(fs::read(dir.path().join("image.tar")).unwrap(), b"tar");
    write("new.tar").unwrap();
    assert_eq!(fs::read(dir.path().join("new.tar")).unwrap(), b"tar");
    for p in ["link.tar", "fifo.tar", "new.tar"] {
        fs::remove_file(dir.path().join(p)).unwrap();
    }
    fs::remove_dir(dir.path().join("dir.tar")).unwrap();

    let failed: Result<(), Error> = out.write_atomically(OsStr::new("other"), 0o644, |_| {
        Err(Error::InvalidPath {
            path: "other".into(),
            reason: "test",
        })
    });
    assert!(failed.is_err());
    let mut names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["image.tar", "victim"]);
}

/// Where the filesystem refuses `RENAME_NOREPLACE` (NFS, 9p, FUSE without
/// `rename2`), a new output is linked into place and its temporary name
/// removed; where hard links are refused too, it is renamed after the name
/// is looked at again. Either way the output is written, nothing is left
/// behind, and a name that is no regular file is still refused.
#[test]
fn an_output_is_written_where_the_filesystem_refuses_rename_noreplace() {
    for faults in [
        fault::NOREPLACE_REFUSED,
        fault::NOREPLACE_REFUSED | fault::LINK_REFUSED,
    ] {
        fault::set(faults);
        let dir = private_dir();
        let out = OutputDir::open(dir.path(), None, None).unwrap();
        out.write_atomically(OsStr::new("new.tar"), 0o644, |f| {
            f.write_all(b"NEW").map_err(|source| Error::Io {
                op: "write",
                path: dir.path().join("new.tar"),
                source,
            })
        })
        .unwrap();
        assert_eq!(fs::read(dir.path().join("new.tar")).unwrap(), b"NEW");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["new.tar"], "faults {faults}: {names:?}");
        // A FIFO at the name is refused before anything is written.
        mkfifo(&dir.path().join("fifo"));
        let e = out
            .write_atomically(OsStr::new("fifo"), 0o644, |_| Ok::<(), Error>(()))
            .unwrap_err();
        assert!(invalid(&e).contains("FIFO"), "{e}");
    }
    fault::set(0);
}

/// An output name close to `NAME_MAX` still gets a temporary name that
/// fits: the name's part in it is cut short.
#[test]
fn a_long_output_name_still_gets_a_temporary_name() {
    let dir = private_dir();
    let out = OutputDir::open(dir.path(), None, None).unwrap();
    let name = "n".repeat(250);
    out.write_atomically(OsStr::new(&name), 0o644, |f| {
        f.write_all(b"LONG").map_err(|source| Error::Io {
            op: "write",
            path: dir.path().join(&name),
            source,
        })
    })
    .unwrap();
    assert_eq!(fs::read(dir.path().join(&name)).unwrap(), b"LONG");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn an_existing_file_is_opened_only_when_it_is_regular() {
    let dir = private_dir();
    let out = OutputDir::open(dir.path(), None, None).unwrap();
    fs::write(dir.path().join("blob"), b"bytes").unwrap();
    symlink(dir.path().join("blob"), dir.path().join("link")).unwrap();
    mkfifo(&dir.path().join("fifo"));
    let mut text = String::new();
    out.open_existing(OsStr::new("blob"))
        .unwrap()
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "bytes");
    assert!(out.open_existing(OsStr::new("link")).unwrap().is_none());
    assert!(out.open_existing(OsStr::new("fifo")).unwrap().is_none());
    assert!(out.open_existing(OsStr::new("missing")).unwrap().is_none());
}

#[test]
fn a_read_follows_symlinks_outside_the_context_and_stops_where_it_enters_it() {
    let dir = private_dir();
    let ctx = dir.path().join("ctx");
    fs::create_dir_all(ctx.join("sub")).unwrap();
    let secret = dir.path().join("secret");
    fs::write(&secret, b"SECRET").unwrap();
    // The context's owner points names at a host file.
    symlink(&secret, ctx.join("Dockerfile")).unwrap();
    symlink(dir.path(), ctx.join("sub/up")).unwrap();
    let fence = Fence::of(&RootPath::open(&ctx).unwrap()).unwrap();

    let inside = |p: &Path| match reach(p, &fence).unwrap() {
        Reached::Inside(rest) => rest,
        Reached::Outside(pinned) => panic!("{}: outside, {pinned:?}", p.display()),
    };
    assert_eq!(inside(&ctx.join("Dockerfile")), Path::new("Dockerfile"));
    assert_eq!(
        inside(&ctx.join("sub/up/secret")),
        Path::new("sub/up/secret")
    );
    assert_eq!(inside(&ctx), Path::new(""));
    // Through an operator's symlink outside the context, into it.
    symlink(&ctx, dir.path().join("alias")).unwrap();
    assert_eq!(
        inside(&dir.path().join("alias/Dockerfile")),
        Path::new("Dockerfile")
    );

    // Outside the context, symlinks are followed (a ConfigMap's `..data`).
    fs::create_dir(dir.path().join("cm-1")).unwrap();
    fs::write(dir.path().join("cm-1/Dockerfile"), b"FROM scratch\n").unwrap();
    symlink("cm-1", dir.path().join("..data")).unwrap();
    symlink("..data/Dockerfile", dir.path().join("Dockerfile")).unwrap();
    let Reached::Outside(pinned) = reach(&dir.path().join("Dockerfile"), &fence).unwrap() else {
        panic!("expected outside");
    };
    let mut text = String::new();
    fs::File::from(pinned.open_regular().unwrap())
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "FROM scratch\n");
    // `..` straight after the context's root leaves it.
    let Reached::Outside(pinned) = reach(&ctx.join("../secret"), &fence).unwrap() else {
        panic!("expected outside");
    };
    assert!(pinned.open_regular().is_ok());
}

/// Outside the context, a symlink is followed only from a directory no
/// other account may write, and only when it has no other name: a link
/// placed in a group- or world-writable directory, or hard-linked in from
/// elsewhere, could be another account's doing (ADR-0076).
#[test]
fn a_symlink_others_could_have_placed_is_not_followed() {
    let dir = private_dir();
    let ctx = dir.path().join("ctx");
    fs::create_dir(&ctx).unwrap();
    fs::write(dir.path().join("secret"), b"SECRET").unwrap();
    let shared = dir.path().join("shared");
    fs::create_dir(&shared).unwrap();
    symlink(dir.path().join("secret"), shared.join("Dockerfile")).unwrap();
    let fence = Fence::of(&RootPath::open(&ctx).unwrap()).unwrap();
    for mode in [0o775, 0o757, 0o1777] {
        fs::set_permissions(&shared, fs::Permissions::from_mode(mode)).unwrap();
        let e = reach(&shared.join("Dockerfile"), &fence).unwrap_err();
        assert!(invalid(&e).contains("does not follow"), "{mode:o}: {e}");
    }
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        reach(&shared.join("Dockerfile"), &fence).unwrap(),
        Reached::Outside(_)
    ));
    // The same link with a second name (a symlink hard-linked elsewhere).
    rustix::fs::linkat(
        rustix::fs::CWD,
        shared.join("Dockerfile"),
        rustix::fs::CWD,
        dir.path().join("again"),
        rustix::fs::AtFlags::empty(),
    )
    .unwrap();
    let e = reach(&shared.join("Dockerfile"), &fence).unwrap_err();
    assert!(invalid(&e).contains("does not follow"), "{e}");
}

/// The link rule as a predicate: a group- or world-writable directory is
/// trusted only when root owns it and it is on a read-only mount, as
/// kubelet's ConfigMap volume is (`0777`, root's, mounted read-only)
/// [inferred]. Other accounts are played by changed fields of a real
/// `statx`.
#[test]
fn a_writable_directory_is_trusted_only_when_root_owns_it_read_only() {
    let dir = private_dir();
    symlink("x", dir.path().join("l")).unwrap();
    let root = RootPath::open(dir.path()).unwrap();
    let link = root.statx("l").unwrap();
    let real = root.statx("").unwrap();
    let link = crate::meta::Stat { uid: 0, ..link };
    let at = |uid: u32, mode: u32| crate::meta::Stat {
        uid,
        mode: (real.mode & !0o7777) | mode,
        ..real
    };
    let other = rustix::process::geteuid().as_raw().wrapping_add(4242);
    // Root's 0755: trusted whatever the mount.
    assert!(trusted_link(&link, &at(0, 0o755), false));
    // Root's 0777 (a ConfigMap volume): only on a read-only mount.
    assert!(!trusted_link(&link, &at(0, 0o777), false));
    assert!(trusted_link(&link, &at(0, 0o777), true));
    assert!(!trusted_link(&link, &at(0, 0o775), false));
    // Another account's directory is never trusted, read-only or not.
    assert!(!trusted_link(&link, &at(other, 0o755), true));
    // A link with another name is never trusted.
    let twice = crate::meta::Stat { nlink: 2, ..link };
    assert!(!trusted_link(&twice, &at(0, 0o755), true));
}

/// A host Dockerfile and the ignore file beside it are judged as a build
/// context's files are (ADR-0076): one the owner of its directory could
/// not read by its mode bits (a root-only file linked in from elsewhere)
/// is refused, never read. As root every directory reads anything, so the
/// check is unprivileged only.
#[test]
fn a_host_dockerfile_its_directory_owner_could_not_read_is_refused() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let dir = private_dir();
    let ctx = dir.path().join("ctx");
    fs::create_dir(&ctx).unwrap();
    let fence = Fence::of(&RootPath::open(&ctx).unwrap()).unwrap();
    let df = dir.path().join("Dockerfile");
    fs::write(&df, b"FROM scratch\n").unwrap();
    fs::write(dir.path().join("Dockerfile.dockerignore"), b"x\n").unwrap();
    let Reached::Outside(file) = reach(&df, &fence).unwrap() else {
        panic!("expected outside");
    };
    assert!(file.open_regular().is_ok());
    assert!(
        file.open_beside(OsStr::new("Dockerfile.dockerignore"))
            .unwrap()
            .is_some()
    );
    for name in ["Dockerfile", "Dockerfile.dockerignore"] {
        fs::set_permissions(dir.path().join(name), fs::Permissions::from_mode(0o200)).unwrap();
    }
    let Reached::Outside(file) = reach(&df, &fence).unwrap() else {
        panic!("expected outside");
    };
    let e = file.open_regular().unwrap_err();
    assert!(invalid(&e).contains("could not read"), "{e}");
    let e = file
        .open_beside(OsStr::new("Dockerfile.dockerignore"))
        .unwrap_err();
    assert!(invalid(&e).contains("could not read"), "{e}");
}

#[test]
fn a_symlink_loop_is_refused() {
    let dir = private_dir();
    let ctx = dir.path().join("ctx");
    fs::create_dir(&ctx).unwrap();
    symlink("b", dir.path().join("a")).unwrap();
    symlink("a", dir.path().join("b")).unwrap();
    let fence = Fence::of(&RootPath::open(&ctx).unwrap()).unwrap();
    let e = reach(&dir.path().join("a"), &fence).unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(rustix::io::Errno::LOOP.raw_os_error())
    );
}

/// T2c (the VM or a default rootful Docker run): outside the context, a
/// symlink another account owns is never followed, so a lower-trust
/// account cannot point a `--dockerfile` outside its context at a host
/// file; root's own links are (ADR-0076).
#[test]
#[ignore = "T2c: default: a symlink owned by another user needs root"]
fn a_symlink_another_account_owns_is_not_followed() {
    let dir = private_dir();
    let ctx = dir.path().join("ctx");
    fs::create_dir(&ctx).unwrap();
    fs::write(dir.path().join("secret"), b"SECRET").unwrap();
    symlink(dir.path().join("secret"), dir.path().join("theirs")).unwrap();
    symlink(dir.path().join("secret"), dir.path().join("roots")).unwrap();
    rustix::fs::chownat(
        rustix::fs::CWD,
        dir.path().join("theirs"),
        Some(rustix::fs::Uid::from_raw(1000)),
        Some(rustix::fs::Gid::from_raw(1000)),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
    let fence = Fence::of(&RootPath::open(&ctx).unwrap()).unwrap();
    let e = reach(&dir.path().join("theirs"), &fence).unwrap_err();
    assert!(invalid(&e).contains("does not follow"), "{e}");
    assert!(matches!(
        reach(&dir.path().join("roots"), &fence).unwrap(),
        Reached::Outside(_)
    ));
}

/// Red team (T22, T3, ADR-0076): the owner of an output's directory swaps
/// names under ranikust while it writes, each swap one atomic
/// `RENAME_EXCHANGE` picked at random: the output directory with a symlink
/// to a victim directory, the tar with a symlink to a victim file, the
/// digest file with a symlink to another and with a hard link to it.
/// Whatever moment each swap lands at, nothing reaches the victim
/// directory: after every round its two files keep their dummy text and no
/// name appears there. Rounds go on until some writes got through meanwhile
/// and some were refused, so the race was run, neither refused outright nor
/// missed; still inconclusive at the deadline, the test fails.
#[test]
fn red_team_names_swapped_during_an_output_never_redirect_it() {
    use std::time::{Duration, Instant};

    // A round takes well under a second; the rest is for a loaded or
    // emulated runner.
    const DEADLINE: Duration = Duration::from_secs(60);
    let start = Instant::now();
    let mut seen = SwapRace::default();
    let mut rounds = 0_u32;
    loop {
        rounds += 1;
        let round = swap_race_round(rounds);
        seen.written += round.written;
        seen.refused += round.refused;
        seen.swaps += round.swaps;
        if seen.written > 0 && seen.refused > 0 || start.elapsed() >= DEADLINE {
            break;
        }
    }
    let SwapRace {
        written,
        refused,
        swaps,
    } = seen;
    let took = start.elapsed();
    println!("{rounds} rounds in {took:?}: {written} written, {refused} refused, {swaps} swaps");
    assert!(
        written > 0,
        "no write got through in {rounds} rounds ({took:?}; {refused} refused, {swaps} swaps): \
         the race proved nothing"
    );
    assert!(
        refused > 0,
        "no swap was ever seen in {rounds} rounds ({took:?}; {written} written, {swaps} swaps): \
         the race proved nothing"
    );
}

/// What a round of the swap race saw.
#[derive(Default)]
struct SwapRace {
    written: u32,
    refused: u32,
    swaps: u64,
}

/// One round of `red_team_names_swapped_during_an_output_never_redirect_it`
/// in a fresh directory: 3000 outputs written while a thread swaps names,
/// then the victim checked.
fn swap_race_round(round: u32) -> SwapRace {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    use rustix::fs::{Mode, OFlags, RenameFlags, renameat_with};

    let dir = private_dir();
    let top = dir.path().canonicalize().unwrap();
    let victim = top.join("victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("out.tar"), b"dummy-victim-tar").unwrap();
    fs::write(victim.join("digest"), b"dummy-victim-digest").unwrap();
    let out = top.join("out");
    fs::create_dir(&out).unwrap();
    symlink(&victim, top.join("outlink")).unwrap();
    fs::write(out.join("out.tar"), b"old").unwrap();
    fs::write(out.join("digest"), b"old").unwrap();
    symlink(victim.join("out.tar"), out.join("tarlink")).unwrap();
    rustix::fs::linkat(
        rustix::fs::CWD,
        out.join("tarlink"),
        rustix::fs::CWD,
        out.join("tarlink.keep"),
        rustix::fs::AtFlags::empty(),
    )
    .unwrap();
    symlink(victim.join("digest"), out.join("digestlink")).unwrap();
    fs::hard_link(victim.join("digest"), out.join("digesthard")).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let started = Arc::new(Barrier::new(2));
    let top_fd = rustix::fs::open(&top, OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap();
    // The real output directory, wherever its name is at the moment.
    let open_out = || rustix::fs::open(&out, OFlags::PATH | OFlags::DIRECTORY, Mode::empty());
    let out_fd = open_out().unwrap();
    let out_here = open_out().unwrap();
    let swapper = {
        let stop = Arc::clone(&stop);
        let started = Arc::clone(&started);
        std::thread::spawn(move || {
            let pairs = [
                (&out_fd, "out.tar", "tarlink"),
                (&out_fd, "digest", "digestlink"),
                (&out_fd, "digest", "digesthard"),
                (&top_fd, "out", "outlink"),
            ];
            // In a fixed order the swaps fall into step with the writer's
            // steps, which then meet the same names every time (#44).
            let mut x = 0x9e37_79b9_7f4a_7c15_u64.wrapping_mul(u64::from(round));
            let mut swaps = 0_u64;
            started.wait();
            while !stop.load(Ordering::Relaxed) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let (fd, a, b) = pairs[usize::try_from(x >> 62).unwrap()];
                if renameat_with(fd, a, fd, b, RenameFlags::EXCHANGE).is_ok() {
                    swaps += 1;
                }
            }
            swaps
        })
    };
    started.wait();
    let mut race = SwapRace::default();
    for _ in 0..3000 {
        if write_both(&out) {
            race.written += 1;
        } else {
            race.refused += 1;
        }
        relink_tar(&out_here);
    }
    stop.store(true, Ordering::Relaxed);
    race.swaps = swapper.join().unwrap();
    assert_eq!(
        fs::read(victim.join("out.tar")).unwrap(),
        b"dummy-victim-tar"
    );
    assert_eq!(
        fs::read(victim.join("digest")).unwrap(),
        b"dummy-victim-digest"
    );
    let mut names: Vec<_> = fs::read_dir(&victim)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["digest", "out.tar"],
        "a name appeared in the victim"
    );
    race
}

/// Writes the tar and the digest file in the output directory `out`, as an
/// export does; whether both got through.
fn write_both(out: &Path) -> bool {
    let Ok(dir) = OutputDir::open(out, Some(0o700), None) else {
        return false;
    };
    let tar = dir.write_atomically(OsStr::new("out.tar"), 0o644, |f| {
        f.write_all(b"rnk-output").map_err(|source| Error::Io {
            op: "write",
            path: "out.tar".into(),
            source,
        })
    });
    let digest = dir.write_in_place(OsStr::new("digest"), 0o644, b"rnk-digest");
    tar.is_ok() && digest.is_ok()
}

/// Puts the tar's link back at `tarlink` where a write's rename replaced it
/// (the swapper had put it at `out.tar`), so the tar's race lasts the whole
/// round. The link's second name, `tarlink.keep`, makes its link count say
/// whether it is still at one of the two names, which no exchange changes;
/// looking at both names instead races with the swapper.
fn relink_tar(out: &rustix::fd::OwnedFd) {
    use rustix::fs::{AtFlags, linkat, renameat, statat};

    if statat(out, "tarlink.keep", AtFlags::SYMLINK_NOFOLLOW)
        .unwrap()
        .st_nlink
        == 1
    {
        // Both names hold files now, and only a write changes that.
        linkat(out, "tarlink.keep", out, "tarlink.new", AtFlags::empty()).unwrap();
        renameat(out, "tarlink.new", out, "tarlink").unwrap();
    }
}
