//! T0 tests of opening, reading and walking a local directory context.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use rnk_context::{ContextSource as _, ContextSpec, DirSource, Error, IgnoreSource};
use rnk_rootfs::FileType;

fn ctx_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn walk_paths(ctx: &rnk_context::Context, m: Option<&rnk_ignore::Matcher>) -> Vec<PathBuf> {
    ctx.walk(m).map(|e| e.unwrap().path).collect()
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
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
    let dockerfile = root.join("Dockerfile");

    let found = ctx.find_dockerignore(Some(&dockerfile)).unwrap().unwrap();
    assert_eq!(
        found.source,
        IgnoreSource::Dockerfile(root.join("Dockerfile.dockerignore"))
    );
    assert_eq!(found.contents, b"beside\n");

    fs::remove_file(root.join("Dockerfile.dockerignore")).unwrap();
    let found = ctx.find_dockerignore(Some(&dockerfile)).unwrap().unwrap();
    assert_eq!(found.source, IgnoreSource::ContextRoot);
    assert_eq!(found.contents, b"root\n");

    fs::remove_file(root.join(".dockerignore")).unwrap();
    assert!(ctx.find_dockerignore(Some(&dockerfile)).unwrap().is_none());
    assert!(ctx.find_dockerignore(None).unwrap().is_none());
}

#[test]
fn dockerignore_symlink_beside_the_dockerfile_is_refused() {
    let outside = ctx_dir();
    fs::write(outside.path().join("secret"), b"top secret\n").unwrap();
    let tmp = ctx_dir();
    let root = tmp.path();
    symlink(
        outside.path().join("secret"),
        root.join("Dockerfile.dockerignore"),
    )
    .unwrap();
    let ctx = DirSource::new(root).open().unwrap();
    let err = ctx
        .find_dockerignore(Some(&root.join("Dockerfile")))
        .unwrap_err();
    assert!(matches!(err, Error::IgnoreFile { .. }), "{err:?}");
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
