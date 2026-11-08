use std::fs;
use std::io::Read as _;
use std::os::fd::AsFd as _;

use super::*;

const EPERM: i32 = libc::EPERM;

/// Every fallback allowed: whether the steps work at all.
const ANY: crate::Fallbacks = crate::Fallbacks {
    proc: true,
    sysfs: true,
};

fn one_id(uid: u32) -> Plan {
    Plan::OneId {
        uid_map: format!("0 {uid} 1\n").into_bytes(),
        gid_map: format!("0 {uid} 1\n").into_bytes(),
    }
}

fn ranges() -> Plan {
    Plan::Ranges {
        uid_map: b"0 0 1\n1 1 65536\n".to_vec(),
        gid_map: b"0 0 1\n1 1 65536\n".to_vec(),
    }
}

fn failed(op: &'static str, errno: i32) -> StepResult {
    StepResult::Failed(StepFailure::errno(op, errno))
}

/// What a child that reached its end reports, with `errno` for the steps
/// named in `refused`.
fn all_steps(refused: &[u8]) -> Vec<(u8, i32)> {
    [
        CHDIR,
        UNSHARE,
        READY,
        CHOWN,
        NEWNS,
        SLAVE,
        TARGETS,
        NEWPID,
        PID1,
        PROC,
        PROC_RBIND,
        SYSFS,
        SYSFS_RBIND,
        MKNOD,
        DONE,
    ]
    .into_iter()
    .map(|op| (op, if refused.contains(&op) { EPERM } else { 0 }))
    .collect()
}

#[test]
fn a_map_is_mirrored_id_for_id() {
    assert_eq!(
        mirror("         0          0 4294967295\n").as_deref(),
        Some(&b"0 0 4294967295\n"[..])
    );
    // Rootless Podman's container: root is the user, then its range.
    assert_eq!(
        mirror("0 1000 1\n1 100000 65536\n").as_deref(),
        Some(&b"0 0 1\n1 1 65536\n"[..])
    );
    assert_eq!(mirror(""), None);
    assert_eq!(mirror("0 0\n"), None);
    assert_eq!(mirror("0 0 1 1\n"), None);
    assert_eq!(mirror("0 x 1\n"), None);
}

#[test]
fn the_other_id_is_the_smallest_mapped_one_but_0() {
    assert_eq!(other_id(b"0 0 4294967295\n"), Some(1));
    assert_eq!(other_id(b"0 0 1\n1 1 65536\n"), Some(1));
    assert_eq!(other_id(b"0 0 1\n5 5 3\n"), Some(5));
    assert_eq!(other_id(b"0 1000 1\n"), None);
    assert_eq!(other_id(b""), None);
    assert_eq!(ranges().other_ids(), Some((1, 1)));
    assert_eq!(one_id(1000).other_ids(), None);
}

#[test]
fn root_tries_its_ranges_then_one_id_and_a_user_newuidmap_then_one_id() {
    let kinds = |plans: Vec<std::result::Result<Plan, (IdMap, StepResult)>>| {
        plans
            .into_iter()
            .map(|p| match p {
                Ok(p) => (p.kind(), None),
                Err((k, r)) => (k, Some(r)),
            })
            .collect::<Vec<_>>()
    };
    let root = kinds(plans(0, 0, BuildIds::Own(None)));
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].0, IdMap::Ranges);
    assert_eq!(root[1], (IdMap::OneId, None));

    let user = kinds(plans(1000, 1001, BuildIds::Own(None)));
    assert_eq!(
        user,
        [
            (
                IdMap::Newuidmap,
                Some(StepResult::NotRun("no subordinate ids for this user"))
            ),
            (IdMap::OneId, None),
        ]
    );
    let sub = SubordinateIds {
        uids: IdRange {
            start: 100_000,
            count: 65536,
        },
        gids: IdRange {
            start: 200_000,
            count: 65536,
        },
    };
    let user = kinds(plans(1000, 1001, BuildIds::Own(Some(sub))));
    assert_eq!(user[0].0, IdMap::Newuidmap);
    let tools = tool("newuidmap").is_some() && tool("newgidmap").is_some();
    assert_eq!(user[0].1.is_none(), tools);
    match plans(1000, 1001, BuildIds::Own(None)).pop() {
        Some(Ok(Plan::OneId { uid_map, gid_map })) => {
            assert_eq!(uid_map, b"0 1000 1\n");
            assert_eq!(gid_map, b"0 1001 1\n");
        }
        other => panic!("{other:?}"),
    }
}

pub(super) fn shifted_ids() -> SubordinateIds {
    SubordinateIds {
        uids: IdRange {
            start: 300_000,
            count: 65536,
        },
        gids: IdRange {
            start: 400_000,
            count: 1000,
        },
    }
}

#[test]
fn a_shifted_build_tries_its_range_only_and_never_one_id() {
    let mut root = plans(0, 0, BuildIds::Shifted(shifted_ids()));
    assert_eq!(root.len(), 1, "{root:?}");
    match root.pop() {
        Some(Ok(p @ Plan::Shifted { .. })) => {
            assert_eq!(p.kind(), IdMap::Shifted);
            assert!(p.by_parent());
            assert!(p.assumes_root());
            assert_eq!(p.own_map(), None);
            assert_eq!(p.other_ids(), Some((1, 1)));
            let Plan::Shifted { uid_map, gid_map } = p else {
                unreachable!()
            };
            assert_eq!(uid_map, b"0 300000 65536\n");
            assert_eq!(gid_map, b"0 400000 1000\n");
        }
        other => panic!("{other:?}"),
    }
    // A user cannot write it, and one id would be its own: nothing to try.
    let user = plans(1000, 1001, BuildIds::Shifted(shifted_ids()));
    assert!(
        matches!(
            user.as_slice(),
            [Err((IdMap::Shifted, StepResult::NotRun(_)))]
        ),
        "{user:?}"
    );
    // A range of one id gives no other id to `chown` to.
    let one = SubordinateIds {
        uids: IdRange {
            start: 300_000,
            count: 1,
        },
        ..shifted_ids()
    };
    match plans(0, 0, BuildIds::Shifted(one)).pop() {
        Some(Ok(p)) => assert_eq!(p.other_ids(), None),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        BuildIds::Shifted(shifted_ids()).shifted_root(),
        Some((300_000, 400_000))
    );
    assert_eq!(BuildIds::Own(None).shifted_root(), None);
    assert!(!ranges().assumes_root() && !one_id(0).assumes_root());
}

#[test]
fn a_refused_unshare_leaves_every_later_step_not_run() {
    let a = assemble(&one_id(1000), &[(CHDIR, 0), (UNSHARE, EPERM)], None, None);
    assert_eq!(a.unshare, failed("unshare(CLONE_NEWUSER)", EPERM));
    assert_eq!(a.map, StepResult::NotRun("no user namespace"));
    assert_eq!(a.chown, StepResult::NotRun(NO_MAP));
    assert_eq!(a.mount_ns, StepResult::NotRun(NO_MAP));
    assert_eq!(a.proc, MountResult::NotRun(NO_MAP));
    assert_eq!(a.mknod, StepResult::NotRun(NO_MAP));

    let a = assemble(&one_id(1000), &[(CHDIR, libc::EACCES)], None, None);
    assert_eq!(
        a.unshare,
        failed("fchdir to the probe's directory", libc::EACCES)
    );
}

#[test]
fn a_map_fails_where_it_was_written() {
    // One id, written by the child: Ubuntu 24.04's restriction (M3).
    let records = [(CHDIR, 0), (UNSHARE, 0), (UID_MAP, EPERM)];
    let a = assemble(&one_id(1000), &records, None, None);
    assert_eq!(a.unshare, StepResult::Ok);
    assert_eq!(a.map, failed("write uid_map", EPERM));
    assert_eq!(a.chown, StepResult::NotRun(NO_MAP));

    let records = [(CHDIR, 0), (UNSHARE, 0), (UID_MAP, 0), (SETGROUPS, 0)];
    let a = assemble(&one_id(1000), &records, None, None);
    assert_eq!(a.map, StepResult::NotRun(STOPPED));

    // Written here: the child's records do not say.
    let records = [(CHDIR, 0), (UNSHARE, 0), (READY, 0)];
    let refused = StepFailure::other(
        "newuidmap",
        "exit status: 1: newuidmap: write to uid_map failed",
    );
    let a = assemble(&ranges(), &records, Some(Err(refused.clone())), None);
    assert_eq!(a.map, StepResult::Failed(refused));
    let a = assemble(&ranges(), &records, None, None);
    assert_eq!(a.map, StepResult::NotRun(STOPPED));

    let records = [(CHDIR, 0), (UNSHARE, 0), (DUMPABLE, EPERM)];
    let a = assemble(&one_id(1000), &records, None, None);
    assert_eq!(a.map, failed("prctl(PR_SET_DUMPABLE)", EPERM));
}

#[test]
fn the_own_namespace_allows_setgroups_and_maps_tty_from_its_files() {
    let own = |setgroups, gid_map| OwnNamespace::parse(setgroups, gid_map);
    let initial = OwnNamespace {
        setgroups: true,
        tty_gid: true,
    };
    assert_eq!(
        own(Some("allow\n"), Some("         0          0 4294967295\n")),
        initial
    );
    // One id written by this process: `setgroups` denied, gid 5 unmapped.
    assert_eq!(
        own(Some("deny\n"), Some("0 1000 1\n")),
        OwnNamespace {
            setgroups: false,
            tty_gid: false
        }
    );
    // `newgidmap`'s ranges: 5 is the fifth subordinate id.
    assert!(own(Some("allow\n"), Some("0 1000 1\n1 100000 65536\n")).tty_gid);
    assert!(own(None, Some("5 100 1\n")).tty_gid);
    assert!(!own(None, Some("0 100 5\n6 200 10\n")).tty_gid);
    // Unreadable or unexpected: the steps run, and fail if they must.
    assert_eq!(own(None, None), initial);
    assert_eq!(own(Some("other"), Some("0 x 1\n")), initial);
}

#[test]
fn setgroups_is_skipped_only_where_denied_in_a_one_id_map() {
    let calls = |setgroups, gid_map| OwnNamespace::parse(Some(setgroups), gid_map).setgroups;
    assert!(!calls("deny\n", Some("0 1000 1\n")));
    assert!(!calls("deny", Some("\n         0       1000          1\n")));
    // Denied with several ids: called, so the `RUN` fails at `Step::Groups`
    // instead of keeping ranikust's supplementary groups.
    assert!(calls("deny\n", Some("0 1000 1\n1 100000 65536\n")));
    assert!(calls("deny\n", Some("0 1000 2\n")));
    assert!(calls("deny\n", Some("0 1000 1\n5 2000 1\n")));
    assert!(calls("allow\n", Some("0 1000 1\n")));
    assert!(calls("denied\n", Some("0 1000 1\n")));
    // A file that cannot be read or parsed never leads to a skip.
    assert!(OwnNamespace::parse(None, Some("0 1000 1\n")).setgroups);
    assert!(calls("deny\n", None));
    assert!(calls("deny\n", Some("0 x 1\n")));
    assert!(calls("deny\n", Some("")));
}

#[test]
fn each_kernel_filesystem_is_new_else_an_rbind() {
    let a = assemble(&ranges(), &all_steps(&[MKNOD]), Some(Ok(())), None);
    assert_eq!(a.map, StepResult::Ok);
    assert_eq!(a.chown, StepResult::Ok);
    assert_eq!(a.mount_ns, StepResult::Ok);
    assert_eq!(a.proc, MountResult::Fresh);
    assert_eq!(a.sysfs, MountResult::Fresh);
    assert_eq!(a.mknod, failed("mknod", EPERM));

    // Rootless Podman: `/proc` is masked, the network namespace not ours.
    let a = assemble(
        &ranges(),
        &all_steps(&[PROC, SYSFS, MKNOD]),
        Some(Ok(())),
        None,
    );
    assert_eq!(
        a.proc,
        MountResult::Rbind {
            fresh: StepFailure::errno("mount proc", EPERM)
        }
    );
    assert_eq!(
        a.sysfs,
        MountResult::Rbind {
            fresh: StepFailure::errno("mount sysfs", EPERM)
        }
    );

    let a = assemble(
        &ranges(),
        &all_steps(&[NEWPID, PROC_RBIND]),
        Some(Ok(())),
        None,
    );
    assert_eq!(
        a.proc,
        MountResult::Failed {
            fresh: StepFailure::errno("unshare(CLONE_NEWPID)", EPERM),
            rbind: Some(StepFailure::errno("rbind /proc", EPERM)),
        }
    );

    // PID 1 died before it reported; the rbind still counts.
    let records: Vec<_> = all_steps(&[]).into_iter().filter(|r| r.0 != PROC).collect();
    let a = assemble(&ranges(), &records, Some(Ok(())), None);
    assert_eq!(
        a.proc,
        MountResult::Rbind {
            fresh: StepFailure::other("mount proc", "not reported")
        }
    );

    let records = [(CHDIR, 0), (UNSHARE, 0), (READY, 0), (CHOWN, 0), (NEWNS, 0)];
    let a = assemble(&ranges(), &records, Some(Ok(())), None);
    assert_eq!(a.proc, MountResult::NotRun(STOPPED));

    let a = assemble(&ranges(), &all_steps(&[SLAVE]), Some(Ok(())), None);
    assert_eq!(
        a.sysfs,
        MountResult::Failed {
            fresh: StepFailure::errno("make / a recursive slave mount", EPERM),
            rbind: None
        }
    );
}

#[test]
fn a_ranged_map_on_ubuntu_24_04_stops_at_chown_then_the_mount_namespace() {
    let records = [
        (CHDIR, 0),
        (UNSHARE, 0),
        (READY, 0),
        (CHOWN, EPERM),
        (NEWNS, EPERM),
        (MKNOD, EPERM),
        (DONE, 0),
    ];
    let plan = ranges();
    let a = assemble(&plan, &records, Some(Ok(())), None);
    assert_eq!(a.chown, failed("chown", EPERM));
    assert_eq!(a.mount_ns, failed("unshare(CLONE_NEWNS)", EPERM));
    assert_eq!(a.proc, MountResult::NotRun("no mount namespace"));
    let report = report_of(vec![(IdMap::Ranges, StepResult::Ok)], Some((plan, a)));
    assert_eq!(report.stop(ANY), Some("chown"));
    assert_eq!(report.map(), Some(IdMap::Ranges));
}

#[test]
fn the_report_names_the_first_step_a_run_needs_that_failed() {
    let passing = |plan: Plan, refused: &[u8]| {
        let a = assemble(&plan, &all_steps(refused), Some(Ok(())), None);
        report_of(vec![(plan.kind(), StepResult::Ok)], Some((plan, a)))
    };
    let r = passing(ranges(), &[PROC, SYSFS, MKNOD]);
    assert_eq!(r.stop(ANY), None);
    // The rbind of /proc passes only at the operator's request; /sys's always.
    assert_eq!(r.stop(crate::Fallbacks::default()), Some("proc"));
    assert_eq!(r.stop(crate::Fallbacks::default().allow_proc_rbind()), None);
    let sysfs_only = passing(ranges(), &[SYSFS]);
    assert_eq!(sysfs_only.stop(crate::Fallbacks::default()), None);
    assert_eq!(
        r.to_string(),
        "unshare(CLONE_NEWUSER): ok; map: multi-range; chown: ok; unshare(CLONE_NEWNS): ok; \
         proc: rbind of /proc (mount proc: EPERM); sysfs: rbind of /sys (mount sysfs: EPERM); \
         mknod: EPERM"
    );
    // The rbind of /proc worked, yet only the operator allows it.
    let sysfs = crate::Fallbacks {
        proc: false,
        sysfs: true,
    };
    assert_eq!(r.fallbacks(), sysfs);
    assert_eq!(passing(ranges(), &[SYSFS]).fallbacks(), sysfs);
    assert_eq!(
        passing(ranges(), &[PROC]).fallbacks(),
        crate::Fallbacks::default()
    );
    // Nothing worked: nothing to fall back to.
    assert_eq!(
        passing(ranges(), &[PROC, PROC_RBIND, SYSFS, SYSFS_RBIND]).fallbacks(),
        crate::Fallbacks::default()
    );
    assert_eq!(
        passing(ranges(), &[]).fallbacks(),
        crate::Fallbacks::default()
    );
    assert_eq!(passing(ranges(), &[CHOWN]).stop(ANY), Some("chown"));
    assert_eq!(
        passing(ranges(), &[NEWNS]).stop(ANY),
        Some("mount namespace")
    );
    assert_eq!(
        passing(ranges(), &[PROC, PROC_RBIND]).stop(ANY),
        Some("proc")
    );
    assert_eq!(
        passing(ranges(), &[SYSFS, SYSFS_RBIND]).stop(ANY),
        Some("sysfs")
    );

    // One id: chown is not tried, and not needed.
    let plan = one_id(1000);
    let records: Vec<_> = [CHDIR, UNSHARE, UID_MAP, SETGROUPS, GID_MAP]
        .into_iter()
        .map(|op| (op, 0))
        .chain(all_steps(&[]).into_iter().skip(4))
        .collect();
    let a = assemble(&plan, &records, None, None);
    let r = report_of(
        vec![
            (
                IdMap::Newuidmap,
                StepResult::NotRun("no subordinate ids for this user"),
            ),
            (IdMap::OneId, StepResult::Ok),
        ],
        Some((plan, a)),
    );
    assert_eq!(r.chown, StepResult::NotRun(ONLY_ID_0));
    assert_eq!(r.stop(ANY), None);
    assert!(
        r.to_string().starts_with(
            "unshare(CLONE_NEWUSER): ok; map: one uid (newuidmap: not run (no subordinate \
             ids for this user)); chown: not run (only id 0 is mapped);"
        ),
        "{r}"
    );

    // No map worked.
    let plan = one_id(1000);
    let a = assemble(
        &plan,
        &[(CHDIR, 0), (UNSHARE, 0), (UID_MAP, EPERM)],
        None,
        Some("the probe's child was killed after 10 s".into()),
    );
    let r = report_of(vec![(IdMap::OneId, a.map.clone())], Some((plan, a)));
    assert_eq!(r.map(), None);
    assert_eq!(r.stop(ANY), Some("map"));
    assert!(
        r.in_namespace(|_| Ok(String::new()), Path::new("/"))
            .is_none()
    );
    assert!(
        r.to_string().starts_with(
            "unshare(CLONE_NEWUSER): ok; map: none (one uid: write uid_map: EPERM); chown: \
             not run (no id map);"
        ),
        "{r}"
    );
    assert!(
        r.to_string()
            .ends_with("; the probe's child was killed after 10 s"),
        "{r}"
    );
}

#[test]
fn a_failure_names_its_errno() {
    assert_eq!(
        StepFailure::errno("mount proc", EPERM).to_string(),
        "mount proc: EPERM"
    );
    assert_eq!(StepFailure::errno("x", 4095).to_string(), "x: errno 4095");
    assert_eq!(
        StepFailure::other("newuidmap", "did not finish").to_string(),
        "newuidmap: did not finish"
    );
    assert_eq!(errno_name(libc::EOPNOTSUPP), Some("EOPNOTSUPP"));
}

#[test]
fn records_are_read_whole_across_reads() {
    let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
    let mut bytes = Vec::new();
    for (op, e) in [(UNSHARE, 0), (READY, 0), (CHOWN, EPERM)] {
        bytes.push(op);
        bytes.extend_from_slice(&e.to_le_bytes());
    }
    bytes.extend(result_frame(1, "overlay: EINVAL"));
    let writer = std::thread::spawn(move || {
        // Split inside a record and inside the result's text.
        for part in [&bytes[..3], &bytes[3..17], &bytes[17..]] {
            rustix::io::write(&w, part).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let mut seen = Vec::new();
    let got = read_records(&r, Instant::now() + Duration::from_secs(10), |op| {
        seen.push(op);
    })
    .unwrap();
    writer.join().unwrap();
    assert!(!got.timed_out);
    assert_eq!(got.list, [(UNSHARE, 0), (READY, 0), (CHOWN, EPERM)]);
    assert_eq!(seen, [UNSHARE, READY, CHOWN]);
    assert_eq!(got.payload, Some((1, b"overlay: EINVAL".to_vec())));

    // A child that never closes its end is cut off at the deadline.
    let (r, _w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
    let got = read_records(&r, Instant::now() + Duration::from_millis(50), |_| {}).unwrap();
    assert!(got.timed_out);
}

#[test]
fn a_result_is_cut_at_its_limit_on_a_character_boundary() {
    let text = format!("{}é", "a".repeat(RESULT_LIMIT - 1));
    let frame = result_frame(2, &text);
    assert_eq!(frame[0], RESULT);
    let value = i32::from_le_bytes([frame[1], frame[2], frame[3], frame[4]]);
    assert_eq!(value >> 16, 2);
    assert_eq!(usize::try_from(value & 0xffff).unwrap(), RESULT_LIMIT - 1);
    assert_eq!(frame.len(), 5 + RESULT_LIMIT - 1);
}

/// A closure that panicked did not finish: not its own refusal; nor did
/// one whose status is unknown.
#[test]
fn a_closure_that_panicked_did_not_finish() {
    let outcome = |status, text: &str| match closure_outcome(status, text.to_owned()) {
        Ok(r) => Ok(r),
        Err(Error::ProbeClosure(t)) => Err(t),
        Err(e) => panic!("{e:?}"),
    };
    assert_eq!(outcome(CLOSURE_OK, "api"), Ok(Ok("api".to_owned())));
    assert_eq!(
        outcome(CLOSURE_REFUSED, "EINVAL"),
        Ok(Err("EINVAL".to_owned()))
    );
    assert_eq!(
        outcome(CLOSURE_PANICKED, "panicked"),
        Err("panicked".to_owned())
    );
    assert_eq!(
        outcome(9, "api"),
        Err("unknown closure status 9: api".to_owned())
    );
}

/// With a map but other threads, the closure is not run, and that is not
/// its own refusal: refused before any fork.
#[test]
fn a_closure_is_not_run_beside_other_threads() {
    let plan = ranges();
    let a = assemble(&plan, &all_steps(&[]), Some(Ok(())), None);
    let r = report_of(vec![(IdMap::Ranges, StepResult::Ok)], Some((plan, a)));
    assert_eq!(r.map(), Some(IdMap::Ranges));
    // A thread that lives until the check is done, whatever the harness
    // does with threads: no child is forked.
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let spare = std::thread::spawn(move || {
        // `Err` once `done` drops: the signal to end.
        let _ = wait.recv();
    });
    let ran = r.in_namespace(|_| Ok("ran".to_owned()), Path::new("/"));
    drop(done);
    spare.join().unwrap();
    match ran {
        Some(Err(Error::NotSingleThreaded { threads })) => assert!(threads > 1),
        other => panic!("{other:?}"),
    }
}

/// A child that ends is reaped; one still there after the grace is killed,
/// then reaped; one still there after the kill's grace is reported, and
/// left (here never sent the kill, as one blocked in the kernel would be
/// untouched by it).
#[test]
fn a_child_is_reaped_within_its_grace_else_reported_left() {
    let fork = |ends: bool| {
        let parent = rustix::process::getpid();
        // SAFETY: the child makes system calls only, and ends with `_exit`
        // or `SIGKILL`.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            if !ends {
                // Killed with this test's thread, should the test fail
                // before it kills the child; a parent gone first is seen.
                if rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).is_err()
                    || rustix::process::getppid() != Some(parent)
                {
                    // SAFETY: `_exit` ends this process at once.
                    unsafe { libc::_exit(4) }
                }
                loop {
                    // SAFETY: a plain system call.
                    unsafe { libc::pause() };
                }
            }
            // SAFETY: `_exit` ends this process at once.
            unsafe { libc::_exit(3) }
        }
        Pid::from_raw(pid).unwrap()
    };
    // Graces long enough on a loaded machine where the child is to be
    // reaped (`reap` returns once it is); short where it is not.
    let ended = reap(fork(true), false, Duration::from_secs(10)).unwrap();
    assert_eq!(ended.unwrap().exit_status(), Some(3));
    let pid = fork(false);
    // Only `reap` sends the kill: a slow machine only waits longer for it.
    let killed = match reap(pid, false, Duration::from_millis(100)).unwrap() {
        Some(st) => st,
        None => reap(pid, true, Duration::from_secs(10)).unwrap().unwrap(),
    };
    let gave_up = "the child exited 4: its death signal refused, or its parent gone";
    assert_ne!(killed.exit_status(), Some(4), "{gave_up}");
    assert_eq!(killed.terminating_signal(), Some(libc::SIGKILL));
    let pid = fork(false);
    let left = reap(pid, true, Duration::from_millis(100));
    // Only a child not reaped yet: a reaped one's number may be another
    // process's.
    let after = matches!(left, Ok(None)).then(|| {
        // A refused kill leaves the second `reap`'s result `None`.
        let _ = rustix::process::kill_process(pid, Signal::KILL);
        reap(pid, true, Duration::from_secs(10))
    });
    let left = left.unwrap();
    assert_ne!(left.and_then(|st| st.exit_status()), Some(4), "{gave_up}");
    assert!(
        left.is_none(),
        "reaped before the kill: {:?}",
        left.map(|st| (st.exit_status(), st.terminating_signal()))
    );
    assert!(after.unwrap().unwrap().is_some());
}

/// [`probe_in`] in the directory at `dir`, opened as a caller that did
/// not make it would.
fn probe_at(dir: &Path, subordinate: Option<SubordinateIds>) -> Result<UsernsReport> {
    probe_in(&RootPath::open(dir)?, BuildIds::Own(subordinate))
}

/// The records the witnesses in a closure's child send: the panic hook's,
/// the subscriber's.
const HOOK_RAN: u8 = 100;
const LOGGED: u8 = 101;

/// A subscriber that sends [`LOGGED`] for each event.
struct Witness(RawFd);

impl tracing::Subscriber for Witness {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        // SAFETY: the report pipe's write end, open in the child.
        send(unsafe { BorrowedFd::borrow_raw(self.0) }, LOGGED, Ok(()));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// The closure's child inherits ranikust's panic hook, which writes the
/// metrics report, and its subscriber, which writes the build's log: the
/// closure runs with neither. Here the inherited ones are witnesses that
/// send a record; the closure logs, then panics. Run in a process of its
/// own ([`in_own_process`]): the child's `set_hook` and `Dispatch::new`
/// take process-wide locks, which another test's thread may hold at the
/// fork.
#[test]
fn a_closures_child_writes_through_no_hook_or_subscriber_of_its_parent() {
    if std::env::var_os(OWN_PROCESS).is_none() {
        in_own_process(
            "userns::tests::a_closures_child_writes_through_no_hook_or_subscriber_of_its_parent",
        );
        return;
    }
    let run = supervise(
        &ranges(),
        false,
        None,
        -1,
        Duration::from_secs(20),
        |args| {
            let report = args.report;
            std::panic::set_hook(Box::new(move |_| {
                // SAFETY: the report pipe's write end, open in the child.
                send(unsafe { BorrowedFd::borrow_raw(report) }, HOOK_RAN, Ok(()));
            }));
            tracing::subscriber::with_default(Witness(report), || {
                // SAFETY: as above.
                let fd = unsafe { BorrowedFd::borrow_raw(report) };
                finish_closure(fd, || {
                    tracing::info!("from the closure");
                    panic!("in the closure")
                });
            });
        },
    )
    .unwrap();
    assert!(!run.records.timed_out);
    assert_eq!(run.records.list, []);
    assert_eq!(
        run.records.payload,
        Some((CLOSURE_PANICKED, b"panicked".to_vec()))
    );
}

/// Set in the copy of this test binary [`in_own_process`] runs.
const OWN_PROCESS: &str = "RNK_TEST_USERNS_OWN_PROCESS";

/// Runs the test `name` (its full path) again in a copy of this test
/// binary, with one test thread and [`OWN_PROCESS`] set, and fails with the
/// copy's output unless it ran that one test and it passed.
fn in_own_process(name: &str) {
    let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", name, "--test-threads=1"])
        .env(OWN_PROCESS, "1")
        .output()
        .expect("run the test binary again");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{name} in its own process: {:?}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A tool whose wait failed is killed, unless the failure is `ECHILD`: it
/// was reaped elsewhere, and its number may be another process's.
#[test]
fn a_tool_reaped_elsewhere_is_not_killed() {
    assert!(!kill_after_wait_error(&io::Error::from_raw_os_error(
        libc::ECHILD
    )));
    assert!(kill_after_wait_error(&io::Error::from_raw_os_error(
        libc::EINTR
    )));
}

/// A probe's child holds stdio and the descriptors it uses only: one this
/// process opened, without close-on-exec as an inherited one may be, is
/// closed in it.
#[test]
fn a_probes_child_holds_only_stdio_and_its_own_descriptors() {
    let file = rustix::fs::open(c"/dev/null", OFlags::RDONLY, Mode::empty()).unwrap();
    let extra = file.as_raw_fd();
    let run = supervise(
        &ranges(),
        false,
        None,
        -1,
        Duration::from_secs(20),
        |args| {
            // SAFETY: the report pipe's write end, open in the child.
            let report = unsafe { BorrowedFd::borrow_raw(args.report) };
            for (op, fd) in [(110, extra), (111, args.go), (112, 2)] {
                // A bare number: a closed descriptor is no `BorrowedFd`.
                // SAFETY: `F_GETFD` only reads the descriptor's flags.
                let r = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                send(report, op, if r < 0 { Err(last_errno()) } else { Ok(()) });
            }
        },
    )
    .unwrap();
    assert_eq!(run.records.list, [(110, libc::EBADF), (111, 0), (112, 0)]);
}

/// A failure part-way through making the probe's files leaves the
/// directory as it was given: here a `sys` already there.
#[test]
fn a_failure_making_the_probes_files_removes_what_it_made() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("sys"), b"").unwrap();
    assert!(probe_at(dir.path(), None).is_err());
    let left: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["sys"]);
}

/// A map this process fails to write ends the waiting child at once, not at
/// the deadline: here the child stays in this user namespace, whose map is
/// already written, so the write is refused without any privilege.
#[test]
fn a_map_the_parent_cannot_write_ends_the_waiting_child() {
    let started = Instant::now();
    let run = supervise(&ranges(), false, None, -1, Duration::from_secs(5), |args| {
        // SAFETY: the report pipe's write end, open in this child.
        let report = unsafe { BorrowedFd::borrow_raw(args.report) };
        map_in_child(args, report);
    })
    .unwrap();
    assert!(!run.records.timed_out, "the child waited for the deadline");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        matches!(run.parent_map, Some(Err(ref e)) if e.op == "write uid_map"),
        "{:?}",
        run.parent_map
    );
    assert_eq!(run.cut_short, None);
}

/// Writes `measured: <line>` to stderr past libtest's capture, for the T2
/// suite's log.
pub(super) fn measured(line: &str) {
    use std::io::Write as _;
    // A lost line loses only what the assertions already checked.
    let _ = writeln!(std::io::stderr().lock(), "measured: {line}");
}

/// Docker's defaults: a seccomp filter and no `CAP_SYS_ADMIN`, where
/// `unshare(CLONE_NEWUSER)` is refused.
fn seccomp_without_sys_admin() -> bool {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .map(str::trim)
            .unwrap()
            .to_owned()
    };
    const CAP_SYS_ADMIN: u32 = 21;
    let effective = u64::from_str_radix(&field("CapEff:"), 16).unwrap();
    field("Seccomp:") == "2" && effective & (1 << CAP_SYS_ADMIN) == 0
}

fn is_eperm(r: &StepResult) -> bool {
    matches!(
        r,
        StepResult::Failed(StepFailure {
            errno: Some(EPERM),
            ..
        })
    )
}

/// As root: on a host every step a `RUN` needs passes with this process's
/// own ranges; under Docker's defaults `unshare` is refused. Nothing is
/// left in the probe's directory, and a closure runs as root of the
/// namespace.
#[test]
#[ignore = "T2c: default: unshare(CLONE_NEWUSER) and mounts as root; Docker's defaults refuse it"]
fn the_user_namespace_steps_as_root_pass_on_a_host_and_stop_at_unshare_in_docker() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = tempfile::tempdir().unwrap();
    let report = probe_at(dir.path(), None).unwrap();
    measured(&format!("user namespace as root: {report}"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    assert_eq!(report.cut_short, None);
    if seccomp_without_sys_admin() {
        assert_eq!(report.stop(ANY), Some("unshare"), "{report}");
        assert!(is_eperm(&report.unshare), "{report}");
        assert_eq!(report.map(), None);
        return;
    }
    assert_eq!(report.stop(ANY), None, "{report}");
    assert_eq!(report.map(), Some(IdMap::Ranges));
    assert_eq!(report.chown, StepResult::Ok);
    assert!(is_eperm(&report.mknod), "{report}");
    let plan = report.plan.as_ref().unwrap();
    let euid = |_: &Path| Ok(rustix::process::geteuid().as_raw().to_string());
    assert_eq!(
        run_closure(plan, euid, dir.path()).unwrap(),
        Ok("0".to_owned())
    );
    let refused = |_: &Path| Err("refused".to_owned());
    assert_eq!(
        run_closure(plan, refused, dir.path()).unwrap(),
        Err("refused".to_owned())
    );
}

/// T2: as root with a shifted range (`--userns`, ADR-0104): every step a
/// `RUN` needs passes, the probe's directory and its mount points are
/// given to the range's first ids (the file the `chown` step gave to the
/// namespace's id 1 is the range's second ids') and nothing in it is
/// deleted from outside the namespace, and a closure runs as root of the
/// namespace with no supplementary group, a file it makes owned on the
/// host by the range's first ids.
#[test]
#[ignore = "T2: real root probes a shifted range"]
fn the_user_namespace_steps_of_a_shifted_range_pass_as_its_first_ids() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let sub = shifted_ids();
    let tmp = tempfile::tempdir().unwrap();
    // Searchable by the range's first uid, as a state directory's parents.
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o711)).unwrap();
    let dir = tmp.path().join("probe");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let report = probe_in(&RootPath::open(&dir).unwrap(), BuildIds::Shifted(sub)).unwrap();
    measured(&format!("user namespace with a shifted range: {report}"));
    let mut left: Vec<(String, u32, u32)> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            (e.file_name().into_string().unwrap(), m.uid(), m.gid())
        })
        .collect();
    left.sort();
    let (u, g) = (sub.uids.start, sub.gids.start);
    assert_eq!(
        left,
        [
            ("chown".into(), u + 1, g + 1),
            ("proc".into(), u, g),
            ("sys".into(), u, g)
        ]
    );
    let meta = fs::metadata(&dir).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (sub.uids.start, sub.gids.start));
    assert_eq!(report.stop(ANY), None, "{report}");
    assert_eq!(report.map(), Some(IdMap::Shifted));
    assert_eq!(report.chown, StepResult::Ok);
    assert!(is_eperm(&report.mknod), "{report}");
    let plan = report.plan.as_ref().unwrap();
    let inside = |p: &Path| {
        let groups = fs::read_to_string("/proc/self/status")
            .map_err(|e| e.to_string())?
            .lines()
            .find_map(|l| l.strip_prefix("Groups:").map(|g| g.trim().to_owned()))
            .ok_or("no Groups line")?;
        fs::write(p.join("made"), b"").map_err(|e| e.to_string())?;
        let euid = rustix::process::geteuid().as_raw();
        let egid = rustix::process::getegid().as_raw();
        Ok(format!("{euid} {egid} groups=[{groups}]"))
    };
    assert_eq!(
        run_closure(plan, inside, &dir).unwrap(),
        Ok("0 0 groups=[]".to_owned())
    );
    let made = fs::metadata(dir.join("made")).unwrap();
    assert_eq!((made.uid(), made.gid()), (sub.uids.start, sub.gids.start));
}

/// The first range of `file` (`/etc/subuid` or `/etc/subgid`) for `name`
/// or `id`.
pub(super) fn subordinate_range(file: &str, name: &str, id: u32) -> Option<IdRange> {
    let text = fs::read_to_string(file).ok()?;
    text.lines().find_map(|l| {
        let mut f = l.split(':');
        let (who, start, count) = (f.next()?, f.next()?, f.next()?);
        (who == name || who == id.to_string()).then_some(())?;
        Some(IdRange {
            start: start.parse().ok()?,
            count: count.parse().ok()?,
        })
    })
}

/// Runs `f` in a child that dropped root for `uid` and `gid`, and returns
/// what it printed.
pub(super) fn as_user(uid: u32, gid: u32, f: impl FnOnce() -> String) -> String {
    let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
    // SAFETY: the child makes system calls, then runs `f`, which allocates:
    // the C library's allocator is safe after a fork. It ends with `_exit`.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: plain system calls on this single-threaded child.
        let dropped = unsafe {
            libc::setgroups(0, std::ptr::null()) == 0
                && libc::setgid(gid) == 0
                && libc::setuid(uid) == 0
        };
        // A panic must not unwind into the test harness's copy here.
        let text = if dropped {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .unwrap_or_else(|_| "error: panicked".to_owned())
        } else {
            format!("error: cannot drop to {uid}:{gid}")
        };
        // The parent's assertions fail on what is missing.
        let _ = write_all(w.as_fd(), text.as_bytes());
        // SAFETY: `_exit` ends this process at once.
        unsafe { libc::_exit(0) }
    }
    assert!(pid > 0);
    drop(w);
    let mut out = String::new();
    std::fs::File::from(r).read_to_string(&mut out).unwrap();
    let mut status = 0;
    // SAFETY: `waitpid` writes the status, which lives across the call.
    assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    out
}

/// As the user who ran sudo (the VM's `rnk`, the runner's `runner`): with
/// one id, then with `newuidmap` and the user's ranges (both harnesses
/// install `uidmap`; `useradd` gives the user subordinate ids).
/// Where `kernel.apparmor_restrict_unprivileged_userns` is 1 (Ubuntu
/// 24.04), the M3 measurements: one id stops at the `uid_map` write, the
/// ranges at `chown`; elsewhere (22.04) both pass.
#[test]
#[ignore = "T2: drops from root to the sudo user to probe as a regular user"]
fn the_user_namespace_steps_as_a_regular_user_stop_where_apparmor_restricts() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let var = |name: &str| std::env::var(name).ok();
    let (Some(uid), Some(gid), Some(name)) = (var("SUDO_UID"), var("SUDO_GID"), var("SUDO_USER"))
    else {
        panic!("run through sudo by a regular user, as the T2 unit suite does");
    };
    let (uid, gid): (u32, u32) = (uid.parse().unwrap(), gid.parse().unwrap());
    assert_ne!(uid, 0, "run through sudo by a regular user");
    let sub = subordinate_range("/etc/subuid", &name, uid)
        .zip(subordinate_range("/etc/subgid", &name, uid))
        .map(|(uids, gids)| SubordinateIds { uids, gids });
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    let dir = tempfile::tempdir().unwrap();
    rustix::fs::chown(
        dir.path(),
        Some(rustix::fs::Uid::from_raw(uid)),
        Some(rustix::fs::Gid::from_raw(gid)),
    )
    .unwrap();
    let path = dir.path().to_owned();
    let line = |r: Result<UsernsReport>| match r {
        Ok(r) => format!(
            "{}\t{}\t{r}\n",
            r.stop(ANY).unwrap_or("none"),
            r.map().map_or("none".to_owned(), |m| m.to_string())
        ),
        Err(e) => format!("error\t\t{e}\n"),
    };
    let out = as_user(uid, gid, || {
        let mut out = line(probe_at(&path, None));
        if let Some(sub) = sub {
            out.push_str(&line(probe_at(&path, Some(sub))));
        }
        out
    });
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    let lines: Vec<Vec<&str>> = out.lines().map(|l| l.split('\t').collect()).collect();
    for l in &lines {
        measured(&format!(
            "user namespace as {name} (restricted: {restricted}): stop {}: {}",
            l[0],
            l.get(2).unwrap_or(&"")
        ));
    }
    let one = &lines[0];
    if restricted {
        assert_eq!((one[0], one[1]), ("map", "none"), "{out}");
        assert!(one[2].contains("one uid: write uid_map: EPERM"), "{out}");
    } else {
        assert_eq!((one[0], one[1]), ("none", "one uid"), "{out}");
    }
    // Both harnesses install `uidmap`, and the user has subordinate ids.
    let tools = tool("newuidmap").is_some() && tool("newgidmap").is_some();
    assert!(
        sub.is_some() && tools,
        "no ranged map to probe (subordinate ids for {name}: {}, newuidmap: {tools})",
        sub.is_some()
    );
    let ranged = &lines[1];
    if restricted {
        assert_eq!((ranged[0], ranged[1]), ("chown", "newuidmap"), "{out}");
    } else {
        assert_eq!((ranged[0], ranged[1]), ("none", "newuidmap"), "{out}");
    }
}

// Red team of the user-namespace probe (#74): a probe directory with names
// planted in it, odd filesystems under it, and the prober killed mid-way.
// Each helper runs as any user (T0, where an AppArmor restriction may stop
// the steps at the map) and as root (T2c, the whole path).

/// Runs `f` in a forked child made a subreaper, so that a process the probe
/// orphans becomes its child, and returns what `f` printed. The child
/// closes every descriptor but stdio and its pipe first: a copy of another
/// test's pipe end would keep that test from seeing its end.
pub(super) fn in_subreaper(f: impl FnOnce() -> String) -> String {
    let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
    let keep = w.as_raw_fd();
    // SAFETY: the child closes what it does not own, runs `f`, which
    // allocates (the C library's allocator is safe after a fork), and ends
    // with `_exit`; it drops no owner of a descriptor it closed.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let mut kept = [0, 1, 2, keep];
        kept.sort_unstable();
        // SAFETY: this forked child ends with `_exit` and uses only `keep`.
        unsafe { child::close_all_except(&kept) };
        let text = match rustix::process::set_child_subreaper(Some(rustix::process::getpid())) {
            Ok(()) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .unwrap_or_else(|_| "error: panicked".to_owned()),
            Err(e) => format!("error: PR_SET_CHILD_SUBREAPER: {e}"),
        };
        // SAFETY: the pipe's write end, open in this child.
        let _ = write_all(unsafe { BorrowedFd::borrow_raw(keep) }, text.as_bytes());
        // SAFETY: `_exit` ends this process at once.
        unsafe { libc::_exit(0) }
    }
    assert!(pid > 0);
    drop(w);
    let mut out = String::new();
    std::fs::File::from(r).read_to_string(&mut out).unwrap();
    let mut status = 0;
    // SAFETY: `waitpid` writes the status, which lives across the call.
    assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status), "status {status:#x}: {out}");
    out
}

/// The children of `pid`, live or not yet reaped.
fn children_of(pid: i32) -> Vec<i32> {
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

/// Reaps every child that ended for up to `grace`, and names those still
/// alive then.
fn settle(grace: Duration) -> Vec<String> {
    let me = i32::try_from(std::process::id()).unwrap();
    let deadline = Instant::now() + grace;
    loop {
        loop {
            let mut status = 0;
            // SAFETY: `waitpid` writes the status, which lives across the call.
            if unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) } <= 0 {
                break;
            }
        }
        let live = children_of(me);
        if live.is_empty() {
            return Vec::new();
        }
        if Instant::now() >= deadline {
            return live
                .iter()
                .map(|p| {
                    let comm = fs::read_to_string(format!("/proc/{p}/comm")).unwrap_or_default();
                    let ns = |p: &str| fs::read_link(format!("/proc/{p}/ns/user")).ok();
                    let other = if ns(&p.to_string()) == ns("self") {
                        ""
                    } else {
                        ", another user namespace"
                    };
                    format!("{p} ({}{other})", comm.trim())
                })
                .collect();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The mount points at or below `dir` in `/proc/<who>/mountinfo` (none
/// when it cannot be read).
fn mounts_below(who: &str, dir: &Path) -> Vec<String> {
    let dir = dir.to_str().unwrap();
    let below = format!("{dir}/");
    fs::read_to_string(format!("/proc/{who}/mountinfo"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .filter(|m| *m == dir || m.starts_with(&below))
        .map(str::to_owned)
        .collect()
}

/// This process's descriptors and what each is.
fn descriptors() -> Vec<String> {
    let mut fds: Vec<String> = fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| {
            let e = e.ok()?;
            let to = fs::read_link(e.path()).ok()?;
            Some(format!(
                "{} {}",
                e.file_name().to_string_lossy(),
                to.display()
            ))
        })
        .collect();
    fds.sort();
    fds
}

/// A closure for [`run_closure`]: a tmpfs on `path`, in its own namespace.
fn mount_tmpfs(path: &Path) -> std::result::Result<String, String> {
    rustix::mount::mount(c"tmpfs", path, c"tmpfs", MountFlags::empty(), None)
        .map(|()| "mounted".to_owned())
        .map_err(|e| e.to_string())
}

/// One probe in `dir`, then, with a map that worked, a closure that mounts
/// a tmpfs in its namespace on `dir/closure`, removed after.
fn probe_once(dir: &Path) -> String {
    let report = match probe_at(dir, None) {
        Ok(r) => r,
        Err(e) => return format!("probe error: {e}"),
    };
    let Some(plan) = report.plan.as_ref() else {
        return format!("probe: {report}");
    };
    let target = dir.join("closure");
    if let Err(e) = fs::create_dir(&target) {
        return format!("probe: {report}; closure: {e}");
    }
    let ran = run_closure(plan, mount_tmpfs, &target);
    let removed = fs::remove_dir(&target);
    format!("probe: {report}; closure: {ran:?}; its directory removed: {removed:?}")
}

/// What `ls -l` would say of each entry below `dir`, contents included.
fn tree(dir: &Path) -> Vec<String> {
    use std::os::unix::fs::MetadataExt as _;
    let mut out = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            let body = if m.is_file() {
                format!("{:?}", fs::read(&p).unwrap())
            } else if m.is_symlink() {
                fs::read_link(&p).unwrap().display().to_string()
            } else {
                String::new()
            };
            out.push(format!(
                "{} {:o} {}:{} {} {}.{} {body}",
                p.strip_prefix(dir).unwrap().display(),
                m.mode(),
                m.uid(),
                m.gid(),
                m.size(),
                m.mtime(),
                m.mtime_nsec()
            ));
            if m.is_dir() {
                todo.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Plants one name in a fresh probe directory, aimed at a file and a
/// directory outside it, for each attack; probes; and names what went
/// wrong: a probe that did not return, a change outside, or a planted name
/// the probe removed or replaced (`mknod`, whose node the probe removes
/// whatever made it, excepted: there only the outside counts).
fn planted_names_are_never_followed(base: &Path) -> Vec<String> {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let outside = base.join("outside");
    fs::create_dir(&outside).unwrap();
    let file = outside.join("file");
    fs::write(&file, b"keep").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    fs::create_dir(outside.join("dir")).unwrap();
    let before = tree(&outside);
    let fifo = |p: &Path| {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            p,
            FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
    };
    let link_dir = |p: &Path| symlink(outside.join("dir"), p).unwrap();
    let link_file = |p: &Path| symlink(&file, p).unwrap();
    let hard = |p: &Path| fs::hard_link(&file, p).unwrap();
    type Plant<'a> = &'a dyn Fn(&Path);
    let attacks: [(&str, &str, Plant<'_>); 9] = [
        ("proc", "a symlink to a directory outside", &link_dir),
        ("proc", "a FIFO", &fifo),
        ("sys", "a symlink to a directory outside", &link_dir),
        ("chown", "a symlink to a file outside", &link_file),
        ("chown", "a hard link of a file outside", &hard),
        ("chown", "a FIFO", &fifo),
        ("mknod", "a symlink to a file outside", &link_file),
        ("mknod", "a hard link of a file outside", &hard),
        ("mknod", "a FIFO", &fifo),
    ];
    let mut bad = Vec::new();
    for (i, (name, what, plant)) in attacks.iter().enumerate() {
        let dir = base.join(format!("probe-{i}"));
        fs::create_dir(&dir).unwrap();
        plant(&dir.join(name));
        let planted = tree(&dir);
        let (done, wait) = std::sync::mpsc::channel();
        let d = dir.clone();
        // A FIFO opened to write would block: the probe runs on a thread.
        std::thread::spawn(move || {
            let _ = done.send(probe_at(&d, None).map(|r| r.to_string()));
        });
        let got = wait.recv_timeout(Duration::from_secs(60));
        let got = match got {
            Ok(r) => format!("{r:?}"),
            Err(_) => {
                bad.push(format!("{name} as {what}: the probe did not return"));
                continue;
            }
        };
        if tree(&outside) != before {
            bad.push(format!("{name} as {what}: changed outside: {got}"));
        }
        if *name != "mknod" && tree(&dir) != planted {
            bad.push(format!(
                "{name} as {what}: the directory was {planted:?}, is {:?}: {got}",
                tree(&dir)
            ));
        }
    }
    // The probe's directory itself a symlink out, or a FIFO, given by path:
    // refused when opened as a root.
    let link = base.join("probe-link");
    symlink(outside.join("dir"), &link).unwrap();
    let fifo_dir = base.join("probe-fifo");
    fifo(&fifo_dir);
    for (dir, what) in [(&link, "a symlink out"), (&fifo_dir, "a FIFO")] {
        let (done, wait) = std::sync::mpsc::channel();
        let d = dir.clone();
        std::thread::spawn(move || {
            let _ = done.send(probe_at(&d, None).is_ok());
        });
        match wait.recv_timeout(Duration::from_secs(60)) {
            Ok(false) => {}
            Ok(true) => bad.push(format!("the probe's directory as {what}: probed")),
            Err(_) => bad.push(format!("the probe's directory as {what}: did not return")),
        }
        if tree(&outside) != before {
            bad.push(format!("the probe's directory as {what}: changed outside"));
        }
    }
    bad
}

/// Red team (#74; T22, T4): names planted in the probe's directory, or as it.
#[test]
fn red_team_names_planted_in_the_probes_directory_are_never_followed() {
    let base = tempfile::tempdir().unwrap();
    let bad = planted_names_are_never_followed(base.path());
    assert!(bad.is_empty(), "{bad:#?}");
}

/// Red team (#74; T22, T4): the same as root, `mknodat` and the mounts reached.
#[test]
#[ignore = "T2c: sysadmin: the user-namespace probe as root, with names planted in its directory"]
fn red_team_as_root_names_planted_in_the_probes_directory_are_never_followed() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let base = tempfile::tempdir().unwrap();
    let bad = planted_names_are_never_followed(base.path());
    assert!(bad.is_empty(), "{bad:#?}");
}

/// [`probe_once`] in a subreaper, then what is left of it after a grace:
/// processes, descriptors, mounts below the directory (here and in PID 1's
/// table), entries in the probe's directory. A line `LEFT ...` for each.
fn probe_leaves_nothing() -> String {
    in_subreaper(|| {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("probe");
        fs::create_dir(&dir).unwrap();
        let fds = descriptors();
        let mut out = vec![probe_once(&dir)];
        let left = settle(Duration::from_secs(3));
        if !left.is_empty() {
            out.push(format!("LEFT processes: {left:?}"));
        }
        let now = descriptors();
        if now != fds {
            out.push(format!("LEFT descriptors: {now:?}, before {fds:?}"));
        }
        let mut mounted = false;
        for who in ["self", "1"] {
            let m = mounts_below(who, base.path());
            if !m.is_empty() {
                mounted = true;
                out.push(format!("LEFT mounts in {who}'s table: {m:?}"));
            }
        }
        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        if !entries.is_empty() {
            out.push(format!("LEFT entries: {entries:?}"));
        }
        if mounted {
            // Never delete through a mount.
            let _ = base.keep();
        }
        out.join("\n")
    })
}

/// Red team (#74; T7, T8, T9): what a probe and a closure that mounts leave.
#[test]
fn red_team_the_probe_leaves_no_process_mount_descriptor_or_file() {
    let out = probe_leaves_nothing();
    assert!(!out.contains("LEFT") && !out.contains("error"), "{out}");
}

/// Red team (#74; T7, T8, T9): the same as root, through the whole path.
#[test]
#[ignore = "T2c: sysadmin: the user-namespace probe as root, then a closure that mounts"]
fn red_team_as_root_the_probe_leaves_no_process_mount_descriptor_or_file() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let out = probe_leaves_nothing();
    measured(&format!("user namespace probe, nothing left: {out}"));
    assert!(!out.contains("LEFT") && !out.contains("error"), "{out}");
    assert!(
        out.contains("closure: Ok(Ok(\"mounted\"))") || !out.contains("map: multi-range"),
        "{out}"
    );
}

/// Probes in a loop in a child of a subreaper, killed in `rounds` rounds
/// once the probe has a child of its own: `SIGKILL`, `SIGINT` to the prober
/// alone, then `SIGINT` to its process group (a terminal's Ctrl-C), each
/// after a short wait that grows with the round. Names any process alive
/// after a grace, any mount below the directory, and how many kills found a
/// child of the probe there.
fn killed_mid_probe_leaves_nothing(rounds: u32) -> String {
    in_subreaper(|| {
        let base = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut caught = 0;
        for round in 0..rounds {
            let sub = base.path().join(round.to_string());
            fs::create_dir(&sub).unwrap();
            // SAFETY: this process is single-threaded; the child probes,
            // then ends with `_exit` unless killed first.
            let prober = unsafe { libc::fork() };
            if prober == 0 {
                // SAFETY: plain system calls. A shell's background job starts
                // with SIGINT ignored, and the attack is one that ends it.
                unsafe {
                    libc::setpgid(0, 0);
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                }
                let start = Instant::now();
                for i in 0..10_000 {
                    let d = sub.join(i.to_string());
                    if start.elapsed() > Duration::from_secs(30) || fs::create_dir(&d).is_err() {
                        break;
                    }
                    let _ = probe_once(&d);
                }
                // SAFETY: `_exit` ends this process at once.
                unsafe { libc::_exit(0) }
            }
            if prober < 0 {
                out.push(format!("error: fork: {}", io::Error::last_os_error()));
                break;
            }
            // SAFETY: plain system call; either side's `setpgid` wins.
            unsafe { libc::setpgid(prober, prober) };
            let start = Instant::now();
            while children_of(prober).is_empty() && start.elapsed() < Duration::from_secs(5) {
                std::hint::spin_loop();
            }
            let wait = Duration::from_micros(u64::from(round / 3 % 8) * 300);
            let start = Instant::now();
            while start.elapsed() < wait {
                std::hint::spin_loop();
            }
            if !children_of(prober).is_empty() {
                caught += 1;
            }
            let (sig, to, how) = match round % 3 {
                0 => (libc::SIGKILL, prober, "SIGKILL"),
                1 => (libc::SIGINT, prober, "SIGINT"),
                _ => (libc::SIGINT, -prober, "SIGINT to the group"),
            };
            // SAFETY: plain system call, to the prober or its group.
            unsafe { libc::kill(to, sig) };
            let left = settle(Duration::from_secs(3));
            if !left.is_empty() {
                out.push(format!("LEFT after {how} in round {round}: {left:?}"));
            }
            let m = mounts_below("self", base.path());
            if !m.is_empty() {
                out.push(format!("LEFT mounts after {how} in round {round}: {m:?}"));
                // Never delete through a mount.
                let _ = base.keep();
                break;
            }
        }
        out.push(format!("caught {caught} of {rounds}"));
        out.join("\n")
    })
}

/// Red team (#74; T8, T9): the prober killed while a child of the probe runs.
#[test]
fn red_team_a_prober_killed_mid_probe_leaves_no_process_or_mount() {
    let out = killed_mid_probe_leaves_nothing(24);
    assert!(!out.contains("LEFT") && !out.contains("error"), "{out}");
    // Where `unshare` is refused the probe's children end too fast to catch.
    let base = tempfile::tempdir().unwrap();
    let stop = probe_at(base.path(), None).unwrap().stop(ANY);
    assert!(
        stop == Some("unshare") || !out.contains("caught 0 of"),
        "no kill found a child: {out}"
    );
}

/// Red team (#74; T8, T9): the same as root, kills landing in the mounts.
#[test]
#[ignore = "T2c: sysadmin: the user-namespace probe as root, killed mid-way"]
fn red_team_as_root_a_prober_killed_mid_probe_leaves_no_process_or_mount() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let out = killed_mid_probe_leaves_nothing(48);
    measured(&format!(
        "user namespace probe killed mid-way: {}",
        out.replace('\n', "; ")
    ));
    assert!(!out.contains("LEFT") && !out.contains("error"), "{out}");
    assert!(!out.contains("caught 0 of"), "no kill found a child: {out}");
}

/// The probe in a directory on each filesystem: a plain tmpfs, one with
/// inodes for the directory and two entries only (the probe runs out
/// part-way), a read-only tmpfs and a ramfs, each mounted in a mount
/// namespace of the child's own. A line `LEFT ...` for each process, mount
/// or entry left.
fn odd_filesystems_leave_nothing() -> String {
    in_subreaper(|| {
        let mut out = Vec::new();
        // SAFETY: `NEWNS` does not unshare the descriptor table, the only
        // flag `unshare_unsafe` is unsafe for; this child is single-threaded.
        let private =
            unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWNS) }
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    rustix::mount::mount_change(
                        "/",
                        rustix::mount::MountPropagationFlags::PRIVATE
                            | rustix::mount::MountPropagationFlags::REC,
                    )
                    .map_err(|e| e.to_string())
                });
        if let Err(e) = private {
            return format!("error: a private mount namespace: {e}");
        }
        let base = tempfile::tempdir().unwrap();
        for (i, (fs_type, data, read_only)) in [
            (c"tmpfs", c"", false),
            (c"tmpfs", c"nr_inodes=4", false),
            (c"tmpfs", c"", true),
            (c"ramfs", c"", false),
        ]
        .into_iter()
        .enumerate()
        {
            let what = format!(
                "{fs_type:?} {data:?}{}",
                if read_only { " read-only" } else { "" }
            );
            let fs_dir = base.path().join(i.to_string());
            fs::create_dir(&fs_dir).unwrap();
            if let Err(e) =
                rustix::mount::mount(fs_type, &fs_dir, fs_type, MountFlags::empty(), data)
            {
                out.push(format!("error: {what}: mount: {e}"));
                continue;
            }
            let dir = fs_dir.join("probe");
            fs::create_dir(&dir).unwrap();
            if read_only {
                let ro = rustix::mount::mount_remount(&fs_dir, MountFlags::RDONLY, c"");
                if let Err(e) = ro {
                    out.push(format!("error: {what}: remount read-only: {e}"));
                    continue;
                }
            }
            out.push(format!("{what}: {}", probe_once(&dir)));
            let left = settle(Duration::from_secs(3));
            if !left.is_empty() {
                out.push(format!("LEFT processes on {what}: {left:?}"));
            }
            let m = mounts_below("self", &dir);
            if !m.is_empty() {
                out.push(format!("LEFT mounts on {what}: {m:?}"));
            }
            let entries: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            if !entries.is_empty() {
                out.push(format!("LEFT entries on {what}: {entries:?}"));
            }
        }
        // The mounts end with this child's mount namespace.
        let _ = base.keep();
        out.join("\n")
    })
}

/// Red team (#74; T4, T9): the probe's directory on odd filesystems.
#[test]
#[ignore = "T2c: sysadmin: the user-namespace probe as root, on filesystems it mounts"]
fn red_team_as_root_the_probe_on_odd_filesystems_leaves_nothing() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let out = odd_filesystems_leave_nothing();
    measured(&format!(
        "user namespace probe on odd filesystems: {}",
        out.replace('\n', "; ")
    ));
    // The harness's errors start a line; the probe's are expected here.
    assert!(
        !out.contains("LEFT") && !out.lines().any(|l| l.starts_with("error: ")),
        "{out}"
    );
    assert!(out.contains("\"nr_inodes=4\": probe error: "), "{out}");
    assert!(out.contains("read-only: probe error: "), "{out}");
}
