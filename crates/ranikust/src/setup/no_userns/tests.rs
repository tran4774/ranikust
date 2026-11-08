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

/// The name [`Stop::at`] matches is rnk-sandbox's own for `unshare`'s
/// operation: its report shows a step refused by its own operation with
/// the errno alone, so a rename on either side fails here.
#[test]
fn the_unshare_operation_is_rnk_sandbox_s() {
    let found = rnk_sandbox::UsernsCapabilities {
        unshare: StepResult::Failed(StepFailure {
            op: UNSHARE_USER,
            errno: Some(libc::EACCES),
            detail: None,
        }),
        mount_ns: StepResult::NotRun("no user namespace"),
        cut_short: None,
    };
    let shown = found.to_string();
    assert!(
        shown.starts_with(&format!("{UNSHARE_USER}: EACCES")),
        "{shown}"
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
    let unshare = |errno| failed(UNSHARE_USER, errno);
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
    // The profile itself only where this host offers one.
    let in_namespace = guidance(Some(shown), Stop::InNamespace);
    assert_eq!(
        in_namespace.contains(PROFILE),
        Here::read().offers(),
        "{in_namespace}"
    );
}

/// A stop AppArmor may cause gets the profile only where the switch reads
/// 1, ranikust runs unconfined (a confined program's own profile decides,
/// and ranikust's would not attach), in the host's user namespace and in no
/// container (a path checked there is not the one a profile on the host
/// attaches to); elsewhere `sudo` then a container, with no path weighed.
#[test]
fn only_an_unconfined_program_on_the_host_gets_the_profile() {
    let host = Here {
        restricted: true,
        unconfined: true,
        on_host: true,
    };
    let elsewhere = [
        Here {
            restricted: false,
            ..host
        },
        Here {
            unconfined: false,
            ..host
        },
        Here {
            on_host: false,
            ..host
        },
    ];
    let shown = Some(Path::new("/usr/local/bin/ranikust"));
    for stop in [Stop::InNamespace, Stop::Unshare(libc::EACCES)] {
        assert!(advise(None, stop, &host).contains(PROFILE), "{stop:?}");
        for here in elsewhere {
            assert_eq!(advise(shown, stop, &here), advice(None), "{here:?}");
            assert_eq!(advise(None, stop, &here), advice(None), "{here:?}");
        }
    }
    assert!(unconfined(b"unconfined\n"));
    assert!(unconfined(b"unconfined"));
    for label in [
        &b"ranikust (unconfined)\n"[..],
        b"docker-default (enforce)\n",
        b"unprivileged_userns (enforce)\n",
        b"unconfined//&:ns:unconfined\n",
        b"",
    ] {
        assert!(!unconfined(label), "{label:?}");
    }
}

/// The host's user namespace maps every id to itself, in one line; a
/// container runtime ranikust detects is not the host either.
#[test]
fn only_the_host_s_user_namespace_outside_a_container_is_the_host() {
    use crate::container::Runtime;
    let initial = "         0          0 4294967295\n";
    assert!(on_host(Some(initial), None));
    assert!(!on_host(Some(initial), Some(Runtime::Docker)));
    assert!(!on_host(Some(initial), Some(Runtime::Podman)));
    for map in [
        "         0       1000          1\n",
        "      1000       1000          1\n",
        "         0     100000      65536\n",
        "         0          0      65536\n",
        "         0          0 4294967295\n         0          0          1\n",
        "         1          0 4294967295\n",
        "",
    ] {
        assert!(!on_host(Some(map), None), "{map:?}");
    }
    assert!(!on_host(None, None));
}

/// The file mapped at an address is read from `/proc/self/maps`' fields;
/// an anonymous mapping, or an address no line holds, has none.
#[test]
fn the_file_behind_an_address_is_read_from_the_maps() {
    let maps = "\
5583a6c00000-5583a6c2e000 r--p 00000000 103:02 1311020                   /usr/bin/x
5583a6c2e000-5583a6d1c000 r-xp 0002e000 103:02 1311020                   /usr/bin/x
7f0e1c000000-7f0e1c021000 rw-p 00000000 00:00 0
7f0e1d400000-7f0e1d600000 r-xp 00000000 00:01 4242                       /memfd:ranikust (deleted)
";
    assert_eq!(backing(maps, 0x5583_a6c2_e000), Some((0x103, 2, 1_311_020)));
    assert_eq!(backing(maps, 0x5583_a6d1_bfff), Some((0x103, 2, 1_311_020)));
    assert_eq!(backing(maps, 0x7f0e_1d40_0010), Some((0, 1, 4242)));
    assert_eq!(backing(maps, 0x7f0e_1c00_0010), None);
    assert_eq!(backing(maps, 0x5583_a6d1_c000 + 0x10), None);
    assert_eq!(backing(maps, 0x10), None);
    assert_eq!(backing("", 0x10), None);
}

/// Only a file with the bytes of the one this code runs from holds
/// ranikust: the running test binary does; a file of its size with other
/// bytes, another program, a symlink to it and a missing file do not.
#[test]
fn only_the_running_program_s_bytes_hold_ranikust() {
    let me = std::fs::canonicalize("/proc/self/exe").unwrap();
    assert_eq!(holds_ranikust(&me), Ok(()));
    let tmp = tempfile::tempdir().unwrap();
    let sized = tmp.path().join("sized");
    let file = std::fs::File::create(&sized).unwrap();
    file.set_len(std::fs::metadata(&me).unwrap().len()).unwrap();
    drop(file);
    assert_eq!(holds_ranikust(&sized), Err("its bytes differ".to_owned()));
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&me, &link).unwrap();
    assert!(holds_ranikust(&link).is_err());
    assert!(holds_ranikust(&tmp.path().join("missing")).is_err());
    if let Ok(env) = std::fs::canonicalize("/usr/bin/env") {
        assert_eq!(holds_ranikust(&env), Err("its size differs".to_owned()));
    }
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
    // The list fails closed: widening it, or changing a value, is a change
    // to this test too.
    assert_eq!(
        LOCAL,
        [
            0xEF53,
            0x5846_5342,
            0x9123_683E,
            0x7371_7368,
            0xE0F5_E1E2,
            0x0102_1994,
            0x794C_7630,
            0x2FC1_2FC1,
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

/// Whether the advice for a stop AppArmor may cause binds the profile to
/// `path` as it is: every form that does not names no install first.
fn binds(path: Option<&Path>) -> bool {
    let advice = guidance(path, Stop::InNamespace);
    advice.contains(PROFILE) && !advice.contains(" first")
}

/// Red team (#81, invariant: the profile only for a path only root may
/// change): a symlink at the leaf or above it, a loop, a link of `/proc`,
/// `..` anywhere, a relative or empty path and a NUL are refused, with no
/// link followed, and the advice binds none of them.
#[test]
fn red_team_no_link_up_level_or_nul_gets_the_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(tmp.path()).unwrap();
    let file = FileType::RegularFile;
    // A target the rule passes on its own, where this host has one.
    let target = ["/usr/bin/env", "/usr/bin/sh", "/bin/sh"]
        .into_iter()
        .map(Path::new)
        .find(|p| only_root_changes(p, file).is_ok())
        .unwrap_or(Path::new("/usr/bin/env"));
    let leaf = tmp.join("leaf");
    std::os::unix::fs::symlink(target, &leaf).unwrap();
    let dir = tmp.join("dir");
    std::os::unix::fs::symlink(target.parent().unwrap(), &dir).unwrap();
    let (a, b, own) = (tmp.join("a"), tmp.join("b"), tmp.join("own"));
    std::os::unix::fs::symlink(&b, &a).unwrap();
    std::os::unix::fs::symlink(&a, &b).unwrap();
    std::os::unix::fs::symlink(&own, &own).unwrap();
    let below = target.strip_prefix("/").unwrap();
    let refused = [
        leaf.clone(),
        dir.join(target.file_name().unwrap()),
        a.clone(),
        a.join("x"),
        own.clone(),
        PathBuf::from("/proc/self/exe"),
        Path::new("/proc/self/root").join(below),
        Path::new("/proc/1/root").join(below),
        Path::new("/proc/self/cwd").join("x"),
        PathBuf::from("/dev/fd/0"),
    ];
    for path in &refused {
        assert!(only_root_changes(path, file).is_err(), "{}", path.display());
        assert!(!binds(Some(path)), "{}", path.display());
    }
    // The symlink is the node read, never its target.
    assert_eq!(
        walk(&leaf).unwrap().last().unwrap().1.kind,
        FileType::Symlink
    );
    for path in [
        "/usr/bin/../bin/env",
        "/../usr/bin/env",
        "/usr/bin/env/..",
        "usr/bin/env",
        "./usr/bin/env",
        "",
    ] {
        assert_eq!(walk(Path::new(path)).unwrap_err().1, Errno::INVAL, "{path}");
        assert!(!binds(Some(Path::new(path))), "{path}");
    }
    let nul = PathBuf::from(std::ffi::OsString::from("/usr/bin/env\0/x".to_owned()));
    assert!(only_root_changes(&nul, file).is_err());
    assert!(!binds(Some(&nul)));
    // A path ranikust could not read binds nothing either.
    assert!(!binds(None));
}

/// Red team (#81): a FIFO, a socket, a directory and a device are never
/// the binary a profile binds, and the walk does not block on a FIFO, which
/// it reads `O_PATH` without opening for reading.
#[test]
fn red_team_special_files_get_no_profile_and_a_fifo_does_not_block() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(tmp.path()).unwrap();
    let fifo = tmp.join("fifo");
    rustix::fs::mknodat(CWD, &fifo, FileType::Fifo, Mode::from_raw_mode(0o755), 0).unwrap();
    let socket = tmp.join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    // On a thread: a walk that opened the FIFO for reading would not return.
    let (tx, rx) = std::sync::mpsc::channel();
    let at = fifo.clone();
    std::thread::spawn(move || {
        let _ = tx.send(walk(&at).map(|nodes| nodes.last().unwrap().1.kind));
    });
    let kind = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the walk blocked on a FIFO");
    assert_eq!(kind, Ok(FileType::Fifo));
    for (path, kind) in [
        (fifo, FileType::Fifo),
        (socket, FileType::Socket),
        (tmp.clone(), FileType::Directory),
        (PathBuf::from("/dev/null"), FileType::CharacterDevice),
    ] {
        assert_eq!(walk(&path).unwrap().last().unwrap().1.kind, kind);
        assert!(
            only_root_changes(&path, FileType::RegularFile).is_err(),
            "{}",
            path.display()
        );
        assert!(!binds(Some(&path)), "{}", path.display());
    }
}

/// An access ACL as `system.posix_acl_access` holds it: the owner `rwx`,
/// the user 65534 `rwx`, the owning group `r-x`, the mask `mask`, others
/// `r-x`.
fn acl_with_mask(mask: u16) -> Vec<u8> {
    const ANY: u32 = u32::MAX;
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in [
        (0x01u16, 7u16, ANY),
        (0x02, 7, 65534),
        (0x04, 5, ANY),
        (0x10, mask, ANY),
        (0x20, 5, ANY),
    ] {
        acl.extend(tag.to_le_bytes());
        acl.extend(perm.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    acl
}

/// Red team (#81): an ACL that lets another user write a node shows as
/// group write in the mode the walk reads, which the rule refuses; a mask
/// without write takes the grant away, and the rule passes it.
#[test]
fn red_team_an_acl_granting_write_shows_in_the_group_bits() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = std::fs::canonicalize(tmp.path()).unwrap();
    let bin = tmp.join("ranikust");
    std::fs::write(&bin, "x").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let set = |path: &Path, mask| {
        rustix::fs::setxattr(
            path,
            "system.posix_acl_access",
            &acl_with_mask(mask),
            rustix::fs::XattrFlags::empty(),
        )
    };
    match set(&bin, 7) {
        Err(Errno::OPNOTSUPP) => {
            eprintln!("skipped: no POSIX ACLs on {}", tmp.display());
            return;
        }
        r => r.unwrap(),
    }
    let dir = tmp.join("dir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    set(&dir, 7).unwrap();
    // The walk's own reading of each node, as if root owned it, on a local
    // filesystem root mounted.
    let as_root = |path: &Path| Node {
        uid: 0,
        fs_type: EXT4,
        nosuid: false,
        ..walk(path).unwrap().last().unwrap().1
    };
    let root = node(FileType::Directory, 0, 0o755);
    let file = FileType::RegularFile;
    assert_eq!(
        verdict(&at(&[root, as_root(&bin)]), file),
        Err("/a is writable by its group or others (mode 775)".to_owned())
    );
    assert_eq!(
        verdict(&at(&[root, as_root(&dir)]), FileType::Directory),
        Err("/a is writable by its group or others (mode 775)".to_owned())
    );
    set(&bin, 5).unwrap();
    set(&dir, 5).unwrap();
    assert_eq!(verdict(&at(&[root, as_root(&bin)]), file), Ok(()));
    assert_eq!(
        verdict(&at(&[root, as_root(&dir)]), FileType::Directory),
        Ok(())
    );
}
