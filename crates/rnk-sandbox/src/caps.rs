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
    fn drops_are_capped_at_the_abi_limit() {
        let (_, len) = bounding_drops(u32::MAX);
        assert_eq!(len, 64 - 14);
    }
}
