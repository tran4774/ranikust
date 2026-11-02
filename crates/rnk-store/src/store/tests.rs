//! Deletion races and mounts in the state directory, which need the
//! store's own `RootPath`.

use std::fs;
use std::path::Path;

use super::*;

fn store(dir: &Path) -> Store {
    Store::open(&BuildPaths::new(dir, dir.join("state"))).unwrap()
}

#[test]
fn a_directory_swapped_for_a_symlink_is_not_emptied() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let state = s.state_dir().to_owned();
    fs::create_dir_all(state.join("tmp/dead/sub")).unwrap();
    let st = s.state.statx("tmp/dead").unwrap();
    // The swap between the `statx` and the open: the name now leads to
    // the state directory itself.
    fs::rename(state.join("tmp/dead"), state.join("tmp/elsewhere")).unwrap();
    std::os::unix::fs::symlink("/", state.join("tmp/dead")).unwrap();

    let err = remove_dir_tree(&s.state, Path::new("tmp/dead"), &st).unwrap_err();
    assert!(matches!(err, Error::BadEntry { .. }), "{err}");
    for d in DIRS {
        assert!(state.join(d).is_dir(), "{d}");
    }
    assert!(state.join(MARKER).is_file());
    assert!(state.join("tmp/elsewhere/sub").is_dir());
}

#[test]
fn a_directory_swapped_for_another_is_not_emptied() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let state = s.state_dir().to_owned();
    fs::create_dir_all(state.join("tmp/dead")).unwrap();
    let st = s.state.statx("tmp/dead").unwrap();
    // The replacement exists before the original goes, so it cannot reuse
    // its inode number (some filesystems hand a freed one straight back).
    fs::create_dir_all(state.join("tmp/other/precious")).unwrap();
    fs::remove_dir(state.join("tmp/dead")).unwrap();
    fs::rename(state.join("tmp/other"), state.join("tmp/dead")).unwrap();

    let err = remove_dir_tree(&s.state, Path::new("tmp/dead"), &st).unwrap_err();
    assert!(matches!(err, Error::BadEntry { .. }), "{err}");
    assert!(state.join("tmp/dead/precious").is_dir());
}

#[test]
#[ignore = "T2c: sysadmin: bind-mounts in a private mount namespace"]
fn a_mount_where_clean_up_deletes_is_not_emptied() {
    // On a thread of its own in a new mount namespace, every mount private,
    // so nothing it mounts reaches another namespace.
    std::thread::spawn(|| {
        use rustix::mount::{MountPropagationFlags, mount_bind};
        use rustix::thread::UnshareFlags;
        // SAFETY: this thread shares nothing that the unshared filesystem
        // attributes or mount namespace would break; it ends with the test.
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
            .expect("unshare(CLONE_FS | CLONE_NEWNS)");
        rustix::mount::mount_change(
            "/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )
        .expect("make every mount private");

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("host-file"), "keep").unwrap();
        let state = dir.path().join("state");
        let dead = store(dir.path());
        let id = dead.build_id().to_owned();
        drop(dead);
        let target = state.join("tmp").join(&id);
        mount_bind(outside.path(), &target).unwrap();
        // Declared after both directories: dropped first, on a panic too.
        let bind = Detach(target);

        // Clean-up meets the mount on a dead build's directory: it warns
        // and goes on, and the mounted directory keeps its contents.
        let next = Store::open(&BuildPaths::new(dir.path(), &state));
        let kept = fs::read_to_string(outside.path().join("host-file"));
        drop(bind);
        let next = next.unwrap();
        assert_eq!(kept.unwrap(), "keep");

        // The same through a fill's discard path and `close`: a mount on
        // the build's own directory is refused, not emptied.
        mount_bind(outside.path(), next.build_dir()).unwrap();
        let bind = Detach(next.build_dir().to_owned());
        let st = next
            .state
            .statx(Path::new("builds").join(&next.id))
            .unwrap();
        let r = remove_dir_tree(&next.state, &Path::new("builds").join(&next.id), &st);
        let kept = fs::read_to_string(outside.path().join("host-file"));
        drop(bind);
        assert!(matches!(r, Err(Error::BadEntry { .. })), "{r:?}");
        assert_eq!(kept.unwrap(), "keep");
    })
    .join()
    .unwrap();
}

/// A mount a T2 test made below its temporary directories, detached on drop
/// (panics included) before they are removed. If it cannot be detached the
/// process aborts, so nothing is removed through it.
struct Detach(std::path::PathBuf);

impl Drop for Detach {
    fn drop(&mut self) {
        if let Err(e) = rustix::mount::unmount(&self.0, rustix::mount::UnmountFlags::DETACH) {
            eprintln!(
                "cannot detach {}: {e}; aborting to remove nothing",
                self.0.display()
            );
            std::process::abort();
        }
    }
}

/// The advice given when an anonymous store opened on the traces of a build
/// cut short, word for word, in its warning and in `named_window`'s refusal.
#[test]
fn the_cut_short_warning_and_refusal_give_the_same_advice() {
    assert_eq!(
        CUT_SHORT,
        "an earlier build in this container did not end cleanly and may have left RUN \
         processes; run ranikust as the container's PID 1, or restart the container, to keep \
         state on disk"
    );
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    // A marker of this container left behind: the next build is tainted.
    let first = Store::open_anonymous(&state).unwrap();
    let run = first.enter_run().unwrap();
    first.leave_run(run, false).unwrap();
    first.close().unwrap();
    let s = Store::open_anonymous(&state).unwrap();
    let e = s.named_window().unwrap_err();
    assert_eq!(
        e.to_string(),
        Error::Anonymous(
            "COPY --from a kept stage or an image, unpacked by name, after a build cut short (an \
             earlier build in this container did not end cleanly and may have left RUN \
             processes; run ranikust as the container's PID 1, or restart the container),"
        )
        .to_string()
    );
    s.close().unwrap();
}

/// Characterization: `scratch_file` and `anonymous_file` take their files
/// from the same place in each kind of store, and name nothing.
#[test]
fn scratch_and_anonymous_files_name_nothing_in_either_store() {
    use std::io::{Read as _, Seek as _, Write as _};
    let names = |d: &Path| -> Vec<std::ffi::OsString> {
        fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
    };
    let dir = tempfile::tempdir().unwrap();
    let persistent = store(dir.path());
    let anon_dir = dir.path().join("anon");
    fs::create_dir(&anon_dir).unwrap();
    let anonymous = Store::open_anonymous(&anon_dir).unwrap();
    for (s, shown) in [
        (&persistent, persistent.build_dir().to_owned()),
        (&anonymous, anon_dir.clone()),
    ] {
        let before = names(&shown);
        let mut f = s.scratch_file().unwrap();
        f.write_all(b"scratch").unwrap();
        f.rewind().unwrap();
        let mut back = String::new();
        f.read_to_string(&mut back).unwrap();
        assert_eq!(back, "scratch");
        let (mut kept, path) = s.anonymous_file().unwrap();
        kept.write_all(b"kept").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"kept");
        assert_eq!(before, names(&shown), "{}", shown.display());
    }
    anonymous.close().unwrap();
}
