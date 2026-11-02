use std::ffi::OsStr;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;

use super::{Fence, OutputDir, Reached, fault, reach};
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
