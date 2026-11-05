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
    {"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":4095,"args":[{"index":0,"value":8,"op":"SCMP_CMP_EQ"}]},
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
        (8, errno(4095)),
        (9, libc::SECCOMP_RET_KILL_THREAD),
    ] {
        assert_eq!(verdict(&p, g, a0(arg)), want, "{arg}");
    }
}

/// The calls the forked checks need, left out of [`long_profile`].
fn spared() -> [u32; 3] {
    [libc::SYS_exit, libc::SYS_exit_group, libc::SYS_getppid].map(nr)
}

/// Distinct errnos for nearly every call, `nr + 1`, and a chain of 60
/// rules on `getppid`, `1000 + arg0`: the search tree and the chain jump
/// further than 8 bits allow.
fn long_profile() -> Program {
    let mut rules: Vec<String> = syscalls::all()
        .filter(|(_, n)| !spared().contains(n))
        .map(|(name, n)| {
            format!(
                r#"{{"names":["{name}"],"action":"SCMP_ACT_ERRNO","errnoRet":{}}}"#,
                n + 1
            )
        })
        .collect();
    for i in 0..60 {
        rules.push(format!(
            r#"{{"names":["getppid"],"action":"SCMP_ACT_ERRNO","errnoRet":{},
            "args":[{{"index":0,"value":{i},"op":"SCMP_CMP_EQ"}}]}}"#,
            1000 + i
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
        let want = errno(i32::try_from(n + 1).unwrap());
        assert_eq!(verdict_on(&p, AUDIT_ARCH, n, [0; 6]), want, "{n}");
    }
    for i in 0..60u32 {
        let want = errno(i32::try_from(1000 + i).unwrap());
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
/// a `RUN` under it then reaches no keyring, and a refused join does not
/// fail it.
#[test]
fn keyrings_are_refused_only_when_no_call_can_reach_one() {
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
            "syscalls[0]: errno 65536 is above 4095",
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
            "defaultAction: errno 65536 is above 4095",
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

/// 512 (`ERESTARTSYS`) to 530, the kernel's restart codes and their
/// neighbours, are refused as an errno of a rule or of the default, even in
/// a rule another architecture keeps or in the tests' faking profiles; 511
/// and 531 compile (ADR-0109).
#[test]
fn errnos_512_to_530_are_refused() {
    let errno_rule = |e: u32, extra: &str| {
        format!(
            r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":["getppid"],
            "action":"SCMP_ACT_ERRNO","errnoRet":{e}{extra}}}]}}"#
        )
    };
    let default = |e: u32| format!(r#"{{"defaultAction":"SCMP_ACT_ERRNO","defaultErrnoRet":{e}}}"#);
    for e in [512, 513, 514, 516, 520, 530] {
        let m = refusal(&errno_rule(e, ""));
        assert!(
            m.starts_with(&format!("syscalls[0]: errnoRet {e} is")),
            "{m}"
        );
        assert!(m.contains("512 to 530"), "{m}");
        let m = refusal(&errno_rule(e, r#","includes":{"arches":["s390x"]}"#));
        assert!(m.starts_with("syscalls[0]: errnoRet"), "{m}");
        assert!(m.contains("512 to 530"), "{m}");
        let m = refusal(&default(e));
        assert!(m.starts_with(&format!("defaultErrnoRet: {e} is")), "{m}");
        let faking = Context {
            faked_success: true,
            ..cx((6, 8))
        };
        for json in [errno_rule(e, ""), default(e)] {
            match compile(json.as_bytes(), &faking) {
                Err(Error::SeccompProfile(m)) => assert!(m.contains("512 to 530"), "{m}"),
                Err(other) => panic!("{json}: {other}"),
                Ok(_) => panic!("{json}: accepted"),
            }
        }
    }
    for e in [511, 531] {
        let p = compiled(&errno_rule(e, ""));
        assert_eq!(
            verdict(&p, libc::SYS_getppid, a0(0)),
            errno(e.cast_signed())
        );
        let p = compiled(&default(e));
        assert_eq!(
            verdict(&p, libc::SYS_getppid, a0(0)),
            errno(e.cast_signed())
        );
    }
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
        let (_, names) = rule(r, "rule").unwrap();
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
                refused(getppid(a0(8)), 4095),
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
        let sample = [
            libc::SYS_read,
            libc::SYS_getpid,
            libc::SYS_getuid,
            libc::SYS_gettid,
            libc::SYS_sched_yield,
            libc::c_long::from(top),
        ];
        exited_ok(under(&p, || {
            let mut n = 0;
            for call in sample {
                n += 1;
                let want = i32::try_from(call + 1).unwrap_or(0);
                // SAFETY: refused by the filter before the kernel runs it.
                let r = unsafe { libc::syscall(call, 0, 0, 0, 0, 0, 0) };
                if !refused(r, want) {
                    return n;
                }
            }
            for (i, want) in [(0, 1000), (59, 1059)] {
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
}
