use std::fs;
use std::io::Read as _;
use std::os::fd::AsFd as _;

use super::*;

const EPERM: i32 = libc::EPERM;

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
    let root = kinds(plans(0, 0, None));
    assert_eq!(root.len(), 2);
    assert_eq!(root[0].0, IdMap::Ranges);
    assert_eq!(root[1], (IdMap::OneId, None));

    let user = kinds(plans(1000, 1001, None));
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
    let user = kinds(plans(1000, 1001, Some(sub)));
    assert_eq!(user[0].0, IdMap::Newuidmap);
    let tools = tool("newuidmap").is_some() && tool("newgidmap").is_some();
    assert_eq!(user[0].1.is_none(), tools);
    match plans(1000, 1001, None).pop() {
        Some(Ok(Plan::OneId { uid_map, gid_map })) => {
            assert_eq!(uid_map, b"0 1000 1\n");
            assert_eq!(gid_map, b"0 1001 1\n");
        }
        other => panic!("{other:?}"),
    }
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
    assert_eq!(report.stop(), Some("chown"));
    assert_eq!(report.map(), Some(IdMap::Ranges));
}

#[test]
fn the_report_names_the_first_step_a_run_needs_that_failed() {
    let passing = |plan: Plan, refused: &[u8]| {
        let a = assemble(&plan, &all_steps(refused), Some(Ok(())), None);
        report_of(vec![(plan.kind(), StepResult::Ok)], Some((plan, a)))
    };
    let r = passing(ranges(), &[PROC, SYSFS, MKNOD]);
    assert_eq!(r.stop(), None);
    assert_eq!(
        r.to_string(),
        "unshare(CLONE_NEWUSER): ok; map: multi-range; chown: ok; unshare(CLONE_NEWNS): ok; \
         proc: rbind of /proc (mount proc: EPERM); sysfs: rbind of /sys (mount sysfs: EPERM); \
         mknod: EPERM"
    );
    assert_eq!(passing(ranges(), &[CHOWN]).stop(), Some("chown"));
    assert_eq!(passing(ranges(), &[NEWNS]).stop(), Some("mount namespace"));
    assert_eq!(passing(ranges(), &[PROC, PROC_RBIND]).stop(), Some("proc"));
    assert_eq!(
        passing(ranges(), &[SYSFS, SYSFS_RBIND]).stop(),
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
    assert_eq!(r.stop(), None);
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
    assert_eq!(r.stop(), Some("map"));
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
        // SAFETY: the child makes system calls only, and ends with `_exit`
        // or `SIGKILL`.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            if !ends {
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
    assert_eq!(killed.terminating_signal(), Some(libc::SIGKILL));
    let pid = fork(false);
    let left = reap(pid, true, Duration::from_millis(100));
    // An error is the next `reap`'s to show.
    let _ = rustix::process::kill_process(pid, Signal::KILL);
    let after = reap(pid, true, Duration::from_secs(10));
    assert!(left.unwrap().is_none());
    assert!(after.unwrap().is_some());
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
/// send a record; the closure logs, then panics. The child allocates
/// beside the harness's threads, as [`as_user`]'s does: the C library's
/// allocator is safe after a fork.
#[test]
fn a_closures_child_writes_through_no_hook_or_subscriber_of_its_parent() {
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
    assert!(probe_in(dir.path(), None).is_err());
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
fn measured(line: &str) {
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
    let report = probe_in(dir.path(), None).unwrap();
    measured(&format!("user namespace as root: {report}"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    assert_eq!(report.cut_short, None);
    if seccomp_without_sys_admin() {
        assert_eq!(report.stop(), Some("unshare"), "{report}");
        assert!(is_eperm(&report.unshare), "{report}");
        assert_eq!(report.map(), None);
        return;
    }
    assert_eq!(report.stop(), None, "{report}");
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

/// The first range of `file` (`/etc/subuid` or `/etc/subgid`) for `name`
/// or `id`.
fn subordinate_range(file: &str, name: &str, id: u32) -> Option<IdRange> {
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
fn as_user(uid: u32, gid: u32, f: impl FnOnce() -> String) -> String {
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
            r.stop().unwrap_or("none"),
            r.map().map_or("none".to_owned(), |m| m.to_string())
        ),
        Err(e) => format!("error\t\t{e}\n"),
    };
    let out = as_user(uid, gid, || {
        let mut out = line(probe_in(&path, None));
        if let Some(sub) = sub {
            out.push_str(&line(probe_in(&path, Some(sub))));
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
