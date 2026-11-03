//! Checks that run before any other work.

use std::io;

use rnk_rootfs::{Cause, Unsupported};

/// The kernel check of ADR-0028: `openat2` with `RESOLVE_IN_ROOT` and the
/// `statx` mount id must work.
///
/// `fault` is the hidden test hook's value: `kernel-missing`,
/// `kernel-blocked` and `kernel-hidden` simulate an old kernel, a seccomp
/// filter and an emulator without `openat2` (Rosetta), so the tests exercise
/// the refusal on any kernel.
///
/// # Errors
///
/// The probe's [`Unsupported`], whose message names the kernel and the fix.
pub(crate) fn check_kernel(fault: Option<&str>) -> Result<(), Unsupported> {
    match fault {
        Some("kernel-missing") => Err(Unsupported {
            feature: "openat2",
            cause: Cause::Missing(io::Error::from(io::ErrorKind::Unsupported)),
            kernel: "4.18.0-simulated".to_owned(),
        }),
        Some("kernel-blocked") => Err(Unsupported {
            feature: "openat2",
            cause: Cause::Blocked(io::Error::from(io::ErrorKind::PermissionDenied)),
            kernel: "6.8.0-simulated".to_owned(),
        }),
        Some("kernel-hidden") => Err(Unsupported {
            feature: "openat2",
            cause: Cause::Hidden(rustix::io::Errno::NOSYS.into()),
            kernel: "7.2.12-simulated".to_owned(),
        }),
        _ => rnk_rootfs::probe_support(),
    }
}
