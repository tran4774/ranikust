//! What ranikust cannot close itself in kaniko mode on `/` (ADR-0051), and
//! warns about: a `RUN` there shares ranikust's PID namespace, so
//!
//! - `/proc/<pid>/cmdline` of every process, ranikust's included, is
//!   readable whatever `PR_SET_DUMPABLE` says: a `--build-arg` value on the
//!   command line is readable by every `RUN`;
//! - another process of the container that holds a credential in its
//!   environment (the CI shell that started ranikust) exposes it through its
//!   own `/proc/<pid>/environ`;
//! - a `--secret src=` file is a file on `/`, which every `RUN` reads; ranikust
//!   reads it once, at the start (ADR-0078), but cannot hide it.
//!
//! It also checks what it refuses to build under: a writable `/proc/sys`
//! ([`proc_sys_read_only`]), and a process of another mount namespace in
//! its PID namespace that it can open through `/proc` as it starts
//! ([`scan_peers`], ADR-0101).

use std::ffi::OsString;
use std::fs;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::config::BuildArg;

/// The credential variables ranikust reads.
pub(crate) const CREDENTIAL_VARS: [&str; 1] = ["DOCKER_AUTH_CONFIG"];

/// Words that make an argument name look like a secret.
const SECRET_WORDS: [&str; 8] = [
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "KEY",
    "CREDENTIAL",
    "AUTH",
    "PRIVATE",
];

/// Whether a build argument's name looks like a secret's.
pub(crate) fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_WORDS.iter().any(|w| upper.contains(w))
}

/// Warns for each `--build-arg NAME=value` whose name looks like a
/// secret's: its value is on ranikust's command line. `--build-arg NAME`
/// alone, which takes the value from the environment, is not on it.
pub(crate) fn warn_build_args(args: &[BuildArg]) {
    for arg in args {
        if arg.value.is_some() && looks_secret(&arg.name) {
            tracing::warn!(
                "--build-arg {}=…: the value is on ranikust's command line, which every RUN can \
                 read through /proc/<pid>/cmdline in kaniko mode on /; pass a secret with \
                 --secret, or as `--build-arg {}` with the value in the environment and \
                 ranikust started as the container's PID 1 (exec it), else the shell that \
                 started it holds the value where a RUN reads it",
                arg.name,
                arg.name
            );
        }
    }
}

/// The warning for each `--secret` read from a file: on `/` it is a file
/// like any other, which every `RUN` can read, not only the one that mounts
/// it (threat model, section 6). ADR-0084 keeps such files allowed, with
/// this warning, and leaves what a `RUN` does with one to the user.
pub(crate) fn file_secret_warnings(secrets: &[(String, rnk_builder::Secret)]) -> Vec<String> {
    secrets
        .iter()
        .filter_map(|(id, secret)| match secret {
            rnk_builder::Secret::File(path) => Some(format!(
                "--secret id={id}: {} is a file on /, which every RUN can read in kaniko mode \
                 on /, not only the RUN that mounts it; ranikust reads it once, at the start, \
                 but cannot hide it: pass the secret in ranikust's environment \
                 (--secret id={id},env=VAR) with ranikust started as the container's PID 1",
                path.display()
            )),
            rnk_builder::Secret::Env(_) => None,
        })
        .collect()
}

/// Every variable of ranikust's environment that holds a credential or a
/// secret: [`CREDENTIAL_VARS`], the variable of each `--secret` read from
/// the environment (`env=`, or the id itself when set), and each
/// `--build-arg NAME` given without a value.
pub(crate) fn secret_variables(config: &crate::config::BuildConfig) -> Vec<String> {
    let mut names: Vec<String> = CREDENTIAL_VARS.iter().map(|&n| n.to_owned()).collect();
    for (_, secret) in &config.secrets {
        if let rnk_builder::Secret::Env(var) = secret {
            names.push(var.to_string_lossy().into_owned());
        }
    }
    for arg in &config.build_args {
        if arg.value.is_none() {
            names.push(arg.name.clone());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The names among `names` set to a non-empty value in an `environ` file's
/// contents (`NAME=value` entries separated by NUL bytes).
pub(crate) fn held<'a>(environ: &[u8], names: &[&'a str]) -> Vec<&'a str> {
    names
        .iter()
        .copied()
        .filter(|name| {
            environ.split(|&b| b == 0).any(|entry| {
                entry
                    .strip_prefix(name.as_bytes())
                    .and_then(|rest| rest.strip_prefix(b"="))
                    .is_some_and(|value| !value.is_empty())
            })
        })
        .collect()
}

/// Warns about each other process listed in `/proc` whose
/// `/proc/<pid>/environ` ranikust can read and which sets one of `variables`
/// ([`secret_variables`]) to a non-empty value ([`credential_warning`]);
/// nothing is said when `/proc` cannot be listed. The caller runs it after
/// [`rnk_sandbox::trim_to_run_capabilities`], so that ranikust reads with
/// the capabilities a root `RUN` keeps (ADR-0078).
pub(crate) fn warn_credential_holders(variables: &[String]) {
    let names: Vec<&str> = variables.iter().map(String::as_str).collect();
    let Ok(dir) = fs::read_dir("/proc") else {
        return;
    };
    let me = std::process::id().to_string();
    for pid in dir.flatten().filter_map(|e| other_pid(e.file_name(), &me)) {
        let Ok(environ) = fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        let found = held(&environ, &names);
        if found.is_empty() {
            continue;
        }
        let process = Process {
            comm: comm(&pid),
            pid,
        };
        tracing::warn!("{}", credential_warning(&process, &found));
    }
}

/// The warning about `process`, whose environment sets the variables
/// `found`: every `RUN` reads it too, through `/proc/<pid>/environ`.
fn credential_warning(process: &Process, found: &[&str]) -> String {
    format!(
        "process {process} holds {} in its environment, which every RUN can read through \
         /proc/{}/environ in kaniko mode on /; start ranikust as the container's PID 1 (exec \
         it) so that no other process holds the credentials",
        found.join(", "),
        process.pid
    )
}

/// The pid an entry of `/proc` names, when it names a process (digits
/// only) other than ranikust (`me`).
fn other_pid(name: OsString, me: &str) -> Option<String> {
    name.into_string()
        .ok()
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && n != me)
}

/// The name a process gives itself (`/proc/<pid>/comm`), escaped, empty
/// when it cannot be read.
fn comm(pid: &str) -> String {
    fs::read(format!("/proc/{pid}/comm"))
        .map(|c| escaped(&c))
        .unwrap_or_default()
}

/// A process's name as read from its `comm`, which that process chose:
/// trimmed, and escaped so that it cannot write to the terminal.
fn escaped(comm: &[u8]) -> String {
    String::from_utf8_lossy(comm)
        .trim()
        .escape_debug()
        .to_string()
}

/// The most threads [`scan_peers`] looks at, a process with none left
/// counting as one; a PID namespace with more is refused rather than walked
/// at length.
pub(crate) const PEER_SCAN_LIMIT: usize = 32_768;

/// A process as a refusal of [`scan_peers`] or a warning of
/// [`warn_credential_holders`] names it: its pid and its escaped name,
/// empty when it could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Process {
    pub(crate) pid: String,
    pub(crate) comm: String,
}

impl std::fmt::Display for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.comm.is_empty() {
            write!(f, "{}", self.pid)
        } else {
            write!(f, "{} ({})", self.pid, self.comm)
        }
    }
}

/// What the probe of one process found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Peer {
    /// No thread in another mount namespace within reach: each thread
    /// looked at was in ranikust's own, or the process or thread was out of
    /// its reach (`EACCES`, `EPERM`), one at least not gone.
    Passed,
    /// A thread in another mount namespace, its `root` opened; the
    /// process's name.
    Reachable(String),
    /// Gone while it was looked at (`ENOENT`, `ESRCH`), with each of its
    /// threads.
    Gone,
    /// Threads past what is left of the bound.
    TooMany,
    /// Any other error: the step that failed, the error and the process's
    /// name.
    Unknown(&'static str, Errno, String),
}

/// What the probe of one thread found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Thread {
    Same,
    Other,
    Unreachable,
    Gone,
    Unknown(&'static str, Errno),
}

/// The steps of a thread's probe, as a refusal names them: through
/// `/proc/<pid>/task/<tid>`, or through `/proc/<pid>` for a process whose
/// threads ranikust may not list.
type Steps = [&'static str; 2];
const THREAD_STEPS: Steps = [
    "opening /proc/<pid>/task/<tid>/root",
    "reading /proc/<pid>/task/<tid>/ns/mnt",
];
const PROCESS_STEPS: Steps = ["opening /proc/<pid>/root", "reading /proc/<pid>/ns/mnt"];

/// What [`scan_peers`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerScan {
    /// No process of another mount namespace that ranikust reaches.
    None,
    /// The first process found with a thread in another mount namespace
    /// whose `root` ranikust opened.
    Reachable(Process),
    /// More than [`PEER_SCAN_LIMIT`] threads.
    TooMany,
    /// A process that could not be told: the step that failed and the
    /// error.
    Unknown(Process, &'static str, Errno),
    /// `/proc` could not be listed.
    Unlisted(Errno),
    /// ranikust's own mount namespace could not be read.
    OwnNamespace(Errno),
}

/// Looks, once, for a process of the PID namespace with a thread in another
/// mount namespace whose root ranikust opens (`/proc/<pid>/task/<tid>/root`,
/// or `/proc/<pid>/root` where its threads may not be listed): a `RUN` on
/// `/` shares the PID namespace and could reach that process's files, which
/// the container's own mount table does not show (ADR-0101). It counts only
/// the processes ranikust can open as it starts.
///
/// `started_in` is ranikust's mount namespace as it started
/// ([`mount_namespace`], read before the set-up may enter one of its own).
/// It and the one ranikust is in now both count as its own: a set-up that
/// failed after its `unshare` left ranikust in a copy no other process is
/// in.
pub(crate) fn scan_peers(started_in: rustix::io::Result<(u64, u64)>) -> PeerScan {
    let own = match own(started_in, mount_namespace()) {
        Ok(own) => own,
        Err(e) => return PeerScan::OwnNamespace(e),
    };
    let errno = |e: &std::io::Error| Errno::from_io_error(e).unwrap_or(Errno::IO);
    let dir = match fs::read_dir("/proc") {
        Ok(dir) => dir,
        Err(e) => return PeerScan::Unlisted(errno(&e)),
    };
    let names = dir.map(|entry| entry.map(|e| e.file_name()).map_err(|e| errno(&e)));
    scan(names, PEER_SCAN_LIMIT, |pid, left| probe(own, pid, left))
}

/// The scan of `/proc`'s entry `names`, at most `limit` threads, with
/// `probe` (given what is left of `limit`, it returns the threads it looked
/// at): the first process reached in another mount namespace, or the first
/// that cannot be told.
fn scan(
    names: impl IntoIterator<Item = Result<OsString, Errno>>,
    limit: usize,
    mut probe: impl FnMut(&str, usize) -> (Peer, usize),
) -> PeerScan {
    let me = std::process::id().to_string();
    let mut seen = 0;
    for name in names {
        let pid = match name {
            Ok(name) => match other_pid(name, &me) {
                Some(pid) => pid,
                None => continue,
            },
            Err(e) => return PeerScan::Unlisted(e),
        };
        let (peer, threads) = probe(&pid, limit - seen);
        // A process with no thread left counts as one, so that the bound
        // holds for a `/proc` of exiting processes too.
        seen += threads.max(1);
        match peer {
            Peer::Passed | Peer::Gone if seen <= limit => {}
            Peer::Passed | Peer::Gone | Peer::TooMany => return PeerScan::TooMany,
            Peer::Reachable(comm) => return PeerScan::Reachable(Process { pid, comm }),
            Peer::Unknown(step, e, comm) => {
                return PeerScan::Unknown(Process { pid, comm }, step, e);
            }
        }
    }
    PeerScan::None
}

/// Probes process `pid` through one descriptor of its `/proc/<pid>`, so
/// that a pid reused meanwhile is not mistaken for it: each of its threads,
/// at most `left`, since a thread may have a mount namespace and
/// credentials of its own and the first one may have exited. Returns the
/// threads it looked at too.
fn probe(own: Own, pid: &str, left: usize) -> (Peer, usize) {
    let dir = match rustix::fs::open(
        format!("/proc/{pid}"),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(dir) => dir,
        Err(Errno::NOENT | Errno::SRCH) => return (Peer::Gone, 0),
        // Not even looked into, so its root is out of reach too.
        Err(Errno::ACCESS | Errno::PERM) => return (Peer::Passed, 1),
        Err(e) => return (Peer::Unknown("opening /proc/<pid>", e, String::new()), 0),
    };
    let tids = rustix::fs::openat(
        &dir,
        "task",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .and_then(rustix::fs::Dir::read_from)
    .map(|list| list.map(|entry| entry.map(|e| e.file_name().to_bytes().to_vec())));
    walk(
        tids,
        left,
        |tid| thread_at(own, &dir, tid),
        || comm_at(&dir),
    )
}

/// The probe of thread `tid` of the process at `dir`, its `/proc/<pid>`,
/// or of the process as one (`None`) where its threads may not be listed.
fn thread_at(own: Own, dir: &OwnedFd, tid: Option<&str>) -> Thread {
    let Some(tid) = tid else {
        return task(own, dir, PROCESS_STEPS);
    };
    match rustix::fs::openat(
        dir,
        format!("task/{tid}"),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(at) => task(own, &at, THREAD_STEPS),
        Err(Errno::NOENT | Errno::SRCH) => Thread::Gone,
        // Not even looked into, so its root is out of reach too.
        Err(Errno::ACCESS | Errno::PERM) => Thread::Unreachable,
        Err(e) => Thread::Unknown("opening /proc/<pid>/task/<tid>", e),
    }
}

/// What [`probe`] decides from a process's thread ids `tids` (its
/// `/proc/<pid>/task`), at most `left`, with `thread` probing one (`None`:
/// the process itself, when its threads may not be listed) and `name`
/// reading the process's name: the first thread reached or that cannot be
/// told, else whether one was there at all; and how many were looked at.
fn walk<T: AsRef<[u8]>>(
    tids: rustix::io::Result<impl IntoIterator<Item = rustix::io::Result<T>>>,
    left: usize,
    mut thread: impl FnMut(Option<&str>) -> Thread,
    name: impl Fn() -> String,
) -> (Peer, usize) {
    let named = |found: Thread| match found {
        Thread::Same | Thread::Unreachable => Peer::Passed,
        Thread::Other => Peer::Reachable(name()),
        Thread::Gone => Peer::Gone,
        Thread::Unknown(step, e) => Peer::Unknown(step, e, name()),
    };
    let tids = match tids {
        Ok(tids) => tids,
        Err(Errno::NOENT | Errno::SRCH) => return (Peer::Gone, 0),
        Err(Errno::ACCESS | Errno::PERM) if left == 0 => return (Peer::TooMany, 1),
        // Threads that may not be listed: the process is probed as one, as
        // a `RUN` reaches it through `/proc/<pid>/root`.
        Err(Errno::ACCESS | Errno::PERM) => return (named(thread(None)), 1),
        Err(e) => return (Peer::Unknown("listing /proc/<pid>/task", e, name()), 0),
    };
    let mut seen = 0;
    let mut found = Peer::Gone;
    for tid in tids {
        let tid = match tid {
            Ok(tid) => tid,
            Err(Errno::NOENT | Errno::SRCH) => break,
            Err(e) => return (Peer::Unknown("listing /proc/<pid>/task", e, name()), seen),
        };
        // Only a tid in ASCII digits names a thread (not `.` or `..`).
        let Some(tid) = std::str::from_utf8(tid.as_ref())
            .ok()
            .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        seen += 1;
        if seen > left {
            return (Peer::TooMany, seen);
        }
        match named(thread(Some(tid))) {
            Peer::Gone => {}
            Peer::Passed => found = Peer::Passed,
            peer @ (Peer::Reachable(_) | Peer::TooMany | Peer::Unknown(..)) => {
                return (peer, seen);
            }
        }
    }
    (found, seen)
}

/// The probe of one thread, or of a process as one, at its `/proc`
/// directory `dir`: whether ranikust opens its `root`, then whether its
/// mount namespace is one of `own`; a failure names the step of `steps`.
fn task(own: Own, dir: &OwnedFd, [root, ns]: Steps) -> Thread {
    // Following the link takes the kernel's access check for it.
    match rustix::fs::openat(dir, "root", OFlags::PATH | OFlags::CLOEXEC, Mode::empty()) {
        Ok(_) => {}
        Err(Errno::ACCESS | Errno::PERM) => return Thread::Unreachable,
        Err(Errno::NOENT | Errno::SRCH) => return Thread::Gone,
        Err(e) => return Thread::Unknown(root, e),
    }
    match namespace(dir, "ns/mnt") {
        Ok(found) if own.contains(&found) => Thread::Same,
        Ok(_) => Thread::Other,
        Err(e) => ns_failure(ns, e, || {
            rustix::fs::openat(
                dir,
                "ns",
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map(drop)
        }),
    }
}

/// What a failure `e` to read a thread's `ns/mnt` means, `step` naming
/// it: a thread gone (`ENOENT`, `ESRCH`), or one freed since its root was
/// opened, which answers `EACCES` or `EPERM` there and whose `ns` is gone
/// too (`ENOENT`, `ESRCH` from `ns`, tried only then); anything else
/// cannot be told, and is named by `e`: the `ns` open only tells whether
/// the thread is gone.
fn ns_failure(step: &'static str, e: Errno, ns: impl FnOnce() -> rustix::io::Result<()>) -> Thread {
    match e {
        Errno::NOENT | Errno::SRCH => Thread::Gone,
        Errno::ACCESS | Errno::PERM => match ns() {
            Err(Errno::NOENT | Errno::SRCH) => Thread::Gone,
            _ => Thread::Unknown(step, e),
        },
        e => Thread::Unknown(step, e),
    }
}

/// The escaped `comm` of the process at `dir`, empty when it cannot be
/// read.
fn comm_at(dir: &OwnedFd) -> String {
    let Ok(fd) = rustix::fs::openat(dir, "comm", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
    else {
        return String::new();
    };
    // The kernel prints at most 64 bytes, its newline included.
    let mut buf = [0u8; 64];
    match rustix::io::read(&fd, &mut buf) {
        Ok(n) => escaped(&buf[..n]),
        Err(_) => String::new(),
    }
}

/// ranikust's own mount namespaces: the one it started in and the one it
/// is in now, the same unless its set-up entered another.
type Own = [(u64, u64); 2];

/// [`Own`] from the namespace ranikust `started_in` and the one it is in
/// `now`, or the error of the first that could not be read.
fn own(
    started_in: rustix::io::Result<(u64, u64)>,
    now: rustix::io::Result<(u64, u64)>,
) -> rustix::io::Result<Own> {
    Ok([started_in?, now?])
}

/// ranikust's mount namespace now ([`namespace`] of `/proc/self/ns/mnt`).
pub(crate) fn mount_namespace() -> rustix::io::Result<(u64, u64)> {
    namespace(rustix::fs::CWD, "/proc/self/ns/mnt")
}

/// A mount namespace's identity: the device and inode of `path` (an
/// `ns/mnt` link) below `dir`, opened `O_PATH`.
fn namespace(dir: impl AsFd, path: &str) -> rustix::io::Result<(u64, u64)> {
    let fd = rustix::fs::openat(dir, path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())?;
    let st = rustix::fs::fstat(&fd)?;
    Ok((st.st_dev, st.st_ino))
}

/// Whether `path` (`/proc/sys`) is on a read-only mount. A writable one lets
/// a uid 0 `RUN` write `kernel/core_pattern` and run a program as the
/// host's root, with no capability at all; Docker and Kubernetes mount it
/// read-only unless told otherwise (`--privileged`,
/// `systempaths=unconfined`, `procMount: Unmasked`). Kaniko mode on `/`
/// refuses to build there (ADR-0051, ADR-0054).
///
/// # Errors
///
/// What `statvfs` returns.
pub(crate) fn proc_sys_read_only(path: &Path) -> rustix::io::Result<bool> {
    let flags = rustix::fs::statvfs(path)?.f_flag;
    Ok(flags.contains(rustix::fs::StatVfsMountFlags::RDONLY))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_writable_directory_is_not_read_only_and_a_missing_one_errs() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(proc_sys_read_only(dir.path()), Ok(false));
        assert!(proc_sys_read_only(&dir.path().join("missing")).is_err());
    }

    fn names(list: &[&str]) -> Vec<Result<OsString, Errno>> {
        list.iter().map(|n| Ok(OsString::from(n))).collect()
    }

    fn process(pid: &str, comm: &str) -> Process {
        Process {
            pid: pid.to_owned(),
            comm: comm.to_owned(),
        }
    }

    #[test]
    fn a_peer_scan_refuses_the_first_process_reached_in_another_mount_namespace() {
        let me = std::process::id().to_string();
        let mut probed = Vec::new();
        let found = scan(
            names(&["self", "1", &me, "2", "3", "x9", "4", "5"]),
            PEER_SCAN_LIMIT,
            |pid, _| {
                probed.push(pid.to_owned());
                let peer = match pid {
                    "1" | "2" => Peer::Passed,
                    "3" => Peer::Gone,
                    "4" => Peer::Reachable("sleep".into()),
                    _ => Peer::Unknown("x", Errno::IO, String::new()),
                };
                (peer, 1)
            },
        );
        assert_eq!(found, PeerScan::Reachable(process("4", "sleep")));
        // Neither ranikust itself nor an entry that is not a pid is probed,
        // and the scan stops at the first process reached.
        assert_eq!(probed, ["1", "2", "3", "4"]);
    }

    #[test]
    fn a_peer_scan_with_no_process_reached_finds_none() {
        let found = scan(names(&["1", "2", "3"]), 3, |pid, _| match pid {
            "1" | "2" => (Peer::Passed, 1),
            _ => (Peer::Gone, 0),
        });
        assert_eq!(found, PeerScan::None);
    }

    #[test]
    fn a_peer_scan_refuses_what_it_cannot_tell() {
        let found = scan(names(&["1", "2"]), 8, |pid, _| match pid {
            "1" => (Peer::Passed, 1),
            _ => (Peer::Unknown("listing", Errno::LOOP, "x".into()), 0),
        });
        assert_eq!(
            found,
            PeerScan::Unknown(process("2", "x"), "listing", Errno::LOOP)
        );
        let mut list = names(&["1"]);
        list.push(Err(Errno::IO));
        list.extend(names(&["2"]));
        assert_eq!(
            scan(list, 8, |_, _| (Peer::Passed, 1)),
            PeerScan::Unlisted(Errno::IO)
        );
    }

    #[test]
    fn a_peer_scan_past_its_bound_refuses() {
        // Entries that are not pids do not count, and a process with no
        // thread left counts as one.
        let list = names(&["1", "self", "2", "sys", "3"]);
        assert_eq!(scan(list, 3, |_, _| (Peer::Gone, 0)), PeerScan::None);
        let list = names(&["1", "2", "3", "4"]);
        assert_eq!(scan(list, 3, |_, _| (Peer::Gone, 0)), PeerScan::TooMany);
        // Each process counts its threads, and a probe is told what is left.
        let mut left = Vec::new();
        let found = scan(names(&["1", "2", "3"]), 4, |_, l| {
            left.push(l);
            (Peer::Passed, 2)
        });
        assert_eq!((found, left), (PeerScan::TooMany, vec![4, 2, 0]));
        let found = scan(names(&["1"]), 4, |_, _| (Peer::TooMany, 5));
        assert_eq!(found, PeerScan::TooMany);
    }

    type Tids = rustix::io::Result<Vec<rustix::io::Result<&'static str>>>;

    fn tids(list: &[&'static str]) -> Tids {
        Ok(list.iter().map(|t| Ok(*t)).collect())
    }

    #[test]
    fn a_later_thread_reached_refuses_its_process_by_the_process_name() {
        let mut probed = Vec::new();
        let found = walk(
            tids(&[".", "1", "..", "2", "3", "4"]),
            PEER_SCAN_LIMIT,
            |tid| {
                probed.push(tid.map(str::to_owned));
                match tid {
                    Some("1") => Thread::Same,
                    Some("2") => Thread::Unreachable,
                    Some("3") => Thread::Other,
                    _ => Thread::Unknown("x", Errno::IO),
                }
            },
            || "java".to_owned(),
        );
        assert_eq!(found, (Peer::Reachable("java".into()), 3));
        assert_eq!(
            probed,
            [Some("1"), Some("2"), Some("3")].map(|t| t.map(str::to_owned))
        );
        let found = walk(
            tids(&["1", "2"]),
            8,
            |tid| match tid {
                Some("1") => Thread::Gone,
                _ => Thread::Unknown("opening /proc/<pid>/task/<tid>/root", Errno::LOOP),
            },
            || "java".to_owned(),
        );
        assert_eq!(
            found,
            (
                Peer::Unknown(
                    "opening /proc/<pid>/task/<tid>/root",
                    Errno::LOOP,
                    "java".into()
                ),
                2
            )
        );
    }

    #[test]
    fn threads_passed_over_or_gone_pass_their_process() {
        let name = || -> String { unreachable!("a process passed over is not named") };
        let passed = |tid: Option<&str>| match tid {
            Some("1") => Thread::Unreachable,
            Some("2") => Thread::Gone,
            _ => Thread::Same,
        };
        assert_eq!(
            walk(tids(&["1", "2", "3"]), 3, passed, name),
            (Peer::Passed, 3)
        );
        assert_eq!(walk(tids(&["2"]), 3, passed, name), (Peer::Gone, 1));
        assert_eq!(walk(tids(&[]), 3, passed, name), (Peer::Gone, 0));
        // A listing cut short by the process's exit keeps what was seen.
        let cut: Tids = Ok(vec![Ok("1"), Err(Errno::SRCH), Ok("3")]);
        assert_eq!(walk(cut, 3, passed, name), (Peer::Passed, 1));
        let gone: Tids = Err(Errno::NOENT);
        assert_eq!(walk(gone, 3, passed, name), (Peer::Gone, 0));
    }

    #[test]
    fn a_process_whose_threads_may_not_be_listed_is_probed_as_one() {
        for denied in [Errno::ACCESS, Errno::PERM] {
            for (found, want) in [
                (Thread::Unreachable, Peer::Passed),
                (Thread::Same, Peer::Passed),
                (Thread::Gone, Peer::Gone),
                (Thread::Other, Peer::Reachable("java".into())),
                (
                    Thread::Unknown("opening /proc/<pid>/root", Errno::LOOP),
                    Peer::Unknown("opening /proc/<pid>/root", Errno::LOOP, "java".into()),
                ),
            ] {
                let mut probed = Vec::new();
                let got = walk(
                    Err::<Vec<rustix::io::Result<&str>>, _>(denied),
                    1,
                    |tid| {
                        probed.push(tid.is_none());
                        found
                    },
                    || "java".to_owned(),
                );
                assert_eq!((got, probed), ((want, 1), vec![true]));
            }
            let none = |_: Option<&str>| -> Thread { unreachable!("past the bound") };
            let denied: Tids = Err(denied);
            assert_eq!(walk(denied, 0, none, String::new), (Peer::TooMany, 1));
        }
        // Any other error refuses, named as the listing.
        let other: Tids = Err(Errno::LOOP);
        let found = walk(other, 1, |_| Thread::Same, || "java".to_owned());
        assert_eq!(
            found,
            (
                Peer::Unknown("listing /proc/<pid>/task", Errno::LOOP, "java".into()),
                0
            )
        );
        let midway: Tids = Ok(vec![Ok("1"), Err(Errno::IO)]);
        let found = walk(midway, 2, |_| Thread::Same, || "java".to_owned());
        assert_eq!(
            found,
            (
                Peer::Unknown("listing /proc/<pid>/task", Errno::IO, "java".into()),
                1
            )
        );
    }

    #[test]
    fn the_threads_of_a_process_count_against_the_bound() {
        let same = |_: Option<&str>| Thread::Same;
        assert_eq!(
            walk(tids(&["1", "2"]), 2, same, String::new),
            (Peer::Passed, 2)
        );
        assert_eq!(
            walk(tids(&["1", "2", "3"]), 2, same, String::new),
            (Peer::TooMany, 3)
        );
    }

    #[test]
    fn every_thread_of_a_process_is_probed_within_the_bound() {
        let now = mount_namespace().unwrap();
        let own = [now, now];
        let me = std::process::id().to_string();
        // A second thread at least, whichever thread runs the test.
        let (stop, parked) = std::sync::mpsc::channel::<()>();
        let second = std::thread::spawn(move || parked.recv());
        let (peer, seen) = probe(own, &me, PEER_SCAN_LIMIT);
        assert_eq!(probe(own, &me, 1), (Peer::TooMany, 2));
        // Another namespace's identity: reached at the first thread, named
        // as the process's `comm` names it.
        let other = (now.0, now.1 ^ 1);
        let reached = probe([other, other], &me, PEER_SCAN_LIMIT);
        drop(stop);
        second.join().unwrap().unwrap_err();
        assert!(peer == Peer::Passed && seen >= 2, "{peer:?} {seen}");
        assert_eq!(reached, (Peer::Reachable(comm(&me)), 1));
        assert!(!comm(&me).is_empty());
        // Past the largest pid_max.
        assert_eq!(probe(own, "4294967295", 8), (Peer::Gone, 0));
    }

    #[test]
    fn ranikusts_own_namespaces_are_the_one_it_started_in_and_the_one_it_is_in() {
        let (start, now) = ((1, 2), (1, 3));
        assert_eq!(own(Ok(start), Ok(now)), Ok([start, now]));
        assert_eq!(own(Err(Errno::LOOP), Ok(now)), Err(Errno::LOOP));
        assert_eq!(own(Ok(start), Err(Errno::IO)), Err(Errno::IO));
        assert_eq!(
            scan_peers(Err(Errno::LOOP)),
            PeerScan::OwnNamespace(Errno::LOOP)
        );
    }

    #[test]
    fn a_thread_or_a_process_as_one_is_told_through_its_own_entries() {
        let now = mount_namespace().unwrap();
        let other = (now.0, now.1 ^ 1);
        let open = |path: &Path| {
            rustix::fs::open(
                path,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap()
        };
        let me = open(Path::new("/proc/self"));
        let pid = std::process::id().to_string();
        // The process as one, and its first thread, whose tid is its pid:
        // the namespace ranikust started in and the one it is in now are
        // both its own.
        for tid in [None, Some(pid.as_str())] {
            for own in [[now, now], [now, other], [other, now]] {
                assert_eq!(thread_at(own, &me, tid), Thread::Same, "{tid:?} {own:?}");
            }
            assert_eq!(
                thread_at([other, other], &me, tid),
                Thread::Other,
                "{tid:?}"
            );
        }
        let own = [now, now];
        // A failure names the step of its path: the process's own `root`, a
        // thread's `root`, then a thread that is not a directory.
        let fake = tempfile::tempdir().unwrap();
        let at = |p: &str| fake.path().join(p);
        std::os::unix::fs::symlink("root", at("root")).unwrap();
        fs::create_dir_all(at("task/1/ns")).unwrap();
        std::os::unix::fs::symlink("root", at("task/1/root")).unwrap();
        fs::write(at("task/3"), "").unwrap();
        let dir = open(fake.path());
        assert_eq!(
            thread_at(own, &dir, None),
            Thread::Unknown("opening /proc/<pid>/root", Errno::LOOP)
        );
        assert_eq!(
            thread_at(own, &dir, Some("1")),
            Thread::Unknown("opening /proc/<pid>/task/<tid>/root", Errno::LOOP)
        );
        assert_eq!(thread_at(own, &dir, Some("2")), Thread::Gone);
        assert_eq!(
            thread_at(own, &dir, Some("3")),
            Thread::Unknown("opening /proc/<pid>/task/<tid>", Errno::NOTDIR)
        );
        // Past the root, a missing `ns/mnt` is a thread gone, and a looped
        // one names its step.
        for root in ["root", "task/1/root"] {
            fs::remove_file(at(root)).unwrap();
            fs::create_dir(at(root)).unwrap();
        }
        assert_eq!(thread_at(own, &dir, None), Thread::Gone);
        assert_eq!(thread_at(own, &dir, Some("1")), Thread::Gone);
        fs::create_dir(at("ns")).unwrap();
        std::os::unix::fs::symlink("mnt", at("ns/mnt")).unwrap();
        std::os::unix::fs::symlink("mnt", at("task/1/ns/mnt")).unwrap();
        assert_eq!(
            thread_at(own, &dir, None),
            Thread::Unknown("reading /proc/<pid>/ns/mnt", Errno::LOOP)
        );
        let looped = Thread::Unknown("reading /proc/<pid>/task/<tid>/ns/mnt", Errno::LOOP);
        assert_eq!(thread_at(own, &dir, Some("1")), looped);
        // Where a mode stops this process, as the kernel tells (a root with
        // CAP_DAC_OVERRIDE or CAP_DAC_READ_SEARCH reads through it): a
        // thread it may not look into is out of reach, and an `ns/mnt` it
        // may not read, its `ns` still there, cannot be told.
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str, mode| {
            fs::set_permissions(at(p), fs::Permissions::from_mode(mode)).unwrap();
        };
        let stopped = |p: &str| {
            fs::symlink_metadata(at(p))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
        };
        mode("task/1/ns", 0o000);
        let ns = (thread_at(own, &dir, Some("1")), stopped("task/1/ns/mnt"));
        mode("task/1/ns", 0o755);
        mode("task", 0o000);
        let task = (thread_at(own, &dir, Some("1")), stopped("task/1"));
        mode("task", 0o755);
        let denied = Thread::Unknown("reading /proc/<pid>/task/<tid>/ns/mnt", Errno::ACCESS);
        assert_eq!(
            ns,
            if ns.1 {
                (denied, true)
            } else {
                (looped, false)
            }
        );
        let unreachable = Thread::Unreachable;
        assert_eq!(
            task,
            if task.1 {
                (unreachable, true)
            } else {
                (looped, false)
            }
        );
    }

    #[test]
    fn a_denied_ns_mnt_is_a_thread_gone_only_once_its_ns_is_gone() {
        let step = "reading /proc/<pid>/task/<tid>/ns/mnt";
        let untried = || -> rustix::io::Result<()> { unreachable!("tried only after a denial") };
        assert_eq!(ns_failure(step, Errno::NOENT, untried), Thread::Gone);
        assert_eq!(ns_failure(step, Errno::SRCH, untried), Thread::Gone);
        assert_eq!(
            ns_failure(step, Errno::LOOP, untried),
            Thread::Unknown(step, Errno::LOOP)
        );
        for denied in [Errno::ACCESS, Errno::PERM] {
            // A thread freed since its root was opened: `ns` went with it.
            for gone in [Errno::NOENT, Errno::SRCH] {
                assert_eq!(ns_failure(step, denied, || Err(gone)), Thread::Gone);
            }
            for there in [Ok(()), Err(Errno::ACCESS), Err(Errno::LOOP)] {
                assert_eq!(
                    ns_failure(step, denied, || there),
                    Thread::Unknown(step, denied),
                    "{there:?}"
                );
            }
        }
    }

    #[test]
    fn a_credential_warning_names_the_process_its_variables_and_its_environ() {
        let found = ["DOCKER_AUTH_CONFIG", "NPM_TOKEN"];
        assert_eq!(
            credential_warning(&process("7", "sh"), &found),
            "process 7 (sh) holds DOCKER_AUTH_CONFIG, NPM_TOKEN in its environment, which \
             every RUN can read through /proc/7/environ in kaniko mode on /; start ranikust \
             as the container's PID 1 (exec it) so that no other process holds the credentials"
        );
        let unnamed = credential_warning(&process("7", ""), &["NPM_TOKEN"]);
        assert!(
            unnamed.starts_with("process 7 holds NPM_TOKEN in"),
            "{unnamed}"
        );
    }

    #[test]
    fn a_process_is_named_by_its_pid_and_its_name_when_read() {
        assert_eq!(process("7", "sleep").to_string(), "7 (sleep)");
        assert_eq!(process("7", "").to_string(), "7");
    }

    #[test]
    fn a_process_name_is_trimmed_and_escaped() {
        assert_eq!(escaped(b"sleep\n"), "sleep");
        assert_eq!(escaped(b"a\x1b[2Jb\nc\n"), "a\\u{1b}[2Jb\\nc");
        assert_eq!(escaped(b"\xff\n"), "\u{fffd}");
    }

    #[test]
    fn secret_looking_names() {
        for n in [
            "NPM_TOKEN",
            "db_password",
            "SSH_PRIVATE_KEY",
            "AUTH",
            "api-key",
        ] {
            assert!(looks_secret(n), "{n}");
        }
        for n in ["VERSION", "NODE_ENV", "TARGETPLATFORM"] {
            assert!(!looks_secret(n), "{n}");
        }
    }

    #[test]
    fn held_variables_are_found_by_exact_name() {
        let env = b"PATH=/bin\0DOCKER_AUTH_CONFIG={\"auths\":{}}\0X_DOCKER_AUTH_CONFIG=1\0";
        assert_eq!(held(env, &CREDENTIAL_VARS), ["DOCKER_AUTH_CONFIG"]);
        assert!(held(b"DOCKER_AUTH_CONFIG=\0", &CREDENTIAL_VARS).is_empty());
        assert!(held(b"DOCKER_AUTH_CONFIGX=a\0", &CREDENTIAL_VARS).is_empty());
    }

    #[test]
    fn secret_variables_cover_env_secrets_and_valueless_build_args() {
        let argv = [
            "ranikust",
            "--no-push",
            "--secret",
            "id=npm,env=NPM_SECRET_VAR",
            "--secret",
            "id=file_one,src=/ranikust/s",
            "--build-arg",
            "GH_TOKEN",
            "--build-arg",
            "VERSION=1",
        ];
        let crate::cli::Command::Build(args) = crate::cli::parse(argv).unwrap().cli.into_command()
        else {
            panic!("expected build");
        };
        let config = crate::config::BuildConfig::from_args(&args).unwrap();
        assert_eq!(
            secret_variables(&config),
            ["DOCKER_AUTH_CONFIG", "GH_TOKEN", "NPM_SECRET_VAR"]
        );
        // The file secret, and only it, gets the warning that every RUN on
        // `/` reads it.
        let warnings = file_secret_warnings(&config.secrets);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("--secret id=file_one: /ranikust/s ")
                && warnings[0].contains("every RUN can read"),
            "{}",
            warnings[0]
        );
    }
}
