//! [`Isolation::Direct`](crate::Isolation::Direct): sequential kaniko mode
//! where nothing can be mounted. The command runs on `/` as a plain child, as
//! kaniko's `runCommandInExec` does: its own process group (here a session),
//! the `USER` ids, `WORKDIR`, then `execve`. No namespace, mount or
//! capability change; the container ranikust runs in is the only isolation.
//!
//! The same rules as in [`crate::child`] hold: no allocation, no lock, only
//! system calls between `clone` and `execve`.

use crate::child::{ChildArgs, ChildFds, ExecPtrs, arm_death_signal, switch_user_and_exec};

/// The `Direct` child.
///
/// # Safety
///
/// As [`crate::child::run`]: only in the child of a `clone` without
/// `CLONE_VM`, right after it returned 0, with everything prepared before
/// the clone.
pub(crate) unsafe fn run(args: &ChildArgs, ptrs: &ExecPtrs<'_>, fds: ChildFds) -> ! {
    arm_death_signal(fds.err_w, fds.parent);
    // `setsid` there also makes the process group the parent kills once the
    // command exits, as kaniko does.
    switch_user_and_exec(fds.err_w, fds.parent, args, ptrs)
}
