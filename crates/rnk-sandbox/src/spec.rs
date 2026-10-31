//! What a `RUN` asks of the sandbox.

use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Network of a `RUN` ([ADR-0010](../../../docs/adr/0010-run-network-host-or-none.md)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// Share the host's network namespace (the default).
    #[default]
    Host,
    /// A new, empty network namespace with only the loopback interface up
    /// (`RUN --network=none`).
    None,
}

/// How a `RUN` is isolated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Isolation {
    /// New mount, PID, UTS and IPC namespaces (plus network for
    /// [`Network::None`]), `proc`, `sysfs` and `/dev` mounted, the root
    /// entered with `pivot_root` or `MS_MOVE`, capabilities cut to Docker's
    /// defaults.
    #[default]
    Full,
    /// Sequential kaniko mode where nothing can be mounted: the command runs
    /// as a plain child process, like kaniko. On the root `/` (in a
    /// container) nothing is placed: the command sees the container's own
    /// `/proc`, `/dev`, `/etc/hosts` and `resolv.conf`. On any other root
    /// (a root in the state directory on a host, ADR-0050) the child
    /// `chroot`s into it, with ranikust's `resolv.conf`, `hosts` and
    /// `hostname` and the `/dev` nodes put in
    /// place and journaled for the `RUN`, and no `/proc`. No namespaces and
    /// no capability changes: the container or host ranikust runs in is
    /// the only isolation.
    Direct,
}

/// The `RLIMIT_NOFILE` a `RUN` starts with; `None` is unlimited. ranikust
/// raises its own soft limit for the kaniko-mode walk, so the limit it
/// started with is captured first ([`NofileLimit::current`]) and given back
/// to every `RUN` ([`crate::Sandbox::set_nofile_limit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NofileLimit {
    /// The soft limit.
    pub soft: Option<u64>,
    /// The hard limit.
    pub hard: Option<u64>,
}

impl NofileLimit {
    /// This process's limit now.
    #[must_use]
    pub fn current() -> Self {
        let l = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        Self {
            soft: l.current,
            hard: l.maximum,
        }
    }
}

/// The `RLIMIT_CORE` this process had before [`forbid_core_dumps`].
static ORIGINAL_CORE: std::sync::OnceLock<rustix::process::Rlimit> = std::sync::OnceLock::new();

/// Sets this process's soft `RLIMIT_CORE` to 0, the hard limit kept: no
/// core file of ranikust, whose memory holds credentials, lands where a
/// `RUN` reads it (ADR-0051). Every `RUN` started afterwards gets the soft
/// limit back, as with [`NofileLimit`]. Called first in `main`; a second
/// call keeps the limit recorded by the first.
///
/// # Errors
///
/// What `setrlimit` returns.
pub fn forbid_core_dumps() -> rustix::io::Result<()> {
    let original =
        *ORIGINAL_CORE.get_or_init(|| rustix::process::getrlimit(rustix::process::Resource::Core));
    rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: original.maximum,
        },
    )
}

/// The limit a `RUN` starts with: the one recorded by [`forbid_core_dumps`],
/// or `None` (ranikust's own) when it was never called.
pub(crate) fn original_core_limit() -> Option<rustix::process::Rlimit> {
    ORIGINAL_CORE.get().copied()
}

/// Whether a bind mount's target is a directory or a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountKind {
    /// A directory, such as `RUN --mount=type=cache`.
    Directory,
    /// A regular file, such as `RUN --mount=type=secret`.
    File,
}

/// A host path bound into the root for one `RUN`
/// (`RUN --mount=type=cache|secret`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindMount {
    /// Absolute host path of the source, owned by ranikust (a cache
    /// directory in the store, a secret file).
    pub source: PathBuf,
    /// Absolute path inside the root. Created as a journaled stub when
    /// missing.
    pub target: PathBuf,
    /// Directory or file.
    pub kind: MountKind,
    /// Mount read-only.
    pub read_only: bool,
}

/// A `RUN --mount=type=secret` file, as BuildKit's secret mount
/// (`solver/llbsolver/mounts/secretmount_unix.go`): the child writes `data`
/// to a file on a tmpfs of its own (`nodev`, `nosuid`, `noexec`), gives it
/// `uid`:`gid` and `mode`, binds it read-only at `target` and detaches the
/// tmpfs. The bytes never reach a disk; they live in this value and in the
/// child's memory until the `RUN` ends.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretFile {
    /// The secret. Mounts sharing one allocation (and owner and mode) share
    /// one file on the tmpfs.
    pub data: std::sync::Arc<[u8]>,
    /// Absolute path inside the root. Created as a journaled stub when
    /// missing.
    pub target: PathBuf,
    /// Owner of the file.
    pub uid: u32,
    /// Group of the file.
    pub gid: u32,
    /// Permission bits of the file; other bits are dropped.
    pub mode: u32,
}

impl std::fmt::Debug for SecretFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretFile")
            .field("data", &format_args!("<{} bytes hidden>", self.data.len()))
            .field("target", &self.target)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("mode", &format_args!("{:o}", self.mode))
            .finish()
    }
}

/// Where a standard stream of the command goes.
#[derive(Debug, Default)]
pub enum Stdio {
    /// The stream of the ranikust process.
    #[default]
    Inherit,
    /// `/dev/null` inside the sandbox.
    Null,
    /// This file descriptor, typically a pipe the caller reads.
    Fd(OwnedFd),
}

/// One `RUN` command and its environment. Its `Debug` shows environment
/// names only: a value may be a secret (`RUN --mount=type=secret,env=`).
#[non_exhaustive]
pub struct RunSpec {
    /// Absolute, canonical host path of the stage root (no symlinks).
    pub root_dir: PathBuf,
    /// The command and its arguments; `argv[0]` is searched on `PATH` inside
    /// the root, after entering it and after switching user.
    pub argv: Vec<OsString>,
    /// The environment, in order. `PATH` and `HOME` get Docker's defaults
    /// when missing.
    pub env: Vec<(OsString, OsString)>,
    /// The `USER` value, `user[:group]`, resolved in the root by
    /// `rnk-rootfs`. Empty means root (`0`).
    pub user: String,
    /// Absolute working directory inside the root; it must exist.
    pub workdir: PathBuf,
    /// Network.
    pub network: Network,
    /// Host name inside the UTS namespace, and in `/etc/hostname`.
    pub hostname: String,
    /// `RUN --mount` binds.
    pub mounts: Vec<BindMount>,
    /// `RUN --mount=type=secret` files, written on a tmpfs by the child.
    pub secrets: Vec<SecretFile>,
    /// Standard input; `/dev/null` by default, as a `RUN` under Docker.
    pub stdin: Stdio,
    /// Standard output.
    pub stdout: Stdio,
    /// Standard error.
    pub stderr: Stdio,
    /// Isolation.
    pub isolation: Isolation,
}

impl std::fmt::Debug for RunSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: Vec<&OsStr> = self.env.iter().map(|(k, _)| k.as_os_str()).collect();
        f.debug_struct("RunSpec")
            .field("root_dir", &self.root_dir)
            .field("argv", &self.argv)
            .field("env (names)", &env)
            .field("user", &self.user)
            .field("workdir", &self.workdir)
            .field("network", &self.network)
            .field("hostname", &self.hostname)
            .field("mounts", &self.mounts)
            .field("secrets", &self.secrets)
            .field("stdin", &self.stdin)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("isolation", &self.isolation)
            .finish()
    }
}

/// The largest secret file the child writes: BuildKit's limit, 500 KiB.
pub(crate) const MAX_SECRET_BYTES: usize = 500 * 1024;

/// The `PATH` Docker gives a container whose image sets none.
pub(crate) const DEFAULT_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The host name used when the caller leaves it empty: fixed, so that it
/// does not depend on the host (Q-075).
pub(crate) const DEFAULT_HOSTNAME: &str = "ranikust";

impl RunSpec {
    /// A `RUN` of `argv` in the root at `root_dir`, as root, in `/`, with the
    /// host network, an empty environment, stdin from `/dev/null` and
    /// inherited stdout and stderr.
    #[must_use]
    pub fn new(root_dir: impl Into<PathBuf>, argv: Vec<OsString>) -> Self {
        Self {
            root_dir: root_dir.into(),
            argv,
            env: Vec::new(),
            user: String::new(),
            workdir: PathBuf::from("/"),
            network: Network::Host,
            hostname: DEFAULT_HOSTNAME.to_owned(),
            mounts: Vec::new(),
            secrets: Vec::new(),
            stdin: Stdio::Null,
            stdout: Stdio::Inherit,
            stderr: Stdio::Inherit,
            isolation: Isolation::Full,
        }
    }

    /// The `USER` value to resolve: `0` when empty.
    pub(crate) fn user_spec(&self) -> &str {
        if self.user.is_empty() {
            "0"
        } else {
            &self.user
        }
    }

    /// Checks everything that can be checked without touching the root.
    pub(crate) fn validate(&self) -> Result<()> {
        absolute_clean("root_dir", &self.root_dir)?;
        absolute_clean("workdir", &self.workdir)?;
        let Some(program) = self.argv.first() else {
            return Err(Error::InvalidSpec("argv is empty".into()));
        };
        if program.is_empty() {
            return Err(Error::InvalidSpec("argv[0] is empty".into()));
        }
        for arg in &self.argv {
            no_nul("an argument", arg)?;
        }
        for (key, value) in &self.env {
            if key.is_empty() || key.as_bytes().contains(&b'=') {
                return Err(Error::InvalidSpec(format!(
                    "invalid environment name {key:?}"
                )));
            }
            no_nul("an environment name", key)?;
            // Only the name: the value may be a secret.
            if value.as_bytes().contains(&0) {
                return Err(Error::InvalidSpec(format!(
                    "the value of environment variable {key:?} contains a NUL byte"
                )));
            }
        }
        if self.hostname.len() > 64 || self.hostname.as_bytes().contains(&0) {
            return Err(Error::InvalidSpec(format!(
                "invalid host name {:?}",
                self.hostname
            )));
        }
        for m in &self.mounts {
            absolute_clean("a mount source", &m.source)?;
            absolute_clean("a mount target", &m.target)?;
            if m.target == Path::new("/") {
                return Err(Error::InvalidSpec("a mount cannot cover /".into()));
            }
        }
        for s in &self.secrets {
            absolute_clean("a secret target", &s.target)?;
            if s.target == Path::new("/") {
                return Err(Error::InvalidSpec("a secret cannot cover /".into()));
            }
            if s.data.len() > MAX_SECRET_BYTES {
                return Err(Error::InvalidSpec(format!(
                    "a secret for {:?} is larger than {MAX_SECRET_BYTES} bytes",
                    s.target
                )));
            }
        }
        if self.isolation == Isolation::Direct
            && (!self.mounts.is_empty()
                || !self.secrets.is_empty()
                || self.network == Network::None)
        {
            return Err(Error::InvalidSpec(
                "Isolation::Direct cannot mount or create a network namespace".into(),
            ));
        }
        Ok(())
    }
}

fn no_nul(what: &str, s: &OsStr) -> Result<()> {
    if s.as_bytes().contains(&0) {
        return Err(Error::InvalidSpec(format!(
            "{what} contains a NUL byte: {s:?}"
        )));
    }
    Ok(())
}

/// Requires an absolute path with no `.`, `..` or NUL byte.
fn absolute_clean(what: &str, p: &Path) -> Result<()> {
    if !p.is_absolute() {
        return Err(Error::InvalidSpec(format!(
            "{what} must be absolute: {p:?}"
        )));
    }
    if p.components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(Error::InvalidSpec(format!(
            "{what} must not contain . or ..: {p:?}"
        )));
    }
    no_nul(what, p.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        RunSpec::new(
            "/state/stages/0/rootfs",
            vec!["sh".into(), "-c".into(), "true".into()],
        )
    }

    #[test]
    fn defaults() {
        let s = spec();
        assert_eq!(s.user_spec(), "0");
        assert_eq!(s.workdir, Path::new("/"));
        assert_eq!(s.hostname, DEFAULT_HOSTNAME);
        assert!(matches!(s.stdin, Stdio::Null));
        assert!(s.validate().is_ok());
    }

    /// An environment value may be a secret: neither an error nor `Debug`
    /// shows it, nor a secret file's bytes.
    #[test]
    fn values_and_secrets_stay_out_of_messages() {
        let mut s = spec();
        s.env.push(("TOKEN".into(), "top\0Zq9val".into()));
        let e = s.validate().unwrap_err().to_string();
        assert!(e.contains("TOKEN"), "{e}");
        assert!(!e.contains("Zq9val"), "{e}");
        s.secrets.push(SecretFile {
            data: b"Zq9bytes".as_slice().into(),
            target: "/run/secrets/t".into(),
            uid: 0,
            gid: 0,
            mode: 0o400,
        });
        let d = format!("{s:?}");
        assert!(d.contains("TOKEN") && !d.contains("Zq9"), "{d}");
        // A secret needs the full sandbox and a clean target.
        let mut s = spec();
        s.secrets.push(SecretFile {
            data: Vec::new().into(),
            target: "/a/../b".into(),
            uid: 0,
            gid: 0,
            mode: 0o400,
        });
        assert!(s.validate().is_err());
        s.secrets[0].target = "/b".into();
        assert!(s.validate().is_ok());
        s.root_dir = "/".into();
        s.isolation = Isolation::Direct;
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_bad_specs() {
        let mut s = spec();
        s.argv.clear();
        assert!(s.validate().is_err());

        let mut s = spec();
        s.workdir = "relative".into();
        assert!(s.validate().is_err());

        let mut s = spec();
        s.root_dir = "/state/../etc".into();
        assert!(s.validate().is_err());

        let mut s = spec();
        s.env.push(("A=B".into(), "c".into()));
        assert!(s.validate().is_err());

        let mut s = spec();
        s.argv.push(OsString::from("a\0b"));
        assert!(s.validate().is_err());

        let mut s = spec();
        s.mounts.push(BindMount {
            source: "/state/caches/x".into(),
            target: "/".into(),
            kind: MountKind::Directory,
            read_only: false,
        });
        assert!(s.validate().is_err());

        let mut s = spec();
        s.hostname = "h".repeat(65);
        assert!(s.validate().is_err());
    }

    #[test]
    fn direct_runs_on_any_root_without_mounts() {
        let mut s = spec();
        s.isolation = Isolation::Direct;
        assert!(s.validate().is_ok());
        s.root_dir = "/".into();
        assert!(s.validate().is_ok());
        s.network = Network::None;
        assert!(s.validate().is_err());
        let mut s = spec();
        s.isolation = Isolation::Direct;
        s.mounts.push(BindMount {
            source: "/state/caches/x".into(),
            target: "/cache".into(),
            kind: MountKind::Directory,
            read_only: false,
        });
        assert!(s.validate().is_err());
    }

    #[test]
    fn the_nofile_limit_is_read_from_the_process() {
        let l = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        assert_eq!(
            NofileLimit::current(),
            NofileLimit {
                soft: l.current,
                hard: l.maximum
            }
        );
    }
}
