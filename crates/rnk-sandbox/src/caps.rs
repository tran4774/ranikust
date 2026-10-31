//! Capability sets: Docker's 14 defaults, kept for every `USER`.
//!
//! Capability numbers are the stable kernel ABI of `linux/capability.h`.

/// `(number, name)` of Docker's default capabilities, the only ones a `RUN`
/// keeps in its bounding, permitted, effective and inheritable sets.
pub(crate) const DOCKER_DEFAULT: [(u32, &str); 14] = [
    (0, "CHOWN"),
    (1, "DAC_OVERRIDE"),
    (3, "FOWNER"),
    (4, "FSETID"),
    (5, "KILL"),
    (6, "SETGID"),
    (7, "SETUID"),
    (8, "SETPCAP"),
    (10, "NET_BIND_SERVICE"),
    (13, "NET_RAW"),
    (18, "SYS_CHROOT"),
    (27, "MKNOD"),
    (29, "AUDIT_WRITE"),
    (31, "SETFCAP"),
];

/// The highest capability number this crate knows how to drop: the bounding
/// set is a 64-bit mask in the kernel ABI.
const MAX_CAP: u32 = 63;

/// The default set as a bit mask, bit `n` for capability `n`.
pub(crate) fn docker_default_mask() -> u64 {
    DOCKER_DEFAULT
        .iter()
        .fold(0, |mask, &(cap, _)| mask | (1u64 << cap))
}

/// Capabilities to drop from the bounding set: every capability up to
/// `last_cap` (from `/proc/sys/kernel/cap_last_cap`) that is not a default.
///
/// Returned as a fixed array plus a length, so the child can use it after
/// `clone` without allocating.
pub(crate) fn bounding_drops(last_cap: u32) -> ([u32; 64], usize) {
    let keep = docker_default_mask();
    let mut drops = [0u32; 64];
    let mut len = 0;
    for cap in 0..=last_cap.min(MAX_CAP) {
        if keep & (1u64 << cap) == 0 {
            drops[len] = cap;
            len += 1;
        }
    }
    (drops, len)
}

/// The kernel's highest capability number, read from
/// `/proc/sys/kernel/cap_last_cap`; `None` when it cannot be read.
pub(crate) fn kernel_last_cap() -> Option<u32> {
    let text = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap").ok()?;
    text.trim().parse().ok()
}

/// The highest capability the kernel knows, or the highest this crate knows
/// when the kernel does not say: dropping a capability the kernel lacks
/// fails with `EINVAL`, which the child treats as the end of the list.
pub(crate) fn last_cap() -> u32 {
    kernel_last_cap().map_or(MAX_CAP, |c| c.min(MAX_CAP))
}

/// Names of the capabilities Linux defines, by number.
const NAMES: [&str; 41] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "KILL",
    "SETGID",
    "SETUID",
    "SETPCAP",
    "LINUX_IMMUTABLE",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_ADMIN",
    "NET_RAW",
    "IPC_LOCK",
    "IPC_OWNER",
    "SYS_MODULE",
    "SYS_RAWIO",
    "SYS_CHROOT",
    "SYS_PTRACE",
    "SYS_PACCT",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_NICE",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "MKNOD",
    "LEASE",
    "AUDIT_WRITE",
    "AUDIT_CONTROL",
    "SETFCAP",
    "MAC_OVERRIDE",
    "MAC_ADMIN",
    "SYSLOG",
    "WAKE_ALARM",
    "BLOCK_SUSPEND",
    "AUDIT_READ",
    "PERFMON",
    "BPF",
    "CHECKPOINT_RESTORE",
];

/// `CAP_<NAME>` of capability `cap`, or `CAP_<n>` for one this crate does
/// not name.
pub(crate) fn name(cap: u32) -> String {
    usize::try_from(cap)
        .ok()
        .and_then(|i| NAMES.get(i))
        .map_or_else(|| format!("CAP_{cap}"), |n| format!("CAP_{n}"))
}

/// What a `Direct` `RUN` on `/` does with ranikust's bounding set
/// (ADR-0051).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DirectCaps {
    /// Nothing outside Docker's defaults: nothing to drop.
    Keep,
    /// The child drops every capability outside Docker's defaults.
    Drop,
    /// Capabilities outside Docker's defaults in the bounding set, and no
    /// `CAP_SETPCAP` to drop them: any of them may reach ranikust's state
    /// (`SYS_PTRACE` its memory, `SYS_ADMIN` a mount over `/proc`, `BPF` with
    /// `PERFMON` kernel memory, `SYS_MODULE`, `SYS_RAWIO`…), so the `RUN`
    /// does not run. Their names, for the message.
    Refuse(Vec<String>),
}

/// The policy for a bounding set `bounding` (bit `n` for capability `n`)
/// and whether ranikust may drop from it (`CAP_SETPCAP` effective).
pub(crate) fn direct_caps(bounding: u64, setpcap: bool) -> DirectCaps {
    let extras = bounding & !docker_default_mask();
    if extras == 0 {
        return DirectCaps::Keep;
    }
    if setpcap {
        return DirectCaps::Drop;
    }
    DirectCaps::Refuse(
        (0..=MAX_CAP)
            .filter(|&cap| extras & (1u64 << cap) != 0)
            .map(name)
            .collect(),
    )
}

/// This thread's bounding set up to `last_cap`, as a bit mask
/// (`PR_CAPBSET_READ`). A capability the kernel does not know reads as
/// absent.
pub(crate) fn bounding_set(last_cap: u32) -> u64 {
    let mut mask = 0u64;
    for cap in 0..=last_cap.min(MAX_CAP) {
        // SAFETY: `PR_CAPBSET_READ` reads one capability; no pointer.
        let r = unsafe { libc::prctl(libc::PR_CAPBSET_READ, libc::c_ulong::from(cap), 0, 0, 0) };
        if r == 1 {
            mask |= 1u64 << cap;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_defaults_are_fourteen_distinct_caps() {
        let mask = docker_default_mask();
        assert_eq!(mask.count_ones(), 14);
        // CAP_SYS_ADMIN (21), CAP_SYS_PTRACE (19), CAP_NET_ADMIN (12) and
        // CAP_DAC_READ_SEARCH (2) are not among them.
        for cap in [2, 12, 19, 21] {
            assert_eq!(mask & (1 << cap), 0, "cap {cap}");
        }
    }

    #[test]
    fn drops_are_the_complement_up_to_last_cap() {
        let (drops, len) = bounding_drops(40);
        let drops = &drops[..len];
        assert_eq!(len, 41 - 14);
        assert!(drops.contains(&21), "CAP_SYS_ADMIN is dropped");
        assert!(drops.contains(&40), "the last cap is dropped");
        assert!(!drops.contains(&18), "CAP_SYS_CHROOT is kept");
        assert!(drops.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn direct_caps_drop_extras_or_refuse_what_reaches_the_state() {
        let defaults = docker_default_mask();
        assert_eq!(direct_caps(defaults, false), DirectCaps::Keep);
        assert_eq!(direct_caps(defaults & !(1 << 8), false), DirectCaps::Keep);
        let ptrace = defaults | (1 << 19);
        assert_eq!(direct_caps(ptrace, true), DirectCaps::Drop);
        assert_eq!(
            direct_caps(ptrace | (1 << 2), false),
            DirectCaps::Refuse(vec![
                "CAP_DAC_READ_SEARCH".to_owned(),
                "CAP_SYS_PTRACE".to_owned()
            ])
        );
        // Any extra one without CAP_SETPCAP: SYS_ADMIN could mount over
        // /proc, NET_ADMIN is refused alike.
        let admin = defaults | (1 << 21) | (1 << 12);
        assert_eq!(
            direct_caps(admin, false),
            DirectCaps::Refuse(vec!["CAP_NET_ADMIN".to_owned(), "CAP_SYS_ADMIN".to_owned()])
        );
        assert_eq!(name(39), "CAP_BPF");
        assert_eq!(name(63), "CAP_63");
    }

    #[test]
    fn this_threads_bounding_set_reads() {
        // Whatever the test runs with, the read works and stays below the
        // kernel's last capability.
        let last = last_cap();
        let set = bounding_set(last);
        assert_eq!(set.checked_shr(last + 1).unwrap_or(0), 0);
    }

    #[test]
    fn drops_are_capped_at_the_abi_limit() {
        let (_, len) = bounding_drops(u32::MAX);
        assert_eq!(len, 64 - 14);
    }
}
