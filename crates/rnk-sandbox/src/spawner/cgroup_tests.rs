//! T2 tests of the `RUN` cgroups (ADR-0110), in the cgroups they need.
//!
//! Each outer test starts this test binary again, as an inner test, in a
//! transient systemd scope (`systemd-run --scope`, with `Delegate=yes` or
//! without) or a throwaway Docker container (`--privileged` or default),
//! and checks what it printed. The inner tests are `#[ignore = "inner:
//! ..."]`, so the T2 harness never lists them, and do nothing unless
//! `RNK_T2_CGROUP` says what they should find. The lines worth keeping go
//! to the T2 log as `measured:` lines.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio as Pipe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::tests::{
    IDLE_INIT, TestMounts, in_private_mount_namespace, kill_named, output_to, root_with_usr,
    slow_init, spinner_root,
};
use super::*;
use crate::cgroup::{Layout, MAKE_RUN, REMOVE_DEPTH, REMOVE_LIMIT, Tree, Unused};

/// What an inner test should find; unset, it does nothing.
const EXPECT: &str = "RNK_T2_CGROUP";
/// Set: the inner test first starts a companion in its cgroup, so that it
/// is not alone there.
const COMPANION: &str = "RNK_T2_CGROUP_COMPANION";
/// The image the Docker cases run this binary in: only the symlinks into
/// the host's `/usr`, which is bound in.
const IMAGE: &str = "rnk-t2-cgroup:latest";
const INNER: &str = "spawner::cgroup_tests::inner_run_cgroups";
const INNER_COMPANION: &str = "spawner::cgroup_tests::inner_companion";
const INNER_SPINNERS: &str = "spawner::cgroup_tests::inner_spinners";
const INNER_LEAVE: &str = "spawner::cgroup_tests::inner_leave";
const INNER_MARKS: &str = "spawner::cgroup_tests::inner_marks";
const INNER_FORGE: &str = "spawner::cgroup_tests::inner_forge";
const INNER_TEARDOWN: &str = "spawner::cgroup_tests::inner_teardown";
const INNER_FROZEN: &str = "spawner::cgroup_tests::inner_frozen";
const MOUNT: &str = "/sys/fs/cgroup";
/// An inner line the outer test keeps in the T2 log.
const KEEP: &str = "RNK ";

/// A `RUN` that mounts a cgroupfs of its own in a user namespace and runs
/// [`LIMITS_PY`] there.
const LIMITS: &str = "mkdir /r/cg; if unshare -Urm -C sh -c 'mount -t cgroup2 none /r/cg && \
     python3 -I /r/limits.py /r/cg'; then echo limits=ran; else echo limits=refused; fi";

/// In the `RUN`'s cgroupfs, rooted at its cgroup namespace's root: makes
/// cgroups below that root, as deep and then as many as the kernel lets it,
/// and leaves a process in the deepest. Then it writes `max` to the limit
/// files it can see and tries to make one more cgroup and one deeper: the
/// limits ranikust set must still refuse both.
const LIMITS_PY: &str = r#"import errno, os, subprocess, sys
top = sys.argv[1]
def mk(path):
    try:
        os.mkdir(path)
    except OSError as e:
        return errno.errorcode.get(e.errno, str(e.errno))
TRY = """import sys
try:
    with open(sys.argv[1], "w") as f:
        f.write("max")
except OSError:
    pass
"""
deep, deep_err, d = 0, None, top
while deep < 40:
    deep_err = mk(d + "/c")
    if deep_err:
        break
    d, deep = d + "/c", deep + 1
wide, wide_err = 0, None
while wide < 5000:
    wide_err = mk(f"{top}/w{wide}")
    if wide_err:
        break
    wide += 1
sleeper = subprocess.Popen(["/rnkcgsleep", "300"])
with open(d + "/cgroup.procs", "w") as f:
    f.write(str(sleeper.pid))
print(f"deep={deep} deep_err={deep_err} wide={wide} wide_err={wide_err}")
# Whatever these writes do, the checks below are what must hold.
LIMIT_FILES = ("cgroup.max.descendants", "cgroup.max.depth")
for name in LIMIT_FILES:
    subprocess.run([sys.executable, "-I", "-c", TRY, f"{top}/{name}"], capture_output=True)
with open(top + "/w0/cgroup.procs", "w") as f:
    f.write("0")
for name in LIMIT_FILES:
    subprocess.run(["unshare", "-C", sys.executable, "-I", "-c", TRY, f"{top}/{name}"],
                   capture_output=True)
extra = mk(top + "/extra")
if extra is None:
    # Within the bounds of ranikust's removal again.
    os.rmdir(top + "/extra")
print(f"extra={extra or 'made'}")
# One freed, so that only the depth limit can refuse the next.
os.rmdir(f"{top}/w{wide - 1}")
deeper = mk(d + "/c")
if deeper is None:
    os.rmdir(d + "/c")
print(f"deeper={deeper or 'made'}")
"#;

/// A `RUN` that mounts a cgroupfs of its own in a user namespace, whose root
/// is its own cgroup, and sets `user.delegate` there.
const FORGE: &str = "mkdir /r/cg; if unshare -Urm -C sh -c 'mount -t cgroup2 none /r/cg && \
     python3 -I -c \"import os; os.setxattr(\\\"/r/cg\\\", \\\"user.delegate\\\", b\\\"1\\\")\"'; \
     then echo forge=set; else echo forge=refused; fi";

/// A `RUN` that runs `body` with a cgroupfs of its own at `/r/cg`, in a user
/// and cgroup namespace it makes, and prints `<case>=ran`, or
/// `<case>=refused` when that or `body` failed. `body` has no `'`.
fn in_own_cgroupfs(case: &str, body: &str) -> String {
    format!(
        "mkdir -p /r/cg; if unshare -Urm -C sh -c 'mount -t cgroup2 none /r/cg || exit 1; {body}'; \
         then echo {case}=ran; else echo {case}=refused; fi"
    )
}

/// What the `RUN`s of [`inner_teardown`] leave in a cgroupfs of their own,
/// with processes in it: a frozen cgroup and an orphan, a threaded subtree,
/// mounts over their cgroups and over the cgroupfs.
const TEARDOWN: [(&str, &str); 3] = [
    (
        "frozen",
        "mkdir /r/cg/f; /rnkcgsleep 300 & echo $! >/r/cg/f/cgroup.procs; \
         echo 1 >/r/cg/f/cgroup.freeze; setsid -f /rnkcgsleep 300; i=0; \
         until grep -qx \"frozen 1\" /r/cg/f/cgroup.events; do i=$((i + 1)); \
         [ $i -lt 100 ] || exit 2; sleep 0.1; done",
    ),
    (
        "threaded",
        "mkdir -p /r/cg/t/a; echo threaded >/r/cg/t/a/cgroup.type; /rnkcgsleep 300 & p=$!; \
         echo $p >/r/cg/t/cgroup.procs; echo $p >/r/cg/t/a/cgroup.threads; \
         grep -qx \"domain threaded\" /r/cg/t/cgroup.type && grep -qx $p /r/cg/t/a/cgroup.threads",
    ),
    (
        "mounted",
        "mkdir /r/cg/x /r/cg/y; /rnkcgsleep 300 & echo $! >/r/cg/x/cgroup.procs; \
         mount -t tmpfs none /r/cg/x && mount --bind /r/cg/y /r/cg/x && mount -t tmpfs none /r/cg",
    ),
];

/// A `RUN` that freezes every cgroup of its own, itself included, after
/// making `/r/ready`.
const FREEZE_ALL: &str = "/rnkcgsleep 300 & mkdir /r/cg/w; echo $$ >/r/cg/w/cgroup.procs; \
     : >/r/ready; exec unshare -C sh -c \"echo 1 >/r/cg/cgroup.freeze; exec /rnkcgsleep 300\"";

/// Whether AppArmor gives a user namespace made without `CAP_SYS_ADMIN` no
/// capability in it (Ubuntu 23.10 and later): a `RUN` cannot mount there.
fn userns_restricted() -> bool {
    fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|s| s.trim() == "1")
}

/// AppArmor's restriction of user namespaces lifted for a test whose `RUN`s
/// need one; the drop restores it.
struct UsernsAllowed(bool);

impl UsernsAllowed {
    const SYSCTL: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";

    fn lift() -> Self {
        let restricted = userns_restricted();
        if restricted {
            fs::write(Self::SYSCTL, "0").unwrap();
        }
        Self(restricted)
    }
}

impl Drop for UsernsAllowed {
    fn drop(&mut self) {
        if !self.0 {
            return;
        }
        let r = fs::write(Self::SYSCTL, "1");
        // The VM's later tests expect it back.
        if !std::thread::panicking() {
            r.unwrap();
        } else if let Err(e) = r {
            eprintln!("{} not restored: {e}", Self::SYSCTL);
        }
    }
}

/// The cgroupfs at [`MOUNT`]: its mount's flags and its superblock's
/// options, as `/proc/self/mountinfo` lists them.
fn cgroupfs_options() -> (String, String) {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    // The last mount there is the one on top.
    info.lines()
        .rev()
        .find_map(|l| {
            let (mount, sb) = l.split_once(" - ")?;
            let mount: Vec<&str> = mount.split(' ').collect();
            let sb: Vec<&str> = sb.split(' ').collect();
            (mount.get(4) == Some(&MOUNT) && sb.first() == Some(&"cgroup2"))
                .then(|| (mount[5].to_owned(), sb.get(2).unwrap_or(&"").to_string()))
        })
        .expect("a cgroup2 mount at /sys/fs/cgroup")
}

/// Whether the cgroupfs is mounted with `nsdelegate`; the limits must hold
/// either way.
fn nsdelegate_on() -> bool {
    cgroupfs_options().1.split(',').any(|o| o == "nsdelegate")
}

/// Writes `measured: <line>` to stderr past libtest's capture, so that the
/// T2 harness's log keeps it for a passing test too.
fn measured(line: &str) {
    use std::io::Write as _;
    // A lost line loses only a number that the assertions bound anyway.
    let _ = writeln!(std::io::stderr().lock(), "measured: {line}");
}

/// This process's cgroup v2 path, relative to its cgroup namespace's root,
/// without the leading `/` (empty at that root).
fn own_cgroup() -> String {
    let text = fs::read_to_string("/proc/self/cgroup").unwrap();
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("0::/"))
        .expect("a cgroup v2 line in /proc/self/cgroup");
    line.to_owned()
}

fn cgroup_dir(rel: &str) -> PathBuf {
    if rel.is_empty() {
        PathBuf::from(MOUNT)
    } else {
        Path::new(MOUNT).join(rel)
    }
}

/// The cgroups ranikust made in `dir`: `ranikust-*`, `run-*`, `probe-*`.
fn made_in(dir: &Path) -> Vec<String> {
    let mut made: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("ranikust-") || n.starts_with("run-") || n.starts_with("probe-"))
        .collect();
    made.sort();
    made
}

fn pids_in(dir: &Path) -> usize {
    fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap()
        .lines()
        .count()
}

/// Caps `dir`'s descendants at those it has, so that the next `mkdir` in
/// it fails with `EAGAIN`; `false` lifts the cap again.
fn cap_descendants(dir: &Path, cap: bool) {
    let value = if cap {
        let stat = fs::read_to_string(dir.join("cgroup.stat")).unwrap();
        stat.lines()
            .find_map(|l| l.strip_prefix("nr_descendants "))
            .expect("nr_descendants in cgroup.stat")
            .to_owned()
    } else {
        "max".into()
    };
    fs::write(dir.join("cgroup.max.descendants"), value).unwrap();
}

/// Starts this binary's [`INNER_COMPANION`] in this process's cgroup.
fn companion() -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "--test-threads=1", INNER_COMPANION])
        .env(COMPANION, "1")
        .stdout(Pipe::null())
        .stderr(Pipe::null())
        .spawn()
        .unwrap()
}

/// Inner: a companion that only waits, to be killed.
#[test]
#[ignore = "inner: started by the cgroup T2 tests"]
fn inner_companion() {
    if std::env::var_os(COMPANION).is_some() {
        std::thread::sleep(Duration::from_secs(300));
    }
}

/// Inner: what `Tree::try_start` finds where it was started, and, with a
/// tree, `RUN`s in it: the command (and the init, nested) in the `RUN`'s
/// cgroups under Docker's default profile, which refuses it a user
/// namespace, a cancel, unconfined `RUN`s that make cgroups below their
/// own, a `RUN` failed by a cgroup that cannot be made, and nothing left
/// once the tree is dropped. `RNK_T2_CGROUP` is `nested`
/// (`single` where `cpu` is not delegated), `single`, `none` or
/// `probe-fails`.
#[test]
#[ignore = "inner: started by the cgroup T2 tests"]
fn inner_run_cgroups() {
    let Ok(expect) = std::env::var(EXPECT) else {
        println!("not started by a cgroup T2 test: nothing to do");
        return;
    };
    let mut buddy = std::env::var_os(COMPANION).map(|_| companion());
    let rel = own_cgroup();
    let dir = cgroup_dir(&rel);
    if expect == "probe-fails" {
        cap_descendants(&dir, true);
    }
    let started = Tree::try_start();
    if expect == "probe-fails" {
        cap_descendants(&dir, false);
    }
    let pid = rustix::process::getpid().as_raw_nonzero().get();
    let cpu = fs::read_to_string(dir.join("cgroup.controllers"))
        .is_ok_and(|c| c.split_whitespace().any(|c| c == "cpu"));
    let tree = match (expect.as_str(), started) {
        ("none", Err(Unused::NotDelegated(why))) => {
            println!("RNK layout=none: {why}");
            None
        }
        ("probe-fails", Err(Unused::ProbeFailed { path, failed })) => {
            println!("RNK layout=none: the probe failed in {path}: {failed:?}");
            assert_eq!(own_cgroup(), rel, "the failed probe left ranikust moved");
            assert_eq!(made_in(&dir), Vec::<String>::new());
            // The claim is released: a later start probes again.
            let again = Tree::try_start();
            assert!(again.is_ok(), "{again:?}");
            None
        }
        ("nested" | "single", Ok(tree)) => {
            let want = if expect == "nested" && cpu {
                Layout::Nested
            } else {
                Layout::Single
            };
            println!(
                "RNK layout={:?} in {} (cpu delegated: {cpu})",
                tree.layout(),
                tree.path()
            );
            assert_eq!(tree.layout(), want);
            Some(tree)
        }
        (e, r) => panic!("expected {e}, got {r:?}"),
    };
    if let Some(tree) = tree {
        let layout = tree.layout();
        let (inner_rel, inner_dir) = (rel.clone(), dir.clone());
        let others = usize::from(buddy.is_some());
        in_private_mount_namespace(move || {
            runs_in_cgroups(tree, &inner_rel, &inner_dir, pid, others);
        });
        // The tree is dropped: ranikust is back, nothing of it is left.
        assert_eq!(own_cgroup(), rel, "ranikust was not moved back");
        assert_eq!(made_in(&dir), Vec::<String>::new());
        if layout == Layout::Nested {
            let control = fs::read_to_string(dir.join("cgroup.subtree_control")).unwrap();
            assert!(
                !control.split_whitespace().any(|c| c == "cpu"),
                "+cpu left on: {control}"
            );
        }
        println!("undone: ranikust back in {}, nothing left", dir.display());
    }
    if let Some(b) = &mut buddy {
        b.kill().unwrap();
        b.wait().unwrap();
    }
}

/// The `RUN`s of [`inner_run_cgroups`], in a private mount namespace;
/// `others` is the number of processes in `dir` besides ranikust.
fn runs_in_cgroups(tree: Tree, rel: &str, dir: &Path, pid: i32, others: usize) {
    let layout = tree.layout();
    let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
    let top = mounts.path().canonicalize().unwrap();
    let root = top.join("root");
    root_with_usr(&mut mounts, &root);
    fs::create_dir(root.join("r")).unwrap();
    fs::write(root.join("r/limits.py"), LIMITS_PY).unwrap();
    fs::copy("/usr/bin/sleep", root.join("rnkcgsleep")).unwrap();
    let parent = ParentPid::new().unwrap();
    let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
    runner.use_cgroups(tree);
    // Docker's default profile, as a build has without `--seccomp`: the
    // command makes its cgroup namespace before this profile, which refuses
    // that.
    runner.seccomp = Some(SeccompProfile::docker_default().unwrap());
    let running = Running::default();
    let run = |script: &str, out: &Path| {
        let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
        output_to(out, &mut spec);
        runner.run(spec, Some(&running), |job| clone_child(job, parent))
    };
    // What a SIGKILLed ranikust with this pid would have left: skipped.
    let left = dir.join(format!("run-{pid}-0"));
    fs::create_dir(&left).unwrap();
    let first = format!("run-{pid}-1");
    // What a RUN reads of its cgroups, from its command's cgroup namespace,
    // rooted at `command`: nothing of ranikust's pid or the RUN's number,
    // which a layer would then hold.
    let views = "echo \"cmd=$(cat /proc/self/cgroup)\"; echo \"init=$(cat /proc/1/cgroup)\"";
    let init_view = match layout {
        Layout::Nested => "0::/../init",
        // The init stays in `D` with ranikust, two levels up.
        Layout::Single => "0::/../..",
    };
    let views_in = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.starts_with("cmd=") || l.starts_with("init="))
            .map(str::to_owned)
            .collect()
    };

    // 1. Under the default profile: the command (and the init, nested) in
    // the RUN's cgroups, which it cannot reach, and no user namespace for
    // it; a process it leaves dies, and the cgroups go, with the RUN.
    let out = top.join("first.out");
    let status = run(
        &format!(
            "{views}; echo \"cgroupfs=$(ls -A /sys/fs/cgroup | wc -l)\"; \
             for p in self 1; do echo \"$p $(grep ^CapPrm: /proc/$p/status) \
             $(grep ^CapEff: /proc/$p/status)\"; done; \
             if unshare -U true; then echo userns=ok; else echo userns=refused; fi; \
             /rnkcgsleep 300 & exit 3"
        ),
        &out,
    )
    .unwrap();
    let text = fs::read_to_string(&out).unwrap();
    println!("first RUN: {status:?} {text:?}");
    assert_eq!(status.code(), Some(3), "{text}");
    // `execve` rebuilds the command's sets from the bounding set, trim or not;
    // only the init's, which no `execve` rebuilds, show the init dropped
    // `CAP_SYS_ADMIN`.
    for (who, set) in [("self", "00000000a80425fb"), ("1", "0000000000000000")] {
        let want = format!("{who} CapPrm:\t{set} CapEff:\t{set}\n");
        assert!(text.contains(&want), "want {want:?}: {text}");
    }
    let first_seen = views_in(&text);
    assert_eq!(
        first_seen,
        ["cmd=0::/".to_owned(), format!("init={init_view}")],
        "{text}"
    );
    println!("{KEEP}{layout:?}: a RUN under the default profile reads {first_seen:?}");
    assert!(text.contains("cgroupfs=0\n"), "{text}");
    assert!(text.contains("userns=refused\n"), "{text}");
    assert_eq!(kill_named("rnkcgsleep"), 0, "a process of the RUN was left");
    assert!(!dir.join(&first).exists(), "{first} was not removed");

    // 2. A cancel, with the init and the command where the layout puts
    // them meanwhile.
    let second = dir.join(format!("run-{pid}-2"));
    let ready = root.join("r/ready");
    let (seen, r, took) = std::thread::scope(|s| {
        let watcher = s.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !ready.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let init = match layout {
                Layout::Nested => pids_in(&second.join("init")),
                Layout::Single => pids_in(dir),
            };
            let seen = (init, pids_in(&second.join("command")), pids_in(&second));
            running.cancel();
            (seen, Instant::now())
        });
        // A builtin makes the file: no other process is in the cgroup.
        let r = run(": >/r/ready; exec /rnkcgsleep 300", &top.join("second.out"));
        let (seen, cancelled) = watcher.join().unwrap();
        (seen, r, cancelled.elapsed())
    });
    println!("{KEEP}{layout:?}: a cancelled RUN returned {took:?} after the cancel: {r:?}");
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    // The RUN's own cgroup, with the limits, holds no process.
    match layout {
        Layout::Nested => assert_eq!(seen, (1, 1, 0), "the init and the command, apart"),
        // ranikust, the others and the init; the command.
        Layout::Single => assert_eq!(seen, (2 + others, 1, 0)),
    }
    assert!(!second.exists(), "{} was not removed", second.display());
    assert_eq!(kill_named("rnkcgsleep"), 0, "a process of the RUN was left");

    // 3. A RUN that mounts a cgroupfs of its own makes cgroups below its
    // own, as deep and as many as the limits ranikust set let it, with a
    // process in the deepest, and writes to the limit files it can see:
    // the limits hold, and all go with the RUN (ADR-0110). Unconfined, as
    // `--seccomp=unconfined`, from here on: this measures what a RUN that
    // may make a user namespace can do, which the default profile refuses.
    // The same `unshare -U` as step 1's is the control: unconfined it runs,
    // even where AppArmor leaves the namespace no capability, unless no
    // AppArmor profile lets it.
    runner.seccomp = None;
    let third = dir.join(format!("run-{pid}-3"));
    let out = top.join("limits.out");
    // `running` stays cancelled.
    let running_again = Running::default();
    let script =
        format!("if unshare -U true; then echo userns=ok; else echo userns=refused; fi; {LIMITS}");
    let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
    output_to(&out, &mut spec);
    let status = runner
        .run(spec, Some(&running_again), |job| clone_child(job, parent))
        .unwrap();
    let text = fs::read_to_string(&out).unwrap();
    let nsdelegate = nsdelegate_on();
    println!(
        "{KEEP}{layout:?}, nsdelegate {nsdelegate}: cgroups below the RUN's own: {status:?}, {}",
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    assert_eq!(status.code(), Some(0), "{text}");
    assert!(
        text.contains("userns=ok\n") || userns_restricted(),
        "an unconfined RUN is refused a user namespace: {text}"
    );
    if text.contains("limits=ran") {
        let field = |key: &str| {
            text.split_whitespace()
                .find_map(|w| w.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in {text}"))
                .to_owned()
        };
        let count = |key: &str| field(key).parse::<usize>().unwrap();
        // The RUN's cgroup namespace's root is `command`, one level below
        // the limited cgroup, beside `init` when nested.
        let leaves = match layout {
            Layout::Nested => 2,
            Layout::Single => 1,
        };
        assert_eq!(count("deep="), REMOVE_DEPTH - 1, "{text}");
        assert_eq!(
            count("deep=") + count("wide=") + leaves,
            REMOVE_LIMIT - 1,
            "{text}"
        );
        assert_eq!(field("deep_err="), "EAGAIN", "{text}");
        assert_eq!(field("wide_err="), "EAGAIN", "{text}");
        // The limits sit on a cgroup above every cgroup namespace a RUN can
        // make, so no cgroupfs it can mount shows them, `nsdelegate` or not:
        // whatever it wrote, the kernel refuses one more cgroup, and one
        // deeper, with `EAGAIN`.
        assert_eq!(field("extra="), "EAGAIN", "{text}");
        assert_eq!(field("deeper="), "EAGAIN", "{text}");
    } else {
        assert!(
            userns_restricted(),
            "a user namespace is allowed, yet: {text}"
        );
    }
    assert!(!third.exists(), "{} was not removed", third.display());
    assert_eq!(kill_named("rnkcgsleep"), 0, "a process of the RUN was left");

    // 4. A RUN whose cgroup cannot be made fails before anything ran; the
    // next one runs.
    if rel.is_empty() {
        // A cgroupns root's own limits are not writable from inside it.
        println!("{KEEP}{layout:?}: no cap at the cgroup namespace's root; checked in scopes");
    } else {
        cap_descendants(dir, true);
        let mut spec = RunSpec::new(&root, vec!["/bin/true".into()]);
        output_to(&top.join("capped.out"), &mut spec);
        let r = runner.run(spec, Some(&running_again), |job| clone_child(job, parent));
        cap_descendants(dir, false);
        println!("{KEEP}{layout:?}: a RUN without a cgroup to make: {r:?}");
        assert!(
            matches!(
                &r,
                Err(e @ Error::Cgroup { op, path, source })
                    if *op == MAKE_RUN
                        && path.parent() == Some(dir)
                        && source.raw_os_error() == Some(libc::EAGAIN)
                        && e.ran_no_command()
            ),
            "{r:?}"
        );
    }
    // A later RUN, with another number and unconfined, reads the same.
    let out = top.join("after.out");
    let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), views.into()]);
    output_to(&out, &mut spec);
    let r = runner.run(spec, Some(&running_again), |job| clone_child(job, parent));
    let text = fs::read_to_string(&out).unwrap();
    assert!(r.as_ref().is_ok_and(ExitStatus::success), "{r:?}: {text}");
    assert_eq!(views_in(&text), first_seen, "{text}");
    fs::remove_dir(&left).unwrap();
    drop(runner);
    drop(mounts);
}

/// Inner: the spinner cases of
/// `red_team_a_run_can_slow_its_inits_death_but_not_stop_it` in a nested
/// tree, with 8 and 32 spinners. `RNK_T2_CGROUP` is `spinners`.
#[test]
#[ignore = "inner: started by the cgroup T2 tests"]
fn inner_spinners() {
    if std::env::var(EXPECT).as_deref() != Ok("spinners") {
        println!("not started by a cgroup T2 test: nothing to do");
        return;
    }
    let tree = Tree::try_start().expect("a delegated tree");
    assert_eq!(
        tree.layout(),
        Layout::Nested,
        "the cases need a nested tree"
    );
    in_private_mount_namespace(move || {
        let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
        let top = mounts.path().canonicalize().unwrap();
        let root = spinner_root(&mut mounts, &top);
        let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
        runner.use_cgroups(tree);
        let cases = [
            ("control, 8 spinners", "", 8),
            ("init idle and pinned under 8 spinners", IDLE_INIT, 8),
            ("init idle and pinned under 32 spinners", IDLE_INIT, 32),
        ];
        let mut report = Vec::new();
        for (i, (case, frob, n)) in cases.into_iter().enumerate() {
            let slow = slow_init(&runner, &top, &root, i, frob, n);
            println!("{KEEP}nested, {case}: {:?}", slow.delay);
            println!("{case}: {}", slow.report);
            report.push((case, slow));
        }
        let policy = fs::read_to_string(root.join("r/policy")).unwrap_or_default();
        drop(runner);
        drop(mounts);
        assert!(
            policy.contains("SCHED_IDLE"),
            "the init was not made idle: {policy:?}"
        );
        for (case, slow) in report {
            assert!(slow.ready, "{case}: the RUN never got ready");
            assert_eq!(
                slow.left, 0,
                "{case}: a process of the RUN outlived its init"
            );
            // Without nesting: 7.6 s with 8 spinners, 30.6 s with 32.
            assert!(
                slow.delay < Duration::from_secs(2),
                "{case}: the namespace took {:?} to die in a nested tree",
                slow.delay
            );
        }
    });
}

/// Inner: starts the tree and makes a `RUN`'s cgroups, then says so and
/// waits to be `SIGKILL`ed. `RNK_T2_CGROUP` is `leave`.
#[test]
#[ignore = "inner: started by the cgroup T2 tests"]
fn inner_leave() {
    if std::env::var(EXPECT).as_deref() != Ok("leave") {
        println!("not started by a cgroup T2 test: nothing to do");
        return;
    }
    let tree = Tree::try_start().expect("a delegated tree");
    let run = tree.run_cgroup().unwrap();
    println!("RNK ready: {:?} in {}", tree.layout(), tree.path());
    std::thread::sleep(Duration::from_secs(300));
    drop(run);
}

/// Inner: prints which of systemd's delegation marks its own cgroup
/// carries, read here rather than through `cgroup.rs`. `RNK_T2_CGROUP` is
/// `marks`.
#[test]
#[ignore = "inner: started by the cgroup T2 tests"]
fn inner_marks() {
    if std::env::var(EXPECT).as_deref() != Ok("marks") {
        println!("not started by a cgroup T2 test: nothing to do");
        return;
    }
    let dir = cgroup_dir(&own_cgroup());
    let marks: Vec<String> = ["trusted.delegate", "user.delegate"]
        .into_iter()
        .map(|name| {
            let mut buf = [0u8; 8];
            let value = rustix::fs::getxattr(&dir, name, &mut buf[..])
                .map(|n| String::from_utf8_lossy(&buf[..n]).into_owned());
            format!("{name}={value:?}")
        })
        .collect();
    println!("RNK marks of {}: {}", dir.display(), marks.join(" "));
}

/// Whether systemd marks a `Delegate=yes` scope's cgroup as delegated with
/// `trusted.delegate`, the only mark a root ranikust trusts, as
/// [`inner_marks`] finds in one; the version alone does not say.
fn scope_marked() -> bool {
    let text = inner_at(
        Place::Scope { delegate: true },
        INNER_MARKS,
        &[(EXPECT, "marks")],
    );
    let line = text
        .lines()
        .find(|l| l.contains("RNK marks of "))
        .unwrap_or_else(|| panic!("no marks line: {text}"));
    measured(&format!("systemd {}: {line}", systemd_version()));
    line.contains("trusted.delegate=Ok(\"1\")")
}

/// Inner, red team (T24; ADR-0110, 1): in a scope systemd did not mark, a
/// root `RUN` that may create a user namespace mounts a cgroupfs of its own
/// and sets `user.delegate` on ranikust's cgroup, its own; a root ranikust
/// still finds no delegated tree there. Where AppArmor keeps the `RUN` from
/// it, the test sets the mark itself. `RNK_T2_CGROUP` is `forge`.
#[test]
#[ignore = "inner: started by the cgroup T2 tests"]
fn inner_forge() {
    if std::env::var(EXPECT).as_deref() != Ok("forge") {
        println!("not started by a cgroup T2 test: nothing to do");
        return;
    }
    let before = Tree::try_start();
    assert!(matches!(before, Err(Unused::NotDelegated(_))), "{before:?}");
    let dir = cgroup_dir(&own_cgroup());
    in_private_mount_namespace(move || {
        let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
        let top = mounts.path().canonicalize().unwrap();
        let root = top.join("root");
        root_with_usr(&mut mounts, &root);
        fs::create_dir(root.join("r")).unwrap();
        let parent = ParentPid::new().unwrap();
        // Unconfined (no profile), as `--seccomp=unconfined`: the forge
        // needs a user namespace, which the default profile refuses.
        let runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
        let running = Running::default();
        let out = top.join("forge.out");
        let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), FORGE.into()]);
        output_to(&out, &mut spec);
        let r = runner.run(spec, Some(&running), |job| clone_child(job, parent));
        let text = fs::read_to_string(&out).unwrap_or_default();
        println!("{KEEP}forging RUN: {r:?}, {}", text.trim());
        assert!(r.as_ref().is_ok_and(ExitStatus::success), "{r:?}: {text}");
        drop(runner);
        drop(mounts);
    });
    let mut buf = [0u8; 8];
    let mark = rustix::fs::getxattr(&dir, "user.delegate", &mut buf[..])
        .map(|n| String::from_utf8_lossy(&buf[..n]).into_owned());
    println!(
        "{KEEP}user.delegate of {} after the RUN: {mark:?}",
        dir.display()
    );
    // The control: the forge took, unless AppArmor kept the RUN from it.
    assert!(
        mark.as_deref() == Ok("1") || userns_restricted(),
        "a user namespace is allowed, yet no mark was forged: {mark:?}"
    );
    if mark.as_deref() != Ok("1") {
        // The mark the RUN could not set, set by the test itself (root).
        rustix::fs::setxattr(&dir, "user.delegate", b"1", rustix::fs::XattrFlags::empty()).unwrap();
        println!("{KEEP}user.delegate set by the test, not the RUN");
    }
    let after = Tree::try_start();
    println!("{KEEP}after the forge: {after:?}");
    assert!(matches!(after, Err(Unused::NotDelegated(_))), "{after:?}");
}

/// The cgroupfs at [`MOUNT`] remounted without `nsdelegate`, a flag of the
/// whole hierarchy; the drop remounts it as it was.
struct NoNsdelegate {
    flags: rustix::mount::MountFlags,
    options: String,
}

impl NoNsdelegate {
    fn remount() -> Self {
        use rustix::mount::MountFlags;
        let (mount, sb) = cgroupfs_options();
        let mut flags = MountFlags::empty();
        for o in mount.split(',') {
            flags |= match o {
                "rw" => MountFlags::empty(),
                "ro" => MountFlags::RDONLY,
                "nosuid" => MountFlags::NOSUID,
                "nodev" => MountFlags::NODEV,
                "noexec" => MountFlags::NOEXEC,
                "relatime" => MountFlags::RELATIME,
                "noatime" => MountFlags::NOATIME,
                "nodiratime" => MountFlags::NODIRATIME,
                o => panic!("a mount flag the test cannot keep: {o} in {mount}"),
            };
        }
        // The superblock's `rw` is the mount's.
        let options: Vec<&str> = sb
            .split(',')
            .filter(|o| !matches!(*o, "rw" | "ro"))
            .collect();
        assert!(options.contains(&"nsdelegate"), "{sb}");
        let without: Vec<&str> = options
            .iter()
            .copied()
            .filter(|o| *o != "nsdelegate")
            .collect();
        rustix::mount::mount_remount(MOUNT, flags, without.join(",").as_str()).unwrap();
        let restore = Self {
            flags,
            options: options.join(","),
        };
        assert!(!nsdelegate_on(), "{:?}", cgroupfs_options());
        assert_eq!(cgroupfs_options().0, mount, "the mount's flags changed");
        restore
    }
}

impl Drop for NoNsdelegate {
    fn drop(&mut self) {
        let r = rustix::mount::mount_remount(MOUNT, self.flags, self.options.as_str());
        // The VM's later tests need it back.
        if !std::thread::panicking() {
            r.unwrap();
            assert!(nsdelegate_on(), "{:?}", cgroupfs_options());
        } else if let Err(e) = r {
            eprintln!("nsdelegate not restored on {MOUNT}: {e}");
        }
    }
}

/// Where an outer test starts an inner one.
#[derive(Clone, Copy, Debug)]
enum Place {
    /// A transient systemd scope, delegated or not.
    Scope { delegate: bool },
    /// A Docker container of [`IMAGE`], `--privileged` or default.
    Docker { privileged: bool },
}

/// The command that starts `inner` at `place` with `env`; `name` names a
/// detached container.
fn inner_command(place: Place, inner: &str, env: &[(&str, &str)], name: Option<&str>) -> Command {
    let exe = std::env::current_exe().unwrap();
    let test_args = [
        "--ignored",
        "--exact",
        "--nocapture",
        "--test-threads=1",
        inner,
    ];
    match place {
        Place::Scope { delegate } => {
            let mut c = Command::new("systemd-run");
            c.args(["--scope", "--quiet", "--collect"]);
            if delegate {
                c.args(["-p", "Delegate=yes"]);
            }
            // With --scope, systemd-run executes the command itself, with
            // this environment.
            c.arg("--")
                .arg(&exe)
                .args(test_args)
                .envs(env.iter().copied());
            c
        }
        Place::Docker { privileged } => {
            let mut c = Command::new("docker");
            c.args([
                "run",
                "--rm",
                "--pull=never",
                "--network=none",
                "--cgroupns=private",
            ]);
            if privileged {
                c.arg("--privileged");
            }
            if let Some(n) = name {
                c.args(["-d", "--name", n]);
            }
            c.arg("-v")
                .arg(format!("{}:/t/rnk-sandbox-test:ro", exe.display()))
                .args(["-v", "/usr:/usr:ro", "--tmpfs", "/tmp:rw,exec,size=512m"]);
            for (k, v) in env {
                c.args(["-e", &format!("{k}={v}")]);
            }
            c.arg(IMAGE).arg("/t/rnk-sandbox-test").args(test_args);
            c
        }
    }
}

/// Runs `cmd` with its output in a file, killed after `limit`; its status
/// and output.
fn run_bounded(mut cmd: Command, limit: Duration) -> (ExitStatus, String) {
    let log = tempfile::NamedTempFile::new().unwrap();
    let file = log.reopen().unwrap();
    let mut child = cmd
        .stdin(Pipe::null())
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            let text = fs::read_to_string(log.path()).unwrap_or_default();
            panic!("{cmd:?} still running after {limit:?}: {text}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    (status, fs::read_to_string(log.path()).unwrap())
}

/// Runs `inner` at `place`, keeps its `RNK ` lines in the T2 log, and
/// returns what it printed; it must pass.
fn inner_at(place: Place, inner: &str, env: &[(&str, &str)]) -> String {
    let (status, text) = run_bounded(
        inner_command(place, inner, env, None),
        Duration::from_secs(300),
    );
    println!("--- {inner} at {place:?} with {env:?}: {status:?}\n{text}");
    // libtest prints the test's name before its first line.
    for line in text
        .lines()
        .filter_map(|l| l.find(KEEP).map(|i| &l[i + KEEP.len()..]))
    {
        measured(&format!("{place:?} {env:?}: {line}"));
    }
    assert!(
        status.success(),
        "{inner} at {place:?} with {env:?} failed: {text}"
    );
    text
}

/// systemd's version, from `systemctl --version`.
fn systemd_version() -> u32 {
    let out = Command::new("systemctl").arg("--version").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .expect("systemctl --version names its version")
}

/// Imports [`IMAGE`] unless Docker has it: the `bin`, `lib`, `lib64` and
/// `sbin` symlinks into `/usr`, which each container binds from the host.
fn test_image() {
    let have = Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .stdout(Pipe::null())
        .stderr(Pipe::null())
        .status()
        .unwrap();
    if have.success() {
        return;
    }
    let tree = tempfile::tempdir().unwrap();
    for l in ["bin", "lib", "lib64", "sbin"] {
        std::os::unix::fs::symlink(format!("usr/{l}"), tree.path().join(l)).unwrap();
    }
    let made = Command::new("sh")
        .args([
            "-c",
            &format!("tar -C \"$1\" -cf - . | docker import - {IMAGE}"),
            "sh",
        ])
        .arg(tree.path())
        .stdout(Pipe::null())
        .status()
        .unwrap();
    assert!(made.success(), "docker import of {IMAGE} failed");
}

/// T2 (the VM only; ADR-0110): a `RUN` gets cgroups of its own only in a
/// delegated systemd scope: nested where ranikust is alone there, else for
/// its command only; a scope without `Delegate=` and the ssh session's
/// scope get none; a probe that cannot make a cgroup leaves everything as it
/// was. Where systemd marks no delegated scope, it gets none.
#[test]
#[ignore = "T2: starts systemd scopes and moves processes between cgroups as real root; run in the test VM only"]
fn run_cgroups_follow_the_delegation_of_a_systemd_scope() {
    let marks = scope_marked();
    let delegated = Place::Scope { delegate: true };
    let layout = |text: &str| {
        text.lines()
            .find_map(|l| l.find("RNK layout=").map(|i| &l[i..]))
            .unwrap_or_default()
            .to_owned()
    };
    let nested = inner_at(
        delegated,
        INNER,
        &[(EXPECT, if marks { "nested" } else { "none" })],
    );
    let single = inner_at(
        delegated,
        INNER,
        &[
            (EXPECT, if marks { "single" } else { "none" }),
            (COMPANION, "1"),
        ],
    );
    let plain = inner_at(Place::Scope { delegate: false }, INNER, &[(EXPECT, "none")]);
    if marks {
        let failed = inner_at(delegated, INNER, &[(EXPECT, "probe-fails")]);
        assert!(layout(&failed).contains("create"), "{failed}");
        assert!(nested.contains("undone: "), "{nested}");
        assert!(single.contains("undone: "), "{single}");
        // The VM's cgroupfs has `nsdelegate`; the test without it remounts it.
        assert!(nested.contains("nsdelegate true"), "{nested}");
        assert!(single.contains("nsdelegate true"), "{single}");
    } else {
        measured(&format!(
            "systemd {}: a delegated scope is not marked",
            systemd_version()
        ));
    }
    assert!(layout(&plain).contains("no delegation mark"), "{plain}");
    // The T2 harness's own place: the ssh session's scope, never delegated.
    let own = own_cgroup();
    let session = own
        .rsplit('/')
        .next()
        .is_some_and(|s| s.starts_with("session-") && s.ends_with(".scope"));
    if session {
        let r = Tree::try_start();
        measured(&format!("the session scope {own}: {r:?}"));
        assert!(matches!(r, Err(Unused::NotDelegated(_))), "{r:?}");
    } else {
        measured(&format!(
            "not in an ssh session's scope ({own}): that case did not run"
        ));
    }
}

/// T2 (the VM only; T24, ADR-0110): on a cgroupfs without `nsdelegate`
/// too, the limits ranikust set on a `RUN`'s cgroup hold in both layouts,
/// as every cgroup namespace the `RUN` can make is rooted at its `command`
/// leaf or below, and ranikust removes what the `RUN` made. Needs a scope systemd
/// marks and a `RUN` that may create a user namespace: unconfined, as
/// `--seccomp=unconfined` (the default profile refuses it one), and Ubuntu
/// 22.04, not 24.04.
#[test]
#[ignore = "T2: remounts the VM's cgroupfs without nsdelegate and starts systemd scopes as real root; run in the test VM only"]
fn run_cgroup_limits_on_a_cgroupfs_without_nsdelegate() {
    let (marked, restricted) = (scope_marked(), userns_restricted());
    if !marked || restricted {
        measured(&format!(
            "the cgroupfs without nsdelegate: not measured (scopes marked: {marked}, user \
             namespaces restricted: {restricted})"
        ));
        return;
    }
    let delegated = Place::Scope { delegate: true };
    let off = NoNsdelegate::remount();
    let nested = inner_at(delegated, INNER, &[(EXPECT, "nested")]);
    let single = inner_at(delegated, INNER, &[(EXPECT, "single"), (COMPANION, "1")]);
    drop(off);
    for text in [&nested, &single] {
        assert!(text.contains("nsdelegate false"), "{text}");
        assert!(text.contains("limits=ran"), "{text}");
        assert!(text.contains("undone: "), "{text}");
    }
}

/// Red team (T2, the VM only; T24, ADR-0110 (1)): a root `RUN` that may
/// create a user namespace (unconfined, as `--seccomp=unconfined`) mounts
/// a cgroupfs of its own and sets
/// `user.delegate` on ranikust's cgroup, which then still does not count as
/// delegated for a root ranikust. Ubuntu 22.04 lets the forge take; where
/// AppArmor gives such a namespace no capability (Ubuntu 24.04) it fails,
/// and the test sets the mark itself, so the root rule is checked there
/// too.
#[test]
#[ignore = "T2: starts a systemd scope whose RUN mounts a cgroupfs, as real root; run in the test VM only"]
fn red_team_a_run_cannot_forge_a_delegation_mark_root_trusts() {
    let text = inner_at(
        Place::Scope { delegate: false },
        INNER_FORGE,
        &[(EXPECT, "forge")],
    );
    assert!(text.contains("after the forge: Err(NotDelegated"), "{text}");
    if userns_restricted() {
        measured(
            "AppArmor restricts user namespaces here: the forge was refused, the test set the mark",
        );
    }
}

/// T2 (the VM only; ADR-0110): in a `--privileged` container ranikust is at
/// the root of its own cgroup namespace on a read-write cgroupfs: nested as
/// the container's only process, else for the command only. A default
/// container's read-only cgroupfs gets none.
#[test]
#[ignore = "T2: runs privileged Docker containers as real root; run in the test VM only"]
fn run_cgroups_in_a_privileged_container_and_none_in_a_default_one() {
    test_image();
    let privileged = Place::Docker { privileged: true };
    let nested = inner_at(privileged, INNER, &[(EXPECT, "nested")]);
    let single = inner_at(privileged, INNER, &[(EXPECT, "single"), (COMPANION, "1")]);
    let default = inner_at(
        Place::Docker { privileged: false },
        INNER,
        &[(EXPECT, "none")],
    );
    assert!(nested.contains("undone: "), "{nested}");
    assert!(single.contains("undone: "), "{single}");
    // The VM's cgroupfs has `nsdelegate`, and a container's shares it.
    assert!(nested.contains("nsdelegate true"), "{nested}");
    assert!(single.contains("nsdelegate true"), "{single}");
    assert!(default.contains("read-only"), "{default}");
}

/// T2 (the VM only; T8, ADR-0083, ADR-0110): the variant of
/// `red_team_a_run_can_slow_its_inits_death_but_not_stop_it` in a nested
/// tree, where the init has a cgroup of its own beside the command's: with
/// 8 and 32 spinners the namespace dies within 2 s
/// (without nesting: 7.6 s and 30.6 s). Not a bound for every `RUN`
/// (threat model section 6). It runs in a delegated systemd scope or, where
/// systemd marks none, in a privileged container.
#[test]
#[ignore = "T2: starts systemd scopes or privileged containers and mounts as real root; run in the test VM only"]
fn a_nested_tree_shortens_the_spinner_cases_init_death() {
    let place = if scope_marked() {
        Place::Scope { delegate: true }
    } else {
        test_image();
        Place::Docker { privileged: true }
    };
    let text = inner_at(place, INNER_SPINNERS, &[(EXPECT, "spinners")]);
    assert_eq!(text.matches("RNK nested").count(), 3, "{text}");
}

/// Waits until `dir` is gone, up to 10 s; how long it took.
fn gone_within(dir: &Path) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if !dir.exists() {
            return Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Polls `output` until a line holds `RNK ready`, for up to 60 s.
fn wait_ready(output: impl Fn() -> String) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let text = output();
        if let Some(l) = text
            .lines()
            .find_map(|l| l.find("RNK ready").map(|i| &l[i..]))
        {
            return Some(l.to_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// The delegated systemd scope case of
/// [`a_killed_ranikusts_cgroups_go_with_its_unit_or_container`].
fn killed_in_a_scope() {
    let work = tempfile::tempdir().unwrap();
    let log = work.path().join("scope.log");
    let file = fs::File::create(&log).unwrap();
    let mut child = inner_command(
        Place::Scope { delegate: true },
        INNER_LEAVE,
        &[(EXPECT, "leave")],
        None,
    )
    .stdin(Pipe::null())
    .stdout(file.try_clone().unwrap())
    .stderr(file)
    .spawn()
    .unwrap();
    let ready = wait_ready(|| fs::read_to_string(&log).unwrap_or_default());
    // With --scope, the child is the test binary itself.
    let pid = child.id();
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    let rel = cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::/"))
        .unwrap_or_default()
        .trim_end_matches(&format!("/ranikust-{pid}"))
        .to_owned();
    let dir = cgroup_dir(&rel);
    let made = made_in(&dir);
    child.kill().unwrap();
    child.wait().unwrap();
    let gone = gone_within(&dir);
    measured(&format!(
        "a delegated systemd scope, {ready:?}: made {made:?}; {} gone after SIGKILL: {gone:?}",
        dir.display()
    ));
    assert!(
        ready.is_some(),
        "{}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert!(made.contains(&format!("run-{pid}-0")), "{made:?}");
    assert!(
        gone.is_some(),
        "systemd kept {} after the scope ended",
        dir.display()
    );
}

/// The privileged container case of
/// [`a_killed_ranikusts_cgroups_go_with_its_unit_or_container`].
fn killed_in_a_container() {
    test_image();
    let name = format!("rnk-t2-cgroup-leave-{}", std::process::id());
    let cmd = inner_command(
        Place::Docker { privileged: true },
        INNER_LEAVE,
        &[(EXPECT, "leave")],
        Some(&name),
    );
    let (status, id) = run_bounded(cmd, Duration::from_secs(60));
    assert!(status.success(), "docker run -d: {id}");
    let id = id.trim().to_owned();
    let logs = || {
        Command::new("docker")
            .args(["logs", &name])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let ready = wait_ready(logs);
    let dir = [
        format!("{MOUNT}/system.slice/docker-{id}.scope"),
        format!("{MOUNT}/docker/{id}"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|d| d.is_dir());
    let made = dir.as_deref().map(made_in).unwrap_or_default();
    let exec = Command::new("docker")
        .args(["exec", &name, "/bin/cat", "/proc/self/cgroup"])
        .output()
        .unwrap();
    let killed = Command::new("docker")
        .args(["kill", &name])
        .stdout(Pipe::null())
        .status()
        .unwrap();
    let gone = dir.as_deref().and_then(gone_within);
    measured(&format!(
        "a privileged container, {ready:?}: made {made:?} in {dir:?}; docker exec while \
         nested: {}, in {:?} {:?}; gone after SIGKILL: {gone:?}",
        exec.status,
        String::from_utf8_lossy(&exec.stdout).trim(),
        String::from_utf8_lossy(&exec.stderr).trim()
    ));
    assert!(killed.success(), "docker kill {name}");
    assert!(ready.is_some(), "the container never got ready");
    assert!(exec.status.success(), "docker exec while nested: {exec:?}");
    // runc puts it in the cgroup of the container's PID 1, ranikust's leaf.
    assert!(
        String::from_utf8_lossy(&exec.stdout)
            .lines()
            .any(|l| l == "0::/ranikust-1"),
        "docker exec while nested: {exec:?}"
    );
    assert!(made.contains(&"run-1-0".to_owned()), "{made:?} in {dir:?}");
    assert!(gone.is_some(), "{dir:?} stayed after the container ended");
}

/// T2 (the VM only; ADR-0110, answer 4): nothing in ranikust removes the
/// cgroups a `SIGKILL`ed ranikust leaves; the unit or the container that
/// ends with it does. Measured for a delegated systemd scope and a
/// privileged container, each nested with one `RUN`'s cgroups made; and
/// that `docker exec` still runs in a container ranikust nested in, in
/// ranikust's leaf.
#[test]
#[ignore = "T2: starts systemd scopes and privileged containers as real root; run in the test VM only"]
fn a_killed_ranikusts_cgroups_go_with_its_unit_or_container() {
    if scope_marked() {
        killed_in_a_scope();
    } else {
        measured("systemd marks no delegated scope: the scope case did not run");
    }
    killed_in_a_container();
}

/// The layout `RNK_T2_CGROUP`'s `<prefix>nested` or `<prefix>single` asks
/// for in `dir`, single where `cpu` is not delegated, as
/// [`inner_run_cgroups`] reads it; `None` unless it starts with `prefix`.
fn expected_layout(prefix: &str, dir: &Path) -> Option<Layout> {
    let expect = std::env::var(EXPECT).ok()?;
    let want = expect.strip_prefix(prefix)?;
    let cpu = fs::read_to_string(dir.join("cgroup.controllers"))
        .is_ok_and(|c| c.split_whitespace().any(|c| c == "cpu"));
    Some(if want == "nested" && cpu {
        Layout::Nested
    } else {
        Layout::Single
    })
}

/// A root with the host's `/usr`, `/r` and `/rnkcgsleep`.
fn teardown_root(mounts: &mut TestMounts, top: &Path) -> PathBuf {
    let root = top.join("root");
    root_with_usr(mounts, &root);
    fs::create_dir(root.join("r")).unwrap();
    fs::copy("/usr/bin/sleep", root.join("rnkcgsleep")).unwrap();
    root
}

/// The `run-` cgroups in `dir`.
fn runs_in(dir: &Path) -> Vec<String> {
    made_in(dir)
        .into_iter()
        .filter(|m| m.starts_with("run-"))
        .collect()
}

/// Waits, up to 30 s or until `done`, for `ready` to exist and a `RUN`'s
/// `command` cgroup in `dir` to read frozen; whether it did.
fn wait_frozen(ready: &Path, dir: &Path, done: &AtomicBool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !done.load(Ordering::Acquire) {
        if ready.exists()
            && runs_in(dir).iter().any(|r| {
                fs::read_to_string(dir.join(r).join("command/cgroup.events"))
                    .is_ok_and(|e| e.lines().any(|l| l == "frozen 1"))
            })
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Inner, red team (T8, T24; ADR-0110): whatever a `RUN` leaves in a
/// cgroupfs of its own ([`TEARDOWN`]), it ends, no process of it is left
/// and its cgroups go; a `RUN` that froze all of its cgroups
/// ([`FREEZE_ALL`]) is cancelled at once, with the same result.
/// `RNK_T2_CGROUP` is `teardown-nested` or `teardown-single`.
#[test]
#[ignore = "inner: started by the cgroup T2 tests"]
fn inner_teardown() {
    let dir = cgroup_dir(&own_cgroup());
    let Some(want) = expected_layout("teardown-", &dir) else {
        println!("not started by a cgroup T2 test: nothing to do");
        return;
    };
    let mut buddy = std::env::var_os(COMPANION).map(|_| companion());
    let tree = Tree::try_start().expect("a delegated tree");
    assert_eq!(tree.layout(), want);
    let inner_dir = dir.clone();
    in_private_mount_namespace(move || teardown_runs(tree, &inner_dir));
    assert_eq!(made_in(&dir), Vec::<String>::new());
    if let Some(b) = &mut buddy {
        b.kill().unwrap();
        b.wait().unwrap();
    }
}

/// The `RUN`s of [`inner_teardown`], in a private mount namespace.
fn teardown_runs(tree: Tree, dir: &Path) {
    let layout = tree.layout();
    let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
    let top = mounts.path().canonicalize().unwrap();
    let root = teardown_root(&mut mounts, &top);
    let parent = ParentPid::new().unwrap();
    // Unconfined (no profile), as `--seccomp=unconfined`: each `RUN` makes a
    // user namespace, which the default profile refuses.
    let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
    runner.use_cgroups(tree);
    let spec = |case: &str, body: &str, out: &Path| {
        let script = in_own_cgroupfs(case, body);
        let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
        output_to(out, &mut spec);
        spec
    };
    for (case, body) in TEARDOWN {
        let out = top.join(format!("{case}.out"));
        let running = Running::default();
        let r = runner.run(spec(case, body, &out), Some(&running), |job| {
            clone_child(job, parent)
        });
        let text = fs::read_to_string(&out).unwrap();
        let (left, runs) = (kill_named("rnkcgsleep"), runs_in(dir));
        println!(
            "{KEEP}{layout:?}, {case}: {r:?}, {}; processes left {left}, RUN cgroups left {runs:?}",
            text.trim()
        );
        assert!(
            r.as_ref().is_ok_and(ExitStatus::success),
            "{case}: {r:?}: {text}"
        );
        assert!(text.contains(&format!("{case}=ran")), "{case}: {text}");
        assert_eq!(left, 0, "{case}: a process of the RUN was left");
        assert_eq!(
            runs,
            Vec::<String>::new(),
            "{case}: a RUN's cgroup was left"
        );
    }

    let (ready, done) = (root.join("r/ready"), AtomicBool::new(false));
    let running = Running::default();
    let out = top.join("frozen-all.out");
    let (frozen, r, took) = std::thread::scope(|s| {
        let watcher = s.spawn(|| {
            let frozen = wait_frozen(&ready, dir, &done);
            running.cancel();
            (frozen, Instant::now())
        });
        let r = runner.run(
            spec("frozen-all", FREEZE_ALL, &out),
            Some(&running),
            |job| clone_child(job, parent),
        );
        done.store(true, Ordering::Release);
        let (frozen, cancelled) = watcher.join().unwrap();
        (frozen, r, cancelled.elapsed())
    });
    let text = fs::read_to_string(&out).unwrap();
    let (left, runs) = (kill_named("rnkcgsleep"), runs_in(dir));
    println!(
        "{KEEP}{layout:?}, a RUN that froze all of its cgroups (frozen: {frozen}): {r:?} {took:?} \
         after the cancel, {}; processes left {left}, RUN cgroups left {runs:?}",
        text.trim()
    );
    assert!(frozen, "the RUN did not freeze: {r:?}: {text}");
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}: {text}");
    // A removal that waited for the cgroups to empty would take 10 s.
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert_eq!(left, 0, "a process of the RUN was left");
    assert_eq!(runs, Vec::<String>::new(), "a RUN's cgroup was left");
    drop(runner);
    drop(mounts);
}

/// Inner: a `RUN` that froze all of its cgroups ([`FREEZE_ALL`]); says so
/// with this process's pid, then waits to be `SIGKILL`ed. `RNK_T2_CGROUP`
/// is `frozen-nested` or `frozen-single`.
#[test]
#[ignore = "inner: started by the cgroup T2 tests"]
fn inner_frozen() {
    let dir = cgroup_dir(&own_cgroup());
    let Some(want) = expected_layout("frozen-", &dir) else {
        println!("not started by a cgroup T2 test: nothing to do");
        return;
    };
    let tree = Tree::try_start().expect("a delegated tree");
    assert_eq!(tree.layout(), want);
    let pid = rustix::process::getpid().as_raw_nonzero().get();
    in_private_mount_namespace(move || {
        let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
        let top = mounts.path().canonicalize().unwrap();
        let root = teardown_root(&mut mounts, &top);
        let parent = ParentPid::new().unwrap();
        // Unconfined, as `--seccomp=unconfined`: the `RUN` makes a user
        // namespace, which the default profile refuses.
        let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
        runner.use_cgroups(tree);
        let (ready, done) = (root.join("r/ready"), AtomicBool::new(false));
        let out = top.join("frozen.out");
        let script = in_own_cgroupfs("frozen-all", FREEZE_ALL);
        let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
        output_to(&out, &mut spec);
        std::thread::scope(|s| {
            s.spawn(|| {
                let frozen = wait_frozen(&ready, &dir, &done);
                println!("RNK ready pid={pid} frozen={frozen}: {want:?}");
            });
            let running = Running::default();
            let r = runner.run(spec, Some(&running), |job| clone_child(job, parent));
            done.store(true, Ordering::Release);
            println!(
                "the RUN ended: {r:?}: {}",
                fs::read_to_string(&out).unwrap_or_default()
            );
        });
        drop(runner);
        drop(mounts);
    });
}

/// Red team (T2, the VM only; T8, T24, ADR-0110): whatever a `RUN` leaves
/// in a cgroupfs of its own (a frozen cgroup with a process in it, a
/// threaded subtree, mounts over its cgroups), it ends with no process left
/// and its cgroups removed, and one that froze all of its cgroups is
/// cancelled at once, nested and single; in a delegated systemd scope or,
/// where systemd marks none, a privileged container. The `RUN`s are
/// unconfined, as `--seccomp=unconfined`, and AppArmor's restriction of
/// user namespaces (Ubuntu 24.04) is lifted meanwhile: they make one.
#[test]
#[ignore = "T2: lifts the VM's user namespace restriction and starts systemd scopes or privileged containers as real root; run in the test VM only"]
fn a_runs_cgroups_go_with_it_whatever_it_left_in_them() {
    let _userns = UsernsAllowed::lift();
    let place = if scope_marked() {
        Place::Scope { delegate: true }
    } else {
        test_image();
        Place::Docker { privileged: true }
    };
    let nested = inner_at(place, INNER_TEARDOWN, &[(EXPECT, "teardown-nested")]);
    let single = inner_at(
        place,
        INNER_TEARDOWN,
        &[(EXPECT, "teardown-single"), (COMPANION, "1")],
    );
    for text in [&nested, &single] {
        assert_eq!(
            text.matches("processes left 0, RUN cgroups left []")
                .count(),
            TEARDOWN.len() + 1,
            "{text}"
        );
    }
}

/// Red team (T2, the VM only; T8, ADR-0110): a `SIGKILL`ed ranikust leaves
/// no process of a `RUN` that froze all of its cgroups, nested and single,
/// in a privileged container whose PID 1, a shell, outlives ranikust. The
/// `RUN` is unconfined, as `--seccomp=unconfined`, and AppArmor's
/// restriction of user namespaces is lifted meanwhile: it makes one.
#[test]
#[ignore = "T2: lifts the VM's user namespace restriction and runs privileged Docker containers as real root; run in the test VM only"]
fn no_process_of_a_frozen_run_outlives_a_killed_ranikust() {
    let _userns = UsernsAllowed::lift();
    test_image();
    killed_while_frozen("nested");
    killed_while_frozen("single");
}

/// One layout of [`no_process_of_a_frozen_run_outlives_a_killed_ranikust`].
fn killed_while_frozen(layout: &str) {
    // Nested, the shell waits in a cgroup beside ranikust's; then it stays,
    // so that the container outlives ranikust.
    let shell = if layout == "nested" {
        "mkdir /sys/fs/cgroup/side && echo $$ >/sys/fs/cgroup/side/cgroup.procs && \
         { (echo 0 >/sys/fs/cgroup/cgroup.procs && exec \"$@\") & wait; }; exec sleep 300"
    } else {
        "\"$@\" & wait; exec sleep 300"
    };
    let name = format!("rnk-t2-cgroup-frozen-{layout}-{}", std::process::id());
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new("docker");
    cmd.args([
        "run",
        "--rm",
        "-d",
        "--pull=never",
        "--network=none",
        "--cgroupns=private",
        "--privileged",
        "--name",
        &name,
    ])
    .arg("-v")
    .arg(format!("{}:/t/rnk-sandbox-test:ro", exe.display()))
    .args(["-v", "/usr:/usr:ro", "--tmpfs", "/tmp:rw,exec,size=512m"])
    .args(["-e", &format!("{EXPECT}=frozen-{layout}")])
    .args([IMAGE, "/bin/sh", "-c", shell, "sh", "/t/rnk-sandbox-test"])
    .args([
        "--ignored",
        "--exact",
        "--nocapture",
        "--test-threads=1",
        INNER_FROZEN,
    ]);
    let (status, id) = run_bounded(cmd, Duration::from_secs(60));
    assert!(status.success(), "docker run -d: {id}");
    let docker = |args: &[&str]| Command::new("docker").args(args).output().unwrap();
    let logs = || String::from_utf8_lossy(&docker(&["logs", &name]).stdout).into_owned();
    // The RUN's init and the sleeper frozen with the rest, as `<state>
    // <name>`.
    let procs = || {
        let o = docker(&["exec", &name, "/usr/bin/ps", "-eo", "stat=,comm="]);
        o.status.success().then(|| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| l.ends_with(" rnkcgsleep") || l.ends_with(" ranikust-init"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
    };
    // A zombie is dead; the init's stays, as the container's PID 1 never reaps.
    let live = |p: &Option<Vec<String>>| {
        p.as_ref()
            .map(|v| v.iter().filter(|l| !l.starts_with('Z')).count())
    };
    let ready = wait_ready(logs);
    let pid = ready
        .as_deref()
        .and_then(|l| l.split_whitespace().find_map(|w| w.strip_prefix("pid=")))
        .map(str::to_owned);
    let frozen = ready.as_deref().is_some_and(|l| l.contains("frozen=true"));
    let before = procs();
    let killed = pid
        .as_deref()
        .map(|p| docker(&["exec", &name, "/usr/bin/kill", "-KILL", p]).status);
    let start = Instant::now();
    let mut left = procs();
    while live(&left) != Some(0) && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
        left = procs();
    }
    let took = start.elapsed();
    let text = logs();
    let _ = docker(&["kill", &name]);
    measured(&format!(
        "a privileged container, {layout}: {ready:?}; the RUN's processes before the SIGKILL: \
         {before:?}, after {took:?}: {left:?}"
    ));
    assert!(ready.is_some(), "never ready: {text}");
    assert!(frozen, "the RUN did not freeze: {text}");
    assert!(killed.is_some_and(|s| s.success()), "kill: {killed:?}");
    assert_eq!(live(&before), Some(2), "control: {before:?}: {text}");
    assert_eq!(
        live(&left),
        Some(0),
        "a process of the frozen RUN outlived ranikust: {left:?}: {text}"
    );
}
