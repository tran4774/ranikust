//! Start-up check that the kernel gives [`RootPath`](crate::RootPath) what it
//! needs (ADR-0028).

use std::fmt;
use std::io;

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags, StatxFlags};
use rustix::io::Errno;

/// Why the kernel cannot confine paths the way `rnk-rootfs` needs.
#[derive(Debug)]
pub struct Unsupported {
    /// The syscall or interface that failed, for example `openat2`.
    pub feature: &'static str,
    /// Whether the kernel lacks the feature or a filter blocks it.
    pub cause: Cause,
    /// The running kernel release (`uname -r`), for the message.
    pub kernel: String,
}

/// The two reasons a required feature is unavailable.
#[derive(Debug)]
pub enum Cause {
    /// The kernel does not have it: `ENOSYS`, or `statx` without the mount
    /// id. Linux 5.8 or later is required.
    Missing(io::Error),
    /// The kernel has it, but a seccomp filter refuses it (`EPERM`).
    Blocked(io::Error),
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            Cause::Missing(err) => write!(
                f,
                "this kernel ({}) lacks {} ({err}); ranikust needs Linux 5.8 or later",
                self.kernel, self.feature
            ),
            Cause::Blocked(err) => write!(
                f,
                "{} is blocked on this kernel ({}): {err}; a seccomp profile must allow \
                 openat2 and statx (Docker's default profile does since Docker 20.10)",
                self.feature, self.kernel
            ),
        }
    }
}

impl std::error::Error for Unsupported {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            Cause::Missing(err) | Cause::Blocked(err) => Some(err),
        }
    }
}

/// Checks, by calling them, that `openat2` with `RESOLVE_IN_ROOT` and the
/// `statx` mount id work. Tests the calls, not the version string: a
/// backported kernel may have them, a seccomp filter may block them.
///
/// Opens `/` as an `O_PATH` directory, resolves `.` inside it with
/// `RESOLVE_IN_ROOT`, and asks `statx` for the mount id. Reads nothing and
/// writes nothing.
///
/// # Errors
///
/// [`Unsupported`] naming the feature, whether it is missing or blocked, and
/// the running kernel. Any other failure of these calls on `/` is reported
/// as missing, with the kernel's answer attached.
pub fn probe_support() -> Result<(), Unsupported> {
    probe().map_err(|(feature, cause)| Unsupported {
        feature,
        cause,
        kernel: kernel_release(),
    })
}

fn probe() -> Result<(), (&'static str, Cause)> {
    let root = rustix::fs::openat2(
        rustix::fs::CWD,
        "/",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )
    .map_err(|e| ("openat2", cause(e)))?;
    rustix::fs::openat2(
        &root,
        ".",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| ("openat2 RESOLVE_IN_ROOT", cause(e)))?;
    let st = rustix::fs::statx(
        &root,
        "",
        AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
    )
    .map_err(|e| ("statx", cause(e)))?;
    if !StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::MNT_ID) {
        return Err((
            "the statx mount id (STATX_MNT_ID)",
            Cause::Missing(io::Error::from(io::ErrorKind::Unsupported)),
        ));
    }
    Ok(())
}

/// `EPERM` on an `O_PATH` open of `/` or on `statx` checks no permission, so
/// it comes from a seccomp filter; everything else means the call is missing.
fn cause(e: Errno) -> Cause {
    if e == Errno::PERM {
        Cause::Blocked(e.into())
    } else {
        Cause::Missing(e.into())
    }
}

fn kernel_release() -> String {
    rustix::system::uname()
        .release()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_kernel_is_supported() {
        // The development and CI kernels are 5.8 or later.
        probe_support().expect("openat2 and the statx mount id work here");
    }

    #[test]
    fn eperm_is_blocked_and_the_rest_is_missing() {
        assert!(matches!(cause(Errno::PERM), Cause::Blocked(_)));
        assert!(matches!(cause(Errno::NOSYS), Cause::Missing(_)));
        assert!(matches!(cause(Errno::INVAL), Cause::Missing(_)));
    }

    #[test]
    fn messages_name_the_kernel_and_the_fix() {
        let missing = Unsupported {
            feature: "openat2",
            cause: Cause::Missing(Errno::NOSYS.into()),
            kernel: "4.18.0-553.el8".to_owned(),
        };
        let text = missing.to_string();
        assert!(text.contains("4.18.0-553.el8"), "{text}");
        assert!(text.contains("Linux 5.8 or later"), "{text}");

        let blocked = Unsupported {
            feature: "openat2",
            cause: Cause::Blocked(Errno::PERM.into()),
            kernel: "6.8.0".to_owned(),
        };
        let text = blocked.to_string();
        assert!(text.contains("seccomp"), "{text}");
        assert!(text.contains("6.8.0"), "{text}");
    }
}
