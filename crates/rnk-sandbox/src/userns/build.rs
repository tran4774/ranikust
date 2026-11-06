//! The build's own user namespace ([`crate::enter_build_user_namespace`]):
//! this process enters it, then a mount namespace it owns, with the first
//! map of the probe's order that works here.
//!
//! A map that fails once this process is in the new user namespace leaves
//! it there, unmapped, for good. So each map is first tried in a forked
//! child, the probe's way ([`supervise`]), up to the mount namespace; only
//! a map whose trial passed is used for this process. A ranged map is
//! written from outside the namespace, by a helper forked before the
//! `unshare`: the kernel takes a map that names other ids only from a
//! writer in the parent namespace (`new_idmap_permitted`).

use std::fmt;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::time::Instant;

use rustix::io::Errno;
use rustix::mount::MountPropagationFlags;
use rustix::pipe::PipeFlags;
use rustix::process::{DumpableBehavior, Pid, Signal};
use rustix::thread::UnshareFlags;

use super::{
    ATTEMPT_DEADLINE, CLOSURE_OK, CLOSURE_REFUSED, DONE, IdMap, NO_MAP, Plan, REAP_GRACE, SLAVE,
    SLAVE_OP, STOPPED, StepFailure, StepResult, SubordinateIds, Supervised, UNSHARE_MOUNT,
    assemble, enter, enter_mount_namespace, map_lines, plans, read_records, reap, result_frame,
    send, supervise, write_all, write_map, write_own,
};
use crate::child;
use crate::error::{Error, Result, sys_err};

/// How long the helper may take to write the map: `newuidmap`, then
/// `newgidmap`, each with the attempt's deadline, and some slack.
const HELPER_DEADLINE: std::time::Duration = ATTEMPT_DEADLINE.saturating_mul(3);

/// The user namespace the build entered, as read back from this process's
/// `/proc` files once in it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildUserNamespace {
    /// The map it got: [`IdMap::OneId`] whenever its uid or gid map gives
    /// one id only, whichever map was written.
    pub map: IdMap,
    /// The maps of the probe's order tried before it, each with the step
    /// that stopped its trial or why it was not tried.
    pub passed_over: Vec<(IdMap, StepResult)>,
    setgroups: bool,
}

impl BuildUserNamespace {
    /// Whether `setgroups` is allowed in it: not where this process wrote
    /// its one-id map itself, nor where the parent namespace denies it.
    #[must_use]
    pub fn setgroups_allowed(&self) -> bool {
        self.setgroups
    }
}

impl fmt::Display for BuildUserNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} map", self.map)?;
        if !self.setgroups_allowed() {
            f.write_str(", setgroups denied")?;
        }
        if !self.passed_over.is_empty() {
            write!(f, " ({})", tried_text(&self.passed_over))?;
        }
        Ok(())
    }
}

/// Each map with its result, for messages: `newuidmap: not run (...)`.
pub(crate) fn tried_text(tried: &[(IdMap, StepResult)]) -> String {
    tried
        .iter()
        .map(|(m, r)| format!("{m}: {r}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a trial of one map found.
#[derive(Debug, PartialEq, Eq)]
enum Trial {
    /// Every step passed: the user namespace, the map, the mount namespace
    /// and `/` a recursive slave.
    Passed,
    /// `unshare(CLONE_NEWUSER)` itself was refused: no later map helps.
    NoNamespace(StepResult),
    /// The map or a later step was refused, or the child stopped first.
    Stopped(StepResult),
}

/// [`crate::enter_build_user_namespace`] once its checks passed: the maps
/// of the probe's order (root: its own ranges, then one id; another user:
/// `newuidmap` with `subordinate`, then one id), each tried in a child,
/// then the first that passed, for this process.
pub(crate) fn enter_build(subordinate: Option<SubordinateIds>) -> Result<BuildUserNamespace> {
    let euid = rustix::process::geteuid().as_raw();
    let egid = rustix::process::getegid().as_raw();
    let user = euid != 0;
    let mut tried = Vec::new();
    for candidate in plans(euid, egid, subordinate) {
        let plan = match candidate {
            Ok(plan) => plan,
            Err((kind, result)) => {
                tried.push((kind, result));
                continue;
            }
        };
        match trial(&plan, user)? {
            Trial::Passed => {
                enter_with(&plan, user)?;
                let (map, setgroups) = measure(plan.kind()).map_err(Error::UserNamespaceBroken)?;
                return Ok(BuildUserNamespace {
                    map,
                    passed_over: tried,
                    setgroups,
                });
            }
            Trial::NoNamespace(r) => {
                tried.push((plan.kind(), r));
                break;
            }
            Trial::Stopped(r) => tried.push((plan.kind(), r)),
        }
    }
    Err(Error::NoUserNamespace { tried })
}

/// Tries `plan` in a forked child: the steps [`enter_with`] takes, which
/// leave this process as it is.
fn trial(plan: &Plan, user: bool) -> Result<Trial> {
    let run = supervise(plan, user, None, -1, ATTEMPT_DEADLINE, |args| {
        // SAFETY: the report pipe's write end, open in this child.
        let report = unsafe { BorrowedFd::borrow_raw(args.report) };
        if enter(args, report) && enter_mount_namespace(report) {
            send(report, DONE, Ok(()));
        }
    })?;
    Ok(judge(plan, run))
}

/// The [`Trial`] a child's records tell.
fn judge(plan: &Plan, run: Supervised) -> Trial {
    let records = &run.records.list;
    let get = |op: u8| records.iter().find(|r| r.0 == op).map(|r| r.1);
    let a = assemble(plan, records, run.parent_map, None);
    if let StepResult::Failed(_) = a.unshare {
        return Trial::NoNamespace(a.unshare);
    }
    let slave = match get(SLAVE) {
        Some(0) => StepResult::Ok,
        Some(e) => StepResult::Failed(StepFailure::errno(SLAVE_OP, e)),
        None => StepResult::NotRun(STOPPED),
    };
    let done = match get(DONE) {
        Some(0) => StepResult::Ok,
        _ => StepResult::NotRun(STOPPED),
    };
    let first = [a.unshare, a.map, a.mount_ns, slave, done]
        .into_iter()
        .find(|r| *r != StepResult::Ok);
    match (first, run.cut_short) {
        (None, _) => Trial::Passed,
        // A child killed or dead early says how instead.
        (Some(StepResult::NotRun(_)), Some(c)) => {
            Trial::Stopped(StepResult::Failed(StepFailure::other("the trial", c)))
        }
        (Some(r), _) => Trial::Stopped(r),
    }
}

/// Enters the user namespace with `plan`'s map, then the mount namespace,
/// as the trial did. Before the `unshare`, an error leaves this process as
/// it was; after it, [`Error::UserNamespaceBroken`].
fn enter_with(plan: &Plan, user: bool) -> Result<()> {
    let helper = if plan.by_parent() {
        Some(MapHelper::start(plan)?)
    } else {
        None
    };
    // SAFETY: `NEWUSER` does not unshare the descriptor table, the only flag
    // `unshare_unsafe` is unsafe for; the process is single-threaded (the
    // caller checked; the helper is a process of its own).
    if let Err(e) = unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWUSER) } {
        if let Some(h) = helper {
            h.cancel();
        }
        return Err(sys_err("unshare(CLONE_NEWUSER)", e));
    }
    // The map's writer opens this process's `/proc` files, which are root's
    // while it is not dumpable (ADR-0051): dumpable for the map only, as the
    // probe's child.
    let dumpable = if user {
        rustix::process::set_dumpable_behavior(DumpableBehavior::Dumpable)
            .map_err(|e| StepFailure::errno("prctl(PR_SET_DUMPABLE, 1)", e.raw_os_error()))
    } else {
        Ok(())
    };
    let mapped = match (dumpable, helper, plan.own_map()) {
        (Err(f), helper, _) => {
            if let Some(h) = helper {
                h.cancel();
            }
            Err(f)
        }
        (Ok(()), Some(h), _) => h.write(),
        (Ok(()), None, Some((uid_map, gid_map))) => write_own_map(uid_map, gid_map),
        (Ok(()), None, None) => Err(StepFailure::other("map", NO_MAP)),
    };
    let undumped = undumpable(user);
    mapped.map_err(Error::UserNamespaceBroken)?;
    undumped.map_err(Error::UserNamespaceBroken)?;
    // Every map makes this process's own ids 0 inside.
    if !rustix::process::geteuid().is_root() {
        return Err(Error::UserNamespaceBroken(StepFailure::other(
            "check the map",
            "this process is not root in its user namespace",
        )));
    }
    // SAFETY: as above, for `NEWNS`.
    unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) }.map_err(|e| {
        Error::UserNamespaceBroken(StepFailure::errno(UNSHARE_MOUNT, e.raw_os_error()))
    })?;
    rustix::mount::mount_change(
        c"/",
        MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC,
    )
    .map_err(|e| Error::UserNamespaceBroken(StepFailure::errno(SLAVE_OP, e.raw_os_error())))
}

/// The map and whether `setgroups` is allowed in the user namespace this
/// process entered with a `plan` map, from its `/proc` files.
fn measure(plan: IdMap) -> std::result::Result<(IdMap, bool), StepFailure> {
    let read = |name: &'static str| {
        std::fs::read_to_string(format!("/proc/self/{name}"))
            .map_err(|e| StepFailure::other(name, format!("cannot be read: {e}")))
    };
    let (uid_map, gid_map, setgroups) = (read("uid_map")?, read("gid_map")?, read("setgroups")?);
    as_entered(plan, &uid_map, &gid_map, &setgroups).ok_or_else(|| {
        StepFailure::other(
            "check the map",
            "its /proc files are not as the kernel writes them",
        )
    })
}

/// What a namespace entered with a `plan` map has: one id when its uid or
/// gid map gives one only (root's own map mirrored can: rootless Podman
/// without subordinate ids, `unshare -r`), and `setgroups` as its file
/// says, since a namespace inherits a parent's `deny`. `None` when a file
/// is not as the kernel writes it, or a map is empty.
fn as_entered(plan: IdMap, uid_map: &str, gid_map: &str, setgroups: &str) -> Option<(IdMap, bool)> {
    let (uids, gids) = (ids(uid_map)?, ids(gid_map)?);
    let map = if uids == 1 || gids == 1 {
        IdMap::OneId
    } else {
        plan
    };
    let allowed = match setgroups.trim_end() {
        "allow" => true,
        "deny" => false,
        _ => return None,
    };
    Some((map, allowed))
}

/// How many ids a map's text gives: `None` when a line is not three
/// numbers, or it gives none.
fn ids(map: &str) -> Option<u64> {
    let total = map_lines(map)?
        .iter()
        .try_fold(0_u64, |t, [_, _, count]| t.checked_add(u64::from(*count)))?;
    (total > 0).then_some(total)
}

/// Writes this process's one-id map itself, `setgroups` denied first, as
/// the kernel asks of a writer without `CAP_SETGID` above the namespace.
fn write_own_map(uid_map: &[u8], gid_map: &[u8]) -> std::result::Result<(), StepFailure> {
    let write = |op, path, text| {
        write_own(path, text).map_err(|e| StepFailure::errno(op, e.raw_os_error()))
    };
    write("write uid_map", c"/proc/self/uid_map", uid_map)?;
    write("write setgroups", c"/proc/self/setgroups", b"deny")?;
    write("write gid_map", c"/proc/self/gid_map", gid_map)
}

/// Makes a user's process not dumpable again after its map (ADR-0051).
fn undumpable(user: bool) -> std::result::Result<(), StepFailure> {
    if !user {
        return Ok(());
    }
    rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)
        .map_err(|e| StepFailure::errno("prctl(PR_SET_DUMPABLE, 0)", e.raw_os_error()))
}

/// The child that writes this process's map ([`write_map`]) once told to,
/// forked before `unshare(CLONE_NEWUSER)`: it stays in the namespace above
/// the new one, where the writer of a ranged map must be.
struct MapHelper {
    pid: Pid,
    /// The write end of its go pipe: one `g`, then closed.
    go: OwnedFd,
    /// The read end of its result: one [`super::RESULT`] record.
    result: OwnedFd,
}

impl MapHelper {
    /// Forks the helper. Call while single-threaded: the helper may run a
    /// tool, which allocates.
    fn start(plan: &Plan) -> Result<Self> {
        let (go_r, go_w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC)
            .map_err(|e| sys_err("make the map helper's pipe", e))?;
        let (result_r, result_w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC)
            .map_err(|e| sys_err("make the map helper's pipe", e))?;
        let parent = rustix::process::getpid();
        // SAFETY: the child closes what it does not use, then makes system
        // calls and runs `write_map`, which allocates and may start a tool:
        // the process is single-threaded (the caller's contract), so its
        // allocator is whole in the child. It ends with `_exit`.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            drop(go_w);
            drop(result_r);
            let mut keep = [0, 1, 2, go_r.as_raw_fd(), result_w.as_raw_fd()];
            keep.sort_unstable();
            // SAFETY: this forked child ends with `_exit` and uses no owner
            // of a descriptor this closes; it keeps its two pipe ends.
            unsafe { child::close_all_except(&keep) };
            helper_main(plan, parent, go_r.as_fd(), result_w.as_fd());
            // SAFETY: `_exit` ends this process at once, running no
            // destructor of the parent's copy.
            unsafe { libc::_exit(0) }
        }
        let fork_error = std::io::Error::last_os_error();
        drop(go_r);
        drop(result_w);
        let pid = (pid > 0)
            .then(|| Pid::from_raw(pid))
            .flatten()
            .ok_or_else(|| sys_err("fork the map helper", fork_error))?;
        Ok(Self {
            pid,
            go: go_w,
            result: result_r,
        })
    }

    /// Tells the helper to write the map, now that this process is in the
    /// new namespace, and waits for its result.
    fn write(self) -> std::result::Result<(), StepFailure> {
        let sent = write_all(self.go.as_fd(), b"g");
        drop(self.go);
        let read = match sent {
            Ok(()) => read_records(&self.result, Instant::now() + HELPER_DEADLINE, |_| {})
                .map_err(|e| e.to_string()),
            Err(e) => Err(format!("cannot tell the map helper to go: {e}")),
        };
        let killed = read.as_ref().map_or(true, |r| r.timed_out);
        if killed {
            // `ESRCH` when it already exited.
            let _ = rustix::process::kill_process(self.pid, Signal::KILL);
        }
        match reap(self.pid, killed, REAP_GRACE) {
            Ok(Some(_)) => {}
            Ok(None) => return Err(StepFailure::other("map", "the map helper did not end")),
            Err(e) => {
                return Err(StepFailure::other(
                    "map",
                    format!("cannot reap the map helper: {e}"),
                ));
            }
        }
        let records = read.map_err(|e| StepFailure::other("map", e))?;
        if records.timed_out {
            return Err(StepFailure::other(
                "map",
                format!(
                    "the map helper was killed after {} s",
                    HELPER_DEADLINE.as_secs()
                ),
            ));
        }
        match records.payload {
            Some((CLOSURE_OK, _)) => Ok(()),
            Some((_, text)) => Err(StepFailure::other(
                "map",
                String::from_utf8_lossy(&text).into_owned(),
            )),
            None => Err(StepFailure::other(
                "map",
                "the map helper ended without a result",
            )),
        }
    }

    /// Ends a helper that will not be told to go: its go pipe closes, so it
    /// writes nothing, and it is reaped, killed if it lingers.
    fn cancel(self) {
        drop(self.go);
        // Nothing is left to do about a helper that did not end: it wrote
        // no map, and it dies with this process (`PR_SET_PDEATHSIG`).
        let _ = reap(self.pid, false, REAP_GRACE);
    }
}

/// The helper's life: death with this process, then, on the go byte, the
/// map of `parent`, reported as a [`super::RESULT`] record.
fn helper_main(plan: &Plan, parent: Pid, go: BorrowedFd<'_>, result: BorrowedFd<'_>) {
    child::default_signals();
    let armed = rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).is_ok();
    if !armed || rustix::process::getppid() != Some(parent) {
        return;
    }
    let mut byte = [0u8];
    let go_ahead = loop {
        match rustix::io::read(go, &mut byte) {
            Err(Errno::INTR) => {}
            r => break matches!(r, Ok(1)) && byte[0] == b'g',
        }
    };
    if !go_ahead {
        return;
    }
    let frame = match write_map(plan, parent) {
        Ok(()) => result_frame(CLOSURE_OK, ""),
        Err(f) => result_frame(CLOSURE_REFUSED, &f.to_string()),
    };
    // A parent gone reads nothing; one still here sees no result.
    let _ = write_all(result, &frame);
}

#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod tests;
