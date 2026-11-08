use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::PathBuf;

use super::*;

/// Where each of `parts` starts in `text`, in order, or a panic naming the
/// one missing or out of order.
fn in_order(text: &str, parts: &[&str]) {
    let mut from = 0;
    for part in parts {
        let Some(at) = text[from..].find(part) else {
            panic!("{part:?} is not after byte {from} of: {text}");
        };
        from += at + part.len();
    }
}

/// The advice gives `sudo`, then, only where the switch is 1, the profile
/// as shipped for its own path, bound to another root-owned path, or after
/// installing the binary under `/usr/local/bin`, then a container.
#[test]
fn the_advice_gives_sudo_then_the_profile_then_a_container() {
    let shipped = Some(Path::new("/usr/local/bin/ranikust"));
    let other = Some(Path::new("/usr/bin/ranikust"));
    let home = Some(Path::new("/home/u/bin/ranikust"));
    let load = "copy ranikust's AppArmor profile (contrib/apparmor/ranikust in its repository; \
                the book's \"Choosing where to run it\" shows it) to /etc/apparmor.d/ranikust";
    let run = "run apparmor_parser -r /etc/apparmor.d/ranikust";

    let a = advice(true, shipped, true);
    in_order(
        &a,
        &[
            "kernel.apparmor_restrict_unprivileged_userns is 1, as here",
            "run ranikust as root, with sudo; ",
            "or, as root, ",
            load,
            run,
            "; or run ranikust in a container",
        ],
    );
    assert!(!a.contains("changed to") && !a.contains("first"), "{a}");

    let b = advice(true, other, true);
    in_order(
        &b,
        &[
            "with sudo; ",
            load,
            "with its path /usr/local/bin/ranikust changed to /usr/bin/ranikust",
            run,
            "; or run ranikust in a container",
        ],
    );

    for (binary, why) in [
        (
            home,
            "/home/u/bin/ranikust is not a path only root can change, or ranikust cannot tell \
             that it is",
        ),
        // Bound to the shipped path only where only root changes it.
        (
            shipped,
            "/usr/local/bin/ranikust is not a path only root can change",
        ),
        (None, "ranikust cannot read the path it runs from"),
    ] {
        let c = advice(true, binary, false);
        in_order(
            &c,
            &[
                "with sudo; ",
                "or, as root, install ranikust as /usr/local/bin/ranikust first, ",
                load,
                run,
                "then run ranikust from /usr/local/bin/ranikust: ",
                why,
                "a profile lets whatever is at its path make user namespaces",
                "; or run ranikust in a container",
            ],
        );
    }

    // Without the switch at 1, no profile, whatever the path.
    for (binary, root_only) in [(shipped, true), (home, false), (None, false)] {
        let plain = advice(false, binary, root_only);
        assert_eq!(
            plain,
            "a user other than root builds in a user namespace of its own, which this user \
             cannot get here: run ranikust as root, with sudo; or run ranikust in a container"
        );
    }
}

fn node(kind: FileType, uid: u32, mode: u32) -> Node {
    Node {
        kind,
        uid,
        mode,
        fuse: false,
    }
}

/// Only root may change a path whose every node is root's, written by no
/// one else, none on FUSE, its directories directories and its file a
/// regular one.
#[test]
fn only_root_s_nodes_written_by_no_one_else_pass() {
    let dir = |uid, mode| node(FileType::Directory, uid, mode);
    let file = |uid, mode| node(FileType::RegularFile, uid, mode);
    let good = [dir(0, 0o755), dir(0, 0o755), file(0, 0o755)];
    assert!(root_only(&good));
    assert!(root_only(&[dir(0, 0o755), file(0, 0o4711)]));
    let mut fused = good;
    fused[1].fuse = true;
    for (nodes, what) in [
        (vec![], "nothing"),
        (vec![file(0, 0o755)], "a file and no /"),
        (vec![dir(0, 0o755)], "/ alone"),
        (vec![dir(0, 0o755), dir(0, 0o755)], "a directory"),
        (
            vec![dir(0, 0o755), node(FileType::Symlink, 0, 0o777)],
            "a symlink",
        ),
        (
            vec![dir(0, 0o755), node(FileType::Socket, 0, 0o755)],
            "a socket",
        ),
        (
            vec![dir(0, 0o755), file(0, 0o755), file(0, 0o755)],
            "a file above",
        ),
        (
            vec![dir(0, 0o755), dir(1000, 0o755), file(0, 0o755)],
            "a user's directory",
        ),
        (
            vec![dir(0, 0o755), dir(0, 0o755), file(1000, 0o755)],
            "a user's file",
        ),
        (
            vec![dir(0, 0o775), dir(0, 0o755), file(0, 0o755)],
            "a group-writable /",
        ),
        (vec![dir(0, 0o755), dir(0, 0o1777), file(0, 0o755)], "/tmp"),
        (
            vec![dir(0, 0o755), dir(0, 0o757), file(0, 0o755)],
            "a directory others write",
        ),
        (
            vec![dir(0, 0o755), dir(0, 0o755), file(0, 0o775)],
            "a group-writable file",
        ),
        (
            vec![dir(0, 0o755), dir(0, 0o755), file(0, 0o757)],
            "a file others write",
        ),
        (fused.to_vec(), "FUSE"),
    ] {
        assert!(!root_only(&nodes), "{what}");
    }
}

/// The walk opens no symlink's target and nothing below it, and refuses a
/// relative path, a `..` and a path that is not there; a user's file is not
/// root's.
#[test]
fn the_walk_follows_no_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("ranikust");
    std::fs::write(&file, "x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink("/usr", &link).unwrap();

    let nodes = walk(&file).unwrap();
    assert_eq!(nodes.len(), file.components().count());
    assert_eq!(nodes.last().unwrap().kind, FileType::RegularFile);
    assert!(!only_root_changes(&file));

    let nodes = walk(&link).unwrap();
    assert_eq!(nodes.last().unwrap().kind, FileType::Symlink);
    assert!(!only_root_changes(&link));
    assert_eq!(walk(&link.join("bin")).unwrap_err(), Errno::NOTDIR);

    let up = tmp.path().join("..").join(tmp.path().file_name().unwrap());
    assert_eq!(walk(&up).unwrap_err(), Errno::INVAL);
    assert_eq!(walk(Path::new("usr/bin")).unwrap_err(), Errno::INVAL);
    assert_eq!(walk(&tmp.path().join("missing")).unwrap_err(), Errno::NOENT);
    // As `/proc/self/exe` names a binary removed since.
    assert!(!only_root_changes(&tmp.path().join("ranikust (deleted)")));
}

/// On system binaries, the check agrees with the standard library's view
/// of each node up from the file, and passes the regular files only root
/// may change.
#[test]
fn the_check_agrees_on_system_binaries() {
    let mut passed = 0;
    for path in [
        "/usr/bin/sh",
        "/usr/bin/dash",
        "/usr/bin/bash",
        "/usr/bin/env",
        "/bin/sh",
    ] {
        let path = PathBuf::from(path);
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        let file = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file());
        let by_std = file
            && path.ancestors().all(|p| {
                std::fs::symlink_metadata(p).is_ok_and(|m| {
                    !m.file_type().is_symlink() && m.uid() == 0 && m.mode() & 0o022 == 0
                })
            });
        assert_eq!(only_root_changes(&path), by_std, "{}", path.display());
        passed += usize::from(by_std);
    }
    if passed == 0 {
        eprintln!("skipped: no system binary here is root's all the way up");
    }
}
