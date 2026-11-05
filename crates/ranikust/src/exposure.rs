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
//! its PID namespace that it reaches through `/proc` ([`scan_peers`],
//! ADR-0101).

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

/// Warns for every other process of the PID namespace whose environment
/// holds one of `variables` ([`secret_variables`]): a `RUN` reads it
/// through that process's `/proc/<pid>/environ`. Called after
/// [`rnk_sandbox::trim_to_run_capabilities`], so ranikust reads exactly the
/// environments a root `RUN` reads: one it cannot read (not dumpable,
/// another user) a `RUN` cannot read either, and is skipped.
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
        tracing::warn!(
            "process {pid} ({}) holds {} in its environment, which every RUN can read through \
             /proc/{pid}/environ in kaniko mode on /; start ranikust as the container's PID 1 \
             (exec it) so that no other process holds the credentials",
            comm(&pid),
            found.join(", ")
        );
    }
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
pub(crate) fn comm(pid: &str) -> String {
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

/// The most processes, threads looked at included, that [`scan_peers`]
/// looks at; a PID namespace with more is refused rather than walked at
/// length.
pub(crate) const PEER_SCAN_LIMIT: usize = 32_768;

/// A process [`scan_peers`] names: its pid and its escaped name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Process {
    pub(crate) pid: String,
    pub(crate) comm: String,
}

/// What the probe of one process found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Peer {
    /// In ranikust's own mount namespace.
    Same,
    /// In another mount namespace, its `/proc/<pid>/root` opened; its name.
    Reachable(String),
    /// Its `/proc/<pid>/root` refused to ranikust (`EACCES`, `EPERM`).
    Unreachable,
    /// Gone while it was looked at (`ENOENT`, `ESRCH`), with each of its
    /// threads.
    Gone,
    /// Threads past what is left of the bound.
    TooMany,
    /// Any other error; its name.
    Unknown(Errno, String),
}

/// What [`scan_peers`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerScan {
    /// No process of another mount namespace that ranikust reaches.
    None,
    /// The first process found in another mount namespace whose
    /// `/proc/<pid>/root` ranikust opened.
    Reachable(Process),
    /// More than [`PEER_SCAN_LIMIT`] processes and threads.
    TooMany,
    /// A process that could not be told, and why.
    Unknown(Process, Errno),
    /// `/proc` could not be listed.
    Unlisted(Errno),
    /// ranikust's own mount namespace could not be read.
    OwnNamespace(Errno),
}

/// Looks, once, for a process of the PID namespace in another mount
/// namespace whose `/proc/<pid>/root` ranikust opens: a `RUN` on `/` shares
/// the PID namespace and could reach that process's files, which the
/// container's own mount table does not show (ADR-0101). Called before
/// ranikust trims its capabilities, so it may reach a process a `RUN`
/// would not: refused all the same (fails closed). A process that appears
/// after the scan is not seen.
pub(crate) fn scan_peers() -> PeerScan {
    let own = match namespace(rustix::fs::CWD, "/proc/self/ns/mnt") {
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

/// The scan of `/proc`'s entry `names`, at most `limit` processes and
/// threads, with `probe` (given what is left of `limit`, it returns the
/// threads it looked at): the first process reached in another mount
/// namespace, or the first that cannot be told.
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
        seen += 1;
        if seen > limit {
            return PeerScan::TooMany;
        }
        let (peer, threads) = probe(&pid, limit - seen);
        seen += threads;
        match peer {
            Peer::Same | Peer::Unreachable | Peer::Gone => {}
            Peer::Reachable(comm) => return PeerScan::Reachable(Process { pid, comm }),
            Peer::TooMany => return PeerScan::TooMany,
            Peer::Unknown(e, comm) => return PeerScan::Unknown(Process { pid, comm }, e),
        }
    }
    PeerScan::None
}

/// Probes process `pid` through one descriptor of its `/proc/<pid>`, so
/// that a pid reused meanwhile is not mistaken for it: whether ranikust
/// opens its `root`, then whether its mount namespace is `own`. A process
/// whose first thread has exited has no `root` of its own: its other
/// threads, at most `left`, are probed instead. Returns the threads it
/// looked at too.
fn probe(own: (u64, u64), pid: &str, left: usize) -> (Peer, usize) {
    let dir = match rustix::fs::open(
        format!("/proc/{pid}"),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(dir) => dir,
        Err(Errno::NOENT | Errno::SRCH) => return (Peer::Gone, 0),
        Err(e) => return (Peer::Unknown(e, comm(pid)), 0),
    };
    match task(own, &dir) {
        Peer::Gone => threads(own, &dir, left),
        peer => (peer, 0),
    }
}

/// The probe of one thread, or of a process through its first thread, at
/// its `/proc` directory `dir`.
fn task(own: (u64, u64), dir: &OwnedFd) -> Peer {
    // Following the link takes the kernel's access check for it.
    match rustix::fs::openat(dir, "root", OFlags::PATH | OFlags::CLOEXEC, Mode::empty()) {
        Ok(_) => {}
        Err(Errno::ACCESS | Errno::PERM) => return Peer::Unreachable,
        Err(Errno::NOENT | Errno::SRCH) => return Peer::Gone,
        Err(e) => return Peer::Unknown(e, comm_at(dir)),
    }
    match namespace(dir, "ns/mnt") {
        Ok(ns) if ns == own => Peer::Same,
        Ok(_) => Peer::Reachable(comm_at(dir)),
        Err(Errno::NOENT | Errno::SRCH) => Peer::Gone,
        Err(e) => Peer::Unknown(e, comm_at(dir)),
    }
}

/// The probe of the threads of the process at `dir`, at most `left`: the
/// first one reached in another mount namespace or that cannot be told, else
/// whether one was in reach at all; and how many were looked at.
fn threads(own: (u64, u64), dir: &OwnedFd, left: usize) -> (Peer, usize) {
    let list = rustix::fs::openat(
        dir,
        "task",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .and_then(rustix::fs::Dir::read_from);
    let list = match list {
        Ok(list) => list,
        Err(Errno::NOENT | Errno::SRCH) => return (Peer::Gone, 0),
        Err(e) => return (Peer::Unknown(e, comm_at(dir)), 0),
    };
    let mut seen = 0;
    let mut found = Peer::Gone;
    for entry in list {
        let entry = match entry {
            Ok(entry) => entry,
            Err(Errno::NOENT | Errno::SRCH) => break,
            Err(e) => return (Peer::Unknown(e, comm_at(dir)), seen),
        };
        let name = entry.file_name().to_bytes();
        if name.is_empty() || !name.iter().all(u8::is_ascii_digit) {
            continue;
        }
        seen += 1;
        if seen > left {
            return (Peer::TooMany, seen);
        }
        let thread = match rustix::fs::openat(
            dir,
            format!("task/{}", String::from_utf8_lossy(name)),
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(thread) => thread,
            Err(Errno::NOENT | Errno::SRCH) => continue,
            Err(e) => return (Peer::Unknown(e, comm_at(dir)), seen),
        };
        match task(own, &thread) {
            Peer::Gone => {}
            peer @ (Peer::Same | Peer::Unreachable) => found = peer,
            peer @ (Peer::Reachable(_) | Peer::TooMany | Peer::Unknown(..)) => {
                return (peer, seen);
            }
        }
    }
    (found, seen)
}

/// The escaped `comm` of the process or thread at `dir`, empty when it
/// cannot be read.
fn comm_at(dir: &OwnedFd) -> String {
    let Ok(fd) = rustix::fs::openat(dir, "comm", OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
    else {
        return String::new();
    };
    // A name is at most 16 bytes, its newline included.
    let mut buf = [0u8; 64];
    match rustix::io::read(&fd, &mut buf) {
        Ok(n) => escaped(&buf[..n]),
        Err(_) => String::new(),
    }
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
                    "1" => Peer::Same,
                    "2" => Peer::Unreachable,
                    "3" => Peer::Gone,
                    "4" => Peer::Reachable("sleep".into()),
                    _ => Peer::Unknown(Errno::IO, String::new()),
                };
                (peer, 0)
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
            "1" => (Peer::Same, 0),
            "2" => (Peer::Unreachable, 0),
            _ => (Peer::Gone, 0),
        });
        assert_eq!(found, PeerScan::None);
    }

    #[test]
    fn a_peer_scan_refuses_what_it_cannot_tell() {
        let found = scan(names(&["1", "2"]), 8, |pid, _| match pid {
            "1" => (Peer::Same, 0),
            _ => (Peer::Unknown(Errno::LOOP, "x".into()), 0),
        });
        assert_eq!(found, PeerScan::Unknown(process("2", "x"), Errno::LOOP));
        let mut list = names(&["1"]);
        list.push(Err(Errno::IO));
        list.extend(names(&["2"]));
        assert_eq!(
            scan(list, 8, |_, _| (Peer::Same, 0)),
            PeerScan::Unlisted(Errno::IO)
        );
    }

    #[test]
    fn a_peer_scan_past_its_bound_refuses() {
        // Entries that are not pids do not count.
        let list = names(&["1", "self", "2", "sys", "3"]);
        assert_eq!(scan(list, 3, |_, _| (Peer::Same, 0)), PeerScan::None);
        let mut probed = 0;
        let list = names(&["1", "2", "3", "4"]);
        let found = scan(list, 3, |_, _| {
            probed += 1;
            (Peer::Unreachable, 0)
        });
        assert_eq!((found, probed), (PeerScan::TooMany, 3));
        // Threads looked at count too, and a probe is told what is left.
        let mut left = Vec::new();
        let found = scan(names(&["1", "2", "3"]), 4, |_, l| {
            left.push(l);
            (Peer::Gone, 2)
        });
        assert_eq!((found, left), (PeerScan::TooMany, vec![3, 0]));
        let found = scan(names(&["1"]), 4, |_, _| (Peer::TooMany, 4));
        assert_eq!(found, PeerScan::TooMany);
    }

    #[test]
    fn ranikust_itself_is_in_its_own_mount_namespace_and_a_missing_process_is_gone() {
        let own = namespace(rustix::fs::CWD, "/proc/self/ns/mnt").unwrap();
        let me = std::process::id().to_string();
        assert_eq!(probe(own, &me, 8), (Peer::Same, 0));
        // Past the largest pid_max.
        assert_eq!(probe(own, "4294967295", 8), (Peer::Gone, 0));
        // Another namespace's identity, for the same process, named as
        // `comm` names it.
        assert_eq!(
            probe((own.0, own.1 ^ 1), &me, 8),
            (Peer::Reachable(comm(&me)), 0)
        );
        assert!(!comm(&me).is_empty());
    }

    #[test]
    fn the_threads_of_a_process_are_probed_within_the_bound() {
        let own = namespace(rustix::fs::CWD, "/proc/self/ns/mnt").unwrap();
        let dir = rustix::fs::open(
            "/proc/self",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let (peer, seen) = threads(own, &dir, PEER_SCAN_LIMIT);
        assert!(peer == Peer::Same && seen >= 1, "{peer:?} {seen}");
        let (peer, seen) = threads((own.0, own.1 ^ 1), &dir, PEER_SCAN_LIMIT);
        assert!(matches!(peer, Peer::Reachable(_)) && seen == 1, "{peer:?}");
        assert_eq!(threads(own, &dir, 0), (Peer::TooMany, 1));
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
