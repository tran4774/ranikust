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
//! Relies on `/proc` (the container's, which a `RUN` without
//! `CAP_SYS_ADMIN` cannot replace) and on nothing else of ranikust's
//! starting processes while a `Direct` `RUN` runs: kaniko mode on `/` runs
//! one `RUN` at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions};

use crate::error::{Error, Result, sys_err};

/// Rounds of kill and reap before giving up on a fork loop.
const ROUNDS: usize = 1000;

/// `pid -> ppid` of every process in `/proc`.
fn process_table() -> Result<BTreeMap<i32, i32>> {
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
        // Gone since the listing: nothing to kill.
        let Ok(stat) = fs::read(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some(ppid) = parent_of(&stat) {
            table.insert(pid, ppid);
        }
    }
    Ok(table)
}

/// The parent pid in a `/proc/<pid>/stat` line: the fourth field, after the
/// command name in parentheses, which may itself hold spaces and `)`.
fn parent_of(stat: &[u8]) -> Option<i32> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
    let mut fields = rest.split_ascii_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// ranikust's children now: the helpers a sweep leaves alone.
pub(crate) fn children_of_self() -> Result<Vec<i32>> {
    let me = rustix::process::getpid().as_raw_nonzero().get();
    Ok(process_table()?
        .into_iter()
        .filter(|&(_, ppid)| ppid == me)
        .map(|(pid, _)| pid)
        .collect())
}

/// The processes of a `RUN` in `table`: every process whose chain of
/// parents reaches `me` through a child of `me` that is not in `helpers`,
/// that child included. `leader` (the `RUN`'s own process, waited for but
/// not reaped yet) is left out: its caller reaps it.
fn run_tree(table: &BTreeMap<i32, i32>, me: i32, leader: i32, helpers: &[i32]) -> BTreeSet<i32> {
    let helpers: BTreeSet<i32> = helpers.iter().copied().collect();
    let mut tree = BTreeSet::new();
    for &pid in table.keys() {
        let mut p = pid;
        // A chain longer than the table loops: the table changed under the
        // read. Such a process is caught on the next round.
        for _ in 0..=table.len() {
            let Some(&ppid) = table.get(&p) else { break };
            if ppid == me {
                if !helpers.contains(&p) {
                    tree.insert(pid);
                }
                break;
            }
            p = ppid;
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
/// [`Error::Survivors`] when processes keep appearing after
/// [`ROUNDS`] rounds; a system call failing otherwise than with `ESRCH` or
/// `ECHILD`.
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
            kill(pid)?;
        }
        // Reap ranikust's own children of the tree; the others become its
        // children when their parents die, and are reaped in a later round.
        for &pid in tree.iter().filter(|&&p| table.get(&p) == Some(&me)) {
            reap(pid)?;
        }
    }
    Err(Error::Survivors { left })
}

fn kill(pid: i32) -> Result<()> {
    let Some(pid) = Pid::from_raw(pid) else {
        return Ok(());
    };
    match rustix::process::kill_process(pid, Signal::KILL) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(e) => Err(sys_err("kill a process the RUN left", e)),
    }
}

fn reap(pid: i32) -> Result<()> {
    let Some(pid) = Pid::from_raw(pid) else {
        return Ok(());
    };
    loop {
        match rustix::process::waitpid(Some(pid), WaitOptions::empty()) {
            Ok(_) | Err(Errno::CHILD) => return Ok(()),
            Err(Errno::INTR) => {}
            Err(e) => return Err(sys_err("reap a process the RUN left", e)),
        }
    }
}

#[cfg(test)]
mod tests;
