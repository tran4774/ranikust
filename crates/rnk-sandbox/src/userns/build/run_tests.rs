//! `RUN`s in the build's user namespace (#78): `proc` and `sysfs` as the
//! probe chose them, an rbind of `/proc` only on request (ADR-0126),
//! `setgroups` where the map allows it, `--network=none` with loopback, in
//! T1 (root of rootless Podman) and T2 (the VM's regular user).

use std::fs;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::sync::Barrier;

use rustix::mount::MountFlags;
use rustix::process::DumpableBehavior;

use super::super::tests::{as_user, in_subreaper, measured, subordinate_range};
use super::tests::one_id_parent;
use crate::{
    BindMount, Caps, Error, KernelFs, MountKind, Network, RunSpec, Sandbox, Stdio, Step,
    SubordinateIds, UsernsReport,
};

/// What a root `RUN` prints about its kernel filesystems, one `key=value`
/// per line, then the `/proc` and `/sys` mounts it sees as `mount: <mount
/// point> <options> <type>`. Only `grep`, `sed` and `cut`: Ubuntu's base
/// image has no `awk`. In `mountinfo`, the optional fields after the
/// options never start with `-`, the separator before the type. `oom=` is
/// `written` or the shell's error when it writes its own
/// `/proc/self/oom_score_adj` unchanged. Last, `stderr=written` comes
/// through `/dev/stderr`, a link through `/proc`, opened again: appended
/// to the log both streams share (`>` would empty it), where a later line
/// through the shared descriptor would write over it.
const KERNEL: &str = r#"
mi=/proc/self/mountinfo
echo "oom=$( (read v </proc/self/oom_score_adj && echo "$v" >/proc/self/oom_score_adj && echo written) 2>&1)"
echo "pid=$$"
echo "levels=$(($(grep NSpid /proc/self/status | wc -w) - 1))"
echo "init=$(cat /proc/1/comm)"
echo "proc=$(stat -f -c %T /proc) sys=$(stat -f -c %T /sys)"
echo "procsys=$(grep -E '^([^ ]+ ){4}/proc/sys ' $mi | tail -n 1 | cut -d' ' -f6)"
if test -s /proc/kcore; then echo kcore=readable; else echo kcore=masked; fi
echo "devpts=$(grep -E '^([^ ]+ ){4}/dev/pts ' $mi | sed -E 's/^([^ ]+ ){6}([^ -][^ ]* )*- [^ ]+ [^ ]+ //')"
echo "net=$(readlink /proc/self/ns/net)"
echo "ids=$(id -u):$(id -g):$(id -G)"
grep -E '^([^ ]+ ){4}/(proc|sys)(/[^ ]*)? ' $mi |
  sed -E 's/^([^ ]+ ){4}([^ ]+) ([^ ]+) ([^ -][^ ]* )*- ([^ ]+) .*/mount: \2 \3 \5/'
echo stderr=written >>/dev/stderr
"#;

/// What a `USER` `RUN` prints.
const IDS: &str = r#"echo "ids=$(id -u):$(id -g):$(id -G)""#;

/// What a `--network=none` `RUN` prints: whether `lo` is up (its
/// `127.0.0.1` local route exists only then) and its network namespace.
const NETWORK: &str = r#"
if grep -q 127.0.0.1 /proc/net/fib_trie; then echo lo=up; else echo lo=down; fi
echo "net=$(readlink /proc/self/ns/net)"
"#;

/// One `RUN`: its `USER`, network and script.
type Run = (&'static str, Network, &'static str);

/// The root's users and groups, Debian's: the host's may differ (a
/// cloud image's `_apt` is not 42).
const PASSWD: &str =
    "root:x:0:0:root:/root:/bin/sh\n_apt:x:42:65534::/nonexistent:/usr/sbin/nologin\n";
const GROUP: &str = "root:x:0:\ntty:x:5:\nnogroup:x:65534:\n";

/// The `RUN`s every leg tries: root and the kernel filesystems, `USER
/// _apt` (42:65534, as in Debian and Ubuntu), and `--network=none`.
const RUNS: [Run; 3] = [
    ("", Network::Host, KERNEL),
    ("_apt", Network::Host, IDS),
    ("", Network::None, NETWORK),
];

/// The `argv[0]` of [`Sleeper`], which [`OUTER`] looks for.
const SLEEPER: &str = "rnk-outer-sleeper";

/// What a root `RUN` prints about a [`Sleeper`] outside it, of its kernel
/// uid, seen through an rbind of `/proc`: `outer=` its PID there, or
/// `none`; `outer-oom=` as [`KERNEL`]'s `oom=`, for its `oom_score_adj`.
const OUTER: &str = r#"
p=none
for d in /proc/[0-9]*; do
  if grep -qs "rnk-outer-[s]leeper" "$d/cmdline"; then p=${d#/proc/}; fi
done
echo "outer=$p"
echo "outer-oom=$( (read v </proc/$p/oom_score_adj && echo "$v" >/proc/$p/oom_score_adj && echo written) 2>&1)"
"#;

/// A `sleep` this test process starts, outside the build's user namespace
/// but of its kernel uid: killed and reaped on drop.
struct Sleeper(std::process::Child);

impl Sleeper {
    fn start() -> Self {
        let child = std::process::Command::new("sleep")
            // GNU coreutils' `sleep` ignores `argv[0]`; a multi-call one
            // (BusyBox, uutils) would not sleep.
            .arg0(SLEEPER)
            .arg("300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start sleep");
        Self(child)
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        // Already gone only if something else ended it: nothing to undo.
        let _ = self.0.kill();
        // Reaped, so that no zombie outlives the test.
        let _ = self.0.wait();
    }
}

/// In this forked, single-threaded child: the user-namespace probe in
/// `top`, the build's user namespace, the sandbox's probe with the
/// fallbacks the first found (and an rbind of `/proc` if `proc_rbind`),
/// then each of `runs` as a full-sandbox `RUN` in a root holding this
/// host's `/usr` (bound read-only) and [`PASSWD`] and [`GROUP`]. One line
/// per step: `caps=`, then each `RUN`'s `run<n>=` outcome and its output
/// lines prefixed `<n>:`; an `error=` line ends it early, `error=probe
/// <step> <errno>` for the sandbox's probe.
fn runs_in_build_namespace(
    top: &Path,
    sub: Option<SubordinateIds>,
    proc_rbind: bool,
    runs: &[Run],
) -> String {
    let mut out = Vec::new();
    if let Err(e) = steps(top, sub, proc_rbind, runs, &mut out) {
        out.push(format!("error={e}"));
    }
    out.join("\n")
}

fn steps(
    top: &Path,
    sub: Option<SubordinateIds>,
    proc_rbind: bool,
    runs: &[Run],
    out: &mut Vec<String>,
) -> Result<(), String> {
    let sandbox = start_sandbox(top, sub, proc_rbind, out)?;
    let root = make_root(top, "root")?;
    for (n, (user, network, script)) in runs.iter().enumerate() {
        let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), (*script).into()]);
        spec.user = (*user).to_owned();
        spec.network = *network;
        out.extend(run_logged(&sandbox, spec, top, n)?);
    }
    drop(sandbox);
    Ok(())
}

/// The user-namespace probe in `top`, the build's user namespace, the
/// sandbox's probe with the fallbacks the first found (and an rbind of
/// `/proc` if `proc_rbind`), and its [`Sandbox`]; `report=`, `userns=`
/// and `caps=` lines in `out`.
fn start_sandbox(
    top: &Path,
    sub: Option<SubordinateIds>,
    proc_rbind: bool,
    out: &mut Vec<String>,
) -> Result<Sandbox, String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    // As ranikust is before it reads a credential (ADR-0051).
    rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable).map_err(|e| s(&e))?;
    let scratch = top.join("userns-probe");
    fs::create_dir(&scratch).map_err(|e| s(&e))?;
    let report: UsernsReport = crate::probe_user_namespace(
        &rnk_rootfs::RootPath::open(&scratch).map_err(|e| s(&e))?,
        sub,
    )
    .map_err(|e| s(&e))?;
    out.push(format!("report={report}"));
    let userns = crate::enter_build_user_namespace(sub).map_err(|e| s(&e))?;
    out.push(format!("userns={userns}"));
    let sandbox_dir = top.join("sandbox");
    let fallbacks = if proc_rbind {
        report.fallbacks().allow_proc_rbind()
    } else {
        report.fallbacks()
    };
    let caps = match crate::probe_with_fallbacks(&sandbox_dir, fallbacks) {
        Ok(caps) => caps,
        Err(Error::Setup { step, source }) => return Err(format!("probe {step:?} {source}")),
        Err(e) => return Err(s(&e)),
    };
    out.push(format!("caps={caps:?}"));
    Sandbox::start(caps, &sandbox_dir).map_err(|e| s(&e))
}

/// `top/name`, a root holding this host's `/usr` (bound read-only, in the
/// build's mount namespace: gone with this child) and [`PASSWD`] and
/// [`GROUP`].
fn make_root(top: &Path, name: &str) -> Result<PathBuf, String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    let root = top.join(name);
    fs::create_dir_all(root.join("usr")).map_err(|e| s(&e))?;
    fs::create_dir(root.join("etc")).map_err(|e| s(&e))?;
    for (f, text) in [("passwd", PASSWD), ("group", GROUP)] {
        fs::write(root.join("etc").join(f), text).map_err(|e| s(&e))?;
    }
    for l in ["bin", "lib", "lib64", "sbin"] {
        std::os::unix::fs::symlink(format!("usr/{l}"), root.join(l)).map_err(|e| s(&e))?;
    }
    rustix::mount::mount_bind("/usr", root.join("usr")).map_err(|e| s(&e))?;
    rustix::mount::mount_remount(root.join("usr"), MountFlags::BIND | MountFlags::RDONLY, c"")
        .map_err(|e| s(&e))?;
    Ok(root)
}

/// Runs `spec` as `RUN` `n`, its output to a log in `top`: the `run<n>=`
/// outcome, then its output lines prefixed `<n>:`.
fn run_logged(
    sandbox: &Sandbox,
    mut spec: RunSpec,
    top: &Path,
    n: usize,
) -> Result<Vec<String>, String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    let log = top.join(format!("run{n}.log"));
    let file = fs::File::create(&log).map_err(|e| s(&e))?;
    spec.stdout = Stdio::Fd(file.try_clone().map_err(|e| s(&e))?.into());
    spec.stderr = Stdio::Fd(file.into());
    let outcome = match sandbox.run(spec) {
        Ok(status) => format!("exit {}", status.code().unwrap_or(-1)),
        Err(Error::Setup { step, source }) => format!("setup {step:?} {source}"),
        Err(e) => format!("error {e}"),
    };
    let text = fs::read_to_string(&log).map_err(|e| s(&e))?;
    Ok(std::iter::once(format!("run{n}={outcome}"))
        .chain(text.lines().map(|l| format!("{n}:{l}")))
        .collect())
}

/// What each of two `RUN`s started together prints, `{me}` and `{other}`
/// being `a` and `b` or the reverse: `overlap=yes` when the other had
/// started and not ended once this one had waited for it (both write to
/// the `/shared` they share), `leftover=yes` when a process the other left
/// behind is still there (seen only through an rbind of `/proc`), and
/// `seen=yes` when the same search finds its own, the control. The marker
/// is built at run time, so that no command line holds it but the one left
/// behind. A `RUN` that sees nothing of the other waits 10 s.
const TURN: &str = r#"
touch /shared/{me}.start
i=0
while ! test -e /shared/{other}.start && test $i -lt 100; do sleep 0.1; i=$((i+1)); done
if test -e /shared/{other}.start && ! test -e /shared/{other}.end; then
  echo overlap=yes
else
  echo overlap=no
fi
m=rnk-left
if grep -qs "$m-[b]ehind-{other}" /proc/[0-9]*/cmdline; then echo leftover=yes; else echo leftover=no; fi
sh -c 'sleep 300; :' "$m-behind-{me}" </dev/null >/dev/null 2>&1 &
j=0
until grep -qs "$m-[b]ehind-{me}" /proc/[0-9]*/cmdline || test $j -ge 50; do sleep 0.1; j=$((j+1)); done
if grep -qs "$m-[b]ehind-{me}" /proc/[0-9]*/cmdline; then echo seen=yes; else echo seen=no; fi
touch /shared/{me}.end
"#;

/// In this forked child: [`start_sandbox`], then two [`TURN`] `RUN`s on
/// roots of their own, started together from two threads, `/shared`
/// bound into both. One line per step, as [`runs_in_build_namespace`].
fn runs_started_together(top: &Path, proc_rbind: bool) -> String {
    let mut out = Vec::new();
    if let Err(e) = together(top, proc_rbind, &mut out) {
        out.push(format!("error={e}"));
    }
    out.join("\n")
}

fn together(top: &Path, proc_rbind: bool, out: &mut Vec<String>) -> Result<(), String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    let sandbox = start_sandbox(top, None, proc_rbind, out)?;
    let shared = top.join("shared");
    fs::create_dir(&shared).map_err(|e| s(&e))?;
    let mut specs = Vec::new();
    for (me, other) in [("a", "b"), ("b", "a")] {
        let root = make_root(top, &format!("root-{me}"))?;
        let script = TURN.replace("{me}", me).replace("{other}", other);
        let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
        spec.mounts.push(BindMount {
            source: shared.clone(),
            target: "/shared".into(),
            kind: MountKind::Directory,
            read_only: false,
        });
        specs.push(spec);
    }
    let start = Barrier::new(specs.len());
    let lines = std::thread::scope(|scope| {
        let runs: Vec<_> = specs
            .into_iter()
            .enumerate()
            .map(|(n, spec)| {
                let (sandbox, start) = (&sandbox, &start);
                scope.spawn(move || {
                    start.wait();
                    run_logged(sandbox, spec, top, n)
                })
            })
            .collect();
        runs.into_iter()
            .map(|r| {
                r.join()
                    .unwrap_or_else(|_| Err("a RUN's thread panicked".to_owned()))
            })
            .collect::<Result<Vec<_>, String>>()
    })?;
    out.extend(lines.into_iter().flatten());
    drop(sandbox);
    Ok(())
}

/// The value of `key=` in `out`'s lines, the first found.
fn value<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("no {key}= in:\n{out}"))
}

/// The `/proc` and `/sys` mounts root's `RUN` saw: every `proc` and
/// `sysfs` mount is `nosuid`, `nodev` and `noexec`, every mount below
/// `/sys` read-only, whatever the probe chose; `/proc/sys` read-only and
/// `/proc/kcore` masked. Every mount below `/proc` is read-only too where
/// `how` is an rbind of it, and the `RUN`'s write of its own
/// `oom_score_adj` refused; a new `proc` takes it. Either way a write
/// through `/dev/stderr`, a link through `/proc`, works.
fn check_kernel_mounts(out: &str, how: KernelFs) {
    let mounts: Vec<Vec<&str>> = out
        .lines()
        .filter_map(|l| l.strip_prefix("0:mount: "))
        .map(|l| l.split(' ').collect())
        .collect();
    assert!(
        mounts.iter().any(|m| m[0] == "/proc") && mounts.iter().any(|m| m[0] == "/sys"),
        "{out}"
    );
    for m in &mounts {
        let opts: Vec<&str> = m[1].split(',').collect();
        let (proc, sys) = (m[2] == "proc", m[0] == "/sys" || m[0].starts_with("/sys/"));
        if proc || sys || m[2] == "sysfs" {
            for flag in ["nosuid", "nodev", "noexec"] {
                assert!(opts.contains(&flag), "{m:?} lacks {flag}:\n{out}");
            }
        }
        if sys {
            assert!(opts.contains(&"ro"), "{m:?} is writable:\n{out}");
        }
        let below_proc = m[0] == "/proc" || m[0].starts_with("/proc/");
        match how {
            KernelFs::Rbind if below_proc => {
                assert!(opts.contains(&"ro"), "{m:?} is writable:\n{out}");
            }
            KernelFs::Fresh if m[0] == "/proc" => {
                assert!(
                    opts.contains(&"rw"),
                    "{m:?}: a new proc is read-only:\n{out}"
                );
            }
            KernelFs::Rbind | KernelFs::Fresh => {}
        }
    }
    assert!(value(out, "0:procsys").starts_with("ro,"), "{out}");
    assert_eq!(value(out, "0:kcore"), "masked", "{out}");
    assert_eq!(
        value(out, "0:proc"),
        "proc sys=sysfs",
        "the kernel filesystems' types:\n{out}"
    );
    let oom = value(out, "0:oom");
    match how {
        // strerror(EROFS), the shell's reason.
        KernelFs::Rbind => assert!(oom.ends_with("Read-only file system"), "{out}"),
        KernelFs::Fresh => assert_eq!(oom, "written", "{out}"),
    }
    assert_eq!(value(out, "0:stderr"), "written", "{out}");
}

/// What a fresh `proc` and an rbind of `/proc` each show: the `RUN` is
/// PID 2 under ranikust's init in a PID namespace of its own either way;
/// its `proc` shows that namespace alone, or, through the rbind, the outer
/// ones too and the outer PID 1.
fn check_proc(out: &str, how: KernelFs) {
    assert_eq!(value(out, "0:pid"), "2", "{out}");
    let levels: u32 = value(out, "0:levels").parse().expect(out);
    let init = value(out, "0:init");
    match how {
        KernelFs::Fresh => {
            assert_eq!(levels, 1, "{out}");
            assert_eq!(init, "ranikust-init", "{out}");
        }
        KernelFs::Rbind => {
            assert!(levels >= 2, "{out}");
            assert!(!init.is_empty() && init != "ranikust-init", "{out}");
        }
    }
}

/// `--network=none`: `lo` up in a network namespace the `RUN` alone has.
fn check_network(out: &str) {
    assert_eq!(value(out, "run2"), "exit 0", "{out}");
    assert_eq!(value(out, "2:lo"), "up", "{out}");
    assert_ne!(value(out, "2:net"), value(out, "0:net"), "{out}");
}

/// The [`Caps`] line's kernel filesystems.
fn chosen(out: &str) -> (KernelFs, KernelFs) {
    let caps = value(out, "caps");
    let how = |name: &str| {
        if caps.contains(&format!("{name}: Rbind")) {
            KernelFs::Rbind
        } else {
            assert!(caps.contains(&format!("{name}: Fresh")), "{caps}");
            KernelFs::Fresh
        }
    };
    (how("proc"), how("sysfs"))
}

/// A directory for a leg, in `TMPDIR` (T1: the container's `/work`).
fn leg_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// T1: root of rootless Podman, whose `/proc` is masked and whose network
/// namespace its user namespace owns, runs in the build's (multi-range)
/// user namespace with an rbind of each, `/proc`'s on request: still
/// Docker's flags and masks, the rbind's mounts all `nosuid`, `nodev`,
/// `noexec` and read-only (ADR-0132), so that a `RUN` writes nothing
/// there, not even the `oom_score_adj` of a process outside it of its
/// kernel uid. `USER _apt` gets its ids through `setgroups`;
/// `--network=none` brings `lo` up in a network namespace the build owns,
/// with no helper (ADR-0033).
#[test]
#[ignore = "T1: default: RUNs in the build's user namespace of root in a rootless Podman container"]
fn runs_in_the_build_user_namespace_get_the_fallbacks_the_probe_chose() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let sleeper = Sleeper::start();
    let runs = [RUNS[0], RUNS[1], RUNS[2], ("", Network::Host, OUTER)];
    let out = in_subreaper(|| runs_in_build_namespace(dir.path(), None, true, &runs));
    drop(sleeper);
    measured(&format!(
        "RUNs in the build's user namespace of root: {}",
        out.replace('\n', "; ")
    ));
    assert!(!out.contains("error="), "{out}");
    assert!(
        value(&out, "userns").starts_with("multi-range map"),
        "{out}"
    );
    let (proc, sysfs) = chosen(&out);
    assert_eq!((proc, sysfs), (KernelFs::Rbind, KernelFs::Rbind), "{out}");
    assert_eq!(value(&out, "run0"), "exit 0", "{out}");
    check_proc(&out, proc);
    check_kernel_mounts(&out, proc);
    // gid 5 as the initial namespace sees it, as mountinfo shows it.
    assert!(value(&out, "0:devpts").contains("gid="), "{out}");
    assert_eq!(value(&out, "run1"), "exit 0", "{out}");
    assert_eq!(value(&out, "1:ids"), "42:65534:65534", "{out}");
    check_network(&out);
    assert_eq!(value(&out, "run3"), "exit 0", "{out}");
    assert_ne!(
        value(&out, "3:outer"),
        "none",
        "the sleeper is not seen:\n{out}"
    );
    // strerror(EROFS), the shell's reason.
    assert!(
        value(&out, "3:outer-oom").ends_with("Read-only file system"),
        "{out}"
    );
}

/// T1: below a one-id user namespace (`unshare -r`), `setgroups` denied
/// and gid 5 unmapped (an rbind of `/proc` on request): root's `RUN` skips
/// `setgroups` and mounts `/dev/pts` without `gid=5`. `USER _apt`'s ids
/// are not mapped: refused at `setgid` (ADR-0098), not at `setgroups`.
#[test]
#[ignore = "T1: default: RUNs in the build's user namespace below a one-id namespace in a rootless Podman container"]
fn runs_below_one_id_skip_setgroups_and_the_tty_gid() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let out = in_subreaper(|| match one_id_parent() {
        Ok(()) => runs_in_build_namespace(dir.path(), None, true, &RUNS),
        Err(e) => format!("error={e}"),
    });
    measured(&format!(
        "RUNs in the build's user namespace below one id: {}",
        out.replace('\n', "; ")
    ));
    assert!(!out.contains("error="), "{out}");
    assert_eq!(
        value(&out, "userns"),
        "one uid map, setgroups denied",
        "{out}"
    );
    let (proc, _) = chosen(&out);
    assert_eq!(value(&out, "run0"), "exit 0", "{out}");
    check_proc(&out, proc);
    check_kernel_mounts(&out, proc);
    assert!(!value(&out, "0:devpts").contains("gid="), "{out}");
    assert!(value(&out, "0:ids").starts_with("0:0:"), "{out}");
    assert!(
        value(&out, "run1").starts_with(&format!("setup {:?} ", Step::Gid)),
        "{out}"
    );
    check_network(&out);
}

/// T2 (the VM, root dropping to the user who ran sudo): the build's user
/// namespace of a regular user through `newuidmap`, with no rbind of
/// `/proc` asked for. Where `kernel.apparmor_restrict_unprivileged_userns`
/// is 1 (Ubuntu 24.04) it is refused before anything runs; elsewhere
/// (22.04) a fresh `proc` mounts, so a `RUN` gets one, PID 2 under
/// ranikust's init, and `sysfs`, whose network namespace the user does not
/// own, an rbind of `/sys`, which needs no request.
#[test]
#[ignore = "T2: drops from root to the sudo user to run RUNs in the build's user namespace"]
fn runs_in_the_build_user_namespace_of_a_regular_user() {
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
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    let dir = leg_dir();
    rustix::fs::chown(
        dir.path(),
        Some(rustix::fs::Uid::from_raw(uid)),
        Some(rustix::fs::Gid::from_raw(gid)),
    )
    .unwrap();
    let out = as_user(uid, gid, || {
        runs_in_build_namespace(dir.path(), Some(sub), false, &RUNS)
    });
    measured(&format!(
        "RUNs in the build's user namespace of {name} (restricted: {restricted}): {}",
        out.replace('\n', "; ")
    ));
    if restricted {
        assert!(
            value(&out, "error").starts_with("no user namespace"),
            "{out}"
        );
        return;
    }
    assert!(!out.contains("error="), "{out}");
    assert!(value(&out, "userns").starts_with("newuidmap map"), "{out}");
    let (proc, sysfs) = chosen(&out);
    assert_eq!((proc, sysfs), (KernelFs::Fresh, KernelFs::Rbind), "{out}");
    assert_eq!(value(&out, "run0"), "exit 0", "{out}");
    check_proc(&out, proc);
    check_kernel_mounts(&out, proc);
    assert_eq!(value(&out, "run1"), "exit 0", "{out}");
    assert_eq!(value(&out, "1:ids"), "42:65534:65534", "{out}");
    check_network(&out);
}

/// T1: root of rootless Podman, where the user-namespace probe found that
/// only an rbind of `/proc` works: without the request, the sandbox's
/// probe refuses at [`Step::MountProc`], and no `RUN` starts.
#[test]
#[ignore = "T1: default: the build's user namespace refuses an unrequested rbind of /proc in a rootless Podman container"]
fn an_rbind_of_proc_needs_a_request() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let out = in_subreaper(|| runs_in_build_namespace(dir.path(), None, false, &RUNS));
    measured(&format!(
        "the build's user namespace of root, no rbind of /proc asked for: {}",
        out.replace('\n', "; ")
    ));
    assert!(
        value(&out, "report").contains("proc: rbind of /proc"),
        "{out}"
    );
    let eperm = io::Error::from_raw_os_error(libc::EPERM);
    assert_eq!(
        value(&out, "error"),
        format!("probe {:?} {eperm}", Step::MountProc),
        "{out}"
    );
    assert!(!out.contains("caps=") && !out.contains("run0="), "{out}");
}

/// T1: root of rootless Podman with an rbind of `/proc` (asked for): two
/// `RUN`s started together run one after the other, and the second finds
/// no process of the first, whose PID namespace went with it (ADR-0127).
#[test]
#[ignore = "T1: default: RUNs take turns under an rbind of /proc in a rootless Podman container"]
fn runs_take_turns_under_an_rbind_of_proc() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let out = in_subreaper(|| runs_started_together(dir.path(), true));
    measured(&format!(
        "two RUNs started together under an rbind of /proc: {}",
        out.replace('\n', "; ")
    ));
    assert!(!out.contains("error="), "{out}");
    assert_eq!(chosen(&out).0, KernelFs::Rbind, "{out}");
    for n in 0..2 {
        assert_eq!(value(&out, &format!("run{n}")), "exit 0", "{out}");
        assert_eq!(value(&out, &format!("{n}:overlap")), "no", "{out}");
        assert_eq!(value(&out, &format!("{n}:leftover")), "no", "{out}");
        assert_eq!(value(&out, &format!("{n}:seen")), "yes", "{out}");
    }
}

/// T1: root of rootless `podman run --privileged`, where a new `proc`
/// mounts: two `RUN`s started together run at once, the control of
/// [`runs_take_turns_under_an_rbind_of_proc`].
#[test]
#[ignore = "T1: privileged: RUNs with a new proc run side by side in the build's user namespace"]
fn runs_with_a_new_proc_run_side_by_side() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let out = in_subreaper(|| runs_started_together(dir.path(), false));
    measured(&format!(
        "two RUNs started together with a new proc: {}",
        out.replace('\n', "; ")
    ));
    assert!(!out.contains("error="), "{out}");
    assert_eq!(chosen(&out).0, KernelFs::Fresh, "{out}");
    for n in 0..2 {
        assert_eq!(value(&out, &format!("run{n}")), "exit 0", "{out}");
    }
    assert!(
        value(&out, "0:overlap") == "yes" || value(&out, "1:overlap") == "yes",
        "{out}"
    );
}

/// [`Caps`] in a line, as [`chosen`] reads it.
#[test]
fn the_caps_line_names_each_kernel_filesystem() {
    let caps = Caps {
        root_entry: crate::RootEntry::PivotRoot,
        proc: KernelFs::Rbind,
        sysfs: KernelFs::Fresh,
    };
    let out = format!("caps={caps:?}");
    assert_eq!(chosen(&out), (KernelFs::Rbind, KernelFs::Fresh));
    assert_eq!((caps.proc(), caps.sysfs()), chosen(&out));
}
