//! [`Isolation::Direct`](crate::Isolation::Direct): sequential kaniko mode
//! where nothing can be mounted. The command runs as a plain child, as
//! kaniko's `runCommandInExec` does (`kaniko@44aa7ddc0
//! pkg/commands/run.go:288-411`: `exec.Command` with `Setpgid`, the `USER`
//! credentials, `WORKDIR` and the environment, nothing else): its own
//! process group (here a session), the `USER` ids, `WORKDIR`, then `execve`.
//! On a root other than `/` (a host `--workdir`) it first `chroot`s into it,
//! which kaniko, always on `/`, never does. No namespace, mount or
//! capability change; the container or host ranikust runs in is the only
//! isolation.
//!
//! The same rules as in [`crate::child`] hold: no allocation, no lock, only
//! system calls between `clone` and `execve`.

use crate::child::{
    ChildArgs, ChildFds, ExecPtrs, ParentCheck, arm_death_signal, check, switch_user_and_exec,
};
use crate::error::Step;

/// The `Direct` child.
///
/// # Safety
///
/// As [`crate::child::run`]: only in the child of a `clone` without
/// `CLONE_VM`, right after it returned 0, with everything prepared before
/// the clone.
pub(crate) unsafe fn run(args: &ChildArgs, ptrs: &ExecPtrs<'_>, fds: ChildFds) -> ! {
    let parent = ParentCheck::open(fds.err_w, fds.parent);
    arm_death_signal(fds.err_w, parent);
    if args.chroot {
        // `ParentCheck` holds its `/proc` descriptor from before: the root
        // has no `/proc`.
        check(
            fds.err_w,
            Step::Chroot,
            rustix::process::chroot(args.root_dir.as_c_str()),
        );
        check(fds.err_w, Step::Chroot, rustix::process::chdir(c"/"));
    }
    // `setsid` there also makes the process group the parent kills once the
    // command exits, as kaniko does.
    switch_user_and_exec(fds.err_w, parent, args, ptrs)
}
