use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use rnk_sandbox::{StepFailure, StepResult};

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
/// installing the binary where only root may change it, then a container.
#[test]
fn the_advice_gives_sudo_then_the_profile_then_a_container() {
    let shipped = Path::new("/usr/local/bin/ranikust");
    let other = Path::new("/usr/bin/ranikust");
    let load = "copy ranikust's AppArmor profile (contrib/apparmor/ranikust in its repository; \
                the book's \"Choosing where to run it\" shows it) to /etc/apparmor.d/ranikust";
    let run = "run apparmor_parser -r /etc/apparmor.d/ranikust";
    let owned = "owned by root and written by no one else (install -o root -g root -m 0755)";
    let any = "a profile lets whatever is at its path make user namespaces";

    let a = advice(Some(Checked {
        bound: Ok(shipped),
        dir: Ok(()),
    }));
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

    let b = advice(Some(Checked {
        bound: Ok(other),
        dir: Err("unused".to_owned()),
    }));
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

    let c = advice(Some(Checked {
        bound: Err("WHY".to_owned()),
        dir: Ok(()),
    }));
    in_order(
        &c,
        &[
            "with sudo; ",
            "or, as root, install ranikust as /usr/local/bin/ranikust first, ",
            owned,
            load,
            run,
            "then run ranikust from /usr/local/bin/ranikust: WHY, and ",
            any,
            "; or run ranikust in a container",
        ],
    );

    let d = advice(Some(Checked {
        bound: Err("WHY".to_owned()),
        dir: Err("DIR".to_owned()),
    }));
    in_order(
        &d,
        &[
            "with sudo; ",
            "or, as root, install ranikust first in a directory only root can change, which \
             /usr/local/bin is not here (DIR), ",
            owned,
            load,
            "with its path /usr/local/bin/ranikust changed to the binary's",
            run,
            "then run ranikust from there: WHY, and ",
            any,
            "; or run ranikust in a container",
        ],
    );

    // Without the switch at 1, no profile.
    assert_eq!(
        advice(None),
        "a user other than root builds in a user namespace of its own, which this user cannot \
         get here: run ranikust as root, with sudo; or run ranikust in a container"
    );
}

/// The profile is offered only where AppArmor may be what refused: a step
/// refused in a user namespace `unshare` made, or `unshare` itself refused
/// with `EACCES`. `unshare` refused otherwise (a seccomp filter's or
/// `kernel.unprivileged_userns_clone`'s `EPERM`,
/// `user.max_user_namespaces`' `ENOSPC`), a step before it refused in its
/// place, a step that did not run, or a stop whose cause is not known, gets
/// `sudo` then a container, whatever the switch reads.
#[test]
fn only_a_stop_apparmor_may_cause_offers_the_profile() {
    let failed = |op, errno| {
        StepResult::Failed(StepFailure {
            op,
            errno,
            detail: None,
        })
    };
    let unshare = |errno| failed("unshare(CLONE_NEWUSER)", errno);
    let at_unshare = |r: &StepResult| Stop::at("unshare", r, r);
    let eacces = Errno::ACCESS.raw_os_error();
    let ok = StepResult::Ok;
    let not_run = StepResult::NotRun("the probe's child stopped before it");
    let mount_ns = failed("unshare(CLONE_NEWNS)", Some(libc::EPERM));
    let uid_map = failed("write uid_map", Some(libc::EPERM));
    let chown = failed("chown", Some(libc::EINVAL));
    let offers = [
        (at_unshare(&unshare(Some(eacces))), Stop::Unshare(eacces)),
        (Stop::at("map", &ok, &uid_map), Stop::InNamespace),
        (Stop::at("chown", &ok, &chown), Stop::InNamespace),
        (
            Stop::at("mount namespace", &ok, &mount_ns),
            Stop::InNamespace,
        ),
    ];
    for (got, want) in offers {
        assert_eq!(got, want);
        assert!(got.offers_profile(), "{got:?}");
    }
    let fchdir = failed("fchdir to the probe's directory", Some(eacces));
    let shown = Path::new("/usr/local/bin/ranikust");
    let others = [
        (
            at_unshare(&unshare(Some(libc::EPERM))),
            Stop::Unshare(libc::EPERM),
        ),
        (
            at_unshare(&unshare(Some(libc::ENOSPC))),
            Stop::Unshare(libc::ENOSPC),
        ),
        (
            at_unshare(&unshare(Some(libc::EINVAL))),
            Stop::Unshare(libc::EINVAL),
        ),
        (at_unshare(&fchdir), Stop::Unknown),
        (at_unshare(&unshare(None)), Stop::Unknown),
        (at_unshare(&not_run), Stop::Unknown),
        (at_unshare(&ok), Stop::Unknown),
        // A child cut short once `unshare` worked.
        (Stop::at("mount namespace", &ok, &not_run), Stop::Unknown),
        (Stop::at("map", &ok, &not_run), Stop::Unknown),
        (
            Stop::at("mount namespace", &not_run, &mount_ns),
            Stop::Unknown,
        ),
        (
            Stop::at("map", &unshare(Some(eacces)), &uid_map),
            Stop::Unknown,
        ),
        (Stop::at("proc", &ok, &mount_ns), Stop::Unknown),
        (Stop::at("overlay", &ok, &mount_ns), Stop::Unknown),
    ];
    for (got, want) in others {
        assert_eq!(got, want);
        assert!(!got.offers_profile(), "{got:?}");
        assert_eq!(guidance(Some(shown), got), advice(None), "{got:?}");
    }
    // The profile itself only where the switch reads 1 here.
    let in_namespace = guidance(Some(shown), Stop::InNamespace);
    assert_eq!(
        in_namespace.contains(PROFILE),
        restricted(),
        "{in_namespace}"
    );
}

/// A profile names only a path of plain characters as it is: no glob,
/// alternation, variable, comment or space.
#[test]
fn only_a_plain_path_is_pasted_into_a_profile() {
    for path in [
        "/usr/local/bin/ranikust",
        "/opt/rnk-0.1_x86+64/bin/ranikust",
    ] {
        assert!(plain(Path::new(path)), "{path}");
    }
    for path in [
        "/opt/a b/ranikust",
        "/opt/*/ranikust",
        "/opt/r?nk",
        "/opt/[ab]/ranikust",
        "/opt/{a,b}/ranikust",
        "/opt/^x",
        "/opt/@{HOME}/ranikust",
        "/opt/#x",
        "/opt/x\"y",
        "/opt/caf\u{e9}/ranikust",
    ] {
        assert!(!plain(Path::new(path)), "{path}");
    }
}

/// ext2, ext3 and ext4's `f_type`.
const EXT4: FsWord = 0xEF53;

fn node(kind: FileType, uid: u32, mode: u32) -> Node {
    Node {
        kind,
        uid,
        mode,
        fs_type: EXT4,
        nosuid: false,
    }
}

/// `nodes` at `/`, `/a`, `/a/b`… in order.
fn at(nodes: &[Node]) -> Vec<(PathBuf, Node)> {
    let mut path = PathBuf::from("/");
    nodes
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            if i > 0 {
                path.push(char::from(b'a' + u8::try_from(i - 1).unwrap()).to_string());
            }
            (path.clone(), n)
        })
        .collect()
}

/// Only root may change a path whose every node is root's, written by no
/// one else, on a local filesystem mounted without `nosuid`, its
/// directories directories and its last node of the kind asked for; the
/// first node that fails is named, with why.
#[test]
fn only_root_s_nodes_written_by_no_one_else_pass() {
    let dir = |uid, mode| node(FileType::Directory, uid, mode);
    let file = |uid, mode| node(FileType::RegularFile, uid, mode);
    let file_kind = FileType::RegularFile;
    let good = [dir(0, 0o755), dir(0, 0o755), file(0, 0o755)];
    assert_eq!(verdict(&at(&good), file_kind), Ok(()));
    assert_eq!(
        verdict(&at(&[dir(0, 0o755), file(0, 0o4711)]), file_kind),
        Ok(())
    );
    assert_eq!(verdict(&at(&good[..2]), FileType::Directory), Ok(()));
    // The list fails closed: widening it is a change to this test too.
    assert_eq!(
        LOCAL,
        [
            0xEF53,      // ext2, ext3, ext4
            0x5846_5342, // xfs
            0x9123_683E, // btrfs
            0x7371_7368, // squashfs
            0xE0F5_E1E2, // erofs
            0x0102_1994, // tmpfs
            0x794C_7630, // overlayfs
            0x2FC1_2FC1, // zfs
        ]
    );
    for local in LOCAL {
        let mut on = good;
        on[1].fs_type = local.into();
        assert_eq!(verdict(&at(&on), file_kind), Ok(()), "{local:#x}");
    }
    let mut fused = good;
    fused[1].fs_type = 0x6573_5546;
    let mut nfs = good;
    nfs[2].fs_type = 0x6969;
    let mut nosuid = good;
    nosuid[1].nosuid = true;
    let mut nosuid_file = good;
    nosuid_file[2].nosuid = true;
    for (nodes, why) in [
        (vec![], "nothing was found"),
        (vec![dir(0, 0o755)], "/ is not a regular file"),
        (
            vec![dir(0, 0o755), dir(0, 0o755)],
            "/a is not a regular file",
        ),
        (
            vec![dir(0, 0o755), node(FileType::Symlink, 0, 0o777)],
            "/a is a symlink",
        ),
        (
            vec![dir(0, 0o755), node(FileType::Socket, 0, 0o755)],
            "/a is not a regular file",
        ),
        (
            vec![dir(0, 0o755), file(0, 0o755), file(0, 0o755)],
            "/a is not a directory",
        ),
        (
            vec![dir(0, 0o755), dir(1000, 0o755), file(0, 0o755)],
            "/a is owned by uid 1000",
        ),
        (
            vec![dir(0, 0o755), dir(0, 0o755), file(1000, 0o755)],
            "/a/b is owned by uid 1000",
        ),
        (
            vec![dir(0, 0o775), dir(0, 0o755), file(0, 0o755)],
            "/ is writable by its group or others (mode 775)",
        ),
        (
            vec![dir(0, 0o755), dir(0, 0o1777), file(0, 0o755)],
            "/a is writable by its group or others (mode 1777)",
        ),
        (
            vec![dir(0, 0o755), dir(0, 0o757), file(0, 0o755)],
            "/a is writable by its group or others (mode 757)",
        ),
        (
            vec![dir(0, 0o755), dir(0, 0o755), file(0, 0o775)],
            "/a/b is writable by its group or others (mode 775)",
        ),
        (
            fused.to_vec(),
            "/a is not on a local filesystem ranikust knows (f_type 0x65735546)",
        ),
        (
            nfs.to_vec(),
            "/a/b is not on a local filesystem ranikust knows (f_type 0x6969)",
        ),
        (
            nosuid.to_vec(),
            "/a is on a nosuid mount, as a user's mount is",
        ),
        (
            nosuid_file.to_vec(),
            "/a/b is on a nosuid mount, as a user's mount is",
        ),
    ] {
        assert_eq!(verdict(&at(&nodes), file_kind), Err(why.to_owned()));
    }
}

/// The walk opens no symlink's target and nothing below it, and refuses a
/// relative path, a `..` and a path that is not there, naming where it
/// stopped; a user's file is not root's.
#[test]
fn the_walk_follows_no_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    // The walk refuses a symlink above the file too, as in some TMPDIRs.
    let tmp = std::fs::canonicalize(tmp.path()).unwrap();
    let file = tmp.join("ranikust");
    std::fs::write(&file, "x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = tmp.join("link");
    std::os::unix::fs::symlink("/usr", &link).unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let file_kind = FileType::RegularFile;

    let nodes = walk(&file).unwrap();
    assert_eq!(nodes.len(), file.components().count());
    assert_eq!(nodes.last().unwrap().0, file);
    assert_eq!(nodes.last().unwrap().1.kind, FileType::RegularFile);
    if uid != 0 {
        // The first node that fails is named: `/tmp`, or the user's own.
        let why = only_root_changes(&file, file_kind).unwrap_err();
        let named = |p: &Path| why.starts_with(&format!("{} is ", p.display()));
        assert!(file.ancestors().any(named), "{why}");
    }

    let nodes = walk(&link).unwrap();
    assert_eq!(nodes.last().unwrap().1.kind, FileType::Symlink);
    assert!(only_root_changes(&link, file_kind).is_err());
    let below = link.join("bin");
    assert_eq!(walk(&below).unwrap_err(), (below, Errno::NOTDIR));

    let up = tmp.join("..").join(tmp.file_name().unwrap());
    assert_eq!(walk(&up).unwrap_err().1, Errno::INVAL);
    assert_eq!(walk(Path::new("usr/bin")).unwrap_err().1, Errno::INVAL);
    let missing = tmp.join("missing");
    assert_eq!(walk(&missing).unwrap_err(), (missing.clone(), Errno::NOENT));
    let why = only_root_changes(&missing, file_kind).unwrap_err();
    assert!(
        why.starts_with(&format!("{}: ", missing.display())),
        "{why}"
    );
    // As `/proc/self/exe` names a binary removed since.
    assert!(only_root_changes(&tmp.join("ranikust (deleted)"), file_kind).is_err());
}

/// The walk reads each node's filesystem: `/proc` is on `proc`, off the
/// local list (and usually `nosuid` too), so the rule refuses it, whatever
/// `/` is on.
#[test]
fn the_walk_reads_each_node_s_filesystem() {
    const PROC_SUPER_MAGIC: FsWord = 0x9FA0;
    let nodes = walk(Path::new("/proc/sys")).unwrap();
    assert_eq!(nodes[1].0, Path::new("/proc"));
    assert_eq!(nodes[1].1.fs_type, PROC_SUPER_MAGIC);
    assert!(!local(nodes[1].1.fs_type));
    let why = verdict(&nodes[1..2], FileType::Directory).unwrap_err();
    assert!(why.starts_with("/proc is "), "{why}");
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
        // The standard library shows no mount: the walk's own view of those.
        let mounts =
            walk(&path).is_ok_and(|nodes| nodes.iter().all(|(_, n)| !n.nosuid && local(n.fs_type)));
        let by_std = file
            && mounts
            && path.ancestors().all(|p| {
                std::fs::symlink_metadata(p).is_ok_and(|m| {
                    !m.file_type().is_symlink() && m.uid() == 0 && m.mode() & 0o022 == 0
                })
            });
        let checked = only_root_changes(&path, FileType::RegularFile);
        assert_eq!(checked.is_ok(), by_std, "{}: {checked:?}", path.display());
        passed += usize::from(by_std);
    }
    if passed == 0 {
        eprintln!("skipped: no system binary here is root's all the way up");
    }
}
