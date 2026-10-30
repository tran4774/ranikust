//! The seccomp filter of a `RUN` that joined a network namespace owned by a
//! user namespace (`--allow network.none-userns`, ADR-0033).
//!
//! That user namespace belongs to uid 0, so a root `RUN` holds every
//! capability in it, whatever its own capability sets: `CAP_NET_ADMIN` over
//! its network namespace, and the namespace itself through
//! `ioctl(NS_GET_USERNS)` on `/proc/self/ns/net` and `setns`. The filter
//! closes the ways to use that beyond loopback:
//!
//! - `setns`: no joining that user namespace (or any other);
//! - `unshare` and `clone` with `CLONE_NEWUSER`: no new user namespace,
//!   whose root would also hold every capability;
//! - `clone3`: its flags are in memory a filter cannot read, so it fails
//!   with `ENOSYS`, which makes the C libraries fall back to `clone` (as
//!   Docker's profile does);
//! - `socket(AF_NETLINK, …)` except `NETLINK_ROUTE`, `NETLINK_SOCK_DIAG` and
//!   `NETLINK_KOBJECT_UEVENT`: no nf_tables, xfrm, generic netlink (ipvs and
//!   others) or audit. rtnetlink stays open: `ip` and the C libraries'
//!   `getifaddrs` need it, so a root `RUN` can still configure interfaces,
//!   routes and traffic control in its own namespace (the residual risk the
//!   ADR records);
//! - `io_uring_*`: its `IORING_OP_SOCKET` creates sockets without `socket`.
//!
//! Each refusal is `EPERM`, except `clone3`. A syscall of another
//! architecture than the binary's (i386 or x32 on x86_64) is refused
//! whatever its number: the numbers above would not apply.

/// `seccomp_data` offsets: the syscall number, the audit architecture and
/// the low 32 bits of an argument (little-endian).
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg(i: u32) -> u32 {
    16 + 8 * i
}

const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JGE_K: u16 = 0x35;
const JSET_K: u16 = 0x45;
const RET_K: u16 = 0x06;

const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

/// x32 syscalls on x86_64 carry this bit in their number.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// The netlink protocols a `RUN` may open.
const NETLINK_ALLOWED: [u32; 3] = [
    0,  // NETLINK_ROUTE
    4,  // NETLINK_SOCK_DIAG
    15, // NETLINK_KOBJECT_UEVENT
];

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

fn ret_errno(errno: i32) -> libc::sock_filter {
    stmt(
        RET_K,
        RET_ERRNO | (u32::try_from(errno).unwrap_or(0) & 0xffff),
    )
}

fn nr(n: libc::c_long) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The filter, or `None` on an architecture it does not know, where the
/// user-namespace path is refused.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub(crate) fn userns_network() -> Option<Vec<libc::sock_filter>> {
    let eperm = || ret_errno(libc::EPERM);
    let mut p = vec![
        stmt(LD_W_ABS, ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        eperm(),
        stmt(LD_W_ABS, NR),
    ];
    #[cfg(target_arch = "x86_64")]
    p.extend([jump(JGE_K, X32_SYSCALL_BIT, 0, 1), eperm()]);
    for n in [
        libc::SYS_setns,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        p.extend([jump(JEQ_K, nr(n), 0, 1), eperm()]);
    }
    p.extend([
        jump(JEQ_K, nr(libc::SYS_clone3), 0, 1),
        ret_errno(libc::ENOSYS),
    ]);
    let newuser = u32::try_from(libc::CLONE_NEWUSER).unwrap_or(u32::MAX);
    // The flags are the first argument of both, on both architectures.
    for n in [libc::SYS_unshare, libc::SYS_clone] {
        p.extend([
            jump(JEQ_K, nr(n), 0, 4),
            stmt(LD_W_ABS, arg(0)),
            jump(JSET_K, newuser, 0, 1),
            eperm(),
            stmt(RET_K, RET_ALLOW),
        ]);
    }
    let netlink = u32::try_from(libc::AF_NETLINK).unwrap_or(u32::MAX);
    let [a, b, c] = NETLINK_ALLOWED;
    p.extend([
        // Not `socket`: the end of the filter.
        jump(JEQ_K, nr(libc::SYS_socket), 0, 8),
        stmt(LD_W_ABS, arg(0)),
        jump(JEQ_K, netlink, 0, 5),
        stmt(LD_W_ABS, arg(2)),
        jump(JEQ_K, a, 3, 0),
        jump(JEQ_K, b, 2, 0),
        jump(JEQ_K, c, 1, 0),
        eperm(),
        stmt(RET_K, RET_ALLOW),
        stmt(RET_K, RET_ALLOW),
    ]);
    Some(p)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(crate) fn userns_network() -> Option<Vec<libc::sock_filter>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// Each jump lands inside the program, and it ends in a return.
    #[test]
    fn jumps_stay_inside_the_filter() {
        let p = userns_network().unwrap();
        for (i, insn) in p.iter().enumerate() {
            if insn.code & 0x07 == 0x05 {
                assert!(i + 1 + usize::from(insn.jt) < p.len(), "{i}");
                assert!(i + 1 + usize::from(insn.jf) < p.len(), "{i}");
            }
        }
        assert_eq!(p.last().unwrap().code, RET_K);
    }

    /// Installs the filter in a forked child (with `no_new_privs`, which an
    /// unprivileged process needs) and tries each refused and allowed call.
    #[test]
    fn the_filter_refuses_what_it_should_and_nothing_else() {
        let filter = userns_network().unwrap();
        let prog = libc::sock_fprog {
            len: u16::try_from(filter.len()).unwrap(),
            filter: filter.as_ptr().cast_mut(),
        };
        // SAFETY: the child only makes syscalls and exits with `_exit`.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: plain syscalls in the forked child; `prog` lives on.
            unsafe {
                let mut bad = 0;
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::syscall(
                        libc::SYS_seccomp,
                        libc::SECCOMP_SET_MODE_FILTER,
                        0u32,
                        &raw const prog,
                    ) != 0
                {
                    libc::_exit(100);
                }
                let raw = libc::SOCK_RAW | libc::SOCK_CLOEXEC;
                if libc::socket(libc::AF_NETLINK, raw, 12) != -1 || errno() != libc::EPERM {
                    bad = bad.max(1); // NETLINK_NETFILTER
                }
                if libc::socket(libc::AF_NETLINK, raw, 6) != -1 || errno() != libc::EPERM {
                    bad = bad.max(2); // NETLINK_XFRM
                }
                if libc::socket(libc::AF_NETLINK, raw, 0) < 0 {
                    bad = bad.max(3); // NETLINK_ROUTE stays open
                }
                if libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) < 0 {
                    bad = bad.max(4);
                }
                if libc::unshare(libc::CLONE_NEWUSER) != -1 || errno() != libc::EPERM {
                    bad = bad.max(5);
                }
                if libc::setns(-1, 0) != -1 || errno() != libc::EPERM {
                    bad = bad.max(6); // EBADF without the filter
                }
                if libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0usize) != -1
                    || errno() != libc::ENOSYS
                {
                    bad = bad.max(7);
                }
                if libc::syscall(libc::SYS_io_uring_setup, 1u32, std::ptr::null::<u8>()) != -1
                    || errno() != libc::EPERM
                {
                    bad = bad.max(8);
                }
                if libc::unshare(libc::CLONE_FILES) != 0 {
                    bad = bad.max(9);
                }
                libc::_exit(bad);
            }
        }
        let mut status = 0;
        // SAFETY: reaping the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status), "{status:#x}");
        assert_eq!(libc::WEXITSTATUS(status), 0, "the last failing check");
    }
}
