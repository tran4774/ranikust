//! T0 tests of opening, reading and walking a local directory context.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use rnk_context::{ContextSource as _, ContextSpec, DirSource, DockerfileAt, Error, IgnoreSource};
use rnk_rootfs::FileType;

fn ctx_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn walk_paths(ctx: &rnk_context::Context, m: Option<&rnk_ignore::Matcher>) -> Vec<PathBuf> {
    ctx.walk(m).map(|e| e.unwrap().path).collect()
}

/// The host Dockerfile at `path`, outside `ctx`, as the binary pins it.
fn host_file(path: &Path, ctx: &rnk_context::Context) -> rnk_rootfs::HostFile {
    match rnk_rootfs::reach(path, ctx.fence()).unwrap() {
        rnk_rootfs::Reached::Outside(file) => file,
        rnk_rootfs::Reached::Inside(rest) => panic!("inside the context: {}", rest.display()),
    }
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

/// `--context` is never resolved through a symlink (ADR-0076): one committed
/// to a checkout (`services/api -> /root/.docker`), or a component a
/// lower-trust account owns, would make a root-only directory the context.
#[test]
fn a_context_path_through_a_symlink_is_refused() {
    let secret_dir = ctx_dir();
    fs::write(secret_dir.path().join("token"), b"DUMMY-TOKEN\n").unwrap();
    let checkout = ctx_dir();
    fs::create_dir(checkout.path().join("services")).unwrap();
    symlink(secret_dir.path(), checkout.path().join("services/api")).unwrap();
    symlink(checkout.path(), checkout.path().join("services/up")).unwrap();
    for path in [
        checkout.path().join("services/api"),
        checkout.path().join("services/up/services"),
    ] {
        let err = DirSource::new(&path).open().unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("symlink") && text.contains("realpath"),
            "{}: {text}",
            path.display()
        );
    }
    // The same directory by its real path, `..` included, is fine.
    let real = checkout.path().join("services/../services");
    let ctx = DirSource::new(&real).open().unwrap();
    assert_eq!(ctx.origin(), checkout.path().join("services"));
}

/// Red team (T22, ADR-0076): `--context` through a magic link is refused
/// like any symlink: `/proc/self/cwd`, `/proc/self/root/...`,
/// `/proc/self/fd/<n>` of a directory held open and `/dev/fd/<n>` all lead
/// to a directory the caller did not name. The same directory by its
/// real path opens.
#[test]
fn red_team_a_context_through_a_magic_link_is_refused() {
    let dir = ctx_dir();
    let real = dir.path().canonicalize().unwrap();
    let held = fs::File::open(&real).unwrap();
    let n = std::os::fd::AsRawFd::as_raw_fd(&held);
    let paths = [
        PathBuf::from("/proc/self/cwd"),
        PathBuf::from("/proc/thread-self/cwd"),
        Path::new("/proc/self/root").join(real.strip_prefix("/").unwrap()),
        PathBuf::from(format!("/proc/self/fd/{n}")),
        PathBuf::from(format!("/dev/fd/{n}")),
    ];
    for path in &paths {
        let err = DirSource::new(path).open().unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "{}: {err}",
            path.display()
        );
    }
    let ctx = DirSource::new(&real).open().unwrap();
    assert_eq!(ctx.origin(), real);
    drop(held);
}

/// T2c (the VM or a default rootful Docker run): a root-only file
/// hardlinked into a context another account owns, as that account can do
/// where `fs.protected_hardlinks=0`, is neither read as the Dockerfile nor
/// copied, though ranikust reads as root (ADR-0076). Root makes the link
/// here, standing in for the account.
#[test]
#[ignore = "T2c: default: chown to another user and a root-only file need root"]
fn a_root_only_file_hardlinked_into_the_context_is_never_read() {
    let tmp = ctx_dir();
    let secret = tmp.path().join("secret");
    fs::write(&secret, b"DUMMY-ROOT-ONLY-7f3a\n").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let ctx_path = tmp.path().join("ctx");
    fs::create_dir(&ctx_path).unwrap();
    fs::hard_link(&secret, ctx_path.join("s")).unwrap();
    fs::write(ctx_path.join("own"), b"mine\n").unwrap();
    for p in [ctx_path.clone(), ctx_path.join("own")] {
        rustix::fs::chown(
            &p,
            Some(rustix::fs::Uid::from_raw(1000)),
            Some(rustix::fs::Gid::from_raw(1000)),
        )
        .unwrap();
    }
    // A stale planted link: one name left, still root's and root-only.
    fs::write(ctx_path.join("stale"), b"DUMMY-STALE\n").unwrap();
    fs::set_permissions(ctx_path.join("stale"), fs::Permissions::from_mode(0o600)).unwrap();
    // A directory root made in the context (`sudo make` in a checkout):
    // judged against its own owner, so its root-only files are copied.
    fs::create_dir(ctx_path.join("rootdir")).unwrap();
    fs::write(ctx_path.join("rootdir/out"), b"built\n").unwrap();
    fs::set_permissions(
        ctx_path.join("rootdir/out"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let ctx = DirSource::new(&ctx_path).open().unwrap();
    assert_eq!(ctx.owner(), 1000);
    let e = ctx.open_file("s").unwrap_err();
    assert!(e.to_string().contains("hardlink"), "{e}");
    let e = ctx.open_file("stale").unwrap_err();
    assert!(e.to_string().contains("could not read"), "{e}");
    assert!(ctx.open_file("own").is_ok());
    assert!(ctx.open_file("rootdir/out").is_ok());

    let dst = ctx_dir();
    let dst_root = rnk_rootfs::RootPath::open(dst.path()).unwrap();
    let opts = rnk_rootfs::CopyOpts {
        from_context: true,
        ..rnk_rootfs::CopyOpts::default()
    };
    let e = rnk_rootfs::copy(ctx.root(), "s", &dst_root, "s", &opts).unwrap_err();
    assert!(e.to_string().contains("hardlink"), "{e}");
    let e = rnk_rootfs::copy(ctx.root(), "stale", &dst_root, "stale", &opts).unwrap_err();
    assert!(e.to_string().contains("could not read"), "{e}");
    let e = rnk_rootfs::copy(ctx.root(), "", &dst_root, "all", &opts).unwrap_err();
    assert!(e.to_string().contains("hardlink"), "{e}");
    assert!(!dst.path().join("s").exists() && !dst.path().join("stale").exists());
    rnk_rootfs::copy(ctx.root(), "rootdir", &dst_root, "rootdir", &opts).unwrap();
    rnk_rootfs::copy(ctx.root(), "own", &dst_root, "own", &opts).unwrap();
}

#[test]
fn walks_in_buildkit_order_without_following_symlinks() {
    let outside = ctx_dir();
    fs::write(outside.path().join("secret"), b"x").unwrap();
    let tmp = ctx_dir();
    let root = tmp.path();
    fs::create_dir_all(root.join("b/c")).unwrap();
    fs::write(root.join("b/c/f.txt"), b"hello").unwrap();
    fs::write(root.join("a.txt"), b"12").unwrap();
    fs::write(root.join("B"), b"").unwrap();
    symlink(outside.path(), root.join("link-out")).unwrap();
    symlink("b", root.join("link-in")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        root.join("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();

    let ctx = DirSource::new(root).open().unwrap();
    let mut walk = ctx.walk(None);
    let entries: Vec<_> = walk.by_ref().map(Result::unwrap).collect();
    let paths: Vec<_> = entries.iter().map(|e| e.path.clone()).collect();
    // Byte-wise per directory, depth first; symlinks reported, not entered.
    assert_eq!(
        paths,
        [
            p("B"),
            p("a.txt"),
            p("b"),
            p("b/c"),
            p("b/c/f.txt"),
            p("fifo"),
            p("link-in"),
            p("link-out"),
        ]
    );
    let kind = |name: &str| {
        entries
            .iter()
            .find(|e| e.path == Path::new(name))
            .unwrap()
            .meta
            .file_type
    };
    assert_eq!(kind("link-out"), FileType::Symlink);
    assert_eq!(kind("fifo"), FileType::Fifo);
    assert_eq!(kind("b"), FileType::Directory);
    let stats = walk.stats();
    assert_eq!((stats.files, stats.dirs, stats.bytes), (6, 2, 7));
    assert_eq!(stats.files_ignored, 0);
}

#[test]
fn non_utf8_names_are_reported() {
    let tmp = ctx_dir();
    let name = OsStr::from_bytes(b"caf\xe9");
    fs::write(tmp.path().join(name), b"").unwrap();
    let ctx = DirSource::new(tmp.path()).open().unwrap();
    assert_eq!(walk_paths(&ctx, None), [PathBuf::from(name)]);
}

#[test]
fn excluded_directory_is_held_back_for_an_exception() {
    let tmp = ctx_dir();
    let root = tmp.path();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/drop.md"), b"").unwrap();
    fs::write(root.join("docs/keep.md"), b"").unwrap();
    fs::create_dir_all(root.join("node_modules/p")).unwrap();
    fs::write(root.join("node_modules/p/i.js"), b"").unwrap();
    fs::write(root.join("app.js"), b"").unwrap();
    let ctx = DirSource::new(root).open().unwrap();
    let m = rnk_ignore::Matcher::new(&["docs", "!docs/keep.md", "node_modules"]).unwrap();
    let mut walk = ctx.walk(Some(&m));
    let paths: Vec<_> = walk.by_ref().map(|e| e.unwrap().path).collect();
    assert_eq!(paths, [p("app.js"), p("docs"), p("docs/keep.md")]);
    // docs/drop.md, and node_modules skipped whole.
    assert_eq!(walk.stats().files_ignored, 2);
}

#[test]
fn dockerignore_beside_the_dockerfile_wins() {
    let tmp = ctx_dir();
    let root = tmp.path();
    fs::write(root.join(".dockerignore"), b"root\n").unwrap();
    fs::write(root.join("Dockerfile.dockerignore"), b"beside\n").unwrap();
    let ctx = DirSource::new(root).open().unwrap();
    let dockerfile = Some(DockerfileAt::Context(Path::new("Dockerfile")));

    let found = ctx.find_dockerignore(dockerfile).unwrap().unwrap();
    assert_eq!(
        found.source,
        IgnoreSource::Dockerfile(root.join("Dockerfile.dockerignore"))
    );
    assert_eq!(found.contents, b"beside\n");

    fs::remove_file(root.join("Dockerfile.dockerignore")).unwrap();
    let found = ctx.find_dockerignore(dockerfile).unwrap().unwrap();
    assert_eq!(found.source, IgnoreSource::ContextRoot);
    assert_eq!(found.contents, b"root\n");

    fs::remove_file(root.join(".dockerignore")).unwrap();
    assert!(ctx.find_dockerignore(dockerfile).unwrap().is_none());
    assert!(ctx.find_dockerignore(None).unwrap().is_none());

    // Beside a Dockerfile on the host, outside the context.
    let host = ctx_dir();
    fs::write(host.path().join("Dockerfile.dockerignore"), b"host\n").unwrap();
    fs::write(host.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
    let dockerfile = host_file(&host.path().join("Dockerfile"), &ctx);
    let found = ctx
        .find_dockerignore(Some(DockerfileAt::Host(&dockerfile)))
        .unwrap()
        .unwrap();
    assert_eq!(found.contents, b"host\n");
}

#[test]
fn dockerignore_symlink_beside_the_dockerfile_is_refused() {
    let outside = ctx_dir();
    fs::write(outside.path().join("secret"), b"top secret\n").unwrap();
    // On the host: refused rather than followed.
    let host = ctx_dir();
    symlink(
        outside.path().join("secret"),
        host.path().join("Dockerfile.dockerignore"),
    )
    .unwrap();
    let tmp = ctx_dir();
    let root = tmp.path();
    let ctx = DirSource::new(root).open().unwrap();
    fs::write(host.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
    let dockerfile = host_file(&host.path().join("Dockerfile"), &ctx);
    let err = ctx
        .find_dockerignore(Some(DockerfileAt::Host(&dockerfile)))
        .unwrap_err();
    assert!(matches!(err, Error::IgnoreFile { .. }), "{err:?}");
    // A FIFO beside it is refused without being opened, so it never blocks.
    fs::remove_file(host.path().join("Dockerfile.dockerignore")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        host.path().join("Dockerfile.dockerignore"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .unwrap();
    let err = ctx
        .find_dockerignore(Some(DockerfileAt::Host(&dockerfile)))
        .unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");
    // In the context: resolved inside it, where the target does not exist.
    symlink(
        outside.path().join("secret"),
        root.join("Dockerfile.dockerignore"),
    )
    .unwrap();
    let found = ctx
        .find_dockerignore(Some(DockerfileAt::Context(Path::new("Dockerfile"))))
        .unwrap();
    assert!(found.is_none(), "{found:?}");
}

#[test]
fn context_dockerignore_symlink_cannot_leave_the_context() {
    let outside = ctx_dir();
    fs::write(outside.path().join("secret"), b"top secret\n").unwrap();
    let tmp = ctx_dir();
    let root = tmp.path();
    // Absolute target: resolved inside the context, where it does not exist.
    symlink(outside.path().join("secret"), root.join(".dockerignore")).unwrap();
    let ctx = DirSource::new(root).open().unwrap();
    assert!(ctx.find_dockerignore(None).unwrap().is_none());
    // `..` cannot climb out either.
    fs::remove_file(root.join(".dockerignore")).unwrap();
    let up = Path::new("../../../../../../..").join(outside.path().strip_prefix("/").unwrap());
    symlink(up.join("secret"), root.join(".dockerignore")).unwrap();
    assert!(ctx.find_dockerignore(None).unwrap().is_none());
}

#[test]
fn sub_context_stays_inside() {
    let tmp = ctx_dir();
    let root = tmp.path();
    fs::create_dir_all(root.join("sub/inner")).unwrap();
    fs::write(root.join("sub/inner/f"), b"").unwrap();
    fs::write(root.join("top"), b"").unwrap();
    let ctx = DirSource::new(root).open().unwrap();
    let sub = ctx.sub_context("sub").unwrap();
    assert_eq!(walk_paths(&sub, None), [p("inner"), p("inner/f")]);
    // `..` is clamped at the context root, not followed out of it.
    let up = ctx.sub_context("../..").unwrap();
    assert_eq!(walk_paths(&up, None), walk_paths(&ctx, None));
}

#[test]
fn unreadable_directory_stops_the_walk() {
    let tmp = ctx_dir();
    let root = tmp.path();
    fs::create_dir(root.join("locked")).unwrap();
    fs::write(root.join("locked/f"), b"").unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(root.join("locked")).is_ok() {
        // Running with CAP_DAC_OVERRIDE (root): nothing to refuse.
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let ctx = DirSource::new(root).open().unwrap();
    let results: Vec<_> = ctx.walk(None).collect();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(results.first(), Some(Ok(e)) if e.path == Path::new("locked")));
    assert!(matches!(results.get(1), Some(Err(_))), "{results:?}");
    assert_eq!(results.len(), 2, "the walk stops at the first error");
}

#[test]
fn open_dispatches_on_the_spec() {
    let tmp = ctx_dir();
    let ctx = rnk_context::open(&ContextSpec::Dir(tmp.path().to_owned())).unwrap();
    assert_eq!(ctx.origin(), fs::canonicalize(tmp.path()).unwrap());
    for spec in [
        ContextSpec::Tar("stdin".into()),
        ContextSpec::Git("example.com/r.git".into()),
        ContextSpec::Https("https://example.com/c.tar.gz".into()),
    ] {
        assert!(matches!(
            rnk_context::open(&spec),
            Err(Error::Unsupported { .. })
        ));
    }
    let missing = tmp.path().join("missing");
    assert!(matches!(
        DirSource::new(&missing).open(),
        Err(Error::Dir { .. })
    ));
}

#[test]
fn bad_patterns_are_reported() {
    let tmp = ctx_dir();
    let ctx = DirSource::new(tmp.path()).open().unwrap();
    let ignore = rnk_context::IgnoreFile {
        source: IgnoreSource::ContextRoot,
        contents: b"!\n".to_vec(),
    };
    assert!(matches!(ctx.matcher(&ignore), Err(Error::Ignore(_))));
}
