//! `RUN`s in the build's user namespace (#78): `proc` and `sysfs` as the
//! probe chose them, an rbind of `/proc` only on request (ADR-0126),
//! `setgroups` where the map allows it, `--network=none` with loopback, in
//! T1 (root of rootless Podman) and T2 (the VM's regular user).

use std::fs;
use std::io;
use std::path::Path;

use rustix::mount::MountFlags;
use rustix::process::DumpableBehavior;

use super::super::tests::{as_user, in_subreaper, measured, subordinate_range};
use super::tests::one_id_parent;
use crate::{
    Caps, Error, KernelFs, Network, RunSpec, Sandbox, Stdio, Step, SubordinateIds, UsernsReport,
};

/// What a root `RUN` prints about its kernel filesystems, one `key=value`
/// per line, then the `/proc` and `/sys` mounts it sees as `mount: <mount
/// point> <options> <type>`. Only `grep`, `sed` and `cut`: Ubuntu's base
/// image has no `awk`. In `mountinfo`, the optional fields after the
/// options never start with `-`, the separator before the type.
const KERNEL: &str = r#"
mi=/proc/self/mountinfo
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
    let root = top.join("root");
    fs::create_dir_all(root.join("usr")).map_err(|e| s(&e))?;
    fs::create_dir(root.join("etc")).map_err(|e| s(&e))?;
    for (f, text) in [("passwd", PASSWD), ("group", GROUP)] {
        fs::write(root.join("etc").join(f), text).map_err(|e| s(&e))?;
    }
    for l in ["bin", "lib", "lib64", "sbin"] {
        std::os::unix::fs::symlink(format!("usr/{l}"), root.join(l)).map_err(|e| s(&e))?;
    }
    // In the build's mount namespace: gone with this child.
    rustix::mount::mount_bind("/usr", root.join("usr")).map_err(|e| s(&e))?;
    rustix::mount::mount_remount(root.join("usr"), MountFlags::BIND | MountFlags::RDONLY, c"")
        .map_err(|e| s(&e))?;
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
    let sandbox = Sandbox::start(caps, &sandbox_dir).map_err(|e| s(&e))?;
    for (n, (user, network, script)) in runs.iter().enumerate() {
        let log = top.join(format!("run{n}.log"));
        let file = fs::File::create(&log).map_err(|e| s(&e))?;
        let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), (*script).into()]);
        spec.user = (*user).to_owned();
        spec.network = *network;
        spec.stdout = Stdio::Fd(file.try_clone().map_err(|e| s(&e))?.into());
        spec.stderr = Stdio::Fd(file.into());
        let outcome = match sandbox.run(spec) {
            Ok(status) => format!("exit {}", status.code().unwrap_or(-1)),
            Err(Error::Setup { step, source }) => format!("setup {step:?} {source}"),
            Err(e) => format!("error {e}"),
        };
        out.push(format!("run{n}={outcome}"));
        let text = fs::read_to_string(&log).map_err(|e| s(&e))?;
        out.extend(text.lines().map(|l| format!("{n}:{l}")));
    }
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
/// `/proc/kcore` masked.
fn check_kernel_mounts(out: &str) {
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
    }
    assert!(value(out, "0:procsys").starts_with("ro,"), "{out}");
    assert_eq!(value(out, "0:kcore"), "masked", "{out}");
    assert_eq!(
        value(out, "0:proc"),
        "proc sys=sysfs",
        "the kernel filesystems' types:\n{out}"
    );
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
/// `noexec`, `/sys` all read-only. `USER _apt` gets
/// its ids through `setgroups`; `--network=none` brings `lo` up in a
/// network namespace the build owns, with no helper (ADR-0033).
#[test]
#[ignore = "T1: default: RUNs in the build's user namespace of root in a rootless Podman container"]
fn runs_in_the_build_user_namespace_get_the_fallbacks_the_probe_chose() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let out = in_subreaper(|| runs_in_build_namespace(dir.path(), None, true, &RUNS));
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
    check_kernel_mounts(&out);
    // gid 5 as the initial namespace sees it, as mountinfo shows it.
    assert!(value(&out, "0:devpts").contains("gid="), "{out}");
    assert_eq!(value(&out, "run1"), "exit 0", "{out}");
    assert_eq!(value(&out, "1:ids"), "42:65534:65534", "{out}");
    check_network(&out);
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
    check_kernel_mounts(&out);
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
    check_kernel_mounts(&out);
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
}
