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
    /// Whether the kernel lacks the feature, a filter blocks it, or something
    /// in between hides it.
    pub cause: Cause,
    /// The running kernel release (`uname -r`), for the message.
    pub kernel: String,
}

/// The reasons a required feature is unavailable.
#[derive(Debug)]
#[non_exhaustive]
pub enum Cause {
    /// The kernel does not have it: `ENOSYS`, or `statx` without the mount
    /// id, on a release before 5.8 (or one that does not parse). Linux 5.8
    /// or later is required.
    Missing(io::Error),
    /// The kernel has it, but a seccomp filter refuses it (`EPERM`).
    Blocked(io::Error),
    /// `ENOSYS`, or `statx` without the mount id, although the kernel is
    /// Linux 5.8 or later: something between ranikust and the kernel lacks
    /// or refuses the call, such as Rosetta running a binary built for
    /// another architecture, or a seccomp filter that answers `ENOSYS`.
    Hidden(io::Error),
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
            Cause::Hidden(err) => write!(
                f,
                "{} is unavailable ({err}) although this kernel ({}) is Linux 5.8 or later: \
                 something between ranikust and the kernel lacks or refuses it, either an \
                 emulator (Rosetta, or QEMU user mode before 9.1, running a binary built for \
                 another architecture, as in an amd64 container on Apple Silicon) or a seccomp \
                 filter that answers ENOSYS; run ranikust built for the machine's own \
                 architecture (user guide: \"An amd64 container on Apple Silicon\"), or allow \
                 openat2 and statx",
                self.feature, self.kernel
            ),
        }
    }
}

impl std::error::Error for Unsupported {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            Cause::Missing(err) | Cause::Blocked(err) | Cause::Hidden(err) => Some(err),
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
/// [`Unsupported`] naming the feature, whether it is missing, blocked or
/// hidden, and the running kernel. Any other failure of these calls on `/`
/// is reported as missing, with the kernel's answer attached.
pub fn probe_support() -> Result<(), Unsupported> {
    probe().map_err(|(feature, failure)| {
        // `uname` only once a call failed: the release is for the message.
        let kernel = kernel_release();
        Unsupported {
            feature,
            cause: cause(failure, &kernel),
            kernel,
        }
    })
}

/// What a failed probe call answered.
enum Failure {
    /// The call failed with this error.
    Errno(Errno),
    /// `statx` succeeded, but without the mount id in its mask.
    NoMountId,
}

fn probe() -> Result<(), (&'static str, Failure)> {
    let root = rustix::fs::openat2(
        rustix::fs::CWD,
        "/",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )
    .map_err(|e| ("openat2", Failure::Errno(e)))?;
    rustix::fs::openat2(
        &root,
        ".",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| ("openat2 RESOLVE_IN_ROOT", Failure::Errno(e)))?;
    let st = rustix::fs::statx(
        &root,
        "",
        AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
    )
    .map_err(|e| ("statx", Failure::Errno(e)))?;
    if !StatxFlags::from_bits_retain(st.stx_mask).contains(StatxFlags::MNT_ID) {
        return Err(("the statx mount id (STATX_MNT_ID)", Failure::NoMountId));
    }
    Ok(())
}

/// `EPERM` on an `O_PATH` open of `/` or on `statx` checks no permission, so
/// it comes from a seccomp filter. Linux 5.8 has both calls and the mount
/// id, so on a 5.8 or later release `ENOSYS` or a missing mount id comes from
/// something in between, not from the kernel; everything else means the
/// call is missing.
fn cause(failure: Failure, kernel: &str) -> Cause {
    let new_enough = at_least_5_8(kernel);
    match failure {
        Failure::Errno(e) if e == Errno::PERM => Cause::Blocked(e.into()),
        Failure::Errno(e) if e == Errno::NOSYS && new_enough => Cause::Hidden(e.into()),
        Failure::Errno(e) => Cause::Missing(e.into()),
        Failure::NoMountId if new_enough => Cause::Hidden(io::ErrorKind::Unsupported.into()),
        Failure::NoMountId => Cause::Missing(io::ErrorKind::Unsupported.into()),
    }
}

/// Whether a `uname -r` release such as `7.2.12-orbstack-00394` is Linux
/// 5.8 or later. Only the message depends on it, never the check itself
/// (ADR-0028); a release that does not parse counts as older.
fn at_least_5_8(release: &str) -> bool {
    let mut parts = release.split('.');
    // The leading digits of the next part: `8` of `8-rc1`; none is no number.
    let mut number = || {
        let part = parts.next()?;
        part.split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse::<u32>()
            .ok()
    };
    matches!((number(), number()), (Some(major), Some(minor)) if (major, minor) >= (5, 8))
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
        let old = "4.18.0-553.el8";
        let errno = |e| cause(Failure::Errno(e), old);
        assert!(matches!(errno(Errno::PERM), Cause::Blocked(_)));
        assert!(matches!(errno(Errno::NOSYS), Cause::Missing(_)));
        assert!(matches!(errno(Errno::INVAL), Cause::Missing(_)));
        assert!(matches!(cause(Failure::NoMountId, old), Cause::Missing(_)));
    }

    #[test]
    fn enosys_on_a_new_kernel_is_hidden() {
        let orbstack = "7.2.12-orbstack-00394-gbecdd6130068";
        let errno = |e, kernel| cause(Failure::Errno(e), kernel);
        assert!(matches!(errno(Errno::NOSYS, orbstack), Cause::Hidden(_)));
        assert!(matches!(errno(Errno::NOSYS, "5.8.0"), Cause::Hidden(_)));
        assert!(matches!(errno(Errno::NOSYS, "5.7.19"), Cause::Missing(_)));
        assert!(matches!(errno(Errno::PERM, orbstack), Cause::Blocked(_)));
        assert!(matches!(errno(Errno::INVAL, orbstack), Cause::Missing(_)));
        assert!(matches!(
            cause(Failure::NoMountId, orbstack),
            Cause::Hidden(_)
        ));
    }

    #[test]
    fn releases_compare_by_major_and_minor() {
        for (release, new) in [
            ("5.8", true),
            ("5.8.0-rc1", true),
            ("5.10.0-28-amd64", true),
            ("6.8.0-45-generic", true),
            ("10.0", true),
            ("5.7.19", false),
            ("4.18.0-553.el8_10.x86_64", false),
            ("5", false),
            ("", false),
            ("orbstack", false),
            ("5.x", false),
            ("4294967296.0", false),
        ] {
            assert_eq!(at_least_5_8(release), new, "{release}");
        }
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

        let hidden = Unsupported {
            feature: "openat2",
            cause: Cause::Hidden(Errno::NOSYS.into()),
            kernel: "7.2.12-orbstack".to_owned(),
        };
        let text = hidden.to_string();
        assert!(text.contains("7.2.12-orbstack"), "{text}");
        assert!(text.contains("Rosetta"), "{text}");
        assert!(text.contains("own architecture"), "{text}");
        assert!(!text.contains("needs Linux 5.8"), "{text}");
    }
}
