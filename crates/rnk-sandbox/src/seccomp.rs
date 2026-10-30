//! The seccomp filter of a `RUN` that joined a network namespace owned by a
//! user namespace (`--allow network.none-userns`, ADR-0033).
//!
//! That user namespace belongs to uid 0, so a root `RUN` holds every
//! capability in it, whatever its own capability sets: `CAP_NET_ADMIN` over
//! its network namespace, and the namespace itself through
//! `ioctl(NS_GET_USERNS)` on `/proc/self/ns/net` and `setns`. The filter is
//! a denylist of the ways to use that, each verified against the kernel's
//! uapi headers:
//!
//! - `setns`, of any kind: no joining that user namespace (or another);
//! - `unshare` and `clone` with `CLONE_NEWUSER`: no new user namespace,
//!   whose root would also hold every capability;
//! - `clone3` gets `ENOSYS`: its flags are in memory a filter cannot read,
//!   and `ENOSYS` makes the C libraries fall back to `clone` (as Docker's
//!   profile does);
//! - `io_uring_*`: `IORING_OP_SOCKET` and its `setsockopt` operations would
//!   pass the rules below;
//! - `socket(AF_KEY)`: xfrm through PF_KEY;
//! - `socket(AF_NETLINK)` except `NETLINK_ROUTE`, `NETLINK_SOCK_DIAG`,
//!   `NETLINK_AUDIT` (which checks capabilities in the initial user
//!   namespace; shadow's `useradd` needs it) and `NETLINK_KOBJECT_UEVENT`:
//!   nf_tables, xfrm, generic netlink (IPVS, WireGuard…) and the rest fail
//!   with `EPROTONOSUPPORT`, as a kernel without them would;
//! - `setsockopt` and `getsockopt` options that need `CAP_NET_ADMIN` over
//!   the namespace: at `SOL_IP`, `IP_IPSEC_POLICY` and `IP_XFRM_POLICY`
//!   (16–17), ip_tables (64–67), arp_tables (96–99), ebtables (128–132),
//!   multicast routing (200–212) and IPVS (1152–1167); at `SOL_IPV6`,
//!   `IPV6_IPSEC_POLICY` and `IPV6_XFRM_POLICY` (34–35), ip6_tables (64–65,
//!   68–69; 66–67 are `IPV6_RECVTCLASS` and `IPV6_TCLASS`)
//!   and multicast routing (200–212); at `SOL_TCP`, `TCP_REPAIR` (19).
//!
//! Every other refusal is `EPERM`. A syscall of another architecture than
//! the binary's (i386 or x32 on x86_64) is refused whatever its number: the
//! numbers above would not apply. Any other interface `CAP_NET_ADMIN` opens
//! over that namespace stays open, rtnetlink first: `ip` and the C
//! libraries' `getifaddrs` need it (ADR-0033 records the residual risk).

/// `seccomp_data` offsets: the syscall number, the audit architecture and
/// the low 32 bits of an argument (little-endian).
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg(i: u32) -> u32 {
    16 + 8 * i
}

const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JGT_K: u16 = 0x25;
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

const AF_KEY: u32 = 15;
const AF_NETLINK: u32 = 16;

/// The netlink protocols a `RUN` may open: route, sock_diag, audit,
/// kobject_uevent.
const NETLINK_ALLOWED: [u32; 4] = [0, 4, 9, 15];

const SOL_IP: u32 = 0;
const SOL_TCP: u32 = 6;
const SOL_IPV6: u32 = 41;

/// Option ranges at `SOL_IP` (`linux/in.h`, `netfilter_ipv4/ip_tables.h`,
/// `netfilter_arp/arp_tables.h`, `netfilter_bridge/ebtables.h`,
/// `mroute.h`, `ip_vs.h`), inclusive.
const SOL_IP_REFUSED: [(u32, u32); 6] = [
    (16, 17),     // IP_IPSEC_POLICY, IP_XFRM_POLICY
    (64, 67),     // IPT_SO_*: IPT_BASE_CTL .. IPT_SO_GET_MAX
    (96, 99),     // ARPT_SO_*: ARPT_BASE_CTL .. ARPT_SO_GET_MAX
    (128, 132),   // EBT_SO_*: EBT_BASE_CTL .. EBT_SO_GET_MAX
    (200, 212),   // MRT_BASE .. MRT_MAX
    (1152, 1167), // IP_VS_BASE_CTL .. IP_VS_SO_SET_MAX
];

/// Option ranges at `SOL_IPV6` (`linux/in6.h`,
/// `netfilter_ipv6/ip6_tables.h`, `mroute6.h`), inclusive.
const SOL_IPV6_REFUSED: [(u32, u32); 4] = [
    (34, 35), // IPV6_IPSEC_POLICY, IPV6_XFRM_POLICY
    // IP6T_SO_*: IP6T_BASE_CTL .. IP6T_SO_GET_MAX, around IPV6_RECVTCLASS
    // (66) and IPV6_TCLASS (67), which share the numbers at this level.
    (64, 65),
    (68, 69),
    (200, 212), // MRT6_BASE .. MRT6_MAX
];

/// `TCP_REPAIR` (`linux/tcp.h`).
const TCP_REPAIR: u32 = 19;

/// A jump target, bound to an instruction index by [`Asm::bind`].
#[derive(Clone, Copy)]
struct Label(usize);

enum Op {
    Ld(u32),
    Jump(u16, u32, Label, Label),
    Ret(u32),
}

/// A classic BPF program built with labels, so that no jump offset is
/// counted by hand.
#[derive(Default)]
struct Asm {
    ops: Vec<Op>,
    labels: Vec<Option<usize>>,
}

impl Asm {
    fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    fn bind(&mut self, l: Label) {
        self.labels[l.0] = Some(self.ops.len());
    }

    fn ld(&mut self, offset: u32) {
        self.ops.push(Op::Ld(offset));
    }

    fn jump(&mut self, code: u16, k: u32, t: Label, f: Label) {
        self.ops.push(Op::Jump(code, k, t, f));
    }

    fn ret(&mut self, action: u32) {
        self.ops.push(Op::Ret(action));
    }

    /// `A` in `lo..=hi` goes to `t`, else on (the next instruction).
    fn in_range(&mut self, (lo, hi): (u32, u32), t: Label) {
        let check_hi = self.label();
        let next = self.label();
        self.jump(JGE_K, lo, check_hi, next);
        self.bind(check_hi);
        self.jump(JGT_K, hi, next, t);
        self.bind(next);
    }

    /// The program; `None` if a jump would not fit in 8 bits or a label is
    /// unbound.
    fn finish(self) -> Option<Vec<libc::sock_filter>> {
        let at = |l: Label| self.labels.get(l.0).copied().flatten();
        self.ops
            .iter()
            .enumerate()
            .map(|(i, op)| {
                Some(match *op {
                    Op::Ld(k) => libc::sock_filter {
                        code: LD_W_ABS,
                        jt: 0,
                        jf: 0,
                        k,
                    },
                    Op::Ret(k) => libc::sock_filter {
                        code: RET_K,
                        jt: 0,
                        jf: 0,
                        k,
                    },
                    Op::Jump(code, k, t, f) => libc::sock_filter {
                        code,
                        jt: u8::try_from(at(t)?.checked_sub(i + 1)?).ok()?,
                        jf: u8::try_from(at(f)?.checked_sub(i + 1)?).ok()?,
                        k,
                    },
                })
            })
            .collect()
    }
}

fn errno(e: i32) -> u32 {
    RET_ERRNO | (u32::try_from(e).unwrap_or(0) & 0xffff)
}

fn nr(n: libc::c_long) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The filter, or `None` on an architecture it does not know, where the
/// user-namespace path is refused.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub(crate) fn userns_network() -> Option<Vec<libc::sock_filter>> {
    let mut a = Asm::default();
    let [deny, allow, enosys] = [a.label(), a.label(), a.label()];
    let [newuser, socket, netlink, sockopt] = [a.label(), a.label(), a.label(), a.label()];
    let [ip, ipv6, tcp] = [a.label(), a.label(), a.label()];

    let arch_ok = a.label();
    a.ld(ARCH);
    a.jump(JEQ_K, AUDIT_ARCH, arch_ok, deny);
    a.bind(arch_ok);
    a.ld(NR);
    #[cfg(target_arch = "x86_64")]
    {
        let native = a.label();
        a.jump(JGE_K, X32_SYSCALL_BIT, deny, native);
        a.bind(native);
    }
    let on = |a: &mut Asm, n: libc::c_long, to: Label| {
        let next = a.label();
        a.jump(JEQ_K, nr(n), to, next);
        a.bind(next);
    };
    for n in [
        libc::SYS_setns,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        on(&mut a, n, deny);
    }
    on(&mut a, libc::SYS_clone3, enosys);
    // The flags are the first argument of both, on both architectures.
    on(&mut a, libc::SYS_unshare, newuser);
    on(&mut a, libc::SYS_clone, newuser);
    on(&mut a, libc::SYS_socket, socket);
    on(&mut a, libc::SYS_setsockopt, sockopt);
    on(&mut a, libc::SYS_getsockopt, sockopt);
    a.ret(RET_ALLOW);

    a.bind(newuser);
    a.ld(arg(0));
    a.jump(
        JSET_K,
        u32::try_from(libc::CLONE_NEWUSER).unwrap_or(u32::MAX),
        deny,
        allow,
    );

    a.bind(socket);
    a.ld(arg(0));
    let not_key = a.label();
    a.jump(JEQ_K, AF_KEY, deny, not_key);
    a.bind(not_key);
    a.jump(JEQ_K, AF_NETLINK, netlink, allow);

    a.bind(netlink);
    a.ld(arg(2));
    for p in NETLINK_ALLOWED {
        let next = a.label();
        a.jump(JEQ_K, p, allow, next);
        a.bind(next);
    }
    a.ret(errno(libc::EPROTONOSUPPORT));

    // `setsockopt` and `getsockopt`: level, then option name.
    a.bind(sockopt);
    a.ld(arg(1));
    for (level, to) in [(SOL_IP, ip), (SOL_IPV6, ipv6), (SOL_TCP, tcp)] {
        let next = a.label();
        a.jump(JEQ_K, level, to, next);
        a.bind(next);
    }
    a.ret(RET_ALLOW);
    a.bind(ip);
    a.ld(arg(2));
    for r in SOL_IP_REFUSED {
        a.in_range(r, deny);
    }
    a.ret(RET_ALLOW);
    a.bind(ipv6);
    a.ld(arg(2));
    for r in SOL_IPV6_REFUSED {
        a.in_range(r, deny);
    }
    a.ret(RET_ALLOW);
    a.bind(tcp);
    a.ld(arg(2));
    a.jump(JEQ_K, TCP_REPAIR, deny, allow);

    a.bind(deny);
    a.ret(errno(libc::EPERM));
    a.bind(allow);
    a.ret(RET_ALLOW);
    a.bind(enosys);
    a.ret(errno(libc::ENOSYS));
    a.finish()
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(crate) fn userns_network() -> Option<Vec<libc::sock_filter>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_errno() -> i32 {
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

    /// The call `f`, whose result is `-1` with `errno` when refused.
    fn refused_with(r: libc::c_long, e: i32) -> bool {
        r == -1 && last_errno() == e
    }

    /// Installs the filter in a forked child (with `no_new_privs`, which an
    /// unprivileged process needs) and tries each refused and allowed call.
    /// Option checks use descriptor -1: without the filter they fail with
    /// `EBADF`, so `EPERM` can only come from it.
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
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::syscall(
                        libc::SYS_seccomp,
                        libc::SECCOMP_SET_MODE_FILTER,
                        0u32,
                        &raw const prog,
                    ) != 0
                {
                    libc::_exit(200);
                }
                let raw = libc::SOCK_RAW | libc::SOCK_CLOEXEC;
                let sock = |d, t, p| libc::c_long::from(libc::socket(d, t, p));
                let opt = |level: u32, name: u32| {
                    let mut v = [0u8; 4];
                    libc::c_long::from(libc::setsockopt(
                        -1,
                        level.cast_signed(),
                        name.cast_signed(),
                        v.as_mut_ptr().cast(),
                        4,
                    ))
                };
                let getopt = |level: u32, name: u32| {
                    let mut v = [0u8; 4];
                    let mut len: libc::socklen_t = 4;
                    libc::c_long::from(libc::getsockopt(
                        -1,
                        level.cast_signed(),
                        name.cast_signed(),
                        v.as_mut_ptr().cast(),
                        &raw mut len,
                    ))
                };
                let mut n = 0;
                let mut fail = |ok: bool| {
                    n += 1;
                    if !ok {
                        libc::_exit(n);
                    }
                };
                fail(refused_with(
                    sock(libc::AF_NETLINK, raw, 12),
                    libc::EPROTONOSUPPORT,
                )); // 1 NETFILTER
                fail(refused_with(
                    sock(libc::AF_NETLINK, raw, 6),
                    libc::EPROTONOSUPPORT,
                )); // 2 XFRM
                fail(refused_with(
                    sock(libc::AF_NETLINK, raw, 16),
                    libc::EPROTONOSUPPORT,
                )); // 3 GENERIC
                fail(sock(libc::AF_NETLINK, raw, 0) >= 0); // 4 ROUTE
                fail(sock(libc::AF_NETLINK, raw, 9) >= 0); // 5 AUDIT
                fail(refused_with(sock(libc::AF_KEY, raw, 2), libc::EPERM)); // 6
                fail(sock(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) >= 0); // 7
                fail(refused_with(
                    libc::c_long::from(libc::unshare(libc::CLONE_NEWUSER)),
                    libc::EPERM,
                )); // 8
                fail(refused_with(
                    libc::c_long::from(libc::setns(-1, 0)),
                    libc::EPERM,
                )); // 9
                fail(refused_with(
                    libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0usize),
                    libc::ENOSYS,
                )); // 10
                fail(refused_with(
                    libc::syscall(libc::SYS_io_uring_setup, 1u32, std::ptr::null::<u8>()),
                    libc::EPERM,
                )); // 11
                fail(libc::unshare(libc::CLONE_FILES) == 0); // 12
                // 13..: each refused option range, both ends, both calls.
                for (level, ranges) in [
                    (SOL_IP, &SOL_IP_REFUSED[..]),
                    (SOL_IPV6, &SOL_IPV6_REFUSED[..]),
                    (SOL_TCP, &[(TCP_REPAIR, TCP_REPAIR)][..]),
                ] {
                    for &(lo, hi) in ranges {
                        for name in [lo, hi] {
                            fail(refused_with(opt(level, name), libc::EPERM));
                            fail(refused_with(getopt(level, name), libc::EPERM));
                        }
                        // Just outside the range: the kernel's EBADF.
                        fail(refused_with(opt(level, hi + 1), libc::EBADF));
                        fail(refused_with(opt(level, lo - 1), libc::EBADF));
                    }
                }
                // IPV6_RECVTCLASS and IPV6_TCLASS sit between the
                // ip6_tables options and stay open.
                fail(refused_with(opt(SOL_IPV6, 66), libc::EBADF));
                fail(refused_with(opt(SOL_IPV6, 67), libc::EBADF));
                fail(refused_with(getopt(SOL_IPV6, 66), libc::EBADF));
                fail(refused_with(getopt(SOL_IPV6, 67), libc::EBADF));
                // Other levels are untouched.
                fail(refused_with(opt(1, 64), libc::EBADF)); // SOL_SOCKET
                libc::_exit(0);
            }
        }
        let mut status = 0;
        // SAFETY: reaping the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status), "{status:#x}");
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "the number of the failing check"
        );
    }
}
