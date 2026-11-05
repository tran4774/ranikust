// Derived in part from moby/profiles, https://github.com/moby/profiles,
// seccomp/seccomp_linux.go and seccomp/kernel_linux.go at v0.2.4.
// Copyright The Moby Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: compiled to classic BPF here instead of libseccomp.

//! Docker's JSON seccomp profiles, compiled to one classic BPF filter for
//! the architecture ranikust is built for (ADR-0003, ADR-0103, ADR-0109).
//!
//! What a profile keeps follows moby's `setupSeccomp`: a rule's `includes`
//! and `excludes` by architecture (Docker's name, `amd64` or `arm64`),
//! capability (the `RUN`'s bounding set, Docker's 14) and kernel version.
//! What the filter does follows libseccomp: arguments compare as 64-bit
//! unsigned values, a rule matches when all its conditions hold, and a call
//! no rule matches gets the default action. Where rules of different
//! actions match one call, the most restrictive wins, as between stacked
//! filters; two of the same kind keep the profile's order.
//!
//! Only the native architecture is compiled. A call of another (i386 or x32
//! on x86_64, arm on aarch64) gets `EPERM`: its numbers differ, and `libc`
//! names only the native ones (ADR-0009). Under a default action that
//! refuses, a number `libc` does not name gets `ENOSYS`, so that the C
//! libraries fall back from a call newer than the table, as under runc.
//!
//! Refused before any `RUN` (ADR-0109): `SCMP_ACT_NOTIFY`, `SCMP_ACT_TRACE`,
//! a listener, a flag other than `TSYNC`, `LOG` and `SPEC_ALLOW`, a name
//! unknown here under an action other than `SCMP_ACT_ALLOW`, two conditions
//! on one argument in a rule, an `SCMP_ACT_ERRNO` whose errno is 0 (a faked
//! success), one of the kernel's own codes, 512 to 530, or above 4095, and
//! anything malformed.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use super::asm::{Asm, AsmError, JEQ_K, JGE_K, JGT_K, Label, MAX_INSNS};
use super::{Program, syscalls};
use crate::caps;
use crate::error::{Error, Result};

/// moby/profiles seccomp v0.2.4's `default.json`, as BuildKit `cd8ee24`
/// vendors it: Docker's and BuildKit's default.
const DOCKER_DEFAULT: &[u8] = include_bytes!("default.json");

/// The kernel caps an errno at `MAX_ERRNO`.
const MAX_ERRNO: u32 = 4095;

/// The kernel's internal codes (`include/linux/errno.h`), refused as a
/// profile's errno.
const KERNEL_ERRNOS: std::ops::RangeInclusive<u32> = 512..=530;

/// The architectures runtime-spec names; only the native one is compiled.
const ARCHES: [&str; 23] = [
    "SCMP_ARCH_X86",
    "SCMP_ARCH_X86_64",
    "SCMP_ARCH_X32",
    "SCMP_ARCH_ARM",
    "SCMP_ARCH_AARCH64",
    "SCMP_ARCH_MIPS",
    "SCMP_ARCH_MIPS64",
    "SCMP_ARCH_MIPS64N32",
    "SCMP_ARCH_MIPSEL",
    "SCMP_ARCH_MIPSEL64",
    "SCMP_ARCH_MIPSEL64N32",
    "SCMP_ARCH_PPC",
    "SCMP_ARCH_PPC64",
    "SCMP_ARCH_PPC64LE",
    "SCMP_ARCH_S390",
    "SCMP_ARCH_S390X",
    "SCMP_ARCH_PARISC",
    "SCMP_ARCH_PARISC64",
    "SCMP_ARCH_RISCV64",
    "SCMP_ARCH_LOONGARCH64",
    "SCMP_ARCH_M68K",
    "SCMP_ARCH_SH",
    "SCMP_ARCH_SHEB",
];

/// A seccomp profile compiled for this architecture, for
/// [`Sandbox::set_seccomp`](crate::Sandbox::set_seccomp): Docker's default
/// or a profile file in Docker's JSON format (ADR-0109).
#[derive(Clone)]
pub struct SeccompProfile {
    pub(crate) program: Program,
    /// The filter refuses `keyctl`, `add_key` and `request_key` whatever
    /// their arguments: a `RUN` under it reaches no keyring.
    refuses_keyrings: bool,
}

impl SeccompProfile {
    /// Docker's default profile (moby/profiles seccomp v0.2.4), what a
    /// [`Sandbox::start`](crate::Sandbox::start) sandbox applies until told
    /// otherwise.
    ///
    /// # Errors
    ///
    /// [`Error::SeccompProfile`] on an architecture other than x86_64 and
    /// aarch64, or when the kernel version cannot be read.
    pub fn docker_default() -> Result<Self> {
        Self::from_json(DOCKER_DEFAULT)
    }

    /// Compiles a profile in Docker's JSON format, as `docker run
    /// --security-opt seccomp=<file>` reads it, for this architecture, the
    /// running kernel and the `RUN`'s capabilities.
    ///
    /// # Errors
    ///
    /// [`Error::SeccompProfile`] when the profile is malformed or asks for
    /// what ranikust does not support (see ADR-0109), or as
    /// [`SeccompProfile::docker_default`].
    pub fn from_json(json: &[u8]) -> Result<Self> {
        compile(json, &Context::live()?)
    }

    /// As [`Self::from_json`], but errno 0, a faked success, compiles: the
    /// tests stand for a filter around ranikust with it, which ranikust
    /// does not control.
    #[cfg(test)]
    pub(crate) fn faking(json: &[u8]) -> Result<Self> {
        compile(
            json,
            &Context {
                faked_success: true,
                ..Context::live()?
            },
        )
    }

    /// Whether the profile refuses `keyctl`, `add_key` and `request_key`
    /// whatever their arguments, with any errno or a kill: then a `RUN`
    /// under it reaches no keyring, ranikust's included (ADR-0109).
    pub(crate) fn refuses_keyrings(&self) -> bool {
        self.refuses_keyrings
    }
}

impl fmt::Debug for SeccompProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SeccompProfile")
            .field("instructions", &self.program.insns.len())
            .field("flags", &self.program.flags)
            .field("refuses_keyrings", &self.refuses_keyrings)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Json {
    default_action: Option<String>,
    default_errno_ret: Option<u64>,
    architectures: Option<Vec<String>>,
    arch_map: Option<Vec<ArchJson>>,
    flags: Option<Vec<String>>,
    listener_path: Option<String>,
    listener_metadata: Option<String>,
    syscalls: Option<Vec<RuleJson>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ArchJson {
    architecture: String,
    sub_architectures: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RuleJson {
    names: Option<Vec<String>>,
    name: Option<String>,
    action: String,
    errno_ret: Option<u64>,
    args: Option<Vec<ArgJson>>,
    #[serde(rename = "comment")]
    _comment: Option<String>,
    includes: Option<FilterJson>,
    excludes: Option<FilterJson>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ArgJson {
    index: u64,
    value: u64,
    value_two: Option<u64>,
    op: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FilterJson {
    caps: Option<Vec<String>>,
    arches: Option<Vec<String>>,
    min_kernel: Option<String>,
}

/// What a profile is compiled for.
struct Context {
    /// Docker's name of the architecture (`amd64`, `arm64`).
    arch: &'static str,
    /// The kernel's version and major revision, read as moby reads them.
    kernel: (u64, u64),
    /// Whether an errno of 0 compiles: the tests' filters only.
    #[cfg(test)]
    faked_success: bool,
}

impl Context {
    fn live() -> Result<Self> {
        let uts = rustix::system::uname();
        let release = uts.release().to_string_lossy();
        let kernel = parse_release(&release)
            .ok_or_else(|| bad(format!("cannot read the kernel version {release:?}")))?;
        Ok(Self {
            arch: native_arch()?,
            kernel,
            #[cfg(test)]
            faked_success: false,
        })
    }

    /// Whether errno 0, a faked success, compiles: in the tests only, so no
    /// build of ranikust holds the switch.
    #[cfg(test)]
    fn fakes(&self) -> bool {
        self.faked_success
    }

    #[cfg(not(test))]
    fn fakes(&self) -> bool {
        false
    }

    /// Whether moby keeps `rule` here: no `excludes` applies and every
    /// `includes` does.
    fn keeps(&self, rule: &RuleJson) -> Result<bool> {
        let min = |f: &Option<FilterJson>| {
            f.as_ref()
                .and_then(|f| f.min_kernel.as_deref())
                .map(min_kernel)
                .transpose()
        };
        let (ex_min, in_min) = (min(&rule.excludes)?, min(&rule.includes)?);
        let at_least = |v: (u64, u64)| self.kernel >= v;
        let excluded = rule.excludes.as_ref().is_some_and(|ex| {
            ex.arches.iter().flatten().any(|a| a == self.arch)
                || ex.caps.iter().flatten().any(|c| has_cap(c))
                || ex_min.is_some_and(at_least)
        });
        let included = rule.includes.as_ref().is_none_or(|inc| {
            let arches = inc.arches.as_deref().unwrap_or_default();
            (arches.is_empty() || arches.iter().any(|a| a == self.arch))
                && inc.caps.iter().flatten().all(|c| has_cap(c))
                && in_min.is_none_or(at_least)
        });
        Ok(!excluded && included)
    }
}

#[cfg(target_arch = "x86_64")]
fn native_arch() -> Result<&'static str> {
    Ok("amd64")
}

#[cfg(target_arch = "aarch64")]
fn native_arch() -> Result<&'static str> {
    Ok("arm64")
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn native_arch() -> Result<&'static str> {
    Err(bad("no seccomp profile on this architecture (ADR-0009)"))
}

/// Whether the `RUN`'s bounding set, Docker's defaults, holds `name`
/// (`CAP_CHOWN`).
fn has_cap(name: &str) -> bool {
    name.strip_prefix("CAP_")
        .is_some_and(|n| caps::DOCKER_DEFAULT.iter().any(|&(_, c)| c == n))
}

/// `"<kernel>.<major>"` of a release (`6.8.0-45-generic`, `3.12-1-amd64`),
/// as moby's `fmt.Sscanf(release, "%d.%d")` reads it.
fn parse_release(release: &str) -> Option<(u64, u64)> {
    let (kernel, rest) = release.split_once('.')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let major = rest.get(..digits)?;
    if kernel.is_empty() || !kernel.bytes().all(|b| b.is_ascii_digit()) || major.is_empty() {
        return None;
    }
    Some((kernel.parse().ok()?, major.parse().ok()?))
}

/// A `minKernel`, `"<kernel>.<major>"` with each below 256 and not `0.0`,
/// as moby parses it; `""` is `0.0`, which every kernel reaches.
fn min_kernel(v: &str) -> Result<(u64, u64)> {
    if v.is_empty() {
        return Ok((0, 0));
    }
    let wrong = || bad(format!("minKernel {v:?}: expected \"<kernel>.<major>\""));
    let (k, m) = v.split_once('.').ok_or_else(wrong)?;
    let part = |p: &str| {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u8>().ok())
            .flatten()
            .map(u64::from)
            .ok_or_else(wrong)
    };
    let (k, m) = (part(k)?, part(m)?);
    if (k, m) == (0, 0) {
        return Err(wrong());
    }
    Ok((k, m))
}

/// What a matching rule, or the default, makes of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    KillProcess,
    KillThread,
    Trap,
    Errno(u32),
    Log,
    Allow,
}

impl Action {
    fn ret(self) -> u32 {
        match self {
            Self::KillProcess => libc::SECCOMP_RET_KILL_PROCESS,
            Self::KillThread => libc::SECCOMP_RET_KILL_THREAD,
            Self::Trap => libc::SECCOMP_RET_TRAP,
            Self::Errno(e) => libc::SECCOMP_RET_ERRNO | (e & libc::SECCOMP_RET_DATA),
            Self::Log => libc::SECCOMP_RET_LOG,
            Self::Allow => libc::SECCOMP_RET_ALLOW,
        }
    }

    /// Whether the call runs: allowed, logged or not.
    fn runs(self) -> bool {
        match self {
            Self::Allow | Self::Log => true,
            Self::KillProcess | Self::KillThread | Self::Trap | Self::Errno(_) => false,
        }
    }

    /// Lower first: the kernel's precedence between stacked filters.
    fn rank(self) -> u8 {
        match self {
            Self::KillProcess => 0,
            Self::KillThread => 1,
            Self::Trap => 2,
            Self::Errno(_) => 3,
            Self::Log => 4,
            Self::Allow => 5,
        }
    }
}

fn action(name: &str, errno: Option<u64>, what: &str) -> Result<Action> {
    let act = match name {
        "SCMP_ACT_KILL" | "SCMP_ACT_KILL_THREAD" => Action::KillThread,
        "SCMP_ACT_KILL_PROCESS" => Action::KillProcess,
        "SCMP_ACT_TRAP" => Action::Trap,
        "SCMP_ACT_ERRNO" => {
            return match errno {
                None => Ok(Action::Errno(libc::EPERM.unsigned_abs())),
                Some(e) => u32::try_from(e)
                    .ok()
                    .filter(|&e| e <= MAX_ERRNO)
                    .map(Action::Errno)
                    .ok_or_else(|| bad(format!("{what}: errno {e} is above {MAX_ERRNO}"))),
            };
        }
        "SCMP_ACT_LOG" => Action::Log,
        "SCMP_ACT_ALLOW" => Action::Allow,
        "SCMP_ACT_TRACE" | "SCMP_ACT_NOTIFY" => {
            return Err(bad(format!("{what}: {name} is not supported")));
        }
        other => return Err(bad(format!("{what}: unknown action {other:?}"))),
    };
    if errno.is_some() {
        return Err(bad(format!(
            "{what}: an errno goes with SCMP_ACT_ERRNO only"
        )));
    }
    Ok(act)
}

fn flags(list: &[String]) -> Result<libc::c_ulong> {
    list.iter().try_fold(0, |acc, f| {
        let flag = match f.as_str() {
            "SECCOMP_FILTER_FLAG_TSYNC" => libc::SECCOMP_FILTER_FLAG_TSYNC,
            "SECCOMP_FILTER_FLAG_LOG" => libc::SECCOMP_FILTER_FLAG_LOG,
            "SECCOMP_FILTER_FLAG_SPEC_ALLOW" => libc::SECCOMP_FILTER_FLAG_SPEC_ALLOW,
            other => return Err(bad(format!("flags: {other:?} is not supported"))),
        };
        Ok(acc | flag)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Ne,
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    MaskedEq,
}

/// `args[index] <op> value`; `MaskedEq` is `args[index] & value == value_two`.
#[derive(Clone, Copy, Debug)]
struct Cond {
    index: u32,
    op: Cmp,
    value: u64,
    value_two: u64,
}

#[derive(Clone, Debug)]
struct Rule {
    action: Action,
    conds: Vec<Cond>,
}

fn cond(a: &ArgJson, what: &str) -> Result<Cond> {
    let index = u32::try_from(a.index)
        .ok()
        .filter(|&i| i < 6)
        .ok_or_else(|| bad(format!("{what}: argument index {} is above 5", a.index)))?;
    let op = match a.op.as_str() {
        "SCMP_CMP_NE" => Cmp::Ne,
        "SCMP_CMP_LT" => Cmp::Lt,
        "SCMP_CMP_LE" => Cmp::Le,
        "SCMP_CMP_EQ" => Cmp::Eq,
        "SCMP_CMP_GE" => Cmp::Ge,
        "SCMP_CMP_GT" => Cmp::Gt,
        "SCMP_CMP_MASKED_EQ" => Cmp::MaskedEq,
        other => return Err(bad(format!("{what}: unknown operator {other:?}"))),
    };
    Ok(Cond {
        index,
        op,
        value: a.value,
        value_two: a.value_two.unwrap_or(0),
    })
}

/// The rule and the names it applies to, checked whatever `includes` and
/// `excludes` say, so that a profile is valid or not on every machine.
fn rule(r: &RuleJson, what: &str) -> Result<(Rule, Vec<String>)> {
    let names = match (&r.name, &r.names) {
        (Some(_), Some(_)) => {
            return Err(bad(format!("{what}: both name and names")));
        }
        (Some(n), None) => vec![n.clone()],
        (None, Some(n)) => n.clone(),
        (None, None) => Vec::new(),
    };
    if names.is_empty() {
        return Err(bad(format!("{what}: names no system call")));
    }
    let action = action(&r.action, r.errno_ret, what)?;
    let conds = r
        .args
        .iter()
        .flatten()
        .map(|a| cond(a, what))
        .collect::<Result<Vec<_>>>()?;
    let mut seen = 0u8;
    for c in &conds {
        let bit = 1u8 << c.index;
        if seen & bit != 0 {
            return Err(bad(format!(
                "{what}: two conditions on argument {}",
                c.index
            )));
        }
        seen |= bit;
    }
    Ok((Rule { action, conds }, names))
}

fn bad(msg: impl Into<String>) -> Error {
    Error::SeccompProfile(msg.into())
}

/// The calls that reach a keyring: the profile must refuse all three for a
/// `RUN` to reach none.
const KEYRING_CALLS: [&str; 3] = ["keyctl", "add_key", "request_key"];

/// Whether a call with these `rules` never runs, whatever its arguments: a
/// rule without conditions refuses it, which wins over any other, or the
/// default refuses it and no rule lets it run.
fn refused_always(rules: &[Rule], default: Action) -> bool {
    rules.iter().any(|r| r.conds.is_empty() && !r.action.runs())
        || (!default.runs() && !rules.iter().any(|r| r.action.runs()))
}

/// Why a profile may not answer with `act`'s errno, if it may not
/// (ADR-0109): 0 fakes a success the set-up after the profile would trust,
/// and the kernel's own codes, 512 (`ERESTARTSYS`) to 530, are no answer:
/// on 516 (`ERESTART_RESTARTBLOCK`) the kernel may run its restart block
/// instead, which can succeed.
fn errno_refused(act: Action, cx: &Context) -> Option<String> {
    const GIVE: &str = "give an errno from 1 to 511 or 531 to 4095";
    match act {
        Action::Errno(0) if !cx.fakes() => Some(format!(
            "0 would fake the success of the calls it refuses; {GIVE}"
        )),
        Action::Errno(e) if KERNEL_ERRNOS.contains(&e) => Some(format!(
            "{e} is one of the kernel's own codes (512 to 530), which can \
             restart the call; {GIVE}"
        )),
        _ => None,
    }
}

fn compile(json: &[u8], cx: &Context) -> Result<SeccompProfile> {
    let p: Json = serde_json::from_slice(json).map_err(|e| bad(e.to_string()))?;
    let default = action(
        p.default_action
            .as_deref()
            .ok_or_else(|| bad("defaultAction is missing"))?,
        p.default_errno_ret,
        "defaultAction",
    )?;
    if let Some(why) = errno_refused(default, cx) {
        return Err(bad(format!("defaultErrnoRet: {why}")));
    }
    if p.architectures.as_ref().is_some_and(|a| !a.is_empty())
        && p.arch_map.as_ref().is_some_and(|m| !m.is_empty())
    {
        return Err(bad("both architectures and archMap"));
    }
    let arches = p
        .architectures
        .iter()
        .flatten()
        .chain(p.arch_map.iter().flatten().flat_map(|m| {
            std::iter::once(&m.architecture).chain(m.sub_architectures.iter().flatten())
        }));
    for a in arches {
        if !ARCHES.contains(&a.as_str()) {
            return Err(bad(format!("unknown architecture {a:?}")));
        }
    }
    let flags = flags(p.flags.as_deref().unwrap_or_default())?;
    if p.listener_path.as_deref().is_some_and(|l| !l.is_empty())
        || p.listener_metadata
            .as_deref()
            .is_some_and(|l| !l.is_empty())
    {
        return Err(bad("a listener is not supported"));
    }

    let mut calls: BTreeMap<u32, Vec<Rule>> = BTreeMap::new();
    for (i, r) in p.syscalls.iter().flatten().enumerate() {
        let what = format!("syscalls[{i}]");
        let (rule, names) = rule(r, &what)?;
        if let Some(why) = errno_refused(rule.action, cx) {
            return Err(bad(format!("{what}: errnoRet {why}")));
        }
        if !cx.keeps(r)? {
            continue;
        }
        for name in names {
            match (syscalls::number(&name), rule.action) {
                (Some(nr), _) => calls.entry(nr).or_default().push(rule.clone()),
                (None, Action::Allow) => {}
                (
                    None,
                    Action::KillProcess
                    | Action::KillThread
                    | Action::Trap
                    | Action::Errno(_)
                    | Action::Log,
                ) => {
                    return Err(bad(format!(
                        "{what}: {name:?} is not a system call of this architecture"
                    )));
                }
            }
        }
    }
    for rules in calls.values_mut() {
        rules.sort_by_key(|r| r.action.rank());
    }
    // A call `libc` does not number counts as reachable.
    let refuses_keyrings = KEYRING_CALLS.iter().all(|name| {
        syscalls::number(name)
            .is_some_and(|nr| refused_always(calls.get(&nr).map_or(&[], Vec::as_slice), default))
    });
    let insns = emit(&calls, default).map_err(|e| match e {
        AsmError::TooLong(n) => bad(format!(
            "the profile compiles to {n} instructions, more than the kernel's {MAX_INSNS}"
        )),
        AsmError::Label => bad("internal error: a jump of the filter is wrong"),
    })?;
    Ok(SeccompProfile {
        program: Program {
            insns: insns.into(),
            flags,
        },
        refuses_keyrings,
    })
}

/// What one interval of system call numbers gets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leaf {
    Ret(u32),
    /// The rules of that system call.
    Rules(u32),
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn emit(
    calls: &BTreeMap<u32, Vec<Rule>>,
    default: Action,
) -> Result<Vec<libc::sock_filter>, AsmError> {
    use super::{ARCH, AUDIT_ARCH, NR};

    let unknown = match default {
        Action::Allow | Action::Log => default,
        Action::KillProcess | Action::KillThread | Action::Trap | Action::Errno(_) => {
            Action::Errno(libc::ENOSYS.unsigned_abs())
        }
    };
    // Numbers at or above the x32 bit never reach the tree.
    #[cfg(target_arch = "x86_64")]
    let top = super::X32_SYSCALL_BIT - 1;
    #[cfg(target_arch = "aarch64")]
    let top = u32::MAX;

    // Intervals as their first number, in order, from 0 to `top`.
    let mut known: Vec<u32> = syscalls::all()
        .map(|(_, nr)| nr)
        .filter(|&n| n <= top)
        .collect();
    known.sort_unstable();
    known.dedup();
    let mut iv: Vec<(u32, Leaf)> = Vec::new();
    let mut push = |lo: u32, leaf: Leaf| {
        if iv.last().map(|&(_, l)| l) != Some(leaf) {
            iv.push((lo, leaf));
        }
    };
    let mut next = Some(0u32);
    for nr in known {
        if let Some(n) = next
            && n < nr
        {
            push(n, Leaf::Ret(unknown.ret()));
        }
        let leaf = match calls.get(&nr).and_then(|r| r.first()) {
            None => Leaf::Ret(default.ret()),
            Some(first) if first.conds.is_empty() => Leaf::Ret(first.action.ret()),
            Some(_) => Leaf::Rules(nr),
        };
        push(nr, leaf);
        next = nr.checked_add(1);
    }
    if let Some(n) = next.filter(|&n| n <= top) {
        push(n, Leaf::Ret(unknown.ret()));
    }

    let mut a = Asm::default();
    let [native, bad_arch] = [a.label(), a.label()];
    a.ld(ARCH);
    a.jump(JEQ_K, AUDIT_ARCH, native, bad_arch);
    a.bind(native);
    a.ld(NR);
    #[cfg(target_arch = "x86_64")]
    a.on(JGE_K, super::X32_SYSCALL_BIT, bad_arch);
    tree(&mut a, &iv, calls, default, unknown);
    a.bind(bad_arch);
    a.ret(Action::Errno(libc::EPERM.unsigned_abs()).ret());
    a.finish()
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn emit(
    _calls: &BTreeMap<u32, Vec<Rule>>,
    _default: Action,
) -> Result<Vec<libc::sock_filter>, AsmError> {
    Err(AsmError::Label)
}

/// A binary search on the number in `A` down to each interval's leaf.
fn tree(
    a: &mut Asm,
    iv: &[(u32, Leaf)],
    calls: &BTreeMap<u32, Vec<Rule>>,
    default: Action,
    unknown: Action,
) {
    let (left, right) = iv.split_at(iv.len() / 2);
    match (left, right) {
        (_, []) => a.ret(unknown.ret()),
        ([], [(_, leaf)]) => match *leaf {
            Leaf::Ret(k) => a.ret(k),
            Leaf::Rules(nr) => chain(a, calls.get(&nr).map_or(&[][..], Vec::as_slice), default),
        },
        (_, [(lo, _), ..]) => {
            let upper = a.label();
            a.on(JGE_K, *lo, upper);
            tree(a, left, calls, default, unknown);
            a.bind(upper);
            tree(a, right, calls, default, unknown);
        }
    }
}

/// The rules of one call, most restrictive first, then the default.
fn chain(a: &mut Asm, rules: &[Rule], default: Action) {
    for r in rules {
        let next = a.label();
        for c in &r.conds {
            test(a, c, next);
        }
        a.ret(r.action.ret());
        a.bind(next);
        if r.conds.is_empty() {
            return;
        }
    }
    a.ret(default.ret());
}

/// Falls through when `c` holds, goes to `no` otherwise. Each 64-bit
/// argument is two words: the high one decides unless equal.
fn test(a: &mut Asm, c: &Cond, no: Label) {
    let base = super::arg(c.index);
    let (hi_at, lo_at) = if cfg!(target_endian = "little") {
        (base + 4, base)
    } else {
        (base, base + 4)
    };
    let (hi, lo) = halves(c.value);
    let yes = a.label();
    let low = a.label();
    match c.op {
        Cmp::Eq => {
            a.ld(hi_at);
            a.unless(JEQ_K, hi, no);
            a.ld(lo_at);
            a.unless(JEQ_K, lo, no);
        }
        Cmp::Ne => {
            a.ld(hi_at);
            a.jump(JEQ_K, hi, low, yes);
            a.bind(low);
            a.ld(lo_at);
            a.jump(JEQ_K, lo, no, yes);
        }
        Cmp::Gt | Cmp::Ge => {
            a.ld(hi_at);
            a.on(JGT_K, hi, yes);
            a.unless(JEQ_K, hi, no);
            a.ld(lo_at);
            let code = if c.op == Cmp::Gt { JGT_K } else { JGE_K };
            a.unless(code, lo, no);
        }
        Cmp::Lt | Cmp::Le => {
            a.ld(hi_at);
            a.on(JGT_K, hi, no);
            a.unless(JEQ_K, hi, yes);
            a.ld(lo_at);
            // `x < v` is `!(x >= v)`, `x <= v` is `!(x > v)`.
            let code = if c.op == Cmp::Lt { JGE_K } else { JGT_K };
            a.on(code, lo, no);
        }
        Cmp::MaskedEq => {
            let (want_hi, want_lo) = halves(c.value_two);
            a.ld(hi_at);
            a.and(hi);
            a.unless(JEQ_K, want_hi, no);
            a.ld(lo_at);
            a.and(lo);
            a.unless(JEQ_K, want_lo, no);
        }
    }
    a.bind(yes);
}

/// The high and low 32 bits.
fn halves(v: u64) -> (u32, u32) {
    let [h0, h1, h2, h3, l0, l1, l2, l3] = v.to_be_bytes();
    (
        u32::from_be_bytes([h0, h1, h2, h3]),
        u32::from_be_bytes([l0, l1, l2, l3]),
    )
}

#[cfg(test)]
mod tests;
