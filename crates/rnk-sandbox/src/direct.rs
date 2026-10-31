//! [`Isolation::Direct`](crate::Isolation::Direct): sequential kaniko mode
//! where nothing can be mounted. The command runs as a plain child, as
//! kaniko's `runCommandInExec` does (`kaniko@44aa7ddc0
//! pkg/commands/run.go:288-411`: `exec.Command` with `Setpgid`, the `USER`
//! credentials, `WORKDIR` and the environment, nothing else): its own
//! process group (here a session), the `USER` ids, `WORKDIR`, then `execve`.
//! On a root other than `/` (a directory of the state directory on a host,
//! ADR-0050, only where the full sandbox cannot run) it first `chroot`s
//! into it, which kaniko, always on `/`, never does, checks that the new `/` is the directory the parent checked,
//! and cuts its capabilities to Docker's defaults. On `/` it keeps the
//! container's capabilities except those outside Docker's defaults, which
//! it drops from its bounding set when ranikust may (ADR-0051). No
//! namespace or mount; the container ranikust runs in is the only
//! isolation. Once the command exits, the parent kills what is left of its
//! process tree, `setsid` ones included ([`crate::spawner`]'s sweep).
//!
//! The same rules as in [`crate::child`] hold: no allocation, no lock, only
//! system calls between `clone` and `execve`.

use rustix::io::Errno;

use crate::child::{
    ChildArgs, ChildFds, ExecPtrs, ParentCheck, arm_death_signal, check, drop_capabilities, fail,
    switch_user_and_exec, trim_capabilities,
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
    if let Some(want) = args.chroot {
        // `ParentCheck` holds its `/proc` descriptor from before: the root
        // has no `/proc`.
        check(
            fds.err_w,
            Step::Chroot,
            rustix::process::chroot(args.root_dir.as_c_str()),
        );
        check(fds.err_w, Step::Chroot, rustix::process::chdir(c"/"));
        // The path was looked up again here: it must still name the
        // directory the parent checked and placed the files in.
        let st = check(
            fds.err_w,
            Step::Chroot,
            rustix::fs::statx(
                rustix::fs::CWD,
                c"/",
                rustix::fs::AtFlags::empty(),
                rustix::fs::StatxFlags::BASIC_STATS,
            ),
        );
        if (st.stx_dev_major, st.stx_dev_minor, st.stx_ino) != want {
            fail(fds.err_w, Step::Chroot, Errno::XDEV);
        }
        // Root in a bare `chroot` must not keep what would let it leave or
        // mount: the bounding set, effective and permitted sets go to
        // Docker's defaults, as in the full sandbox.
        drop_capabilities(fds.err_w, args);
    } else if args.direct_drop {
        // On `/` the container's capabilities stay, as kaniko, except
        // those outside Docker's defaults: a `RUN` with `CAP_SYS_PTRACE`
        // would read ranikust's descriptors and memory (ADR-0051).
        drop_capabilities(fds.err_w, args);
    } else {
        // Nothing to drop from the bounding set (or no `CAP_SETPCAP`, and
        // the parent checked nothing is there to drop): the other sets
        // still go to Docker's defaults, which needs no privilege.
        trim_capabilities(fds.err_w, args);
    }
    // `setsid` there also makes the process group the parent kills once the
    // command exits, as kaniko does.
    switch_user_and_exec(fds.err_w, parent, args, ptrs)
}
