use std::ffi::CStr;
use std::fs;
use std::path::Path;

use rustix::process::DumpableBehavior;

use super::super::tests::{as_user, in_subreaper, measured, subordinate_range};
use super::super::{
    CHDIR, DUMPABLE, GID_MAP, IdRange, NEWNS, READY, Records, SETGROUPS, UID_MAP, UNSHARE,
    UNSHARE_USER, mirror,
};
use super::*;

const EPERM: i32 = libc::EPERM;

fn one_id() -> Plan {
    Plan::OneId {
        uid_map: b"0 1000 1\n".to_vec(),
        gid_map: b"0 1000 1\n".to_vec(),
    }
}

fn ranges() -> Plan {
    Plan::Ranges {
        uid_map: b"0 0 1\n1 1 65536\n".to_vec(),
        gid_map: b"0 0 1\n1 1 65536\n".to_vec(),
    }
}

fn run(
    list: &[(u8, i32)],
    parent_map: Option<std::result::Result<(), StepFailure>>,
    cut_short: Option<&str>,
) -> Supervised {
    Supervised {
        records: Records {
            list: list.to_vec(),
            payload: None,
            timed_out: false,
        },
        parent_map,
        cut_short: cut_short.map(str::to_owned),
    }
}

fn failed(op: &'static str, errno: i32) -> StepResult {
    StepResult::Failed(StepFailure::errno(op, errno))
}

/// Each step a trial takes, in order, every one passing.
const ONE_ID_STEPS: [(u8, i32); 7] = [
    (UNSHARE, 0),
    (UID_MAP, 0),
    (SETGROUPS, 0),
    (GID_MAP, 0),
    (NEWNS, 0),
    (SLAVE, 0),
    (DONE, 0),
];

#[test]
fn a_trial_passes_only_with_every_step_and_its_end() {
    assert_eq!(
        judge(&one_id(), run(&ONE_ID_STEPS, None, None)),
        Trial::Passed
    );
    let ranged = [(UNSHARE, 0), (READY, 0), (NEWNS, 0), (SLAVE, 0), (DONE, 0)];
    assert_eq!(
        judge(&ranges(), run(&ranged, Some(Ok(())), None)),
        Trial::Passed
    );
    // Every step, but no end: the child stopped before it said so.
    assert_eq!(
        judge(&one_id(), run(&ONE_ID_STEPS[..6], None, None)),
        Trial::Stopped(StepResult::NotRun(STOPPED))
    );
}

#[test]
fn a_refused_unshare_ends_the_trials() {
    assert_eq!(
        judge(&one_id(), run(&[(UNSHARE, EPERM)], None, None)),
        Trial::NoNamespace(failed(UNSHARE_USER, EPERM))
    );
    // A trial that never reached the `unshare` says nothing about it.
    assert_eq!(
        judge(&one_id(), run(&[], None, None)),
        Trial::Stopped(StepResult::NotRun(STOPPED))
    );
    assert!(matches!(
        judge(&one_id(), run(&[(CHDIR, EPERM)], None, None)),
        Trial::NoNamespace(StepResult::Failed(_))
    ));
}

#[test]
fn a_trial_stops_at_the_first_step_refused() {
    // One id where AppArmor restricts unprivileged user namespaces (M3).
    let list = [(UNSHARE, 0), (UID_MAP, EPERM)];
    assert_eq!(
        judge(&one_id(), run(&list, None, None)),
        Trial::Stopped(failed("write uid_map", EPERM))
    );
    // A ranged map there: written, then no mount namespace.
    let list = [(UNSHARE, 0), (READY, 0), (NEWNS, EPERM)];
    assert_eq!(
        judge(&ranges(), run(&list, Some(Ok(())), None)),
        Trial::Stopped(failed(UNSHARE_MOUNT, EPERM))
    );
    let list = [(UNSHARE, 0), (READY, 0), (NEWNS, 0), (SLAVE, libc::EINVAL)];
    assert_eq!(
        judge(&ranges(), run(&list, Some(Ok(())), None)),
        Trial::Stopped(failed(SLAVE_OP, libc::EINVAL))
    );
    let refused = StepFailure::other("newuidmap", "exit status: 1: write to uid_map failed");
    let list = [(UNSHARE, 0), (READY, 0)];
    assert_eq!(
        judge(&ranges(), run(&list, Some(Err(refused.clone())), None)),
        Trial::Stopped(StepResult::Failed(refused))
    );
    let list = [(UNSHARE, 0), (DUMPABLE, EPERM)];
    assert_eq!(
        judge(&one_id(), run(&list, None, None)),
        Trial::Stopped(failed("prctl(PR_SET_DUMPABLE)", EPERM))
    );
}

#[test]
fn a_trial_cut_short_says_how() {
    let list = [(UNSHARE, 0), (READY, 0)];
    let killed = "the probe's child was killed after 10 s";
    assert_eq!(
        judge(&ranges(), run(&list, Some(Ok(())), Some(killed))),
        Trial::Stopped(StepResult::Failed(StepFailure::other("the trial", killed)))
    );
    // A refusal it reported first stands.
    let list = [(UNSHARE, 0), (READY, 0), (NEWNS, EPERM)];
    assert_eq!(
        judge(&ranges(), run(&list, Some(Ok(())), Some(killed))),
        Trial::Stopped(failed(UNSHARE_MOUNT, EPERM))
    );
}

/// The namespace is told as the kernel gave it, not as planned: root's own
/// one-id map mirrored is one uid, and a parent's `deny` stands.
#[test]
fn the_namespace_is_told_as_read_back() {
    let text = |m: Vec<u8>| String::from_utf8(m).unwrap();
    let one = text(mirror("0 1000 1\n").unwrap());
    assert_eq!(
        as_entered(IdMap::Ranges, &one, &one, "deny\n"),
        Some((IdMap::OneId, false))
    );
    assert_eq!(
        as_entered(IdMap::Ranges, &one, &one, "allow\n"),
        Some((IdMap::OneId, true))
    );
    let several = text(mirror("0 1000 1\n1 100000 65536\n").unwrap());
    assert_eq!(
        as_entered(IdMap::Ranges, &several, &one, "allow\n"),
        Some((IdMap::OneId, true))
    );
    assert_eq!(
        as_entered(IdMap::Ranges, &several, &several, "deny\n"),
        Some((IdMap::Ranges, false))
    );
    assert_eq!(
        as_entered(IdMap::Newuidmap, &several, &several, "allow\n"),
        Some((IdMap::Newuidmap, true))
    );
    for (uid_map, setgroups) in [
        ("", "allow\n"),
        ("0 0 0\n", "allow\n"),
        ("0 0\n", "allow\n"),
        ("0 0 1 1\n", "allow\n"),
        ("0 0 x\n", "allow\n"),
        (several.as_str(), ""),
        (several.as_str(), "allowed\n"),
    ] {
        assert_eq!(
            as_entered(IdMap::Ranges, uid_map, &several, setgroups),
            None,
            "{uid_map:?} {setgroups:?}"
        );
    }
}

#[test]
fn the_namespace_names_its_map_and_what_was_passed_over() {
    let ns = BuildUserNamespace {
        map: IdMap::OneId,
        passed_over: vec![(
            IdMap::Newuidmap,
            StepResult::NotRun("no subordinate ids for this user"),
        )],
        setgroups: false,
    };
    assert!(!ns.setgroups_allowed());
    assert_eq!(
        ns.to_string(),
        "one uid map, setgroups denied (newuidmap: not run (no subordinate ids for this user))"
    );
    let ns = BuildUserNamespace {
        map: IdMap::Newuidmap,
        passed_over: Vec::new(),
        setgroups: true,
    };
    assert!(ns.setgroups_allowed());
    assert_eq!(ns.to_string(), "newuidmap map");
    let e = Error::NoUserNamespace {
        tried: vec![
            (IdMap::Newuidmap, failed(UNSHARE_MOUNT, EPERM)),
            (IdMap::OneId, failed("write uid_map", EPERM)),
        ],
    };
    assert_eq!(
        e.to_string(),
        "no user namespace for the build: newuidmap: unshare(CLONE_NEWNS): EPERM, one uid: \
         write uid_map: EPERM"
    );
}

/// A helper that cannot write the map says why, and is reaped: here this
/// process is still in its first user namespace, whose map is written
/// already, so the write is refused without any privilege.
#[test]
fn a_helper_whose_map_is_refused_reports_it_and_ends() {
    let out = in_subreaper(|| {
        let helper = match MapHelper::start(&ranges()) {
            Ok(h) => h,
            Err(e) => return format!("error: {e}"),
        };
        let pid = helper.pid;
        let r = helper.write();
        // Reaped: no child of that number is left, not even a zombie.
        let gone = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG)
            .is_err_and(|e| e == Errno::CHILD);
        format!("{r:?} {gone}")
    });
    assert!(
        out.contains("write uid_map: EPERM") && out.ends_with(" true"),
        "{out}"
    );
}

/// A helper never told to go writes nothing and ends.
#[test]
fn a_cancelled_helper_ends_without_writing() {
    let out = in_subreaper(|| {
        let helper = match MapHelper::start(&ranges()) {
            Ok(h) => h,
            Err(e) => return format!("error: {e}"),
        };
        let pid = helper.pid;
        helper.cancel();
        let gone = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG)
            .is_err_and(|e| e == Errno::CHILD);
        gone.to_string()
    });
    assert_eq!(out, "true");
}

/// The build's user namespace is not entered beside other threads, before
/// anything changes.
#[test]
fn the_build_user_namespace_is_refused_beside_other_threads() {
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let spare = std::thread::spawn(move || {
        // Only waits for the end of the check.
        let _ = wait.recv();
    });
    let before = fs::read_link("/proc/self/ns/user").unwrap();
    let r = crate::enter_build_user_namespace(None);
    drop(done);
    spare.join().unwrap();
    assert!(
        matches!(r, Err(Error::NotSingleThreaded { threads }) if threads > 1),
        "{r:?}"
    );
    assert_eq!(fs::read_link("/proc/self/ns/user").unwrap(), before);
}

/// The fields [`describe`] writes, tab-separated.
struct Described {
    /// `ok`, or `err` with the error.
    outcome: String,
    map: String,
    uid_map: String,
    gid_map: String,
    setgroups: String,
    euid: String,
    /// `new`, or `same` user namespace as before.
    userns: String,
    /// `new`, or `same` mount namespace as before.
    mountns: String,
    /// Whether the process is dumpable after.
    dumpable: String,
    passed_over: String,
    /// What [`BuildUserNamespace::setgroups_allowed`] says: `allow` or
    /// `deny`; empty when not entered.
    told_setgroups: String,
}

impl Described {
    fn parse(line: &str) -> Self {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 11, "{line}");
        let s = |i: usize| f[i].to_owned();
        Self {
            outcome: s(0),
            map: s(1),
            uid_map: s(2),
            gid_map: s(3),
            setgroups: s(4),
            euid: s(5),
            userns: s(6),
            mountns: s(7),
            dumpable: s(8),
            passed_over: s(9),
            told_setgroups: s(10),
        }
    }
}

/// Enters the build's user namespace in this process, which must be a
/// single-threaded child that ends after, made not dumpable first as
/// ranikust is (ADR-0051), and describes the outcome as one line of
/// [`Described`]'s fields.
fn describe(sub: Option<SubordinateIds>) -> String {
    let link = |ns: &str| {
        fs::read_link(format!("/proc/self/ns/{ns}"))
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    };
    let (user, mount) = (link("user"), link("mnt"));
    if let Err(e) = rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable) {
        return format!("err\tPR_SET_DUMPABLE: {e}\t\t\t\t\t\t\t\t\t");
    }
    let r = crate::enter_build_user_namespace(sub);
    let flat = |path: &str| {
        fs::read_to_string(path).map_or_else(
            |e| format!("unreadable: {e}"),
            |t| t.split_whitespace().collect::<Vec<_>>().join(" "),
        )
    };
    let same = |before: &str, ns: &str| if link(ns) == before { "same" } else { "new" };
    let dumpable = match rustix::process::dumpable_behavior() {
        Ok(DumpableBehavior::NotDumpable) => "0",
        Ok(_) => "1",
        Err(_) => "?",
    };
    let (outcome, map, passed, told) = match r {
        Ok(ns) => (
            "ok".to_owned(),
            ns.map.to_string(),
            tried_text(&ns.passed_over),
            if ns.setgroups_allowed() {
                "allow"
            } else {
                "deny"
            },
        ),
        Err(Error::NoUserNamespace { tried }) => {
            ("err".to_owned(), "none".to_owned(), tried_text(&tried), "")
        }
        Err(e) => (format!("err {e}"), "none".to_owned(), String::new(), ""),
    };
    [
        outcome,
        map,
        flat("/proc/self/uid_map"),
        flat("/proc/self/gid_map"),
        flat("/proc/self/setgroups"),
        rustix::process::geteuid().as_raw().to_string(),
        same(&user, "user").to_owned(),
        same(&mount, "mnt").to_owned(),
        dumpable.to_owned(),
        passed,
        told.to_owned(),
    ]
    .join("\t")
}

/// What every outcome must keep: entered, the map is the one named and
/// this process is root of it; refused, nothing changed. Either way, the
/// process is not dumpable after.
fn check_kept(d: &Described, uid: u32, gid: u32, line: &str) {
    assert_eq!(d.dumpable, "0", "{line}");
    match d.outcome.as_str() {
        "ok" => {
            assert_eq!(
                (d.userns.as_str(), d.mountns.as_str()),
                ("new", "new"),
                "{line}"
            );
            assert_eq!(d.euid, "0", "{line}");
            assert_eq!(d.setgroups, d.told_setgroups, "{line}");
            let ids = |map: &str| -> u64 {
                let f: Vec<&str> = map.split(' ').collect();
                f.chunks(3).map(|l| l[2].parse::<u64>().unwrap()).sum()
            };
            let (uids, gids) = (ids(&d.uid_map), ids(&d.gid_map));
            if d.map == "one uid" {
                assert!(uids == 1 || gids == 1, "{line}");
                if uids == 1 {
                    assert_eq!(d.uid_map, format!("0 {uid} 1"), "{line}");
                }
                if gids == 1 {
                    assert_eq!(d.gid_map, format!("0 {gid} 1"), "{line}");
                }
            } else {
                assert!(uids > 1 && gids > 1, "{line}");
            }
        }
        "err" => {
            assert_eq!(
                (d.userns.as_str(), d.mountns.as_str()),
                ("same", "same"),
                "{line}"
            );
            assert_eq!(d.euid, uid.to_string(), "{line}");
        }
        other => panic!("not refused before any change: {other}: {line}"),
    }
}

/// As whoever runs the tests: where this host lets a user namespace work,
/// it is entered with the map named; where not (Ubuntu 24.04's AppArmor
/// restriction), nothing changes. With this user's subordinate ids from
/// `/etc/subuid` too, when it has some.
#[test]
fn red_team_the_build_user_namespace_is_whole_or_not_entered() {
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    let name = std::env::var("USER").unwrap_or_default();
    let sub = subordinate_range("/etc/subuid", &name, uid)
        .zip(subordinate_range("/etc/subgid", &name, uid))
        .map(|(uids, gids)| SubordinateIds { uids, gids });
    for sub in [None, sub] {
        let line = in_subreaper(|| describe(sub));
        measured(&format!("build user namespace as uid {uid}: {line}"));
        let d = Described::parse(&line);
        check_kept(&d, uid, gid, &line);
        if uid != 0 && sub.is_none() {
            assert!(
                d.passed_over
                    .starts_with("newuidmap: not run (no subordinate ids for this user)"),
                "{line}"
            );
        }
    }
}

/// T1: root in rootless Podman maps every id it has, each to itself, in
/// several ranges, written by the helper from outside the namespace; and
/// `setgroups` is allowed.
#[test]
#[ignore = "T1: default: root writing its own multi-range map in a rootless Podman container"]
fn the_build_user_namespace_of_root_maps_every_id_it_has() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let mirrored = |name: &str| {
        let text = fs::read_to_string(format!("/proc/self/{name}")).unwrap();
        let m = mirror(&text).unwrap();
        String::from_utf8(m)
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (uid_map, gid_map) = (mirrored("uid_map"), mirrored("gid_map"));
    let line = in_subreaper(|| describe(None));
    measured(&format!("build user namespace as root: {line}"));
    let d = Described::parse(&line);
    check_kept(&d, 0, 0, &line);
    assert_eq!(
        (d.outcome.as_str(), d.map.as_str()),
        ("ok", "multi-range"),
        "{line}"
    );
    assert_eq!((d.uid_map, d.gid_map), (uid_map, gid_map), "{line}");
    assert_eq!(d.setgroups, "allow");
    assert_eq!(d.passed_over, "");
}

/// Moves this (forked, single-threaded) process into a user namespace of
/// its own whose map is its own uid and gid as 0, `setgroups` denied, as
/// `unshare -r` makes it.
fn one_id_parent() -> std::result::Result<(), String> {
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    // SAFETY: `NEWUSER` does not unshare the descriptor table; this forked
    // child is single-threaded.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWUSER) }
        .map_err(|e| format!("unshare(CLONE_NEWUSER): {e}"))?;
    let write = |path: &CStr, text: String| {
        write_own(path, text.as_bytes()).map_err(|e| format!("write {path:?}: {e}"))
    };
    write(c"/proc/self/uid_map", format!("0 {uid} 1\n"))?;
    write(c"/proc/self/setgroups", "deny".to_owned())?;
    write(c"/proc/self/gid_map", format!("0 {gid} 1\n"))
}

/// T1: root of a one-id user namespace, `setgroups` denied, mirrors its
/// map, which passes: the build's namespace is told as the kernel gives it,
/// one uid, `setgroups` denied, not as the map planned.
#[test]
#[ignore = "T1: default: root of a one-id user namespace in a rootless Podman container enters the build's user namespace"]
fn the_build_user_namespace_below_one_id_is_told_one_uid() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let line = in_subreaper(|| match one_id_parent() {
        Ok(()) => describe(None),
        Err(e) => format!("error: {e}"),
    });
    measured(&format!("build user namespace below one id: {line}"));
    let d = Described::parse(&line);
    check_kept(&d, 0, 0, &line);
    assert_eq!(
        (d.outcome.as_str(), d.map.as_str()),
        ("ok", "one uid"),
        "{line}"
    );
    assert_eq!((d.uid_map.as_str(), d.gid_map.as_str()), ("0 0 1", "0 0 1"));
    assert_eq!(
        (d.setgroups.as_str(), d.told_setgroups.as_str()),
        ("deny", "deny")
    );
    // Root's own map, mirrored, is the one that passed.
    assert_eq!(d.passed_over, "");
}

/// Binds `/dev/null` over `newuidmap` and `newgidmap` in a private mount
/// namespace of this (forked, single-threaded, root) process: a host
/// without `uidmap`.
fn hide_uidmap() -> std::result::Result<(), String> {
    // SAFETY: `NEWNS` does not unshare the descriptor table; this forked
    // child is single-threaded.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) }
        .map_err(|e| format!("unshare(CLONE_NEWNS): {e}"))?;
    rustix::mount::mount_change(
        c"/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| format!("make / private: {e}"))?;
    for name in ["newuidmap", "newgidmap"] {
        for dir in ["/usr/bin", "/bin"] {
            let path = format!("{dir}/{name}");
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                rustix::mount::mount_bind("/dev/null", path.as_str())
                    .map_err(|e| format!("hide {path}: {e}"))?;
            }
        }
    }
    Ok(())
}

/// T2 (the VM, root dropping to the user who ran sudo, as the probe's T2
/// test): with `newuidmap` and the user's ranges, the user's ids as 0 and
/// its whole range from 1, `setgroups` allowed; without ranges, and with
/// `newuidmap` hidden (a host without `uidmap`, M4), one uid, reported so,
/// `setgroups` denied. Where `kernel.apparmor_restrict_unprivileged_userns`
/// is 1 (Ubuntu 24.04) no map works for an unconfined binary (M3): refused,
/// nothing changed.
#[test]
#[ignore = "T2: drops from root to the sudo user to enter the build's user namespace"]
fn the_build_user_namespace_of_a_regular_user_maps_its_ranges_else_one_uid() {
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
        .map(|(uids, gids)| SubordinateIds { uids, gids })
        .expect("the user has subordinate ids (useradd gives them)");
    assert!(
        super::super::tool("newuidmap").is_some() && super::super::tool("newgidmap").is_some(),
        "the harness installs uidmap"
    );
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    let ranged = as_user(uid, gid, || describe(Some(sub)));
    let no_ranges = as_user(uid, gid, || describe(None));
    let no_uidmap = in_subreaper(|| match hide_uidmap() {
        Ok(()) => as_user(uid, gid, || describe(Some(sub))),
        Err(e) => format!("error: {e}"),
    });
    for (leg, line) in [
        ("newuidmap", &ranged),
        ("no ranges", &no_ranges),
        ("no uidmap", &no_uidmap),
    ] {
        measured(&format!(
            "build user namespace as {name} ({leg}, restricted: {restricted}): {line}"
        ));
    }
    let (ranged, no_ranges, no_uidmap) = (
        Described::parse(&ranged),
        Described::parse(&no_ranges),
        Described::parse(&no_uidmap),
    );
    for d in [&ranged, &no_ranges, &no_uidmap] {
        check_kept(d, uid, gid, &d.passed_over);
    }
    if restricted {
        for d in [&ranged, &no_ranges, &no_uidmap] {
            assert_eq!(d.outcome, "err", "{}", d.passed_over);
            assert!(
                d.passed_over.ends_with("one uid: write uid_map: EPERM"),
                "{}",
                d.passed_over
            );
        }
        assert!(
            ranged
                .passed_over
                .starts_with("newuidmap: unshare(CLONE_NEWNS): EPERM"),
            "{}",
            ranged.passed_over
        );
        return;
    }
    assert_eq!(
        (ranged.outcome.as_str(), ranged.map.as_str()),
        ("ok", "newuidmap")
    );
    let whole = |r: IdRange, own: u32| format!("0 {own} 1 1 {} {}", r.start, r.count);
    assert_eq!(ranged.uid_map, whole(sub.uids, uid));
    assert_eq!(ranged.gid_map, whole(sub.gids, gid));
    assert_eq!(ranged.setgroups, "allow");
    assert_eq!(ranged.passed_over, "");
    for (d, why) in [
        (&no_ranges, "no subordinate ids for this user"),
        (&no_uidmap, "no newuidmap and newgidmap in /usr/bin or /bin"),
    ] {
        assert_eq!((d.outcome.as_str(), d.map.as_str()), ("ok", "one uid"));
        assert_eq!(d.passed_over, format!("newuidmap: not run ({why})"));
    }
}

/// The mount points of `/proc/<pid>/mountinfo` (`self` for this process).
fn mount_points(pid: &str) -> Vec<(String, String)> {
    fs::read_to_string(format!("/proc/{pid}/mountinfo"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            let sep = f.iter().position(|x| *x == "-")?;
            Some((f.get(4)?.to_string(), f[6..sep].join(" ")))
        })
        .collect()
}

/// Forks a child that runs `f`, then reports `ready` with what `f` said
/// and waits until the returned pipe end is dropped; returns its pid,
/// what it reported, and that end.
fn hold(f: impl FnOnce() -> String) -> (i32, String, OwnedFd) {
    use rustix::pipe::{PipeFlags, pipe_with};
    let (report_r, report_w) = pipe_with(PipeFlags::CLOEXEC).unwrap();
    let (go_r, go_w) = pipe_with(PipeFlags::CLOEXEC).unwrap();
    // SAFETY: called from a single-threaded forked child; this child runs
    // `f`, which allocates (the C library's allocator is safe after a fork),
    // and ends with `_exit`.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        drop((report_r, go_w));
        let text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .unwrap_or_else(|_| "error: panicked".to_owned());
        let _ = write_all(report_w.as_fd(), text.as_bytes());
        drop(report_w);
        let mut byte = [0u8];
        while matches!(rustix::io::read(&go_r, &mut byte), Err(Errno::INTR)) {}
        // SAFETY: `_exit` ends this process at once.
        unsafe { libc::_exit(0) }
    }
    assert!(pid > 0);
    drop((report_w, go_r));
    let mut out = String::new();
    std::io::Read::read_to_string(&mut fs::File::from(report_r), &mut out).unwrap();
    (pid, out, go_w)
}

/// Ends a child [`hold`] started.
fn release(pid: i32, go: OwnedFd) {
    drop(go);
    let mut status = 0;
    // SAFETY: `waitpid` writes the status, which lives across the call.
    unsafe { libc::waitpid(pid, &raw mut status, 0) };
}

/// As root that may mount, in a private mount namespace of a forked child
/// (no mount outlives the test): a shared tmpfs at `base`; a plain mount
/// namespace's mount below it shows here (the control), the build's
/// mount namespace's does not. `control\tmap\tinside\toutside\tshared`.
fn mount_below_a_shared_one(base: &Path) -> String {
    use rustix::mount::{MountFlags, MountPropagationFlags as P, mount, mount_change};
    let base = base.to_owned();
    in_subreaper(move || {
        let setup = || -> std::result::Result<(), String> {
            // SAFETY: `NEWNS` does not unshare the descriptor table; this
            // forked child is single-threaded.
            unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) }
                .map_err(|e| format!("unshare(CLONE_NEWNS): {e}"))?;
            mount_change(c"/", P::PRIVATE | P::REC).map_err(|e| format!("private /: {e}"))?;
            mount(c"none", &base, c"tmpfs", MountFlags::empty(), None)
                .map_err(|e| format!("tmpfs: {e}"))?;
            mount_change(&base, P::SHARED).map_err(|e| format!("shared: {e}"))?;
            for d in ["control", "inner"] {
                fs::create_dir(base.join(d)).map_err(|e| format!("{d}: {e}"))?;
            }
            Ok(())
        };
        if let Err(e) = setup() {
            return format!("error: {e}");
        }
        let at = |d: &str| base.join(d).display().to_string();
        let mounted = |pid: &str, d: &str| mount_points(pid).iter().any(|(p, _)| *p == at(d));
        let tmpfs = |d: &Path| mount(c"none", d, c"tmpfs", MountFlags::empty(), None);
        // The control: a mount namespace of the same user namespace.
        let control_dir = base.join("control");
        let (pid, said, go) = hold(move || {
            // SAFETY: as above, in this forked child.
            match unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) }
                .and_then(|()| tmpfs(&control_dir))
            {
                Ok(()) => "mounted".to_owned(),
                Err(e) => format!("error: {e}"),
            }
        });
        let control = format!("{said}:{}", mounted("self", "control"));
        release(pid, go);
        // The build's: entered, then a tmpfs and a file in it.
        let inner = base.join("inner");
        let (pid, said, go) = hold(move || match crate::enter_build_user_namespace(None) {
            Ok(ns) => match tmpfs(&inner).and_then(|()| {
                fs::write(inner.join("x"), b"x").map_err(|e| Errno::from_io_error(&e).unwrap())
            }) {
                Ok(()) => ns.map.to_string(),
                Err(e) => format!("error: mount: {e}"),
            },
            Err(e) => format!("error: {e}"),
        });
        let line = [
            control,
            said,
            mounted(&pid.to_string(), "inner").to_string(),
            (mounted("self", "inner") || base.join("inner/x").exists()).to_string(),
            mount_points("self")
                .iter()
                .any(|(p, opt)| *p == base.display().to_string() && opt.contains("shared:"))
                .to_string(),
        ]
        .join("\t");
        release(pid, go);
        line
    })
}

/// Checks [`mount_below_a_shared_one`]'s line: the control's mount
/// propagated, the build's was made and stayed inside, and the parent's
/// mount is still shared.
fn check_mount_stays_inside(line: &str) {
    let f: Vec<&str> = line.split('\t').collect();
    assert_eq!(f.len(), 5, "{line}");
    assert_eq!(
        f[0], "mounted:true",
        "the control did not propagate: {line}"
    );
    assert!(!f[1].starts_with("error"), "{line}");
    assert_eq!(&f[2..], ["true", "false", "true"], "{line}");
}

/// T1: root of a privileged rootless Podman container enters the build's
/// user namespace below a shared mount; a mount made there never shows
/// outside (T5, ADR-0098).
#[test]
#[ignore = "T1: privileged: mounts in a private mount namespace and the build's user namespace"]
fn red_team_a_mount_in_the_build_s_namespace_never_shows_outside() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let tmp = tempfile::tempdir().unwrap();
    let line = mount_below_a_shared_one(tmp.path());
    measured(&format!(
        "mount below a shared one, rootless Podman: {line}"
    ));
    check_mount_stays_inside(&line);
}

/// T2: the same as real root, whose map is every id.
#[test]
#[ignore = "T2: mounts in a private mount namespace and the build's user namespace as root"]
fn red_team_a_mount_in_a_root_build_s_namespace_never_shows_outside() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let tmp = tempfile::tempdir().unwrap();
    let line = mount_below_a_shared_one(tmp.path());
    measured(&format!("mount below a shared one, root: {line}"));
    check_mount_stays_inside(&line);
}

/// In a one-id user namespace of a forked child whose `/proc/sys/user/<knob>`
/// is `limit`, the build's user namespace entered `rounds` times, each in
/// a child of its own: [`describe`]'s lines.
fn under_limit(knob: &str, limit: u32, rounds: usize) -> Vec<String> {
    let knob = knob.to_owned();
    let out = in_subreaper(move || {
        if let Err(e) = one_id_parent() {
            return format!("error: {e}");
        }
        if let Err(e) = fs::write(format!("/proc/sys/user/{knob}"), limit.to_string()) {
            return format!("error: {knob}: {e}");
        }
        (0..rounds)
            .map(|_| in_subreaper(|| describe(None)))
            .collect::<Vec<_>>()
            .join("\n")
    });
    out.lines().map(str::to_owned).collect()
}

/// T1: limits on new user and mount namespaces that a trial's namespace,
/// freed late, may still hold when this process enters: entered whole; or
/// unchanged, by an error other than [`Error::UserNamespaceBroken`], which
/// may fall back; or that error, in the new user namespace (ADR-0098).
#[test]
#[ignore = "T1: privileged: namespace limits in a user namespace of the test's own"]
fn red_team_namespace_limits_leave_the_build_whole_or_unchanged() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    for (knob, limit) in [
        ("max_user_namespaces", 0),
        ("max_user_namespaces", 1),
        ("max_mnt_namespaces", 0),
        ("max_mnt_namespaces", 1),
    ] {
        let lines = under_limit(knob, limit, 8);
        assert_eq!(lines.len(), 8, "{knob}={limit}: {lines:?}");
        for line in &lines {
            let d = Described::parse(line);
            assert_eq!(d.dumpable, "0", "{line}");
            let broken = d.outcome.starts_with("err the build's user namespace: ");
            let kind = match d.outcome.as_str() {
                "ok" => {
                    assert_eq!((d.userns.as_str(), d.mountns.as_str()), ("new", "new"));
                    assert_eq!(d.euid, "0", "{line}");
                    "entered"
                }
                _ if broken => {
                    assert_eq!(d.userns, "new", "{line}");
                    "broken"
                }
                _ => {
                    assert_eq!(
                        (d.userns.as_str(), d.mountns.as_str(), d.euid.as_str()),
                        ("same", "same", "0"),
                        "not left as it was, but may fall back: {line}"
                    );
                    "unchanged"
                }
            };
            *seen.entry(format!("{knob}={limit}: {kind}")).or_default() += 1;
        }
        if limit == 0 {
            assert!(lines.iter().all(|l| l.starts_with("err\t")), "{lines:?}");
        }
    }
    measured(&format!("namespace limits: {seen:?}"));
}
