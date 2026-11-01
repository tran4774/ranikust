//! Red team (ADR-0060, ADR-0051, T12, T18): what the library's public API
//! lets a caller run with the sandbox kaniko mode on a container's `/`
//! uses (`Sandbox::start_direct`). It runs `Isolation::Direct` on `/` only:
//! a bare `chroot` into any other directory, a full-sandbox `RUN` or a
//! root spelled so that it compares equal to `/` without being written
//! `/` is refused before anything is placed or cloned. T0: every request
//! here is refused before a child exists, whatever the privileges.

use std::fs;
use std::path::{Path, PathBuf};

use rnk_sandbox::{Error, Isolation, RunSpec, Sandbox};

fn spec(root: &Path, isolation: Isolation) -> RunSpec {
    let mut spec = RunSpec::new(root, vec!["/bin/true".into()]);
    spec.isolation = isolation;
    spec.workdir = "/made-by-the-run".into();
    spec
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    v
}

#[test]
fn a_direct_only_sandbox_refuses_any_root_but_slash_and_the_full_sandbox() {
    let sandbox = Sandbox::start_direct().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("stage")).unwrap();
    let stage = root.join("stage");
    // A symlink to `/` and a directory that holds a `/`-like tree.
    std::os::unix::fs::symlink("/", root.join("to-slash")).unwrap();
    let before = entries(&stage);

    // A bare `chroot` into a stage directory (ADR-0014's plan, ADR-0060).
    let r = sandbox.run(spec(&stage, Isolation::Direct));
    assert!(
        matches!(&r, Err(Error::InvalidSpec(m)) if m.contains("on / only")),
        "{r:?}"
    );
    // Through a symlink to `/`: not `/` by name, so refused too (the root
    // is opened without following it, or the name is not `/`).
    let r = sandbox.run(spec(&root.join("to-slash"), Isolation::Direct));
    assert!(
        matches!(&r, Err(Error::InvalidSpec(_) | Error::Rootfs(_))),
        "{r:?}"
    );
    // The full sandbox needs the probe's capabilities.
    for r in [&stage, Path::new("/")] {
        let out = sandbox.run(spec(r, Isolation::Full));
        assert!(matches!(&out, Err(Error::Privilege(_))), "{r:?}: {out:?}");
    }
    assert_eq!(entries(&stage), before, "something was placed in the root");
    assert!(!Path::new("/made-by-the-run").exists());
}

/// Spellings of `/` that `Path` compares equal to `/` (`//`, `/.`) or that
/// a mountinfo comparison would miss are refused for either isolation;
/// so is any root with `..`.
#[test]
fn roots_spelled_unlike_the_kernel_are_refused() {
    let sandbox = Sandbox::start_direct().unwrap();
    for bad in [
        "//", "/.", "/./", "///", "/tmp/", "/tmp//x", "/tmp/./x", "/tmp/..", "/tmp/../",
    ] {
        for isolation in [Isolation::Direct, Isolation::Full] {
            let r = sandbox.run(spec(Path::new(bad), isolation));
            assert!(
                matches!(&r, Err(Error::InvalidSpec(_))),
                "{bad} ({isolation:?}): {r:?}"
            );
        }
    }
}
