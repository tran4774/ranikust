use super::super::asm::run;
use super::super::{AUDIT_ARCH, syscalls};
use super::*;

fn cx(kernel: (u64, u64)) -> Context {
    Context {
        arch: native_arch().unwrap(),
        kernel,
        faked_success: false,
    }
}

fn nr(n: libc::c_long) -> u32 {
    u32::try_from(n).unwrap()
}

fn errno(e: i32) -> u32 {
    Action::Errno(e.unsigned_abs()).ret()
}

const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;

/// The action `p` gives call `nr` with `args`, from architecture `arch`.
fn verdict_on(p: &Program, arch: u32, nr: u32, args: [u64; 6]) -> u32 {
    let mut d = [0u32; 16];
    d[0] = nr;
    d[1] = arch;
    for (i, &a) in args.iter().enumerate() {
        let (hi, lo) = halves(a);
        let (h, l) = if cfg!(target_endian = "little") {
            (5 + 2 * i, 4 + 2 * i)
        } else {
            (4 + 2 * i, 5 + 2 * i)
        };
        d[h] = hi;
        d[l] = lo;
    }
    run(&p.insns, &d)
}

fn verdict(p: &Program, n: libc::c_long, args: [u64; 6]) -> u32 {
    verdict_on(p, AUDIT_ARCH, nr(n), args)
}

fn a0(v: u64) -> [u64; 6] {
    [v, 0, 0, 0, 0, 0]
}

fn compiled(json: &str) -> Program {
    compile(json.as_bytes(), &cx((6, 8))).unwrap().program
}

fn refusal(json: &str) -> String {
    match compile(json.as_bytes(), &cx((6, 8))) {
        Err(Error::SeccompProfile(m)) => m,
        Err(e) => panic!("{json}: {e}"),
        Ok(_) => panic!("{json}: accepted"),
    }
}

struct Compare {
    op: &'static str,
    value: u64,
    two: u64,
    hits: &'static [u64],
    misses: &'static [u64],
}

const V: u64 = 0x1_0000_0005;

/// Each operator around a value whose words both matter, and values that
/// differ from it in one word only.
const COMPARES: [Compare; 7] = [
    Compare {
        op: "SCMP_CMP_EQ",
        value: V,
        two: 0,
        hits: &[V],
        misses: &[5, 0x1_0000_0004, 0x2_0000_0005, 0, u64::MAX],
    },
    Compare {
        op: "SCMP_CMP_NE",
        value: V,
        two: 0,
        hits: &[5, 0x1_0000_0004, 0x2_0000_0005, 0, u64::MAX],
        misses: &[V],
    },
    Compare {
        op: "SCMP_CMP_GT",
        value: V,
        two: 0,
        hits: &[V + 1, 0x2_0000_0000, 0x1_ffff_ffff, u64::MAX],
        misses: &[V, 5, 6, 0x1_0000_0004, 0xffff_ffff, 0],
    },
    Compare {
        op: "SCMP_CMP_GE",
        value: V,
        two: 0,
        hits: &[V, V + 1, 0x2_0000_0000, u64::MAX],
        misses: &[V - 1, 6, 0xffff_ffff, 0],
    },
    Compare {
        op: "SCMP_CMP_LT",
        value: V,
        two: 0,
        hits: &[V - 1, 6, 0xffff_ffff, 0x1_0000_0000, 0],
        misses: &[V, V + 1, 0x2_0000_0000, 0x2_0000_0004, u64::MAX],
    },
    Compare {
        op: "SCMP_CMP_LE",
        value: V,
        two: 0,
        hits: &[V, V - 1, 0xffff_ffff, 0],
        misses: &[V + 1, 0x2_0000_0000, 0x2_0000_0004, u64::MAX],
    },
    Compare {
        op: "SCMP_CMP_MASKED_EQ",
        value: 0xff00_0000_0000_00ff,
        two: 0x1200_0000_0000_0034,
        hits: &[0x1200_0000_0000_0034, 0x12ab_cdef_0123_4534],
        misses: &[0x1300_0000_0000_0034, 0x1200_0000_0000_0035, 0x34, 0],
    },
];

fn compare_profile(c: &Compare, index: u32) -> String {
    format!(
        r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["getppid"],
        "action":"SCMP_ACT_ERRNO","errnoRet":77,"args":[{{"index":{index},
        "value":{},"valueTwo":{},"op":"{}"}}]}}]}}"#,
        c.value, c.two, c.op
    )
}

fn at(index: u32, v: u64) -> [u64; 6] {
    let mut args = [0; 6];
    args[usize::try_from(index).unwrap()] = v;
    args
}

/// The 64-bit comparisons, around both words.
#[test]
fn arguments_compare_as_64_bit_values() {
    for c in &COMPARES {
        for index in [0, 1, 5] {
            let p = compiled(&compare_profile(c, index));
            let getppid_nr = libc::SYS_getppid;
            for &v in c.hits {
                assert_eq!(
                    verdict(&p, getppid_nr, at(index, v)),
                    errno(77),
                    "{} {v:#x}",
                    c.op
                );
            }
            for &v in c.misses {
                assert_eq!(
                    verdict(&p, getppid_nr, at(index, v)),
                    ALLOW,
                    "{} {v:#x}",
                    c.op
                );
            }
        }
    }
}

/// One rule per action on `getppid`, chosen by its first argument.
const ACTIONS: &str = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
    {"names":["getppid"],"action":"SCMP_ACT_ALLOW","args":[{"index":0,"value":7,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":11,"args":[{"index":0,"value":1,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":12,"args":[{"index":0,"value":2,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":13,"args":[{"index":0,"value":7,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_LOG","args":[{"index":0,"value":3,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_KILL_PROCESS","args":[{"index":0,"value":4,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_TRAP","args":[{"index":0,"value":5,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_KILL","args":[{"index":0,"value":6,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":14,"args":[{"index":0,"value":1,"op":"SCMP_CMP_EQ"}]},
    {"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":511,"args":[{"index":0,"value":8,"op":"SCMP_CMP_EQ"}]},
    {"name":"getppid","action":"SCMP_ACT_KILL_THREAD","args":[{"index":0,"value":9,"op":"SCMP_CMP_EQ"}]}
]}"#;

/// Each rule's action, the most restrictive winning, and the profile's
/// order between two of a kind.
#[test]
fn each_rule_has_its_action() {
    let p = compiled(ACTIONS);
    let g = libc::SYS_getppid;
    for (arg, want) in [
        (0, ALLOW),
        (1, errno(11)),
        (2, errno(12)),
        (7, errno(13)),
        (3, libc::SECCOMP_RET_LOG),
        (4, libc::SECCOMP_RET_KILL_PROCESS),
        (5, libc::SECCOMP_RET_TRAP),
        (6, libc::SECCOMP_RET_KILL_THREAD),
        (8, errno(511)),
        (9, libc::SECCOMP_RET_KILL_THREAD),
    ] {
        assert_eq!(verdict(&p, g, a0(arg)), want, "{arg}");
    }
}

/// The calls the forked checks need, left out of [`long_profile`].
fn spared() -> [u32; 3] {
    [libc::SYS_exit, libc::SYS_exit_group, libc::SYS_getppid].map(nr)
}

/// [`long_profile`]'s errno for call `n`: its place among the numbers, from
/// 1 to 451, distinct and below the chain's and 512 (ADR-0109); a longer
/// table fails here rather than give two calls one errno.
fn long_errno(n: u32) -> u32 {
    let numbers: std::collections::BTreeSet<u32> = syscalls::all()
        .map(|(_, n)| n)
        .filter(|n| !spared().contains(n))
        .collect();
    let place = u32::try_from(numbers.iter().position(|&m| m == n).unwrap()).unwrap();
    assert!(
        place < 451,
        "{} numbers: too many for long_profile",
        numbers.len()
    );
    1 + place
}

/// [`long_profile`]'s errno for `getppid` with `arg0` from 0 to 59.
fn chain_errno(arg0: u32) -> u32 {
    452 + arg0
}

/// Distinct errnos for nearly every call ([`long_errno`]) and a chain of
/// 60 rules on `getppid` ([`chain_errno`]): the search tree and the chain
/// jump further than 8 bits allow.
fn long_profile() -> Program {
    let mut rules: Vec<String> = syscalls::all()
        .filter(|(_, n)| !spared().contains(n))
        .map(|(name, n)| {
            format!(
                r#"{{"names":["{name}"],"action":"SCMP_ACT_ERRNO","errnoRet":{}}}"#,
                long_errno(n)
            )
        })
        .collect();
    for i in 0..60 {
        rules.push(format!(
            r#"{{"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":{},
            "args":[{{"index":0,"value":{i},"op":"SCMP_CMP_EQ"}}]}}"#,
            chain_errno(i)
        ));
    }
    compiled(&format!(
        r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{}]}}"#,
        rules.join(",")
    ))
}

#[test]
fn long_jumps_reach_their_targets() {
    let p = long_profile();
    assert!(p.insns.len() > 600, "{}", p.insns.len());
    let far = p
        .insns
        .iter()
        .any(|i| i.code == super::super::asm::JA && i.k > 255);
    assert!(far, "a jump through ja");
    for (_, n) in syscalls::all().filter(|(_, n)| !spared().contains(n)) {
        let want = errno(i32::try_from(long_errno(n)).unwrap());
        assert_eq!(verdict_on(&p, AUDIT_ARCH, n, [0; 6]), want, "{n}");
    }
    for i in 0..60u32 {
        let want = errno(i32::try_from(chain_errno(i)).unwrap());
        assert_eq!(verdict(&p, libc::SYS_getppid, a0(u64::from(i))), want);
    }
    assert_eq!(verdict(&p, libc::SYS_getppid, a0(60)), ALLOW);
}

/// Docker's default profile, checked call by call, as compiled for the
/// `RUN` (Docker's capabilities, kernel 6.8).
#[test]
fn docker_default_allows_and_refuses_what_moby_says() {
    let p = compile(DOCKER_DEFAULT, &cx((6, 8))).unwrap().program;
    let eperm = errno(libc::EPERM);
    let enosys = errno(libc::ENOSYS);
    let newuser = u64::try_from(libc::CLONE_NEWUSER).unwrap();
    let newns = u64::try_from(libc::CLONE_NEWNS).unwrap();
    let sigchld = u64::try_from(libc::SIGCHLD).unwrap();
    let cases: &[(libc::c_long, [u64; 6], u32)] = &[
        // What the RUN's init and command call after the install.
        (libc::SYS_prctl, a0(0), ALLOW),
        (libc::SYS_capget, a0(0), ALLOW),
        (libc::SYS_capset, a0(0), ALLOW),
        (libc::SYS_pread64, a0(0), ALLOW),
        (libc::SYS_setsid, a0(0), ALLOW),
        (libc::SYS_pipe2, a0(0), ALLOW),
        (libc::SYS_clone, a0(sigchld), ALLOW),
        (libc::SYS_close, a0(0), ALLOW),
        (libc::SYS_read, a0(0), ALLOW),
        (libc::SYS_write, a0(0), ALLOW),
        (libc::SYS_kill, a0(0), ALLOW),
        (libc::SYS_wait4, a0(0), ALLOW),
        (libc::SYS_ptrace, a0(0), ALLOW),
        (libc::SYS_close_range, a0(0), ALLOW),
        (libc::SYS_openat, a0(0), ALLOW),
        (libc::SYS_openat2, a0(0), ALLOW),
        (libc::SYS_fchdir, a0(0), ALLOW),
        (libc::SYS_getcwd, a0(0), ALLOW),
        (libc::SYS_setgroups, a0(0), ALLOW),
        (libc::SYS_setresgid, a0(0), ALLOW),
        (libc::SYS_setresuid, a0(0), ALLOW),
        (libc::SYS_umask, a0(0), ALLOW),
        (libc::SYS_rt_sigaction, a0(0), ALLOW),
        (libc::SYS_rt_sigprocmask, a0(0), ALLOW),
        (libc::SYS_fcntl, a0(0), ALLOW),
        (libc::SYS_getdents64, a0(0), ALLOW),
        (libc::SYS_fstatfs, a0(0), ALLOW),
        (libc::SYS_prlimit64, a0(0), ALLOW),
        (libc::SYS_execve, a0(0), ALLOW),
        (libc::SYS_execveat, a0(0), ALLOW),
        (libc::SYS_exit_group, a0(0), ALLOW),
        // `CAP_SYS_CHROOT` is one of Docker's capabilities.
        (libc::SYS_chroot, a0(0), ALLOW),
        (libc::SYS_process_vm_readv, a0(0), ALLOW),
        // Namespaces: no `CAP_SYS_ADMIN`.
        (libc::SYS_clone, a0(newuser | sigchld), eperm),
        (libc::SYS_clone, a0(newns), eperm),
        (libc::SYS_clone, a0(sigchld | (1 << 32)), ALLOW),
        (libc::SYS_clone3, a0(0), enosys),
        (libc::SYS_unshare, a0(0), eperm),
        (libc::SYS_setns, a0(0), eperm),
        (libc::SYS_mount, a0(0), eperm),
        (libc::SYS_umount2, a0(0), eperm),
        (libc::SYS_pivot_root, a0(0), eperm),
        // Sockets: not AF_ALG (38) nor AF_VSOCK (40).
        (libc::SYS_socket, a0(0), ALLOW),
        (libc::SYS_socket, a0(2), ALLOW),
        (libc::SYS_socket, a0(16), ALLOW),
        (libc::SYS_socket, a0(37), ALLOW),
        (libc::SYS_socket, a0(38), eperm),
        (libc::SYS_socket, a0(39), ALLOW),
        (libc::SYS_socket, a0(40), eperm),
        (libc::SYS_socket, a0(41), ALLOW),
        (libc::SYS_socket, a0(45), ALLOW),
        (libc::SYS_socket, a0(46), eperm),
        (libc::SYS_socket, a0(0x1_0000_0002), eperm),
        // personality: the five values moby lists.
        (libc::SYS_personality, a0(0), ALLOW),
        (libc::SYS_personality, a0(8), ALLOW),
        (libc::SYS_personality, a0(0x2_0000), ALLOW),
        (libc::SYS_personality, a0(0x2_0008), ALLOW),
        (libc::SYS_personality, a0(0xffff_ffff), ALLOW),
        (libc::SYS_personality, a0(0x4_0000), eperm),
        (libc::SYS_personality, a0(1), eperm),
        // Not listed, or behind a capability a RUN lacks.
        (libc::SYS_keyctl, a0(0), eperm),
        (libc::SYS_add_key, a0(0), eperm),
        (libc::SYS_request_key, a0(0), eperm),
        (libc::SYS_io_uring_setup, a0(0), eperm),
        (libc::SYS_userfaultfd, a0(0), eperm),
        (libc::SYS_bpf, a0(0), eperm),
        (libc::SYS_perf_event_open, a0(0), eperm),
        (libc::SYS_init_module, a0(0), eperm),
        (libc::SYS_finit_module, a0(0), eperm),
        (libc::SYS_reboot, a0(0), eperm),
        (libc::SYS_open_by_handle_at, a0(0), eperm),
        (libc::SYS_kexec_load, a0(0), eperm),
        (libc::SYS_settimeofday, a0(0), eperm),
        (libc::SYS_syslog, a0(0), eperm),
        (libc::SYS_fanotify_init, a0(0), eperm),
    ];
    for &(call, args, want) in cases {
        assert_eq!(verdict(&p, call, args), want, "{call} {args:x?}");
    }
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(verdict(&p, libc::SYS_arch_prctl, a0(0)), ALLOW);
        assert_eq!(verdict(&p, libc::SYS_modify_ldt, a0(0)), ALLOW);
        assert_eq!(verdict(&p, libc::SYS_iopl, a0(0)), eperm);
        // x32 calls and i386 ones.
        let x32 = super::super::X32_SYSCALL_BIT;
        assert_eq!(
            verdict_on(&p, AUDIT_ARCH, x32 | nr(libc::SYS_getpid), [0; 6]),
            eperm
        );
        assert_eq!(verdict_on(&p, 0x4000_0003, 20, [0; 6]), eperm);
    }
    #[cfg(target_arch = "aarch64")]
    assert_eq!(verdict_on(&p, 0x4000_0028, 20, [0; 6]), eperm, "arm");
    // Numbers `libc` does not name: ENOSYS, for a fallback.
    for n in [1000, 0x3fff_ffff, 0x7fff_0000] {
        if syscalls::all().all(|(_, k)| k != n) && n < 0x4000_0000 {
            assert_eq!(verdict_on(&p, AUDIT_ARCH, n, [0; 6]), enosys, "{n}");
        }
    }
    // ptrace and process_vm_* need 4.8.
    let old = compile(DOCKER_DEFAULT, &cx((4, 7))).unwrap().program;
    assert_eq!(verdict(&old, libc::SYS_ptrace, a0(0)), eperm);
    assert_eq!(verdict(&old, libc::SYS_process_vm_writev, a0(0)), eperm);
    let new = compile(DOCKER_DEFAULT, &cx((5, 0))).unwrap().program;
    assert_eq!(verdict(&new, libc::SYS_ptrace, a0(0)), ALLOW);
}

/// Whether the argument rules of `json` have the form threat model section 6
/// advises: its default action refuses, and each rule with conditions
/// allows, with `EQ`, `LT`, `LE` or `MASKED_EQ` ones.
fn restricts_arguments_by_allowing(json: &[u8]) -> bool {
    let p: Json = serde_json::from_slice(json).unwrap();
    let default = p.default_action.as_deref().unwrap();
    let default = action(default, p.default_errno_ret, "", "", false).unwrap();
    !default.runs()
        && p.syscalls.iter().flatten().all(|r| {
            let (r, _) = rule(r, "", false).unwrap();
            r.conds.is_empty()
                || (r.action == Action::Allow
                    && r.conds
                        .iter()
                        .all(|c| matches!(c.op, Cmp::Eq | Cmp::Lt | Cmp::Le | Cmp::MaskedEq)))
        })
}

/// Docker's default profile gives a `RUN` neither `clone3` nor the
/// `io_uring_*` calls, as threat model section 6 advises: each is refused,
/// with an errno, a trap or a kill.
#[test]
fn docker_default_gives_a_run_neither_clone3_nor_io_uring() {
    let p = compile(DOCKER_DEFAULT, &cx((6, 8))).unwrap().program;
    for call in [
        libc::SYS_clone3,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        let v = verdict(&p, call, a0(0));
        assert!(
            precedence(v) < precedence(libc::SECCOMP_RET_USER_NOTIF),
            "call {call}: {v:#x}"
        );
    }
}

/// Docker's default profile has the argument-rule form threat model section
/// 6 advises. The controls show the check tells apart the profiles that do
/// otherwise.
#[test]
fn docker_default_restricts_arguments_by_allowing_values() {
    assert!(restricts_arguments_by_allowing(DOCKER_DEFAULT));
    let with = |default: &str, act: &str, op: &str| {
        format!(
            r#"{{"defaultAction":"{default}","syscalls":[
            {{"names":["getppid"],"action":"{act}","args":[{{"index":0,"value":1,"op":"{op}"}}]}},
            {{"names":["getpid"],"action":"SCMP_ACT_ERRNO","errnoRet":38}}]}}"#
        )
    };
    let (allow, errno) = ("SCMP_ACT_ALLOW", "SCMP_ACT_ERRNO");
    for op in ["EQ", "LT", "LE", "MASKED_EQ"] {
        let json = with(errno, allow, &format!("SCMP_CMP_{op}"));
        assert!(restricts_arguments_by_allowing(json.as_bytes()), "{json}");
    }
    for json in [
        with(allow, allow, "SCMP_CMP_EQ"),
        with("SCMP_ACT_LOG", allow, "SCMP_CMP_EQ"),
        with(errno, errno, "SCMP_CMP_EQ"),
        with(errno, "SCMP_ACT_KILL_PROCESS", "SCMP_CMP_EQ"),
        with(errno, allow, "SCMP_CMP_NE"),
        with(errno, allow, "SCMP_CMP_GT"),
        with(errno, allow, "SCMP_CMP_GE"),
    ] {
        assert!(!restricts_arguments_by_allowing(json.as_bytes()), "{json}");
    }
}

#[test]
fn includes_and_excludes_follow_moby() {
    let rule = |filter: &str| {
        compiled(&format!(
            r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["getppid"],
            "action":"SCMP_ACT_ERRNO","errnoRet":5,{filter}}}]}}"#
        ))
    };
    let native = native_arch().unwrap();
    for (filter, kept) in [
        (r#""includes":{}"#.to_owned(), true),
        (
            r#""includes":{"caps":["CAP_CHOWN","CAP_SYS_CHROOT"]}"#.to_owned(),
            true,
        ),
        (
            r#""includes":{"caps":["CAP_CHOWN","CAP_SYS_ADMIN"]}"#.to_owned(),
            false,
        ),
        (r#""excludes":{"caps":["CAP_SYS_ADMIN"]}"#.to_owned(), true),
        (
            r#""excludes":{"caps":["CAP_SYS_ADMIN","CAP_KILL"]}"#.to_owned(),
            false,
        ),
        (
            format!(r#""includes":{{"arches":["s390x","{native}"]}}"#),
            true,
        ),
        (r#""includes":{"arches":["s390x"]}"#.to_owned(), false),
        (format!(r#""excludes":{{"arches":["{native}"]}}"#), false),
        (r#""includes":{"minKernel":"6.8"}"#.to_owned(), true),
        (r#""includes":{"minKernel":"6.9"}"#.to_owned(), false),
        (r#""includes":{"minKernel":"7.0"}"#.to_owned(), false),
        (r#""excludes":{"minKernel":"5.10"}"#.to_owned(), false),
        (r#""excludes":{"minKernel":"6.9"}"#.to_owned(), true),
        (r#""excludes":{"minKernel":""}"#.to_owned(), false),
        (r#""includes":{"minKernel":""}"#.to_owned(), true),
    ] {
        let want = if kept { errno(5) } else { ALLOW };
        assert_eq!(
            verdict(&rule(&filter), libc::SYS_getppid, a0(0)),
            want,
            "{filter}"
        );
    }
}

#[test]
fn defaults_unknown_calls_and_flags() {
    let p = compiled(r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":13,"syscalls":[]}"#);
    assert_eq!(verdict(&p, libc::SYS_getppid, a0(0)), errno(13));
    assert_eq!(
        verdict_on(&p, AUDIT_ARCH, 1000, [0; 6]),
        errno(libc::ENOSYS)
    );
    let p = compiled(r#"{"defaultAction":"SCMP_ACT_KILL_PROCESS"}"#);
    assert_eq!(
        verdict(&p, libc::SYS_getppid, a0(0)),
        libc::SECCOMP_RET_KILL_PROCESS
    );
    assert_eq!(
        verdict_on(&p, AUDIT_ARCH, 1000, [0; 6]),
        errno(libc::ENOSYS)
    );
    let p = compiled(r#"{"defaultAction":"SCMP_ACT_ERRNO"}"#);
    assert_eq!(verdict(&p, libc::SYS_getppid, a0(0)), errno(libc::EPERM));
    let p = compiled(r#"{"defaultAction":"SCMP_ACT_LOG"}"#);
    assert_eq!(
        verdict_on(&p, AUDIT_ARCH, 1000, [0; 6]),
        libc::SECCOMP_RET_LOG
    );
    // A denylist, and a name this architecture lacks, skipped under ALLOW.
    let p = compiled(
        r#"{"defaultAction":"SCMP_ACT_ALLOW","flags":["SECCOMP_FILTER_FLAG_LOG",
        "SECCOMP_FILTER_FLAG_SPEC_ALLOW","SECCOMP_FILTER_FLAG_TSYNC"],"syscalls":[
        {"names":["getppid","no_such_call"],"action":"SCMP_ACT_ALLOW"},
        {"names":["keyctl"],"action":"SCMP_ACT_ERRNO","errnoRet":1}]}"#,
    );
    assert_eq!(verdict(&p, libc::SYS_keyctl, a0(0)), errno(libc::EPERM));
    assert_eq!(verdict(&p, libc::SYS_getppid, a0(0)), ALLOW);
    assert_eq!(verdict_on(&p, AUDIT_ARCH, 1000, [0; 6]), ALLOW);
    assert_eq!(verdict_on(&p, 0x4000_0003, 20, [0; 6]), errno(libc::EPERM));
    assert_eq!(
        p.flags,
        libc::SECCOMP_FILTER_FLAG_LOG
            | libc::SECCOMP_FILTER_FLAG_SPEC_ALLOW
            | libc::SECCOMP_FILTER_FLAG_TSYNC
    );
    // A stricter rule on a name another architecture has, kept out by
    // `includes`, is not refused.
    compiled(
        r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["s390_runtime_instr"],
        "action":"SCMP_ACT_KILL","includes":{"arches":["s390x"]}}]}"#,
    );
}

/// A profile refuses the keyring calls only when none of `keyctl`,
/// `add_key` and `request_key` can run, whatever its arguments (ADR-0109):
/// a refused join does not fail a `RUN` under it.
#[test]
fn keyrings_are_refused_only_when_no_keyring_call_can_run() {
    let refuses = |json: &str| {
        compile(json.as_bytes(), &cx((6, 8)))
            .unwrap()
            .refuses_keyrings()
    };
    let rule = |names: &str, action: &str, args: &str| {
        format!(r#"{{"names":[{names}],"action":"{action}","args":[{args}]}}"#)
    };
    let all = r#""keyctl","add_key","request_key""#;
    let two = r#""add_key","request_key""#;
    let on_join = r#"{"index":0,"value":1,"op":"SCMP_CMP_EQ"}"#;
    let profile = |default: &str, rules: &[String]| {
        format!(
            r#"{{"defaultAction":"{default}","syscalls":[{}]}}"#,
            rules.join(",")
        )
    };
    let errno = "SCMP_ACT_ERRNO";
    let allow = "SCMP_ACT_ALLOW";
    assert!(SeccompProfile::docker_default().unwrap().refuses_keyrings());
    for (json, want) in [
        (profile(allow, &[]), false),
        (profile("SCMP_ACT_LOG", &[]), false),
        (profile(errno, &[]), true),
        (profile("SCMP_ACT_KILL_PROCESS", &[]), true),
        (profile(allow, &[rule(all, errno, "")]), true),
        (profile(allow, &[rule(all, "SCMP_ACT_TRAP", "")]), true),
        (profile(allow, &[rule(two, errno, "")]), false),
        (
            profile(
                allow,
                &[rule(two, errno, ""), rule(r#""keyctl""#, errno, on_join)],
            ),
            false,
        ),
        (profile(errno, &[rule(r#""keyctl""#, allow, "")]), false),
        (
            profile(errno, &[rule(r#""keyctl""#, allow, on_join)]),
            false,
        ),
        (
            profile(errno, &[rule(r#""add_key""#, "SCMP_ACT_LOG", "")]),
            false,
        ),
        // The strictest matching rule wins: an unconditional refusal
        // covers a conditional allow.
        (
            profile(
                errno,
                &[
                    rule(r#""keyctl""#, allow, on_join),
                    rule(r#""keyctl""#, errno, ""),
                ],
            ),
            true,
        ),
        // A rule `includes` keeps out does not count.
        (
            profile(
                errno,
                &[
                    r#"{"names":["keyctl"],"action":"SCMP_ACT_ALLOW","includes":{"arches":["s390x"]}}"#
                        .to_owned(),
                ],
            ),
            true,
        ),
    ] {
        assert_eq!(refuses(&json), want, "{json}");
    }
}

#[test]
fn malformed_or_unsupported_profiles_are_refused() {
    let rule = |r: &str| format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{r}]}}"#);
    for (json, says) in [
        ("not json".to_owned(), "expected"),
        ("{}".to_owned(), "defaultAction is missing"),
        (r#"{"defaultAction":"SCMP_ACT_ALLOW","defaultaction":"x"}"#.to_owned(), "unknown field"),
        (r#"{"defaultAction":"SCMP_ACT_TRACE"}"#.to_owned(), "not supported"),
        (r#"{"defaultAction":"SCMP_ACT_NOTIFY"}"#.to_owned(), "not supported"),
        (r#"{"defaultAction":""}"#.to_owned(), "unknown action"),
        (r#"{"defaultAction":"SCMP_ACT_ALLOW","defaultErrnoRet":1}"#.to_owned(), "errno"),
        (r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":4096}"#.to_owned(), "above"),
        (r#"{"defaultAction":"SCMP_ACT_ALLOW","listenerPath":"/run/x"}"#.to_owned(), "listener"),
        (r#"{"defaultAction":"SCMP_ACT_ALLOW","listenerMetadata":"x"}"#.to_owned(), "listener"),
        (
            r#"{"defaultAction":"SCMP_ACT_ALLOW","flags":["SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV"]}"#.to_owned(),
            "not supported",
        ),
        (r#"{"defaultAction":"SCMP_ACT_ALLOW","flags":["X"]}"#.to_owned(), "not supported"),
        (
            r#"{"defaultAction":"SCMP_ACT_ALLOW","architectures":["SCMP_ARCH_X86_64"],
            "archMap":[{"architecture":"SCMP_ARCH_X86_64","subArchitectures":null}]}"#
                .to_owned(),
            "both architectures and archMap",
        ),
        (r#"{"defaultAction":"SCMP_ACT_ALLOW","architectures":["x86_64"]}"#.to_owned(), "unknown architecture"),
        (rule(r#"{"name":"getppid","names":["getppid"],"action":"SCMP_ACT_ALLOW"}"#), "both name and names"),
        (rule(r#"{"names":[],"action":"SCMP_ACT_ALLOW"}"#), "names no system call"),
        (rule(r#"{"action":"SCMP_ACT_ALLOW"}"#), "names no system call"),
        (rule(r#"{"names":["getppid"],"action":"SCMP_ACT_NOTIFY"}"#), "not supported"),
        (rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW","errnoRet":1}"#), "errno"),
        // Errno 0 would fake a success; above 4095 the low bits could be 0.
        (
            rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":0}"#),
            "syscalls[0]: errnoRet 0 would fake",
        ),
        (
            rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":65536}"#),
            "syscalls[0]: errnoRet 65536 is above 4095",
        ),
        (
            rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":-1}"#),
            "expected u64",
        ),
        (
            r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":0}"#.to_owned(),
            "defaultErrnoRet: 0 would fake",
        ),
        (
            r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":65536}"#.to_owned(),
            "defaultErrnoRet: 65536 is above 4095",
        ),
        (rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW","extra":1}"#), "unknown field"),
        (
            rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ERRNO","args":[{"index":6,"value":0,"op":"SCMP_CMP_EQ"}]}"#),
            "above 5",
        ),
        (
            rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ERRNO","args":[{"index":0,"value":0,"op":"EQ"}]}"#),
            "unknown operator",
        ),
        (
            rule(
                r#"{"names":["getppid"],"action":"SCMP_ACT_ERRNO","args":[{"index":0,"value":0,"op":"SCMP_CMP_EQ"},
                {"index":0,"value":1,"op":"SCMP_CMP_EQ"}]}"#,
            ),
            "two conditions on argument 0",
        ),
        (rule(r#"{"names":["no_such_call"],"action":"SCMP_ACT_ERRNO"}"#), "not a system call"),
        (rule(r#"{"names":["no_such_call"],"action":"SCMP_ACT_LOG"}"#), "not a system call"),
        (rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW","includes":{"minKernel":"4"}}"#), "minKernel"),
        (rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW","includes":{"minKernel":"4.8.1"}}"#), "minKernel"),
        (rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW","includes":{"minKernel":"0.0"}}"#), "minKernel"),
        (rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW","includes":{"minKernel":"256.1"}}"#), "minKernel"),
        // Checked even where `excludes` already leaves the rule out.
        (
            rule(r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW","excludes":{"caps":["CAP_KILL"],"minKernel":"x.y"}}"#),
            "minKernel",
        ),
    ] {
        let m = refusal(&json);
        assert!(m.contains(says), "{json}: {m}");
    }
}

/// Every errno a profile may not give, 0, from 512 up or above 4095, is
/// refused in one form: the field, the value, why, and what to give instead
/// (ADR-0109).
#[test]
fn errno_refusals_name_the_field_and_ask_for_1_to_511() {
    let refused = (512..=600).chain([0, 4095, 4096, 65536, 1 << 32, u64::MAX]);
    for e in refused {
        let cases = [
            (
                format!(
                    r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["getppid"],
                    "action":"SCMP_ACT_ERRNO","errnoRet":{e}}}]}}"#
                ),
                format!("syscalls[0]: errnoRet {e} "),
            ),
            (
                format!(r#"{{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":{e}}}"#),
                format!("defaultErrnoRet: {e} "),
            ),
        ];
        for (json, field) in cases {
            let m = refusal(&json);
            assert!(m.starts_with(&field), "{m}");
            assert!(m.ends_with("; give an errno from 1 to 511"), "{m}");
        }
    }
}

/// Every errno from 512 up, where the kernel keeps its own codes, is refused
/// as an errno of a rule or of the default, even in a rule another
/// architecture keeps or in the tests' faking profiles; 511 compiles
/// (ADR-0109).
#[test]
fn errnos_from_512_up_are_refused() {
    let errno_rule = |e: u32, extra: &str| {
        format!(
            r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["getppid"],
            "action":"SCMP_ACT_ERRNO","errnoRet":{e}{extra}}}]}}"#
        )
    };
    let default = |e: u32| format!(r#"{{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":{e}}}"#);
    let faking = Context {
        faked_success: true,
        ..cx((6, 8))
    };
    for e in (512..=600).chain([4095]) {
        let m = refusal(&errno_rule(e, ""));
        assert!(
            m.starts_with(&format!("syscalls[0]: errnoRet {e} is")),
            "{m}"
        );
        assert!(m.contains("from 512 up"), "{m}");
        let m = refusal(&errno_rule(e, r#","includes":{"arches":["s390x"]}"#));
        assert!(m.starts_with("syscalls[0]: errnoRet"), "{m}");
        assert!(m.contains("from 512 up"), "{m}");
        let m = refusal(&default(e));
        assert!(m.starts_with(&format!("defaultErrnoRet: {e} is")), "{m}");
        for json in [errno_rule(e, ""), default(e)] {
            match compile(json.as_bytes(), &faking) {
                Err(Error::SeccompProfile(m)) => assert!(m.contains("from 512 up"), "{m}"),
                Err(other) => panic!("{json}: {other}"),
                Ok(_) => panic!("{json}: accepted"),
            }
        }
    }
    let p = compiled(&errno_rule(511, ""));
    assert_eq!(verdict(&p, libc::SYS_getppid, a0(0)), errno(511));
    let p = compiled(&default(511));
    assert_eq!(verdict(&p, libc::SYS_getppid, a0(0)), errno(511));
}

#[test]
fn a_profile_too_long_for_the_kernel_is_refused() {
    let rules: Vec<String> = (0..1000)
        .map(|i| {
            format!(
                r#"{{"names":["getppid"],"action":"SCMP_ACT_ERRNO","args":[{{"index":0,"value":{i},"op":"SCMP_CMP_EQ"}}]}}"#
            )
        })
        .collect();
    let json = format!(
        r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{}]}}"#,
        rules.join(",")
    );
    let m = refusal(&json);
    assert!(m.contains("more than the kernel's 4096"), "{m}");
}

/// A rule's `names`, held in one buffer, read as a list of strings does:
/// escapes decoded, in order, and anything else refused in serde's words.
#[test]
fn names_read_as_a_list_of_strings() {
    let with = |names: &str| {
        format!(
            r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":{names},"action":"SCMP_ACT_ERRNO","errnoRet":1}}]}}"#
        )
    };
    for names in [
        r#"["getppid","getpid"]"#,
        r#"["\u0067etppid","get\u0070id"]"#,
    ] {
        let p = compiled(&with(names));
        assert_eq!(verdict(&p, libc::SYS_getppid, a0(0)), errno(1), "{names}");
        assert_eq!(verdict(&p, libc::SYS_getpid, a0(0)), errno(1), "{names}");
        assert_eq!(verdict(&p, libc::SYS_gettid, a0(0)), ALLOW, "{names}");
    }
    let single = r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":null,"name":"getppid","action":"SCMP_ACT_ERRNO","errnoRet":1}]}"#;
    assert_eq!(
        verdict(&compiled(single), libc::SYS_getppid, a0(0)),
        errno(1)
    );
    for (names, want) in [
        (r#""getppid""#, "expected a sequence"),
        ("[1]", "expected a string"),
        (r#"["getppid",null]"#, "expected a string"),
        ("[]", "names no system call"),
        (
            r#"["getppid","not_a_call_1","not_a_call_2"]"#,
            r#""not_a_call_1" is not a system call"#,
        ),
    ] {
        let m = refusal(&with(names));
        assert!(m.contains(want), "{names}: {m}");
    }
}

/// The names moby's profile allows here that `libc` gives no number for:
/// most are other architectures' calls, the rest (`cachestat`,
/// `futex_wait`, `statmount`, `uretprobe`...) get `ENOSYS` where Docker
/// allows them (sandbox.md, Differences from Docker). Pinned per target,
/// so that a `libc` bump or a re-vendored profile shows here: CI runs the
/// x86_64 gnu and aarch64 musl lists. The two others follow `syscalls.rs`:
/// x86_64 musl is gnu's without `io_pgetevents`, aarch64 gnu is musl's
/// with the five names musl alone gives there.
#[test]
fn calls_the_default_allows_but_libc_does_not_name_are_pinned() {
    let p: Json = serde_json::from_slice(DOCKER_DEFAULT).unwrap();
    let cx = cx((6, 8));
    let mut unnamed = Vec::new();
    for r in p.syscalls.iter().flatten() {
        let (_, names) = rule(r, "rule", false).unwrap();
        if cx.keeps(r).unwrap() {
            unnamed.extend(names.into_iter().filter(|n| syscalls::number(n).is_none()));
        }
    }
    unnamed.sort_unstable();
    unnamed.dedup();
    assert_eq!(unnamed, UNNAMED, "unnamed calls changed");
}

#[cfg(all(target_arch = "x86_64", target_env = "gnu"))]
const UNNAMED: &[&str] = &[
    "_llseek",
    "_newselect",
    "cachestat",
    "chown32",
    "clock_adjtime64",
    "clock_getres_time64",
    "clock_gettime64",
    "clock_nanosleep_time64",
    "fadvise64_64",
    "fchown32",
    "fcntl64",
    "fstat64",
    "fstatat64",
    "fstatfs64",
    "ftruncate64",
    "futex_requeue",
    "futex_time64",
    "futex_wait",
    "futex_wake",
    "getegid32",
    "geteuid32",
    "getgid32",
    "getgroups32",
    "getresgid32",
    "getresuid32",
    "getuid32",
    "getxattrat",
    "io_pgetevents",
    "io_pgetevents_time64",
    "ipc",
    "lchown32",
    "listmount",
    "listxattrat",
    "lstat64",
    "map_shadow_stack",
    "mmap2",
    "mq_timedreceive_time64",
    "mq_timedsend_time64",
    "ppoll_time64",
    "pselect6_time64",
    "recv",
    "recvmmsg_time64",
    "removexattrat",
    "riscv_hwprobe",
    "rt_sigtimedwait_time64",
    "sched_rr_get_interval_time64",
    "semtimedop_time64",
    "send",
    "sendfile64",
    "setfsgid32",
    "setfsuid32",
    "setgid32",
    "setgroups32",
    "setregid32",
    "setresgid32",
    "setresuid32",
    "setreuid32",
    "setuid32",
    "setxattrat",
    "sigprocmask",
    "sigreturn",
    "socketcall",
    "stat64",
    "statfs64",
    "statmount",
    "timer_gettime64",
    "timer_settime64",
    "timerfd_gettime64",
    "timerfd_settime64",
    "truncate64",
    "ugetrlimit",
    "uretprobe",
    "utimensat_time64",
    "waitpid",
];
#[cfg(all(target_arch = "x86_64", target_env = "musl"))]
const UNNAMED: &[&str] = &[
    "_llseek",
    "_newselect",
    "cachestat",
    "chown32",
    "clock_adjtime64",
    "clock_getres_time64",
    "clock_gettime64",
    "clock_nanosleep_time64",
    "fadvise64_64",
    "fchown32",
    "fcntl64",
    "fstat64",
    "fstatat64",
    "fstatfs64",
    "ftruncate64",
    "futex_requeue",
    "futex_time64",
    "futex_wait",
    "futex_wake",
    "getegid32",
    "geteuid32",
    "getgid32",
    "getgroups32",
    "getresgid32",
    "getresuid32",
    "getuid32",
    "getxattrat",
    "io_pgetevents_time64",
    "ipc",
    "lchown32",
    "listmount",
    "listxattrat",
    "lstat64",
    "map_shadow_stack",
    "mmap2",
    "mq_timedreceive_time64",
    "mq_timedsend_time64",
    "ppoll_time64",
    "pselect6_time64",
    "recv",
    "recvmmsg_time64",
    "removexattrat",
    "riscv_hwprobe",
    "rt_sigtimedwait_time64",
    "sched_rr_get_interval_time64",
    "semtimedop_time64",
    "send",
    "sendfile64",
    "setfsgid32",
    "setfsuid32",
    "setgid32",
    "setgroups32",
    "setregid32",
    "setresgid32",
    "setresuid32",
    "setreuid32",
    "setuid32",
    "setxattrat",
    "sigprocmask",
    "sigreturn",
    "socketcall",
    "stat64",
    "statfs64",
    "statmount",
    "timer_gettime64",
    "timer_settime64",
    "timerfd_gettime64",
    "timerfd_settime64",
    "truncate64",
    "ugetrlimit",
    "uretprobe",
    "utimensat_time64",
    "waitpid",
];
#[cfg(all(target_arch = "aarch64", target_env = "gnu"))]
const UNNAMED: &[&str] = &[
    "_llseek",
    "_newselect",
    "access",
    "alarm",
    "arm_fadvise64_64",
    "arm_sync_file_range",
    "breakpoint",
    "cacheflush",
    "cachestat",
    "chmod",
    "chown",
    "chown32",
    "clock_adjtime64",
    "clock_getres_time64",
    "clock_gettime64",
    "clock_nanosleep_time64",
    "creat",
    "dup2",
    "epoll_create",
    "epoll_ctl_old",
    "epoll_wait",
    "epoll_wait_old",
    "eventfd",
    "fadvise64_64",
    "fchmodat2",
    "fchown32",
    "fcntl64",
    "fork",
    "fstat64",
    "fstatat64",
    "fstatfs64",
    "ftruncate64",
    "futex_requeue",
    "futex_time64",
    "futex_wait",
    "futex_wake",
    "futimesat",
    "get_thread_area",
    "getdents",
    "getegid32",
    "geteuid32",
    "getgid32",
    "getgroups32",
    "getpgrp",
    "getresgid32",
    "getresuid32",
    "getrlimit",
    "getuid32",
    "getxattrat",
    "inotify_init",
    "io_pgetevents",
    "io_pgetevents_time64",
    "ipc",
    "lchown",
    "lchown32",
    "link",
    "listmount",
    "listxattrat",
    "lstat",
    "lstat64",
    "map_shadow_stack",
    "mkdir",
    "mknod",
    "mmap2",
    "mq_timedreceive_time64",
    "mq_timedsend_time64",
    "open",
    "pause",
    "pipe",
    "poll",
    "ppoll_time64",
    "pselect6_time64",
    "readlink",
    "recv",
    "recvmmsg_time64",
    "removexattrat",
    "rename",
    "renameat",
    "riscv_hwprobe",
    "rmdir",
    "rt_sigtimedwait_time64",
    "sched_rr_get_interval_time64",
    "select",
    "semtimedop_time64",
    "send",
    "sendfile64",
    "set_thread_area",
    "set_tls",
    "setfsgid32",
    "setfsuid32",
    "setgid32",
    "setgroups32",
    "setregid32",
    "setresgid32",
    "setresuid32",
    "setreuid32",
    "setrlimit",
    "setuid32",
    "setxattrat",
    "signalfd",
    "sigprocmask",
    "sigreturn",
    "socketcall",
    "stat",
    "stat64",
    "statfs64",
    "statmount",
    "symlink",
    "sync_file_range",
    "sync_file_range2",
    "time",
    "timer_gettime64",
    "timer_settime64",
    "timerfd_gettime64",
    "timerfd_settime64",
    "truncate64",
    "ugetrlimit",
    "unlink",
    "uretprobe",
    "utime",
    "utimensat_time64",
    "utimes",
    "vfork",
    "waitpid",
];
#[cfg(all(target_arch = "aarch64", target_env = "musl"))]
const UNNAMED: &[&str] = &[
    "_llseek",
    "_newselect",
    "access",
    "alarm",
    "arm_fadvise64_64",
    "arm_sync_file_range",
    "breakpoint",
    "cacheflush",
    "cachestat",
    "chmod",
    "chown",
    "chown32",
    "clock_adjtime64",
    "clock_getres_time64",
    "clock_gettime64",
    "clock_nanosleep_time64",
    "creat",
    "dup2",
    "epoll_create",
    "epoll_ctl_old",
    "epoll_wait",
    "epoll_wait_old",
    "eventfd",
    "fadvise64_64",
    "fchmodat2",
    "fchown32",
    "fcntl64",
    "fork",
    "fstat64",
    "fstatat64",
    "fstatfs64",
    "ftruncate64",
    "futex_requeue",
    "futex_time64",
    "futex_wait",
    "futex_wake",
    "futimesat",
    "get_thread_area",
    "getdents",
    "getegid32",
    "geteuid32",
    "getgid32",
    "getgroups32",
    "getpgrp",
    "getresgid32",
    "getresuid32",
    "getuid32",
    "getxattrat",
    "inotify_init",
    "io_pgetevents_time64",
    "ipc",
    "lchown",
    "lchown32",
    "link",
    "listmount",
    "listxattrat",
    "lstat",
    "lstat64",
    "map_shadow_stack",
    "mkdir",
    "mknod",
    "mmap2",
    "mq_timedreceive_time64",
    "mq_timedsend_time64",
    "open",
    "pause",
    "pipe",
    "poll",
    "ppoll_time64",
    "pselect6_time64",
    "readlink",
    "recv",
    "recvmmsg_time64",
    "removexattrat",
    "rename",
    "riscv_hwprobe",
    "rmdir",
    "rt_sigtimedwait_time64",
    "sched_rr_get_interval_time64",
    "select",
    "semtimedop_time64",
    "send",
    "sendfile64",
    "set_thread_area",
    "set_tls",
    "setfsgid32",
    "setfsuid32",
    "setgid32",
    "setgroups32",
    "setregid32",
    "setresgid32",
    "setresuid32",
    "setreuid32",
    "setuid32",
    "setxattrat",
    "signalfd",
    "sigprocmask",
    "sigreturn",
    "socketcall",
    "stat",
    "stat64",
    "statfs64",
    "statmount",
    "symlink",
    "sync_file_range2",
    "time",
    "timer_gettime64",
    "timer_settime64",
    "timerfd_gettime64",
    "timerfd_settime64",
    "truncate64",
    "ugetrlimit",
    "unlink",
    "uretprobe",
    "utime",
    "utimensat_time64",
    "utimes",
    "vfork",
    "waitpid",
];

#[test]
fn kernel_releases_parse_as_moby_reads_them() {
    assert_eq!(parse_release("6.8.0-45-generic"), Some((6, 8)));
    assert_eq!(parse_release("3.12-1-amd64"), Some((3, 12)));
    assert_eq!(parse_release("7.0"), Some((7, 0)));
    assert_eq!(parse_release("6"), None);
    assert_eq!(parse_release("x.1"), None);
    assert_eq!(parse_release("6.x"), None);
    assert_eq!(min_kernel("4.8").unwrap(), (4, 8));
    assert_eq!(min_kernel("").unwrap(), (0, 0));
}

/// xorshift64*: deterministic, so that a failing case names its seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len())]
    }
}

/// A rule as [`reference`] reads it: names, return value, conditions as
/// (index, operator, value, valueTwo).
struct RefRule {
    names: Vec<&'static str>,
    ret: u32,
    conds: Vec<(usize, &'static str, u64, u64)>,
}

/// The kernel's precedence of a return value: lower wins.
fn precedence(ret: u32) -> i32 {
    (ret & libc::SECCOMP_RET_ACTION_FULL).cast_signed()
}

fn holds(op: &str, arg: u64, v: u64, two: u64) -> bool {
    match op {
        "SCMP_CMP_NE" => arg != v,
        "SCMP_CMP_LT" => arg < v,
        "SCMP_CMP_LE" => arg <= v,
        "SCMP_CMP_EQ" => arg == v,
        "SCMP_CMP_GE" => arg >= v,
        "SCMP_CMP_GT" => arg > v,
        "SCMP_CMP_MASKED_EQ" => arg & v == two,
        other => panic!("{other}"),
    }
}

/// The verdict sandbox.md gives, read from the profile alone: another
/// architecture or an x32 number gets `EPERM`; a number `libc` does not
/// name gets the default where it lets calls run, else `ENOSYS`; a named
/// one gets the strictest rule matching it, the first in the profile of
/// two refusals with different errnos, else the default.
fn reference(rules: &[RefRule], default: u32, arch: u32, nr: u32, args: [u64; 6]) -> u32 {
    let eperm = errno(libc::EPERM);
    if arch != AUDIT_ARCH {
        return eperm;
    }
    #[cfg(target_arch = "x86_64")]
    if nr >= super::super::X32_SYSCALL_BIT {
        return eperm;
    }
    let runs = |r: u32| precedence(r) >= precedence(libc::SECCOMP_RET_LOG);
    if syscalls::all().all(|(_, n)| n != nr) {
        return if runs(default) {
            default
        } else {
            errno(libc::ENOSYS)
        };
    }
    rules
        .iter()
        .filter(|r| r.names.iter().any(|n| syscalls::number(n) == Some(nr)))
        .filter(|r| {
            r.conds
                .iter()
                .all(|&(i, op, v, two)| holds(op, args[i], v, two))
        })
        .map(|r| r.ret)
        .min_by_key(|&r| precedence(r))
        .unwrap_or(default)
}

/// Values around both 32-bit words, where a two-word compare can slip.
const EDGES: [u64; 12] = [
    0,
    1,
    5,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_ffff,
    0x1_0000_0000,
    0x1_0000_0005,
    0x1_ffff_ffff,
    0x8000_0000_0000_0000,
    u64::MAX - 1,
    u64::MAX,
];

const OPS: [&str; 7] = [
    "SCMP_CMP_NE",
    "SCMP_CMP_LT",
    "SCMP_CMP_LE",
    "SCMP_CMP_EQ",
    "SCMP_CMP_GE",
    "SCMP_CMP_GT",
    "SCMP_CMP_MASKED_EQ",
];

/// The calls random profiles name; the forked checks make only the first
/// two, which read no argument.
const POOL: [&str; 7] = [
    "getppid",
    "getpid",
    "keyctl",
    "socket",
    "personality",
    "uname",
    "gettid",
];

fn edge_or_random(rng: &mut Rng) -> u64 {
    match rng.below(4) {
        0 => rng.next(),
        1 => rng.pick(&EDGES).wrapping_add(rng.pick(&[1, u64::MAX])),
        _ => rng.pick(&EDGES),
    }
}

/// A random action: its JSON (`"action":…` and maybe `"errnoRet":…`) and
/// the return value it means.
fn random_action(rng: &mut Rng, action: &str, errno_ret: &str) -> (String, u32) {
    let named = |a: &str| format!(r#""{action}":"{a}""#);
    match rng.below(7) {
        0 => (named("SCMP_ACT_KILL"), libc::SECCOMP_RET_KILL_THREAD),
        1 => (
            named("SCMP_ACT_KILL_PROCESS"),
            libc::SECCOMP_RET_KILL_PROCESS,
        ),
        2 => (named("SCMP_ACT_TRAP"), libc::SECCOMP_RET_TRAP),
        3 => (named("SCMP_ACT_LOG"), libc::SECCOMP_RET_LOG),
        4 => (named("SCMP_ACT_ALLOW"), ALLOW),
        5 => (named("SCMP_ACT_ERRNO"), errno(libc::EPERM)),
        _ => {
            let e = 1 + rng.below(511);
            (
                format!(r#"{},"{errno_ret}":{e}"#, named("SCMP_ACT_ERRNO")),
                errno(i32::try_from(e).unwrap()),
            )
        }
    }
}

/// A random profile of rules on [`POOL`]: its JSON, its rules and its
/// default's return value. `exit` and `exit_group` are allowed first, so
/// that the forked child can exit (#103).
fn random_profile(rng: &mut Rng) -> (String, Vec<RefRule>, u32) {
    let (default_json, default) = random_action(rng, "defaultAction", "defaultErrnoRet");
    let mut json = vec![r#"{"names":["exit","exit_group"],"action":"SCMP_ACT_ALLOW"}"#.to_owned()];
    let mut rules = vec![RefRule {
        names: vec!["exit", "exit_group"],
        ret: ALLOW,
        conds: Vec::new(),
    }];
    for _ in 0..rng.below(10) {
        let names: Vec<&str> = (0..=rng.below(2)).map(|_| rng.pick(&POOL)).collect();
        let (action, ret) = random_action(rng, "action", "errnoRet");
        let mut free = vec![0, 1, 2, 3, 4, 5];
        let mut conds = Vec::new();
        for _ in 0..rng.below(4) {
            let index = free.remove(rng.below(free.len()));
            conds.push((
                index,
                rng.pick(&OPS),
                edge_or_random(rng),
                edge_or_random(rng),
            ));
        }
        let args: Vec<String> = conds
            .iter()
            .map(|(i, op, v, two)| {
                format!(r#"{{"index":{i},"value":{v},"valueTwo":{two},"op":"{op}"}}"#)
            })
            .collect();
        let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
        let named = if quoted.len() == 1 && rng.below(2) == 0 {
            format!(r#""name":{}"#, quoted[0])
        } else {
            format!(r#""names":[{}]"#, quoted.join(","))
        };
        json.push(format!(
            r#"{{{named},{action},"args":[{}]}}"#,
            args.join(",")
        ));
        rules.push(RefRule { names, ret, conds });
    }
    let json = format!(r#"{{{default_json},"syscalls":[{}]}}"#, json.join(","));
    (json, rules, default)
}

/// Arguments that often meet a rule's values, or miss them by one in
/// either word.
fn random_args(rng: &mut Rng, rules: &[RefRule]) -> [u64; 6] {
    let near: Vec<u64> = rules
        .iter()
        .flat_map(|r| r.conds.iter().flat_map(|&(_, _, v, two)| [v, two]))
        .collect();
    std::array::from_fn(|_| match rng.below(5) {
        0 if !near.is_empty() => rng.pick(&near),
        1 if !near.is_empty() => rng
            .pick(&near)
            .wrapping_add(rng.pick(&[1, u64::MAX, 1 << 32])),
        2 => rng.next(),
        _ => edge_or_random(rng),
    })
}

/// Random profiles: the compiled filter answers every call, from every
/// architecture, as a reading of the profile alone does (ADR-0109),
/// whatever its actions, conditions and their order.
#[test]
fn random_profiles_compile_to_what_the_profile_says() {
    let numbers: Vec<u32> = syscalls::all().map(|(_, n)| n).collect();
    for seed in 1..=400 {
        let mut rng = Rng::new(seed);
        let (json, rules, default) = random_profile(&mut rng);
        let p = compiled(&json);
        for _ in 0..300 {
            let nr = match rng.below(6) {
                0 => rng.pick(&numbers),
                1 => u32::try_from(rng.next() >> 32).unwrap(),
                2 => 1000 + u32::try_from(rng.below(64)).unwrap(),
                _ => syscalls::number(rng.pick(&POOL)).unwrap(),
            };
            let arch = if rng.below(20) == 0 {
                rng.pick(&[0x4000_0003, 0x4000_0028, 0xc000_003e, 0xc000_00b7])
            } else {
                AUDIT_ARCH
            };
            let args = random_args(&mut rng, &rules);
            assert_eq!(
                verdict_on(&p, arch, nr, args),
                reference(&rules, default, arch, nr, args),
                "seed {seed}: arch {arch:#x}, call {nr}, {args:x?}\n{json}"
            );
        }
    }
}

/// The same calls of the same profile, from the x32 ABI, get `EPERM`
/// whatever the profile says of the native call.
#[cfg(target_arch = "x86_64")]
#[test]
fn x32_numbers_are_refused_whatever_the_profile_says() {
    let x32 = super::super::X32_SYSCALL_BIT;
    for default in ["SCMP_ACT_ALLOW", "SCMP_ACT_LOG", "SCMP_ACT_ERRNO"] {
        let p = compiled(&format!(
            r#"{{"defaultAction":"{default}","syscalls":[{{"names":["keyctl","getpid"],
            "action":"SCMP_ACT_ALLOW"}}]}}"#
        ));
        for (name, n) in syscalls::all() {
            for high in [x32, x32 | 0x8000_0000, 0xffff_ff00 | (n & 0xff)] {
                assert_eq!(
                    verdict_on(&p, AUDIT_ARCH, n | high, [0; 6]),
                    errno(libc::EPERM),
                    "{default}: {name} | {high:#x}"
                );
            }
        }
    }
}

/// [`SeccompProfile::from_json`] refuses `json`, never panics: the message.
fn refused_live(json: &[u8]) -> String {
    match SeccompProfile::from_json(json) {
        Err(Error::SeccompProfile(m)) => m,
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("accepted: {}", String::from_utf8_lossy(json)),
    }
}

/// A key given twice is refused, never read as its first or last value,
/// which could loosen the profile (Go's `encoding/json` keeps the last).
#[test]
fn a_key_given_twice_is_refused() {
    let rule = |r: &str| format!(r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{r}]}}"#);
    for json in [
        r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultAction":"SCMP_ACT_ALLOW"}"#.to_owned(),
        r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":1,"defaultErrnoRet":1}"#.to_owned(),
        r#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[],"syscalls":[{"names":["keyctl"],"action":"SCMP_ACT_ALLOW"}]}"#.to_owned(),
        r#"{"defaultAction":"SCMP_ACT_ALLOW","flags":[],"flags":["SECCOMP_FILTER_FLAG_SPEC_ALLOW"]}"#.to_owned(),
        rule(r#"{"names":["keyctl"],"action":"SCMP_ACT_ERRNO","action":"SCMP_ACT_ALLOW"}"#),
        rule(r#"{"names":["keyctl"],"names":["getppid"],"action":"SCMP_ACT_ERRNO"}"#),
        rule(r#"{"name":"keyctl","name":"getppid","action":"SCMP_ACT_ERRNO"}"#),
        rule(r#"{"names":["keyctl"],"action":"SCMP_ACT_ERRNO","errnoRet":1,"errnoRet":1}"#),
        rule(
            r#"{"names":["socket"],"action":"SCMP_ACT_ERRNO","args":[],
            "args":[{"index":0,"value":2,"op":"SCMP_CMP_EQ"}]}"#,
        ),
        rule(
            r#"{"names":["socket"],"action":"SCMP_ACT_ERRNO",
            "args":[{"index":0,"value":2,"value":40,"op":"SCMP_CMP_EQ"}]}"#,
        ),
        rule(
            r#"{"names":["socket"],"action":"SCMP_ACT_ERRNO",
            "args":[{"index":0,"index":1,"value":2,"op":"SCMP_CMP_EQ"}]}"#,
        ),
        rule(
            r#"{"names":["socket"],"action":"SCMP_ACT_ERRNO",
            "args":[{"index":0,"value":2,"op":"SCMP_CMP_EQ","op":"SCMP_CMP_NE"}]}"#,
        ),
        rule(
            r#"{"names":["keyctl"],"action":"SCMP_ACT_ERRNO",
            "includes":{"caps":["CAP_CHOWN"],"caps":["CAP_SYS_ADMIN"]}}"#,
        ),
        rule(
            r#"{"names":["keyctl"],"action":"SCMP_ACT_ERRNO",
            "excludes":{},"excludes":{"arches":["amd64","arm64"]}}"#,
        ),
        r#"{"defaultAction":"SCMP_ACT_ALLOW","archMap":[{"architecture":"SCMP_ARCH_X86_64",
        "architecture":"SCMP_ARCH_AARCH64"}]}"#
            .to_owned(),
    ] {
        let m = refused_live(json.as_bytes());
        assert!(m.contains("duplicate field"), "{json}: {m}");
    }
}

/// Deep nesting, a megabyte of it, and anything around one profile are
/// refused without a stack overflow. Every field is typed and an unknown
/// one is refused, so each deep case ends at its first levels today: they
/// guard against a future untyped field.
#[test]
fn nesting_and_bytes_around_a_profile_are_refused() {
    let deep = 1 << 20;
    let mut cases: Vec<Vec<u8>> = vec![
        b"[".repeat(deep),
        b"{\"a\":".repeat(deep / 5),
        [&br#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":"#[..], &b"[".repeat(deep)].concat(),
        [
            &br#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["keyctl"],"action":"SCMP_ACT_ERRNO","comment":"#[..],
            &b"[".repeat(deep),
        ]
        .concat(),
        [
            &br#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["keyctl"],"action":"SCMP_ACT_ERRNO","includes":"#[..],
            &b"{\"caps\":".repeat(deep / 8),
        ]
        .concat(),
    ];
    for extra in [
        &br#"{"defaultAction":"SCMP_ACT_ERRNO"}{"defaultAction":"SCMP_ACT_ALLOW"}"#[..],
        br#"{"defaultAction":"SCMP_ACT_ERRNO"},"#,
        br#"{"defaultAction":"SCMP_ACT_ERRNO"} // allow"#,
        b"\xef\xbb\xbf{\"defaultAction\":\"SCMP_ACT_ERRNO\"}",
        b"{\"defaultAction\":\"SCMP_ACT_ERRNO\",\"syscalls\":[{\"names\":[\"key\xffctl\"],\"action\":\"SCMP_ACT_ALLOW\"}]}",
        b"{\"defaultAction\":\"SCMP_ACT_ERRNO\",\"syscalls\":[{\"names\":[\"keyctl\x00\"],\"action\":\"SCMP_ACT_ALLOW\"}]}",
        b"null",
        b"[]",
        b"\"SCMP_ACT_ALLOW\"",
        b"",
    ] {
        cases.push(extra.to_vec());
    }
    // The control: the same profiles, without the nesting or odd bytes,
    // are read.
    for json in [
        &br#"{"defaultAction":"SCMP_ACT_ERRNO"}"#[..],
        br#"{"defaultAction":"SCMP_ACT_ERRNO","syscalls":[{"names":["keyctl"],"action":"SCMP_ACT_ALLOW"}]}"#,
        br#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["keyctl"],"action":"SCMP_ACT_ERRNO",
            "comment":"x","includes":{"caps":["CAP_SYS_ADMIN"]}}]}"#,
    ] {
        SeccompProfile::from_json(json).unwrap();
    }
    for json in cases {
        let shown = String::from_utf8_lossy(&json[..json.len().min(80)]).into_owned();
        let m = refused_live(&json);
        assert!(!m.is_empty(), "{shown}");
    }
}

/// A number given in a form that is not a plain unsigned integer is
/// refused, never read as 0 or truncated.
#[test]
fn numbers_in_other_forms_are_refused() {
    let errno_rule = |e: &str| {
        format!(
            r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["keyctl"],
            "action":"SCMP_ACT_ERRNO","errnoRet":{e}}}]}}"#
        )
    };
    let arg = |field: &str, v: &str| {
        let (index, value, two) = match field {
            "index" => (v, "0", "0"),
            "value" => ("0", v, "0"),
            "valueTwo" => ("0", "0", v),
            other => panic!("{other}"),
        };
        format!(
            r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["socket"],
            "action":"SCMP_ACT_ERRNO","args":[{{"index":{index},"value":{value},
            "valueTwo":{two},"op":"SCMP_CMP_EQ"}}]}}]}}"#
        )
    };
    let odd = [
        "-0",
        "0.0",
        "1.0",
        "1e0",
        "1E2",
        "\"1\"",
        "true",
        "18446744073709551616",
        "1e400",
        "-1",
        "0x1",
        "01",
        "+1",
        "NaN",
        "[]",
        "{}",
    ];
    // The control: a plain number is read in each of those places.
    for json in [
        errno_rule("1"),
        r#"{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":1}"#.to_owned(),
        arg("index", "1"),
        arg("value", "1"),
        arg("valueTwo", "1"),
    ] {
        SeccompProfile::from_json(json.as_bytes()).unwrap();
    }
    for v in odd {
        for json in [
            errno_rule(v),
            format!(r#"{{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":{v}}}"#),
            arg("value", v),
            arg("index", v),
            arg("valueTwo", v),
        ] {
            refused_live(json.as_bytes());
        }
    }
}

/// Rules on one call that disagree, in either order, by `name` or
/// `names`, or given twice: the strictest that matches wins (ADR-0109).
#[test]
fn rules_that_disagree_settle_on_the_strictest_in_any_order() {
    let g = libc::SYS_getppid;
    let allow = r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW"}"#;
    let refuse = r#"{"name":"getppid","action":"SCMP_ACT_ERRNO","errnoRet":9}"#;
    let log = r#"{"names":["getppid","getppid"],"action":"SCMP_ACT_LOG"}"#;
    let kill_on_7 = r#"{"names":["getppid"],"action":"SCMP_ACT_KILL_PROCESS",
        "args":[{"index":0,"value":7,"op":"SCMP_CMP_EQ"}]}"#;
    let allow_on_7 = r#"{"names":["getppid"],"action":"SCMP_ACT_ALLOW",
        "args":[{"index":0,"value":7,"op":"SCMP_CMP_EQ"}]}"#;
    for rules in [
        [allow, refuse, log, kill_on_7, allow_on_7],
        [allow_on_7, kill_on_7, log, refuse, allow],
        [log, allow_on_7, allow, kill_on_7, refuse],
    ] {
        for default in ["SCMP_ACT_ALLOW", "SCMP_ACT_ERRNO"] {
            let p = compiled(&format!(
                r#"{{"defaultAction":"{default}","syscalls":[{}]}}"#,
                rules.join(",")
            ));
            assert_eq!(verdict(&p, g, a0(0)), errno(9), "{rules:?}");
            assert_eq!(
                verdict(&p, g, a0(7)),
                libc::SECCOMP_RET_KILL_PROCESS,
                "{rules:?}"
            );
            assert_eq!(verdict(&p, g, a0(8)), errno(9), "{rules:?}");
        }
    }
}

/// A profile that compiles to exactly `want` instructions, with an errno
/// rule on `getppid` whose first argument is 0 (`EPERM`) among them.
fn profile_of_length(want: usize) -> Option<(String, Program)> {
    // An EQ rule is 5 instructions, a MASKED_EQ one 7: one to five
    // MASKED_EQ rules, the first of which also adds `getpid`'s branch, give
    // every length modulo 5, and EQ rules fill the rest.
    let eq = |i: usize| {
        format!(
            r#"{{"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":{},
            "args":[{{"index":0,"value":{i},"op":"SCMP_CMP_EQ"}}]}}"#,
            1 + i % 500
        )
    };
    let masked = |i: usize| {
        format!(
            r#"{{"names":["getpid"],"action":"SCMP_ACT_ERRNO","errnoRet":{},
            "args":[{{"index":1,"value":{},"valueTwo":{},"op":"SCMP_CMP_MASKED_EQ"}}]}}"#,
            1 + i % 500,
            u64::MAX,
            i
        )
    };
    for m in 1..=5 {
        let mut lo = 0usize;
        let mut hi = 1000;
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let rules: Vec<String> = (0..mid).map(eq).chain((0..m).map(masked)).collect();
            let json = format!(
                r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{}]}}"#,
                rules.join(",")
            );
            let len = match compile(json.as_bytes(), &cx((6, 8))) {
                Ok(p) if p.program.insns.len() == want => return Some((json, p.program)),
                Ok(p) => p.program.insns.len(),
                Err(Error::SeccompProfile(m)) if m.contains("more than the kernel's") => usize::MAX,
                Err(e) => panic!("{e}"),
            };
            if len < want {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
    }
    None
}

/// The kernel's bound is the compiler's: 4096 instructions compile (the
/// kernel installs them, `kernel::a_profile_of_4096_instructions_installs`),
/// one rule more is refused.
#[test]
fn profiles_compile_up_to_the_kernels_4096_instructions() {
    let (json, p) = profile_of_length(MAX_INSNS).expect("a profile of 4096 instructions");
    assert_eq!(verdict(&p, libc::SYS_getppid, a0(0)), errno(1));
    let one_more = json.replacen(
        r#""syscalls":["#,
        r#""syscalls":[{"names":["getppid"],"action":"SCMP_ACT_ERRNO","args":[{"index":2,"value":1,"op":"SCMP_CMP_EQ"}]},"#,
        1,
    );
    let m = refusal(&one_more);
    assert!(m.contains("more than the kernel's 4096"), "{m}");
}

/// The same filters run by the kernel, in a forked child. Skipped under
/// qemu-user, which cannot install an aarch64 filter in an x86_64 kernel.
mod kernel {
    use super::*;

    /// [`under_filter`](crate::seccomp::under_filter); `check` gives 0, or
    /// the number of the check that failed.
    fn under(prog: &Program, check: impl Fn() -> i32) -> i32 {
        crate::seccomp::under_filter(prog, check)
    }

    fn exited_ok(status: i32) {
        assert!(libc::WIFEXITED(status), "{status:#x}");
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "the number of the failing check"
        );
    }

    fn last_errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// `getppid` with these arguments, which it ignores and the filter reads.
    fn getppid(args: [u64; 6]) -> libc::c_long {
        let [a, b, c, d, e, f] = args.map(u64::cast_signed);
        // SAFETY: `getppid` reads no argument.
        unsafe { libc::syscall(libc::SYS_getppid, a, b, c, d, e, f) }
    }

    /// `-1` with `errno` `e`.
    fn refused(r: libc::c_long, e: i32) -> bool {
        r == -1 && last_errno() == e
    }

    #[test]
    fn arguments_compare_as_64_bit_values() {
        for c in &COMPARES {
            for index in [0, 1, 5] {
                let p = compiled(&compare_profile(c, index));
                let status = under(&p, || {
                    let mut n = 0;
                    for &v in c.hits {
                        n += 1;
                        if !refused(getppid(at(index, v)), 77) {
                            return n;
                        }
                    }
                    for &v in c.misses {
                        n += 1;
                        if getppid(at(index, v)) <= 0 {
                            return n;
                        }
                    }
                    0
                });
                exited_ok(status);
            }
        }
    }

    #[test]
    fn each_rule_has_its_action() {
        let p = compiled(ACTIONS);
        exited_ok(under(&p, || {
            let checks = [
                getppid(a0(0)) > 0,
                refused(getppid(a0(1)), 11),
                refused(getppid(a0(2)), 12),
                refused(getppid(a0(7)), 13),
                getppid(a0(3)) > 0,
                refused(getppid(a0(8)), 511),
            ];
            checks
                .iter()
                .position(|ok| !ok)
                .map_or(0, |i| i32::try_from(i + 1).unwrap_or(99))
        }));
        for arg in [4, 5, 6, 9] {
            let status = under(&p, || {
                getppid(a0(arg));
                1
            });
            assert!(
                libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSYS,
                "{arg}: {status:#x}"
            );
        }
    }

    #[test]
    fn long_jumps_reach_their_targets() {
        let p = long_profile();
        let top = syscalls::all()
            .map(|(_, n)| n)
            .filter(|n| !spared().contains(n))
            .max()
            .unwrap();
        // Worked out before the filter, which refuses the allocator's calls.
        let sample = [
            libc::SYS_read,
            libc::SYS_getpid,
            libc::SYS_getuid,
            libc::SYS_gettid,
            libc::SYS_sched_yield,
            libc::c_long::from(top),
        ]
        .map(|call| {
            let want = long_errno(u32::try_from(call).unwrap());
            (call, i32::try_from(want).unwrap())
        });
        let chain = [0, 59].map(|i| (u64::from(i), i32::try_from(chain_errno(i)).unwrap()));
        exited_ok(under(&p, || {
            let mut n = 0;
            for (call, want) in sample {
                n += 1;
                // SAFETY: refused by the filter before the kernel runs it.
                let r = unsafe { libc::syscall(call, 0, 0, 0, 0, 0, 0) };
                if !refused(r, want) {
                    return n;
                }
            }
            for (i, want) in chain {
                n += 1;
                if !refused(getppid(a0(i)), want) {
                    return n;
                }
            }
            n += 1;
            if getppid(a0(60)) <= 0 {
                return n;
            }
            0
        }));
    }

    /// The default profile as the kernel runs it: calls it refuses, with an
    /// errno the kernel itself would not give, and calls it allows.
    #[test]
    fn docker_default_loads_and_refuses() {
        let p = SeccompProfile::docker_default().unwrap().program;
        exited_ok(under(&p, || {
            // SAFETY: plain syscalls with integer or null arguments.
            let checks = unsafe {
                let stream = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
                [
                    getppid([0; 6]) > 0,
                    refused(
                        libc::syscall(libc::SYS_io_uring_setup, 1u32, std::ptr::null::<u8>()),
                        libc::EPERM,
                    ),
                    refused(
                        libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0usize),
                        libc::ENOSYS,
                    ),
                    refused(libc::c_long::from(libc::socket(40, stream, 0)), libc::EPERM),
                    refused(libc::c_long::from(libc::socket(38, stream, 0)), libc::EPERM),
                    libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) >= 0,
                    // KEYCTL_GET_KEYRING_ID of the session keyring.
                    refused(libc::syscall(libc::SYS_keyctl, 0, -3, 0), libc::EPERM),
                    libc::personality(0xffff_ffff) >= 0,
                    refused(libc::c_long::from(libc::personality(0x4_0000)), libc::EPERM),
                    refused(libc::syscall(1000), libc::ENOSYS),
                ]
            };
            checks
                .iter()
                .position(|ok| !ok)
                .map_or(0, |i| i32::try_from(i + 1).unwrap_or(99))
        }));
    }

    /// Random profiles of [`super::random_profile`]: the kernel installs
    /// each, and answers `getppid` and `getpid`, which read no argument, as
    /// the profile says, a kill or a trap included.
    #[test]
    fn random_profiles_install_and_answer_as_the_profile_says() {
        let fatal = |ret: u32| precedence(ret) < precedence(errno(1));
        for seed in 1..=150 {
            let mut rng = Rng::new(seed);
            let (json, rules, default) = random_profile(&mut rng);
            let p = compiled(&json);
            let calls: Vec<(libc::c_long, [u64; 6], u32)> = (0..40)
                .map(|_| {
                    let call = rng.pick(&[libc::SYS_getppid, libc::SYS_getpid]);
                    let args = random_args(&mut rng, &rules);
                    let n = u32::try_from(call).unwrap();
                    (call, args, reference(&rules, default, AUDIT_ARCH, n, args))
                })
                .collect();
            let status = under(&p, || {
                for (i, &(call, args, want)) in (1..).zip(&calls) {
                    if fatal(want) {
                        continue;
                    }
                    let [a, b, c, d, e, f] = args.map(u64::cast_signed);
                    // SAFETY: `getppid` and `getpid` read no argument.
                    let r = unsafe { libc::syscall(call, a, b, c, d, e, f) };
                    let errno_ret = want & libc::SECCOMP_RET_ACTION_FULL == libc::SECCOMP_RET_ERRNO;
                    let ok = if errno_ret {
                        refused(r, i32::try_from(want & libc::SECCOMP_RET_DATA).unwrap_or(0))
                    } else {
                        r > 0
                    };
                    if !ok {
                        return i;
                    }
                }
                0
            });
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "seed {seed}: {status:#x} (200: install refused)\n{json}"
            );
            if let Some(&(call, args, want)) = calls.iter().find(|c| fatal(c.2)) {
                let status = under(&p, || {
                    let [a, b, c, d, e, f] = args.map(u64::cast_signed);
                    // SAFETY: as above.
                    unsafe { libc::syscall(call, a, b, c, d, e, f) };
                    1
                });
                assert!(
                    libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSYS,
                    "seed {seed}: {want:#x} gave {status:#x}\n{json}"
                );
            }
        }
    }

    /// The longest profile `--seccomp` compiles is one the kernel installs.
    #[test]
    fn a_profile_of_4096_instructions_installs() {
        let (_, p) = profile_of_length(MAX_INSNS).unwrap();
        exited_ok(under(&p, || {
            if !refused(getppid(a0(0)), libc::EPERM) {
                return 1;
            }
            if getppid(a0(1 << 40)) <= 0 {
                return 2;
            }
            0
        }));
    }

    /// A forked child under no filter of the test's: the control.
    #[cfg(target_arch = "x86_64")]
    fn unfiltered(check: impl Fn() -> i32) -> i32 {
        // SAFETY: the child only makes syscalls and exits with `_exit`, as
        // in `under_filter`, whose note on a panicking `check` holds here.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: plain syscalls in the forked child.
            unsafe {
                // A SIGSEGV of `int 0x80` without i386 emulation dumps no
                // core.
                if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
                    libc::_exit(200);
                }
                libc::_exit(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(&check)).unwrap_or(201),
                );
            }
        }
        let mut status = 0;
        // SAFETY: reaping the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        status
    }

    /// An i386 system call through `int 0x80`: its result, `-errno` on a
    /// failure.
    ///
    /// # Safety
    ///
    /// The kernel runs the call if no filter stops it, reading any pointer
    /// argument as an i386 address. The caller must be a forked child and
    /// pass a call that, if it runs, changes nothing outside that child but
    /// kernel state the child's user may create for itself, such as its own
    /// keyrings.
    #[cfg(target_arch = "x86_64")]
    unsafe fn int80(nr: u32, a: u32, b: u32, c: u32) -> i32 {
        let ret: u32;
        // SAFETY: `int 0x80` with the i386 ABI's registers; `rbx`, which
        // inline assembly cannot name, is swapped in and back, and the
        // registers a 64-bit kernel may clobber on that path are marked.
        // The call itself is the caller's to make safe.
        unsafe {
            std::arch::asm!(
                "xchg {a:r}, rbx",
                "int 0x80",
                "xchg {a:r}, rbx",
                a = inout(reg) u64::from(a) => _,
                inlateout("eax") nr => ret,
                in("ecx") b,
                in("edx") c,
                out("r8") _,
                out("r9") _,
                out("r10") _,
                out("r11") _,
            );
        }
        ret.cast_signed()
    }

    /// i386 calls and their first two arguments: `getpid`;
    /// `personality(ADDR_NO_RANDOMIZE)`; `unshare(CLONE_NEWUSER)`;
    /// `keyctl(KEYCTL_GET_KEYRING_ID, KEY_SPEC_SESSION_KEYRING)`;
    /// `socket(AF_INET, SOCK_DGRAM)`. The numbers are literal because
    /// `libc` has no i386 table on x86_64.
    #[cfg(target_arch = "x86_64")]
    const I386: [(u32, u32, u32); 5] = [
        (20, 0, 0),
        (136, 0x4_0000, 0),
        (310, 0x1000_0000, 0),
        (288, 0, 0xffff_fffd),
        (359, 2, 2),
    ];

    /// The i386 and x32 ABIs reach no call, under the default profile and
    /// under one that refuses only the native `keyctl`: `getpid`, which
    /// nothing refuses natively, gets `EPERM` there, where the control
    /// without a filter gets its pid (i386) or `ENOSYS` (no x32 kernel).
    /// A kernel without i386 emulation faults on `int 0x80` in the
    /// control: the i386 checks are then skipped, with a message.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn other_abis_reach_no_call() {
        let x32 = libc::c_long::from(super::super::super::X32_SYSCALL_BIT);
        let control = unfiltered(|| {
            // SAFETY: an i386 `getpid`, which reads no argument, in a
            // forked child.
            i32::from(unsafe { int80(20, 0, 0, 0) } <= 0)
        });
        let i386 = !(libc::WIFSIGNALED(control) && libc::WTERMSIG(control) == libc::SIGSEGV);
        if i386 {
            exited_ok(control);
        } else {
            eprintln!("skipped: no i386 emulation in this kernel, the int 0x80 checks");
        }
        exited_ok(unfiltered(|| {
            // SAFETY: an x32 `getpid`, which reads no argument.
            let r = unsafe { libc::syscall(x32 | libc::SYS_getpid) };
            i32::from(!(r > 0 || refused(r, libc::ENOSYS)))
        }));
        let denylist = compiled(
            r#"{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[
            {"names":["keyctl"],"action":"SCMP_ACT_ERRNO","errnoRet":77}]}"#,
        );
        for (name, p) in [
            ("default", SeccompProfile::docker_default().unwrap().program),
            ("denylist", denylist),
        ] {
            let status = under(&p, || {
                for (i, (n, a, b)) in (1..).zip(I386) {
                    // SAFETY: in this forked child, each call of `I386`
                    // changes at most the child, or makes the user's own
                    // keyrings exist, if it runs: a pid read, its own
                    // personality or user namespace, a keyring lookup with
                    // create 0, a socket its exit closes.
                    if i386 && unsafe { int80(n, a, b, 0) } != -libc::EPERM {
                        return i;
                    }
                }
                let x32_calls = [libc::SYS_getpid, libc::SYS_keyctl, libc::SYS_unshare];
                for (i, n) in (10..).zip(x32_calls) {
                    // SAFETY: each changes at most the child, or makes the
                    // user's own keyrings exist, if it runs: an x32
                    // `getpid`, a `keyctl` lookup of the session keyring
                    // with create 0, `unshare(0)`.
                    let r = unsafe { libc::syscall(x32 | n, 0, -3, 0) };
                    if !refused(r, libc::EPERM) {
                        return i;
                    }
                }
                // SAFETY: -1 numbers no call; the kernel would answer
                // `ENOSYS`.
                if !refused(unsafe { libc::syscall(-1) }, libc::EPERM) {
                    return 20;
                }
                0
            });
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "{name}: {status:#x}, i386 checked: {i386}"
            );
        }
    }
}
