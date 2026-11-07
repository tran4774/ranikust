//! Red team of a `RUN` in the build's user namespace under rbinds of `/proc`
//! and `/sys` (#78, ADR-0127, ADR-0130, ADR-0132): writes under them, the
//! ptrace-gated files of processes outside the build, and the turns. The
//! attacks are Perl, which every harness's `/usr` holds; a write attack only
//! opens a file for writing, and never writes.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{chosen, in_subreaper, leg_dir, make_root, measured, run_logged, start_sandbox, value};
use crate::{BindMount, Error, KernelFs, MountKind, RunSpec, Sandbox, SecretFile};

/// Writes: opens for writing the kernel files of the `RUN` itself and of
/// `$outer` under `/proc`, then tries to make each mount of `@ARGV[1..]`
/// writable, in its own namespaces and then in a user, mount and PID
/// namespace of its own. One `key=ok`, `key=<errno>` or `key=missing` per
/// attempt; `ctl-` keys are controls that must work.
const WRITE_PL: &str = r#"
use strict;
use warnings;
use Fcntl qw(O_WRONLY O_RDWR);
$| = 1;
my ($outer, @targets) = @ARGV;
my %n = (unshare => @unshare@, mount => @mount@, umount2 => @umount2@,
  open_tree => @open_tree@, move_mount => @move_mount@,
  mount_setattr => @mount_setattr@, fsopen => @fsopen@,
  fsconfig => @fsconfig@, fsmount => @fsmount@);
my ($remount, $nosx, $rec) = (32 | 4096, 2 | 4 | 8, 0x8000);
my $clone = 1 | 0x80000 | $rec;
my $clr_ro = pack('QQQQ', 0, 1, 0, 0);
my $set_ro = pack('QQQQ', 1, 0, 0, 0);
sub say_r { my ($k, $ok) = @_; print "$k=", ($ok ? 'ok' : 0 + $!), "\n"; $ok }
sub sys { my ($k, $call, @a) = @_; my $r = syscall($n{$call}, @a); say_r($k, $r >= 0); $r }
sub wr {
  my ($k, $p, $mode) = @_;
  return print "$k=missing\n" unless -e $p;
  my $ok = sysopen(my $f, $p, $mode // O_WRONLY);
  say_r($k, $ok);
  close $f if $ok;
}
sub kernel_files {
  my ($pre, $proc) = @_;
  for my $t (['self', 'self'], ['outer', $outer]) {
    my ($who, $pid) = @$t;
    for my $f (qw(oom_score_adj autogroup coredump_filter clear_refs comm sched
        timerslack_ns loginuid attr/current attr/exec uid_map setgroups)) {
      wr("$pre$who-$f", "$proc/$pid/$f");
    }
    wr("$pre$who-mem", "$proc/$pid/mem", O_RDWR);
  }
  wr("${pre}thread-comm", "$proc/thread-self/comm");
  for my $s (qw(kernel/core_pattern kernel/modprobe kernel/hostname vm/drop_caches
      net/ipv4/ip_forward)) {
    (my $k = $s) =~ tr{/}{-};
    wr("${pre}sysctl-$k", "$proc/sys/$s");
  }
  wr("${pre}sysrq", "$proc/sysrq-trigger");
}
sub sysfs_files {
  my ($pre) = @_;
  for my $s (qw(kernel/uevent_helper power/state kernel/mm/transparent_hugepage/enabled
      fs/cgroup/cgroup.procs fs/cgroup/cgroup.subtree_control
      kernel/tracing/kprobe_events kernel/debug/tracing/kprobe_events bus/pci/rescan
      module/printk/parameters/time)) {
    (my $k = $s) =~ tr{/}{-};
    wr("${pre}sysfs-$k", "/sys/$s");
  }
}
mkdir '/mnt';
mkdir "/mnt/$_" for qw(ctl ctl2 proc), map { ("clone$_", "rbind$_") } 0 .. $#targets;
kernel_files('', '/proc');
sysfs_files('');
if (-d '/sys/fs/cgroup') {
  my $ok = mkdir '/sys/fs/cgroup/rnk-red-team';
  say_r('sysfs-cgroup-mkdir', $ok);
  rmdir '/sys/fs/cgroup/rnk-red-team' if $ok;
}
for my $t (@targets) {
  sys("remount:$t", 'mount', 0, $t, 0, $remount | $nosx, 0);
  sys("sb-remount:$t", 'mount', 0, $t, 0, 32 | $nosx, 0);
  sys("setattr:$t", 'mount_setattr', -100, $t, $rec, $clr_ro, 32);
  sys("clone:$t", 'open_tree', -100, $t, $clone);
}
# Every capability in a user namespace of its own, no id map needed: the
# mounts copied into its mount namespace are locked.
exit 0 if sys('unshare', 'unshare', 0x10000000 | 0x20000) < 0;
for my $i (0 .. $#targets) {
  my $t = $targets[$i];
  sys("ns-remount:$t", 'mount', 0, $t, 0, $remount | $nosx, 0);
  sys("ns-sb-remount:$t", 'mount', 0, $t, 0, 32 | $nosx, 0);
  sys("ns-setattr:$t", 'mount_setattr', -100, $t, 0, $clr_ro, 32);
  sys("ns-setattr-rec:$t", 'mount_setattr', -100, $t, $rec, $clr_ro, 32);
  if (sys("ns-rbind:$t", 'mount', $t, "/mnt/rbind$i", 0, 4096 | 16384, 0) >= 0) {
    sys("ns-rbind-remount:$t", 'mount', 0, "/mnt/rbind$i", 0, $remount | $nosx, 0);
  }
  my $fd = sys("ns-clone:$t", 'open_tree', -100, $t, $clone);
  next if $fd < 0;
  sys("ns-clone-setattr:$t", 'mount_setattr', $fd, '', 0x1000 | $rec, $clr_ro, 32);
  next if sys("ns-clone-move:$t", 'move_mount', $fd, '', -100, "/mnt/clone$i", 4) < 0;
  sys("ns-clone-remount:$t", 'mount', 0, "/mnt/clone$i", 0, $remount | $nosx, 0);
  kernel_files('ns-clone-', "/mnt/clone$i") if $t eq '/proc';
}
for my $t (qw(/proc/sys /proc/kcore /proc/acpi /sys/firmware /proc)) {
  sys("ns-umount:$t", 'umount2', $t, 2) if -e $t;
}
kernel_files('ns-', '/proc');
sysfs_files('ns-');
# Every mount below /proc and /sys, automounts the walks above made
# included: each one's flags, after an attempt to make it writable.
sub below { $_[0] =~ m{^/(proc|sys)(/|$)} }
sub mounts {
  open(my $mi, '<', '/proc/self/mountinfo') or return ();
  map { my @f = split / /; [$f[4], $f[5]] } <$mi>;
}
for my $m (mounts()) {
  my $mp = $m->[0];
  next if !below($mp) || grep { $_ eq $mp } @targets;
  sys("ns-sub-remount:$mp", 'mount', 0, $mp, 0, $remount | $nosx, 0);
  sys("ns-sub-setattr:$mp", 'mount_setattr', -100, $mp, 0, $clr_ro, 32);
}
print "ns-mount:$_->[0]=$_->[1]\n" for grep { below($_->[0]) } mounts();
# Controls: a mount this namespace made is not locked, so the same calls work.
if (sys('ctl-tmpfs', 'mount', 'none', '/mnt/ctl', 'tmpfs', 1 | $nosx, 0) >= 0) {
  sys('ctl-setattr', 'mount_setattr', -100, '/mnt/ctl', 0, $clr_ro, 32);
  sys('ctl-setattr-ro', 'mount_setattr', -100, '/mnt/ctl', 0, $set_ro, 32);
  sys('ctl-remount', 'mount', 0, '/mnt/ctl', 0, $remount | $nosx, 0);
  my $fd = sys('ctl-clone', 'open_tree', -100, '/mnt/ctl', $clone);
  sys('ctl-move', 'move_mount', $fd, '', -100, '/mnt/ctl2', 4) if $fd >= 0;
}
# A PID namespace of its own, for a new proc.
exit 0 if sys('ns-pid', 'unshare', 0x20000000) < 0;
my $child = fork;
if (defined $child && $child == 0) {
  sys('pid-proc', 'mount', 'proc', '/mnt/proc', 'proc', $nosx, 0);
  my $fs = sys('pid-fsopen', 'fsopen', 'proc', 1);
  $fs >= 0 and sys('pid-fscreate', 'fsconfig', $fs, 6, 0, 0, 0) >= 0
    and sys('pid-fsmount', 'fsmount', $fs, 1, 0);
  my $t = sys('ctl-fsopen', 'fsopen', 'tmpfs', 1);
  $t >= 0 and sys('ctl-fscreate', 'fsconfig', $t, 6, 0, 0, 0) >= 0
    and sys('ctl-fsmount', 'fsmount', $t, 1, 0);
  exit 0;
}
waitpid($child, 0);
"#;

/// Reads: for the `RUN` itself (`self.`, the control) and each `name:pid`
/// of `@targets`, the ptrace-gated files of `/proc/<pid>`, its magic links
/// (read, opened, listed past the dots, and through them `$path` and its
/// base name), and a signal 0 through a descriptor of its `/proc`
/// directory. `$secret` arrives reversed, so that no command line holds
/// it; `leak=yes` when any target's file held it. One `key=ok`,
/// `key=<errno>`, `key=read-<errno>` or `key=empty-<errno>` per attempt.
const READ_PL: &str = r#"
use strict;
use warnings;
use Fcntl qw(O_RDONLY);
$| = 1;
my ($reversed, $path, $signal, @targets) = @ARGV;
my $secret = reverse $reversed;
(my $base = $path) =~ s{.*/}{};
my $leak = 0;
sub say_r { my ($k, $ok) = @_; print "$k=", ($ok ? 'ok' : 0 + $!), "\n"; $ok }
sub slurp {
  my ($k, $p) = @_;
  open(my $f, '<', $p) or return say_r($k, 0);
  my $r = sysread($f, my $t, 1 << 20);
  return print "$k=read-", 0 + $!, "\n" unless defined $r;
  $leak = 1 if $k !~ /^self\./ && index($t, $secret) >= 0;
  print "$k=ok\n";
}
sub opened { my ($k, $p) = @_; my $ok = sysopen(my $f, $p, O_RDONLY); say_r($k, $ok); close $f if $ok }
sub dir {
  my ($k, $p) = @_;
  opendir(my $d, $p) or return say_r($k, 0);
  # Kernels list the dots before the ptrace check refuses the rest.
  my @e = grep { !/^\.\.?$/ } readdir $d;
  return print "$k=empty-", 0 + $!, "\n" unless @e;
  print "$k=ok\n";
}
my $self = readlink '/proc/self';
for my $t ("self:$self", @targets) {
  my ($name, $pid) = split /:/, $t, 2;
  my $d = "/proc/$pid";
  slurp("$name.cmdline", "$d/cmdline");
  slurp("$name.$_", "$d/$_") for qw(environ maps smaps auxv io syscall personality stack fdinfo/0);
  slurp("$name.task-environ", "$d/task/$pid/environ");
  opened("$name.$_", "$d/$_") for qw(mem pagemap);
  for my $l (qw(fd/0 cwd root exe ns/mnt ns/user ns/pid ns/net)) {
    say_r("$name.readlink-$l", defined readlink "$d/$l");
    opened("$name.open-$l", "$d/$l");
  }
  dir("$name.dir-$_", "$d/$_/") for qw(root cwd map_files);
  dir("$name.dir-task-root", "$d/task/$pid/root/");
  slurp("$name.fd0", "$d/fd/0");
  slurp("$name.root-file", "$d/root$path");
  slurp("$name.cwd-file", "$d/cwd/$base");
  if (sysopen(my $h, $d, O_RDONLY)) {
    say_r("$name.signal", syscall($signal, fileno($h), 0, 0, 0) >= 0);
  } else {
    say_r("$name.signal", 0);
  }
}
print 'leak=', ($leak ? 'yes' : 'no'), "\n";
"#;

/// The keys of [`READ_PL`] a process outside the `RUN`'s user namespace,
/// or not dumpable, must refuse it: every ptrace-gated file, magic link
/// and signal. `stack` needs `CAP_SYS_ADMIN`, the `RUN`'s own too.
const GATED: [&str; 36] = [
    "environ",
    "maps",
    "smaps",
    "auxv",
    "io",
    "syscall",
    "personality",
    "stack",
    "fdinfo/0",
    "task-environ",
    "mem",
    "pagemap",
    "readlink-fd/0",
    "open-fd/0",
    "readlink-cwd",
    "open-cwd",
    "readlink-root",
    "open-root",
    "readlink-exe",
    "open-exe",
    "readlink-ns/mnt",
    "open-ns/mnt",
    "readlink-ns/user",
    "open-ns/user",
    "readlink-ns/pid",
    "open-ns/pid",
    "readlink-ns/net",
    "open-ns/net",
    "dir-root",
    "dir-cwd",
    "dir-map_files",
    "dir-task-root",
    "fd0",
    "root-file",
    "cwd-file",
    "signal",
];

/// What the `RUN` reads of itself, the control that the reads work.
const OWN: [&str; 12] = [
    "cmdline",
    "environ",
    "maps",
    "fdinfo/0",
    "mem",
    "readlink-root",
    "dir-root",
    "dir-cwd",
    "dir-map_files",
    "open-ns/mnt",
    "fd0",
    "signal",
];

/// [`WRITE_PL`] or [`READ_PL`] with this target's syscall numbers.
fn perl(script: &str) -> String {
    [
        ("@unshare@", libc::SYS_unshare),
        ("@mount@", libc::SYS_mount),
        ("@umount2@", libc::SYS_umount2),
        ("@open_tree@", libc::SYS_open_tree),
        ("@move_mount@", libc::SYS_move_mount),
        ("@mount_setattr@", libc::SYS_mount_setattr),
        ("@fsopen@", libc::SYS_fsopen),
        ("@fsconfig@", libc::SYS_fsconfig),
        ("@fsmount@", libc::SYS_fsmount),
    ]
    .into_iter()
    .fold(script.to_owned(), |s, (name, nr)| {
        s.replace(name, &nr.to_string())
    })
}

/// The marker the outer processes hold: in their environment, in the file
/// `cred` their working directory holds, which is also their stdin.
const SECRET: &str = "rnk-red-team-secret-7f3a91";

/// A `sleep` outside the build, as `argv[0]` `name`, with [`SECRET`] in
/// its environment, its working directory `dir` and stdin `dir/cred`, as
/// `ids` (else root): killed and reaped on drop.
struct Outer(std::process::Child);

impl Outer {
    fn start(name: &str, dir: &Path, ids: Option<(u32, u32)>) -> Self {
        let mut c = std::process::Command::new("sleep");
        c.arg0(name)
            .arg("300")
            .env("RNK_RED_TEAM", SECRET)
            .current_dir(dir)
            .stdin(fs::File::open(dir.join("cred")).expect("open cred"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some((uid, gid)) = ids {
            c.uid(uid).gid(gid);
        }
        Self(c.spawn().expect("start sleep"))
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Outer {
    fn drop(&mut self) {
        // Already gone only if something else ended it.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `top/name`, holding a `cred` file with [`SECRET`], owned by `ids`.
fn cred_dir(top: &Path, name: &str, ids: Option<(u32, u32)>) -> std::path::PathBuf {
    let d = top.join(name);
    fs::create_dir(&d).unwrap();
    fs::write(d.join("cred"), SECRET).unwrap();
    if let Some((uid, gid)) = ids {
        std::os::unix::fs::chown(&d, Some(uid), Some(gid)).unwrap();
        std::os::unix::fs::chown(d.join("cred"), Some(uid), Some(gid)).unwrap();
    }
    fs::set_permissions(d.join("cred"), fs::Permissions::from_mode(0o600)).unwrap();
    d
}

/// A `RUN` of `argv`, as `user`, with `script` written at `/<file>` of a
/// root of its own (`name`): its output lines, `<n>:`-prefixed.
fn run_perl(
    sandbox: &Sandbox,
    top: &Path,
    name: &str,
    user: &str,
    (file, script): (&str, &str),
    argv: &[String],
    n: usize,
) -> Result<Vec<String>, String> {
    let root = make_root(top, name)?;
    fs::write(root.join(file), script).map_err(|e| e.to_string())?;
    let mut spec = RunSpec::new(
        &root,
        ["/usr/bin/perl".to_owned(), format!("/{file}")]
            .into_iter()
            .chain(argv.iter().cloned())
            .map(Into::into)
            .collect(),
    );
    user.clone_into(&mut spec.user);
    run_logged(sandbox, spec, top, n)
}

/// The values in `out` of every `<n>:<prefix>…` key.
fn keys<'a>(out: &'a str, n: usize, prefix: &str) -> Vec<(&'a str, &'a str)> {
    let p = format!("{n}:{prefix}");
    out.lines()
        .filter_map(|l| l.strip_prefix(&p)?.split_once('='))
        .collect()
}

/// strerror's number of `e`, as Perl's `0 + $!` prints it.
fn errno(e: i32) -> String {
    e.to_string()
}

/// `<n>:<key>` is the errno `e`.
#[track_caller]
fn assert_errno(out: &str, key: &str, e: i32) {
    assert_eq!(value(out, key), errno(e), "{key}:\n{out}");
}

/// T1: root of rootless Podman, `--seccomp=unconfined` and an rbind of
/// `/proc` (asked for): a root `RUN` opens for writing no kernel file of
/// its own or of a process outside the build of its kernel uid, no
/// sysctl and no `sysrq-trigger`, and makes no mount below `/proc` or
/// `/sys` writable: not by a remount of the mount or of its superblock,
/// `mount_setattr`, or `open_tree` and `move_mount`, in its own namespaces
/// (no `CAP_SYS_ADMIN`) or in a user and mount namespace of its own, where
/// every copied mount is locked (`EPERM`), nor by a new `proc` in a PID
/// namespace of its own; util-linux `unshare -Urm` stops at
/// `/proc/self/uid_map` (`EROFS`), and `mount` under `unshare -Um` as an
/// unmapped uid refuses before its call. Controls: the test opens the
/// outer process's `oom_score_adj` for writing, and a tmpfs the `RUN`
/// mounts in its namespace takes each call (T17, ADR-0132).
#[test]
#[ignore = "T1: default: red team of writes under an rbind of /proc in a rootless Podman container"]
fn red_team_a_run_writes_nothing_under_an_rbind_of_proc() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let top = dir.path();
    let home = cred_dir(top, "outer", None);
    let outer = Outer::start("rnk-red-team-outer", &home, None);
    let control = fs::OpenOptions::new()
        .write(true)
        .open(format!("/proc/{}/oom_score_adj", outer.pid()))
        .map(drop);
    let pid = outer.pid().to_string();
    let out = in_subreaper(|| {
        let mut out = Vec::new();
        let result = (|| {
            let mut sandbox = start_sandbox(top, None, true, &mut out)?;
            sandbox.set_seccomp(None);
            // Before Perl, in the RUN's own namespaces: the literal commands.
            let lit = r#"
for c in "unshare -Urm" "unshare -Um"; do
  o=$($c sh -c 'mount -o remount,rw /proc 2>&1 && echo REMOUNTED; mount -o remount,bind,rw /proc 2>&1 && echo REMOUNTED' 2>&1)
  echo "lit-$(echo $c | tr ' ' '_')=$(echo "$o" | tr '\n' '|')"
done
exec /usr/bin/perl /w.pl "$@"
"#;
            let root = make_root(top, "root")?;
            fs::write(root.join("w.pl"), perl(WRITE_PL)).map_err(|e| e.to_string())?;
            let spec = RunSpec::new(
                &root,
                [
                    "/bin/sh",
                    "-c",
                    lit,
                    "sh",
                    pid.as_str(),
                    "/proc",
                    "/proc/sys",
                    "/sys",
                ]
                .into_iter()
                .map(Into::into)
                .collect(),
            );
            out.extend(run_logged(&sandbox, spec, top, 0)?);
            // After every attempt, from a new process: still read-only.
            let after = "use Fcntl; print 'after-outer=', (sysopen(F, $ARGV[0], O_WRONLY) ? 'ok' : 0 + $!), qq(\n);";
            let spec = RunSpec::new(
                &root,
                [
                    "/usr/bin/perl",
                    "-e",
                    after,
                    &format!("/proc/{pid}/oom_score_adj"),
                ]
                .into_iter()
                .map(Into::into)
                .collect(),
            );
            out.extend(run_logged(&sandbox, spec, top, 1)?);
            drop(sandbox);
            Ok::<_, String>(())
        })();
        if let Err(e) = result {
            out.push(format!("error={e}"));
        }
        out.join("\n")
    });
    drop(outer);
    measured(&format!(
        "red team, writes under an rbind of /proc: {}",
        out.replace('\n', "; ")
    ));
    assert!(
        control.is_ok(),
        "control: the test may write it: {control:?}"
    );
    assert!(!out.contains("error="), "{out}");
    assert_eq!(chosen(&out), (KernelFs::Rbind, KernelFs::Rbind), "{out}");
    assert_eq!(value(&out, "run0"), "exit 0", "{out}");
    for (k, v) in keys(&out, 0, "lit-") {
        assert!(!v.contains("REMOUNTED"), "lit-{k}: {v}:\n{out}");
    }
    // The namespaces of its own and each call worked where nothing is locked.
    for k in [
        "unshare",
        "ns-clone:/proc",
        "ns-clone-move:/proc",
        "ns-rbind:/proc",
        "ctl-tmpfs",
        "ctl-setattr",
        "ctl-setattr-ro",
        "ctl-remount",
        "ctl-clone",
        "ctl-move",
        "ns-pid",
        "ctl-fsopen",
        "ctl-fscreate",
        "ctl-fsmount",
    ] {
        assert_eq!(value(&out, &format!("0:{k}")), "ok", "control {k}:\n{out}");
    }
    let tried = |k: &str| {
        !k.starts_with("ctl-")
            && !["unshare", "ns-pid", "pid-fsopen", "pid-fscreate"].contains(&k)
            && !k.starts_with("ns-clone:")
            && !k.starts_with("ns-clone-move:")
            && !k.starts_with("ns-rbind:")
    };
    for (k, v) in keys(&out, 0, "") {
        if tried(k) && !k.starts_with("lit-") {
            assert_ne!(v, "ok", "{k} was allowed:\n{out}");
        }
    }
    for k in [
        "outer-oom_score_adj",
        "self-oom_score_adj",
        "ns-outer-oom_score_adj",
    ] {
        assert_errno(&out, &format!("0:{k}"), libc::EROFS);
    }
    assert_errno(&out, "0:ns-clone-outer-oom_score_adj", libc::EROFS);
    for t in ["/proc", "/proc/sys", "/sys"] {
        for k in [
            "ns-remount",
            "ns-sb-remount",
            "ns-setattr",
            "ns-setattr-rec",
        ] {
            assert_errno(&out, &format!("0:{k}:{t}"), libc::EPERM);
        }
    }
    for k in [
        "ns-clone-setattr:/proc",
        "ns-clone-remount:/proc",
        "ns-rbind-remount:/proc",
    ] {
        assert_errno(&out, &format!("0:{k}"), libc::EPERM);
    }
    assert_errno(&out, "0:pid-proc", libc::EPERM);
    assert_read_only(&out, |_| true);
    assert_eq!(value(&out, "run1"), "exit 0", "{out}");
    assert_errno(&out, "1:after-outer", libc::EROFS);
}

/// Every mount [`WRITE_PL`] listed in its namespaces of its own, `below`
/// what is checked, is read-only.
#[track_caller]
fn assert_read_only(out: &str, check: impl Fn(&str) -> bool) {
    let listed = keys(out, 0, "ns-mount:");
    assert!(!listed.is_empty(), "no mount listed:\n{out}");
    for (m, opts) in listed.into_iter().filter(|(m, _)| check(m)) {
        assert!(
            opts.split(',').any(|o| o == "ro"),
            "{m} is writable ({opts}):\n{out}"
        );
    }
}

/// One [`READ_PL`] `RUN`'s verdicts: each target of `outer` refuses every
/// [`GATED`] key, sees no [`SECRET`], yet is seen (`cmdline`, not gated:
/// the residual); the `RUN` reads [`OWN`] of itself.
#[track_caller]
fn check_reads(out: &str, n: usize, outer: &[&str]) {
    assert_eq!(value(out, &format!("run{n}")), "exit 0", "{out}");
    for k in OWN {
        assert_eq!(
            value(out, &format!("{n}:self.{k}")),
            "ok",
            "control self.{k}:\n{out}"
        );
    }
    for t in outer {
        assert_eq!(
            value(out, &format!("{n}:{t}.cmdline")),
            "ok",
            "{t} unseen:\n{out}"
        );
        for k in GATED {
            let v = value(out, &format!("{n}:{t}.{k}"));
            assert_ne!(v, "ok", "{t}.{k} was allowed:\n{out}");
        }
        // pidfd_send_signal: a /proc directory of a process outside the
        // caller's PID namespace is no pidfd it may use.
        assert_errno(out, &format!("{n}:{t}.signal"), libc::EINVAL);
    }
    assert_eq!(value(out, &format!("{n}:leak")), "no", "{out}");
}

/// The [`READ_PL`] arguments: the reversed secret, the path to look for,
/// `pidfd_send_signal`'s number, then the targets.
fn read_args(path: &Path, targets: &[(&str, u32)]) -> Vec<String> {
    [
        SECRET.chars().rev().collect(),
        path.to_str().unwrap().to_owned(),
        libc::SYS_pidfd_send_signal.to_string(),
    ]
    .into_iter()
    .chain(targets.iter().map(|(n, p)| format!("{n}:{p}")))
    .collect()
}

/// T1: root of rootless Podman with an rbind of `/proc` (asked for),
/// measured end to end through ranikust's sandbox (ADR-0130's scenario 3):
/// a root `RUN` reads no ptrace-gated file (`environ`, `mem`, `maps`,
/// `fd/N`, `root`, `cwd`, `exe`, `ns/*`…), follows no magic link and sends
/// no signal through a `/proc/<pid>` directory, of a dumpable `sleep`
/// outside the build of its kernel uid, the test process, the container's
/// PID 1 or the non-dumpable process that holds the `Sandbox` (in the
/// build's user namespace); nor reaches through them the credential file
/// they hold open and sit in. `USER _apt` gets the same of a `sleep` of its
/// own kernel uid. It sees each one's command line (the residual). Control:
/// the test reads the outer `sleep`'s environment (T11, T12, T21).
#[test]
#[ignore = "T1: default: red team of ptrace-gated /proc files under an rbind of /proc in a rootless Podman container"]
fn red_team_a_run_reads_no_gated_proc_file_of_a_process_outside_the_build() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let top = dir.path();
    // The `_apt` sleeper's directory is below it.
    fs::set_permissions(top, fs::Permissions::from_mode(0o711)).unwrap();
    let apt = Some((42, 65534));
    let home = cred_dir(top, "outer", None);
    let home42 = cred_dir(top, "outer42", apt);
    let outer = Outer::start("rnk-red-team-outer", &home, None);
    let outer42 = Outer::start("rnk-red-team-outer42", &home42, apt);
    let control = fs::read(format!("/proc/{}/environ", outer.pid()))
        .map(|e| String::from_utf8_lossy(&e).contains(SECRET));
    let me = std::process::id();
    let (pid, pid42) = (outer.pid(), outer42.pid());
    let out = in_subreaper(|| {
        let mut out = Vec::new();
        let result = (|| {
            // The process holding the `Sandbox` holds the credential too.
            let cred = fs::File::open(home.join("cred")).map_err(|e| e.to_string())?;
            // SAFETY: both descriptors are open; this forked child is
            // single-threaded and owns no other handle of descriptor 0.
            if unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&cred), 0) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            std::env::set_current_dir(&home).map_err(|e| e.to_string())?;
            let holder = std::process::id();
            let sandbox = start_sandbox(top, None, true, &mut out)?;
            let script = ("r.pl", READ_PL);
            let targets = [
                ("outer", pid),
                ("test", me),
                ("init", 1),
                ("holder", holder),
            ];
            let args = read_args(&home.join("cred"), &targets);
            out.extend(run_perl(&sandbox, top, "root", "", script, &args, 0)?);
            let args = read_args(&home42.join("cred"), &[("outer42", pid42)]);
            out.extend(run_perl(&sandbox, top, "root42", "_apt", script, &args, 1)?);
            drop(sandbox);
            Ok::<_, String>(())
        })();
        if let Err(e) = result {
            out.push(format!("error={e}"));
        }
        out.join("\n")
    });
    drop((outer, outer42));
    measured(&format!(
        "red team, gated /proc files under an rbind of /proc: {}",
        out.replace('\n', "; ")
    ));
    assert!(matches!(control, Ok(true)), "control: {control:?}");
    assert!(!out.contains("error="), "{out}");
    assert_eq!(chosen(&out).0, KernelFs::Rbind, "{out}");
    check_reads(&out, 0, &["outer", "test", "init", "holder"]);
    check_reads(&out, 1, &["outer42"]);
}

/// T1: two builds in one container, each in a user namespace of its own,
/// both with an rbind of `/proc`: their `RUN`s are not each other's turns
/// (ADR-0127's lock is per process), so they run at once, yet a root `RUN`
/// of one reads no ptrace-gated file of the other's root `RUN`, of the same
/// kernel uid and dumpable, nor its secret mount through its `root`, nor
/// signals it (T12).
#[test]
#[ignore = "T1: default: red team of two builds' RUNs under rbinds of /proc in a rootless Podman container"]
fn red_team_runs_of_two_builds_reach_no_gated_proc_file_of_each_other() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let shared = dir.path().join("shared");
    fs::create_dir(&shared).unwrap();
    let tops = ["a", "b"].map(|t| dir.path().join(t));
    for t in &tops {
        fs::create_dir(t).unwrap();
    }
    let bind = BindMount {
        source: shared.clone(),
        target: "/shared".into(),
        kind: MountKind::Directory,
        read_only: false,
    };
    let secret_path = "/run/secrets/rnk-red-team";
    let waiting = r#"
touch /shared/b.started
i=0
while ! test -e /shared/a.done && test $i -lt 900; do sleep 0.1; i=$((i+1)); done
"#;
    let reading = r#"
i=0
while ! test -e /shared/b.started && test $i -lt 900; do sleep 0.1; i=$((i+1)); done
p=$(grep -ls "rnk-other-build-[r]un" /proc/[0-9]*/cmdline | head -n 1 | cut -d/ -f3)
echo "other=${p:-none}"
/usr/bin/perl /r.pl "$@" "other:${p:-none}"
touch /shared/a.done
"#;
    let build = |top: &Path, a: bool| {
        let mut out = Vec::new();
        let result = (|| {
            let sandbox = start_sandbox(top, None, true, &mut out)?;
            let root = make_root(top, "root")?;
            fs::create_dir_all(root.join("run/secrets")).map_err(|e| e.to_string())?;
            let mut spec = if a {
                fs::write(root.join("r.pl"), READ_PL).map_err(|e| e.to_string())?;
                let args = read_args(Path::new(secret_path), &[]);
                RunSpec::new(
                    &root,
                    [
                        "/bin/sh".to_owned(),
                        "-c".into(),
                        reading.into(),
                        "sh".into(),
                    ]
                    .into_iter()
                    .chain(args)
                    .map(Into::into)
                    .collect(),
                )
            } else {
                let mut spec = RunSpec::new(
                    &root,
                    ["/bin/sh", "-c", waiting, "rnk-other-build-run"]
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                );
                spec.env.push(("RNK_RED_TEAM".into(), SECRET.into()));
                spec.secrets.push(SecretFile {
                    data: Arc::from(SECRET.as_bytes()),
                    target: secret_path.into(),
                    uid: 0,
                    gid: 0,
                    mode: 0o400,
                });
                spec
            };
            spec.mounts.push(bind.clone());
            out.extend(run_logged(&sandbox, spec, top, 0)?);
            drop(sandbox);
            Ok::<_, String>(())
        })();
        if let Err(e) = result {
            out.push(format!("error={e}"));
        }
        out.join("\n")
    };
    let (a, b) = std::thread::scope(|s| {
        let b = s.spawn(|| in_subreaper(|| build(&tops[1], false)));
        let a = s.spawn(|| in_subreaper(|| build(&tops[0], true)));
        (a.join().unwrap(), b.join().unwrap())
    });
    measured(&format!(
        "red team, two builds' RUNs under rbinds of /proc: a: {}; b: {}",
        a.replace('\n', "; "),
        b.replace('\n', "; ")
    ));
    assert!(!a.contains("error=") && !b.contains("error="), "{a}\n{b}");
    assert_eq!(chosen(&a).0, KernelFs::Rbind, "{a}");
    assert_eq!(value(&b, "run0"), "exit 0", "{b}");
    assert_ne!(
        value(&a, "0:other"),
        "none",
        "the other RUN is unseen:\n{a}"
    );
    assert_eq!(value(&a, "run0"), "exit 0", "{a}");
    for k in OWN {
        assert_eq!(
            value(&a, &format!("0:self.{k}")),
            "ok",
            "control self.{k}:\n{a}"
        );
    }
    assert_eq!(value(&a, "0:other.cmdline"), "ok", "{a}");
    for k in GATED {
        let v = value(&a, &format!("0:other.{k}"));
        assert!(v != "ok", "other.{k} was allowed:\n{a}");
    }
    assert_errno(&a, "0:other.signal", libc::EINVAL);
    assert_eq!(value(&a, "0:leak"), "no", "{a}");
}

/// What `RUN` `a` does before it is cancelled: leaves a process behind in
/// each way a shell can (background, `setsid`, double fork, `nohup`
/// ignoring `SIGHUP` and `SIGTERM`), sees all four, then waits.
const LEAVER: &str = r#"
m=rnk-left
sh -c 'sleep 300; :' "$m-behind-a-bg" </dev/null >/dev/null 2>&1 &
setsid sh -c 'sleep 300; :' "$m-behind-a-setsid" </dev/null >/dev/null 2>&1 &
( sh -c 'sleep 300; :' "$m-behind-a-double" </dev/null >/dev/null 2>&1 & )
nohup sh -c 'trap "" HUP TERM; sleep 300; :' "$m-behind-a-nohup" </dev/null >/dev/null 2>&1 &
j=0
until test "$(grep -ls "$m-[b]ehind-a-" /proc/[0-9]*/cmdline | wc -l)" -ge 4 || test $j -ge 100; do sleep 0.1; j=$((j+1)); done
echo "a-left=$(grep -ls "$m-[b]ehind-a-" /proc/[0-9]*/cmdline | wc -l)"
touch /shared/a.ready
sleep 300
"#;

/// What `RUN`s `b` and `c` print: `a-leftover=` whether a process `a` left
/// is still there; the rest as [`super::TURN`].
const AFTER: &str = r#"
touch /shared/{me}.start
m=rnk-left
if grep -qs "$m-[b]ehind-a-" /proc/[0-9]*/cmdline; then echo a-leftover=yes; else echo a-leftover=no; fi
i=0
while ! test -e /shared/{other}.start && test $i -lt 100; do sleep 0.1; i=$((i+1)); done
if test -e /shared/{other}.start && ! test -e /shared/{other}.end; then echo overlap=yes; else echo overlap=no; fi
if grep -qs "$m-[b]ehind-{other}" /proc/[0-9]*/cmdline; then echo leftover=yes; else echo leftover=no; fi
setsid sh -c 'sleep 300; :' "$m-behind-{me}" </dev/null >/dev/null 2>&1 &
j=0
until grep -qs "$m-[b]ehind-{me}" /proc/[0-9]*/cmdline || test $j -ge 50; do sleep 0.1; j=$((j+1)); done
if grep -qs "$m-[b]ehind-{me}" /proc/[0-9]*/cmdline; then echo seen=yes; else echo seen=no; fi
touch /shared/{me}.end
"#;

/// The command lines in this process's `/proc` holding `marker`.
fn holding(marker: &str) -> Vec<String> {
    let mut found = Vec::new();
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        if let Ok(c) = fs::read(e.path().join("cmdline")) {
            let c = String::from_utf8_lossy(&c).replace('\0', " ");
            if c.contains(marker) {
                found.push(format!("{}: {c}", e.file_name().to_string_lossy()));
            }
        }
    }
    found
}

/// Until `path` exists, for up to `limit`: whether it does.
fn appears(path: &Path, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    while !path.exists() {
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// T1: root of rootless Podman with an rbind of `/proc` (asked for), two
/// `Sandbox`es in one process: `RUN` `a` of the first leaves processes
/// behind in four ways and is cancelled mid-turn; `RUN`s `b` and `c` of the
/// second, as two jobs, wait until it is reaped, then run one at a time
/// and find nothing `a` left. Nothing any of them left outlives them
/// (T8, T12, ADR-0127).
#[test]
#[ignore = "T1: default: red team of a RUN cancelled mid-turn under an rbind of /proc in a rootless Podman container"]
fn red_team_a_cancelled_run_leaves_no_process_for_the_next_turn() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let top = dir.path();
    let out = in_subreaper(|| {
        let mut out = Vec::new();
        let result = (|| {
            let s = |e: &dyn std::fmt::Display| e.to_string();
            let first = start_sandbox(top, None, true, &mut out)?;
            let both = crate::Fallbacks {
                proc: true,
                sysfs: true,
            };
            let caps =
                crate::probe_with_fallbacks(&top.join("sandbox2"), both).map_err(|e| s(&e))?;
            let second = Sandbox::start(caps, &top.join("sandbox2")).map_err(|e| s(&e))?;
            let shared = top.join("shared");
            fs::create_dir(&shared).map_err(|e| s(&e))?;
            let spec = |name: &str, script: String| -> Result<RunSpec, String> {
                let root = make_root(top, name)?;
                let mut spec =
                    RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
                spec.mounts.push(BindMount {
                    source: shared.clone(),
                    target: "/shared".into(),
                    kind: MountKind::Directory,
                    read_only: false,
                });
                Ok(spec)
            };
            let a = spec("root-a", LEAVER.to_owned())?;
            let [b, c] = [("b", "c"), ("c", "b")].map(|(me, other)| {
                spec(
                    &format!("root-{me}"),
                    AFTER.replace("{me}", me).replace("{other}", other),
                )
            });
            let (b, c) = (b?, c?);
            let lines = std::thread::scope(|scope| {
                let ra = scope.spawn(|| run_logged(&first, a, top, 0));
                let ready = appears(&shared.join("a.ready"), Duration::from_secs(60));
                let rb = scope.spawn(|| run_logged(&second, b, top, 1));
                let rc = scope.spawn(|| run_logged(&second, c, top, 2));
                // Both wait for `a`'s turn.
                std::thread::sleep(Duration::from_secs(2));
                let waited = !shared.join("b.start").exists() && !shared.join("c.start").exists();
                let left = holding("rnk-left-behind-a-").len();
                first.cancel();
                let mut lines = vec![
                    format!("ready={ready}"),
                    format!("waited={waited}"),
                    format!("left-before-cancel={left}"),
                ];
                for r in [ra, rb, rc] {
                    lines.extend(
                        r.join()
                            .map_err(|_| "a RUN's thread panicked".to_owned())??,
                    );
                }
                Ok::<_, String>(lines)
            })?;
            out.extend(lines);
            out.push(format!(
                "survivors={}",
                holding("rnk-left-behind").join(" | ")
            ));
            drop((first, second));
            Ok::<_, String>(())
        })();
        if let Err(e) = result {
            out.push(format!("error={e}"));
        }
        out.join("\n")
    });
    measured(&format!(
        "red team, a RUN cancelled mid-turn: {}",
        out.replace('\n', "; ")
    ));
    assert!(!out.contains("error="), "{out}");
    assert_eq!(chosen(&out).0, KernelFs::Rbind, "{out}");
    assert_eq!(value(&out, "ready"), "true", "{out}");
    assert_eq!(value(&out, "0:a-left"), "4", "control: a left four:\n{out}");
    assert_eq!(value(&out, "left-before-cancel"), "4", "control:\n{out}");
    assert_eq!(
        value(&out, "waited"),
        "true",
        "b or c ran during a's turn:\n{out}"
    );
    assert_eq!(
        value(&out, "run0"),
        format!("error {}", Error::Cancelled),
        "{out}"
    );
    for n in 1..3 {
        assert_eq!(value(&out, &format!("run{n}")), "exit 0", "{out}");
        for (k, want) in [
            ("a-leftover", "no"),
            ("overlap", "no"),
            ("leftover", "no"),
            ("seen", "yes"),
        ] {
            assert_eq!(value(&out, &format!("{n}:{k}")), want, "{n}:{k}:\n{out}");
        }
    }
    assert_eq!(value(&out, "survivors"), "", "{out}");
}

/// T2c (rootful Docker with `CAP_SYS_ADMIN`, whose `/proc` is masked; in
/// the VM, the host's `/proc` and `/sys`): real root's build user namespace
/// maps every id, so a root `RUN` is kernel uid 0, where a writable sysctl
/// or `/sys` file reaches the host (`core_pattern`, `uevent_helper`,
/// `power/state`, the root cgroup). Under `--seccomp=unconfined`, with the
/// rbind of `/sys` (and of `/proc` where a new one is refused), every mount
/// below `/sys` is read-only, and below `/proc` too under the rbind; the
/// `RUN` opens none of them for writing, nor a sysctl or `sysrq-trigger`,
/// and makes none writable, in its namespaces or new ones (where
/// Ubuntu 24.04's AppArmor leaves its user namespace no capability, they
/// stop there; the new namespaces' locks show elsewhere, as on 22.04). A
/// new `proc` keeps the `RUN`'s own files writable (T17, T24, ADR-0132).
#[test]
#[ignore = "T2c: sysadmin: red team of a uid 0 RUN under rbinds of /proc and /sys in the build's user namespace of real root"]
fn red_team_a_uid_0_run_writes_nothing_under_rbinds_of_proc_and_sys() {
    assert!(rustix::process::geteuid().is_root(), "run as root");
    let dir = leg_dir();
    let top = dir.path();
    let home = cred_dir(top, "outer", None);
    let outer = Outer::start("rnk-red-team-outer", &home, None);
    let pid = outer.pid().to_string();
    let restricted = fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1");
    let out = in_subreaper(|| {
        let mut out = Vec::new();
        let result = (|| {
            let mut sandbox = start_sandbox(top, None, true, &mut out)?;
            sandbox.set_seccomp(None);
            let sh = r#"
mi=/proc/self/mountinfo
grep -E '^([^ ]+ ){4}/(proc|sys)(/[^ ]*)? ' $mi |
  sed -E 's/^([^ ]+ ){4}([^ ]+) ([^ ]+) ([^ -][^ ]* )*- ([^ ]+) .*/mount: \2 \3 \5/'
echo "uid=$(cat /proc/self/uid_map | tr -s ' ' | tr '\n' '|')"
set -- "$@" /proc /proc/sys /sys
grep -qE '^([^ ]+ ){4}/sys/fs/cgroup ' $mi && set -- "$@" /sys/fs/cgroup
exec /usr/bin/perl /w.pl "$@"
"#;
            let root = make_root(top, "root")?;
            fs::write(root.join("w.pl"), perl(WRITE_PL)).map_err(|e| e.to_string())?;
            let spec = RunSpec::new(
                &root,
                ["/bin/sh", "-c", sh, "sh", pid.as_str()]
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            );
            out.extend(run_logged(&sandbox, spec, top, 0)?);
            drop(sandbox);
            Ok::<_, String>(())
        })();
        if let Err(e) = result {
            out.push(format!("error={e}"));
        }
        out.join("\n")
    });
    drop(outer);
    // Ubuntu 24.04's AppArmor gives an unconfined task's user namespace no
    // capability when it had none: the new namespaces' attacks stop there
    // (`EACCES`, `EPERM`), and only a machine without it shows the locks.
    let confined = restricted && value(&out, "0:ctl-tmpfs") == errno(libc::EACCES);
    measured(&format!(
        "red team, a uid 0 RUN under rbinds (new user namespaces without capabilities under AppArmor: {confined}): {}",
        out.replace('\n', "; ")
    ));
    assert!(!out.contains("error="), "{out}");
    let (proc, sysfs) = chosen(&out);
    assert_eq!(sysfs, KernelFs::Rbind, "{out}");
    assert_eq!(value(&out, "run0"), "exit 0", "{out}");
    assert!(
        value(&out, "0:uid").trim_start().starts_with("0 0 "),
        "not uid 0:\n{out}"
    );
    let below_proc = |m: &str| m == "/proc" || m.starts_with("/proc/");
    let mut mounts = 0;
    for l in out.lines().filter_map(|l| l.strip_prefix("0:mount: ")) {
        let m: Vec<&str> = l.split(' ').collect();
        if below_proc(m[0]) && proc == KernelFs::Fresh {
            continue;
        }
        mounts += 1;
        assert!(
            m[1].split(',').any(|o| o == "ro"),
            "{m:?} is writable:\n{out}"
        );
    }
    assert!(mounts > 0, "{out}");
    assert_eq!(value(&out, "0:unshare"), "ok", "control unshare:\n{out}");
    if !confined {
        for k in [
            "ctl-tmpfs",
            "ctl-setattr",
            "ctl-remount",
            "ctl-clone",
            "ctl-move",
        ] {
            assert_eq!(value(&out, &format!("0:{k}")), "ok", "control {k}:\n{out}");
        }
    }
    // A new proc is the RUN's own: its own files, and its flags, are its to keep.
    let own = [
        "self-",
        "thread-",
        "ns-self-",
        "ns-thread-",
        "ns-clone-self-",
        "ns-clone-thread-",
    ];
    let own_proc = |k: &str| {
        proc == KernelFs::Fresh
            && (own.iter().any(|p| k.starts_with(p))
                || (k.starts_with("ns-") && k.split_once(':').is_some_and(|(_, m)| below_proc(m))))
    };
    for (k, v) in keys(&out, 0, "") {
        let control = k.starts_with("ctl-")
            || ["unshare", "ns-pid", "pid-fsopen", "pid-fscreate"].contains(&k)
            || ["ns-clone:", "ns-clone-move:", "ns-rbind:"]
                .iter()
                .any(|p| k.starts_with(p));
        if !control && !own_proc(k) && !k.starts_with("mount") && k != "uid" {
            assert_ne!(v, "ok", "{k} was allowed:\n{out}");
        }
    }
    assert_read_only(&out, |m| proc == KernelFs::Rbind || !below_proc(m));
    assert_errno(&out, "0:sysctl-kernel-core_pattern", libc::EROFS);
    assert_errno(&out, "0:sysrq", libc::EROFS);
    if !confined {
        for k in [
            "ns-remount",
            "ns-sb-remount",
            "ns-setattr",
            "ns-setattr-rec",
        ] {
            assert_errno(&out, &format!("0:{k}:/sys"), libc::EPERM);
        }
    }
}
