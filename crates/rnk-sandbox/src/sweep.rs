//! What an [`Isolation::Direct`](crate::Isolation::Direct) `RUN` leaves
//! behind, killed once it exits (ADR-0051).
//!
//! A `Direct` `RUN` shares ranikust's PID namespace: a `sleep 300 &`, or a
//! `setsid` daemon that left the `RUN`'s process group, would live on into
//! the walk, the export and the push, writing the root and reading what it
//! can. The process is a child subreaper ([`crate::Sandbox::start_direct`]),
//! so every orphan of the `RUN` becomes ranikust's child. After the `RUN`'s
//! own process exits, everything below ranikust that was not one of its
//! children before the `RUN` started is the `RUN`'s: killed, then reaped by
//! pid, round after round until none is left. ranikust never calls
//! `waitpid(-1)`, which would take children other code waits for.
//!
//! A pid read from `/proc` may be reused before the kill: each process is
//! opened as a pidfd first, its `/proc/<pid>/stat` read again and compared
//! (parent, start time), and the signal sent through the pidfd, which names
//! that process and no later one. Where seccomp refuses `pidfd_open` (an
//! older Docker or libseccomp), ranikust's own children still get a plain
//! `kill`: a child's pid cannot be reused before ranikust reaps it, which
//! it does only after the kill. Deeper processes are left for a later
//! round, when their parents are dead and they are ranikust's children;
//! a tree still there after [`ROUNDS`] rounds fails the build. Reaps never
//! block: a child some `RUN` process holds traced stays for the next round.
//!
//! Relies on `/proc` (the container's, which a `RUN` without
//! `CAP_SYS_ADMIN` cannot replace) and on nothing else of ranikust's
//! starting processes while a `Direct` `RUN` runs: kaniko mode on `/` runs
//! one `RUN` at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::Duration;

use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, WaitOptions};

use crate::error::{Error, Result, sys_err};

/// Rounds of kill and reap before giving up on a fork loop.
const ROUNDS: usize = 1000;
/// The pause between two rounds that still had processes to reap.
const PAUSE: Duration = Duration::from_millis(5);

/// What a `/proc/<pid>/stat` line tells about a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    ppid: i32,
    /// Field 22: clock ticks after boot; with the pid, identifies the
    /// process even after its pid is reused.
    start: u64,
}

/// `pid -> Stat` of every process in `/proc`.
fn process_table() -> Result<BTreeMap<i32, Stat>> {
    let mut table = BTreeMap::new();
    let dir = fs::read_dir("/proc").map_err(|e| sys_err("list /proc", e))?;
    for entry in dir {
        let entry = entry.map_err(|e| sys_err("list /proc", e))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some(stat) = read_stat(pid) {
            table.insert(pid, stat);
        }
    }
    Ok(table)
}

/// `/proc/<pid>/stat`, or `None` when the process is gone.
fn read_stat(pid: i32) -> Option<Stat> {
    parse_stat(&fs::read(format!("/proc/{pid}/stat")).ok()?)
}

/// The parent pid (field 4) and start time (field 22) of a
/// `/proc/<pid>/stat` line, read after the command name in parentheses,
/// which may itself hold spaces and `)`.
fn parse_stat(stat: &[u8]) -> Option<Stat> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
    // Field 3 (state) is the first after the name.
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    Some(Stat {
        ppid: fields.get(1)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

/// ranikust's children now: the helpers a sweep leaves alone.
pub(crate) fn children_of_self() -> Result<Vec<i32>> {
    let me = rustix::process::getpid().as_raw_nonzero().get();
    Ok(process_table()?
        .into_iter()
        .filter(|(_, s)| s.ppid == me)
        .map(|(pid, _)| pid)
        .collect())
}

/// The processes of a `RUN` in `table`: every process whose chain of
/// parents reaches `me` through a child of `me` that is not in `helpers`,
/// that child included. `leader` (the `RUN`'s own process, waited for but
/// not reaped yet) is left out: its caller reaps it.
fn run_tree(table: &BTreeMap<i32, Stat>, me: i32, leader: i32, helpers: &[i32]) -> BTreeSet<i32> {
    let helpers: BTreeSet<i32> = helpers.iter().copied().collect();
    let mut tree = BTreeSet::new();
    for &pid in table.keys() {
        let mut p = pid;
        // A chain longer than the table loops: the table changed under the
        // read. Such a process is caught on the next round.
        for _ in 0..=table.len() {
            let Some(stat) = table.get(&p) else { break };
            if stat.ppid == me {
                if !helpers.contains(&p) {
                    tree.insert(pid);
                }
                break;
            }
            p = stat.ppid;
        }
    }
    tree.remove(&leader);
    tree.remove(&me);
    tree
}

/// Kills and reaps what the `RUN` led by `leader` left, until nothing is
/// left; `helpers` are ranikust's children from before the `RUN`.
///
/// # Errors
///
/// [`Error::Survivors`] when processes are still there after [`ROUNDS`]
/// rounds; a system call failing otherwise than with `ESRCH` or `ECHILD`.
pub(crate) fn kill_run_tree(leader: Pid, helpers: &[i32]) -> Result<()> {
    let me = rustix::process::getpid().as_raw_nonzero().get();
    let leader = leader.as_raw_nonzero().get();
    let mut left = 0;
    for _ in 0..ROUNDS {
        let table = process_table()?;
        let tree = run_tree(&table, me, leader, helpers);
        if tree.is_empty() {
            return Ok(());
        }
        left = tree.len();
        for &pid in &tree {
            if let Some(&stat) = table.get(&pid) {
                kill(pid, stat, me)?;
            }
        }
        // Reap ranikust's own children of the tree; the others become its
        // children when their parents die, and are reaped in a later round.
        for &pid in tree
            .iter()
            .filter(|p| table.get(p).map(|s| s.ppid) == Some(me))
        {
            reap(pid)?;
        }
        std::thread::sleep(PAUSE);
    }
    Err(Error::Survivors { left })
}

/// What to do with a process `pidfd_open` failed on with `errno`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoPidfd {
    /// Gone already.
    Gone,
    /// ranikust's own child: its pid stays its own until reaped, so a plain
    /// `kill` names it.
    Kill,
    /// Deeper: left for a later round, when it is ranikust's child.
    Later,
    /// Any other failure.
    Fail,
}

/// [`NoPidfd`] for a process whose parent is `ppid`; `me` is ranikust.
/// `ENOSYS` and `EPERM` are what a seccomp filter returns for a system call
/// it does not know. `EINVAL` (for a valid pid and no flags) says the pid
/// now names a thread other than a thread-group leader: the process listed
/// is gone and its pid was taken by another process's thread, which must
/// not be touched.
fn without_pidfd(errno: Errno, ppid: i32, me: i32) -> NoPidfd {
    match errno {
        Errno::SRCH | Errno::INVAL => NoPidfd::Gone,
        Errno::NOSYS | Errno::PERM if ppid == me => NoPidfd::Kill,
        Errno::NOSYS | Errno::PERM => NoPidfd::Later,
        _ => NoPidfd::Fail,
    }
}

/// `SIGKILL` to `pid` if it is still the process `stat` describes: opened
/// as a pidfd, then checked again, then signalled through the pidfd; or,
/// where seccomp refuses pidfds, as [`without_pidfd`] says. `me` is
/// ranikust's pid.
fn kill(pid: i32, stat: Stat, me: i32) -> Result<()> {
    let Some(raw) = Pid::from_raw(pid) else {
        return Ok(());
    };
    let fd = match rustix::process::pidfd_open(raw, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            return match without_pidfd(e, stat.ppid, me) {
                NoPidfd::Gone | NoPidfd::Later => Ok(()),
                // Not reaped yet, so still this child (or its zombie).
                NoPidfd::Kill => match rustix::process::kill_process(raw, Signal::KILL) {
                    Ok(()) | Err(Errno::SRCH) => Ok(()),
                    Err(e) => Err(sys_err("kill a process the RUN left", e)),
                },
                NoPidfd::Fail => Err(sys_err("pidfd_open a process the RUN left", e)),
            };
        }
    };
    // The pidfd holds the process: if the stat still matches, the pidfd is
    // that process and no other one that took its pid since.
    if read_stat(pid) != Some(stat) {
        return Ok(());
    }
    match rustix::process::pidfd_send_signal(&fd, Signal::KILL) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(e) => Err(sys_err("kill a process the RUN left", e)),
    }
}

/// Reaps `pid` if it has exited; never waits.
fn reap(pid: i32) -> Result<()> {
    let Some(pid) = Pid::from_raw(pid) else {
        return Ok(());
    };
    loop {
        match rustix::process::waitpid(Some(pid), WaitOptions::NOHANG) {
            Ok(_) | Err(Errno::CHILD) => return Ok(()),
            Err(Errno::INTR) => {}
            Err(e) => return Err(sys_err("reap a process the RUN left", e)),
        }
    }
}

/// Warns when `kernel.yama.ptrace_scope` is 0: a `RUN` process may then
/// attach to any other process of its uid that is dumpable, other `RUN`
/// processes included (ranikust itself is not dumpable). Silent when Yama
/// is absent or stricter.
pub(crate) fn warn_ptrace_scope() {
    if let Ok(scope) = fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        && scope.trim() == "0"
    {
        tracing::warn!(
            "kernel.yama.ptrace_scope is 0 on this host: a RUN process may attach to another \
             dumpable process of its user; ranikust itself is not dumpable"
        );
    }
}

#[cfg(test)]
mod tests;
