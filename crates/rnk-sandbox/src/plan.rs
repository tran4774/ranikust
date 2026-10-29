//! The pure parts of a `RUN`: what is mounted where and in which order, the
//! environment, the programs `execve` tries, the generated files. Data only,
//! tested without privileges; `child` performs it.

use std::ffi::{CStr, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use rustix::fs::StatVfsMountFlags;
use rustix::mount::MountFlags;

use crate::spec::{DEFAULT_PATH, MountKind, Network, RunSpec};

/// Where `proc` is mounted.
pub(crate) const PROC: &str = "/proc";
/// Where the read-only `sysfs` is mounted.
pub(crate) const SYS: &str = "/sys";
/// Where the `/dev` tmpfs is mounted.
pub(crate) const DEV: &str = "/dev";
/// The writable copy of the host's resolver configuration.
pub(crate) const RESOLV_CONF: &str = "/etc/resolv.conf";
/// The generated or copied hosts file.
pub(crate) const HOSTS: &str = "/etc/hosts";
/// The host name file.
pub(crate) const HOSTNAME: &str = "/etc/hostname";

/// Device nodes bound from the outer `/dev` into the tmpfs: `mknod` needs
/// `CAP_MKNOD` in the initial user namespace, a bind does not.
pub(crate) const DEV_NODES: [(&CStr, &CStr); 6] = [
    (c"null", c"/dev/null"),
    (c"zero", c"/dev/zero"),
    (c"full", c"/dev/full"),
    (c"random", c"/dev/random"),
    (c"urandom", c"/dev/urandom"),
    (c"tty", c"/dev/tty"),
];

/// `(link name, target)` of the symlinks in `/dev`, as Docker creates them.
pub(crate) const DEV_SYMLINKS: [(&CStr, &CStr); 5] = [
    (c"ptmx", c"pts/ptmx"),
    (c"fd", c"/proc/self/fd"),
    (c"stdin", c"/proc/self/fd/0"),
    (c"stdout", c"/proc/self/fd/1"),
    (c"stderr", c"/proc/self/fd/2"),
];

/// Options of the `/dev` tmpfs, as Docker's.
pub(crate) const DEV_TMPFS: &CStr = c"mode=755,size=65536k";
/// Options of `/dev/pts`: a private instance, as Docker's.
pub(crate) const DEVPTS: &CStr = c"newinstance,ptmxmode=0666,mode=0620,gid=5";
/// Options of `/dev/shm`, as Docker's.
pub(crate) const SHM: &CStr = c"mode=1777,size=65536k";

/// The writable files bound over `/etc/resolv.conf`, `/etc/hosts` and
/// `/etc/hostname`: host paths in the run's scratch directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EtcFiles {
    pub(crate) resolv_conf: PathBuf,
    pub(crate) hosts: PathBuf,
    pub(crate) hostname: PathBuf,
}

impl EtcFiles {
    /// The three files in `dir`.
    pub(crate) fn in_dir(dir: &Path) -> Self {
        Self {
            resolv_conf: dir.join("resolv.conf"),
            hosts: dir.join("hosts"),
            hostname: dir.join("hostname"),
        }
    }
}

/// One mount of the per-`RUN` sequence. Targets are paths inside the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MountOp {
    /// A new `proc`.
    Proc { target: PathBuf },
    /// A read-only `sysfs`.
    Sysfs { target: PathBuf },
    /// A tmpfs on `/dev` with nodes, `devpts`, `shm` and symlinks.
    Dev { target: PathBuf },
    /// A writable copy of a file of ranikust's, bound read-write.
    File { source: PathBuf, target: PathBuf },
    /// A `RUN --mount`; a read-only one is remounted with `remount` flags.
    Bind {
        source: PathBuf,
        target: PathBuf,
        read_only: bool,
    },
}

impl MountOp {
    /// The target inside the root.
    #[cfg(test)]
    pub(crate) fn target(&self) -> &Path {
        match self {
            MountOp::Proc { target }
            | MountOp::Sysfs { target }
            | MountOp::Dev { target }
            | MountOp::File { target, .. }
            | MountOp::Bind { target, .. } => target,
        }
    }
}

/// Every mount target the root must have, with its kind, in mount order.
/// Missing ones become journaled stubs.
pub(crate) fn mount_targets(spec: &RunSpec) -> Vec<(PathBuf, MountKind)> {
    let mut targets = vec![
        (PathBuf::from(PROC), MountKind::Directory),
        (PathBuf::from(SYS), MountKind::Directory),
        (PathBuf::from(DEV), MountKind::Directory),
        (PathBuf::from(RESOLV_CONF), MountKind::File),
        (PathBuf::from(HOSTS), MountKind::File),
        (PathBuf::from(HOSTNAME), MountKind::File),
    ];
    targets.extend(spec.mounts.iter().map(|m| (m.target.clone(), m.kind)));
    targets
}

/// The mounts of a `RUN`, in the order of `design/sandbox.md`: `proc`,
/// `sysfs`, `/dev`, the three `/etc` files, then `RUN --mount`.
pub(crate) fn mount_plan(spec: &RunSpec, files: &EtcFiles) -> Vec<MountOp> {
    let mut plan = vec![
        MountOp::Proc {
            target: PROC.into(),
        },
        MountOp::Sysfs { target: SYS.into() },
        MountOp::Dev { target: DEV.into() },
        MountOp::File {
            source: files.resolv_conf.clone(),
            target: RESOLV_CONF.into(),
        },
        MountOp::File {
            source: files.hosts.clone(),
            target: HOSTS.into(),
        },
        MountOp::File {
            source: files.hostname.clone(),
            target: HOSTNAME.into(),
        },
    ];
    plan.extend(spec.mounts.iter().map(|m| MountOp::Bind {
        source: m.source.clone(),
        target: m.target.clone(),
        read_only: m.read_only,
    }));
    plan
}

/// Flags of a read-only bind remount that keep the source mount's flags:
/// in a user namespace, dropping a locked flag (`nosuid`, `nodev`,
/// `noexec`, the atime flags) fails with `EPERM`.
pub(crate) fn read_only_remount_flags(source: StatVfsMountFlags) -> MountFlags {
    let mut flags = MountFlags::BIND | MountFlags::RDONLY;
    for (st, ms) in [
        (StatVfsMountFlags::NOSUID, MountFlags::NOSUID),
        (StatVfsMountFlags::NODEV, MountFlags::NODEV),
        (StatVfsMountFlags::NOEXEC, MountFlags::NOEXEC),
        (StatVfsMountFlags::NOATIME, MountFlags::NOATIME),
        (StatVfsMountFlags::NODIRATIME, MountFlags::NODIRATIME),
        (StatVfsMountFlags::RELATIME, MountFlags::RELATIME),
    ] {
        if source.contains(st) {
            flags |= ms;
        }
    }
    flags
}

/// The `RUN` environment: `spec.env` with Docker's defaults for `PATH` and
/// `HOME` (the user's home from the root's `/etc/passwd`) when missing.
pub(crate) fn environment(env: &[(OsString, OsString)], home: &Path) -> Vec<(OsString, OsString)> {
    let mut out = env.to_vec();
    if !env.iter().any(|(k, _)| k == "PATH") {
        out.push(("PATH".into(), DEFAULT_PATH.into()));
    }
    if !env.iter().any(|(k, _)| k == "HOME") {
        out.push(("HOME".into(), home.as_os_str().to_owned()));
    }
    out
}

/// The value of `PATH` in `env`: the last one, as `getenv` sees a list
/// built by appending.
pub(crate) fn path_var(env: &[(OsString, OsString)]) -> Option<&OsStr> {
    env.iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_os_str())
}

/// The paths `execve` tries for `program`, in order, as `execvp` and Go's
/// `exec.LookPath` (which runc uses) do: `program` itself when it contains a
/// `/`, else each `PATH` entry joined with it, an empty entry meaning `.`.
/// Resolved by the kernel after `pivot_root`, `setuid` and `chdir`.
pub(crate) fn exec_candidates(program: &OsStr, path: Option<&OsStr>) -> Vec<OsString> {
    if program.as_bytes().contains(&b'/') {
        return vec![program.to_owned()];
    }
    let path = path.unwrap_or(OsStr::new(DEFAULT_PATH));
    path.as_bytes()
        .split(|&b| b == b':')
        .map(|dir| {
            let dir: &[u8] = if dir.is_empty() { b"." } else { dir };
            let mut full = dir.to_vec();
            if !dir.ends_with(b"/") {
                full.push(b'/');
            }
            full.extend_from_slice(program.as_bytes());
            OsString::from_vec(full)
        })
        .collect()
}

/// The `/etc/hosts` of a `RUN` [open: Q-076]. With the host network, the
/// file ranikust itself sees (as kaniko's `RUN` sees it), with the host
/// name added; with `--network=none`, only loopback names.
pub(crate) fn hosts_file(network: Network, hostname: &str, host_hosts: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    match (network, host_hosts) {
        (Network::Host, Some(host)) => {
            out.extend_from_slice(host);
            if !out.is_empty() && !out.ends_with(b"\n") {
                out.push(b'\n');
            }
        }
        _ => {
            out.extend_from_slice(b"127.0.0.1\tlocalhost\n");
            out.extend_from_slice(b"::1\tlocalhost ip6-localhost ip6-loopback\n");
        }
    }
    out.extend_from_slice(format!("127.0.0.1\t{hostname}\n").as_bytes());
    out
}

/// The `/etc/hostname` of a `RUN`.
pub(crate) fn hostname_file(hostname: &str) -> Vec<u8> {
    format!("{hostname}\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::BindMount;

    fn spec() -> RunSpec {
        let mut s = RunSpec::new("/r", vec!["sh".into()]);
        s.mounts.push(BindMount {
            source: "/state/caches/go".into(),
            target: "/root/.cache/go".into(),
            kind: MountKind::Directory,
            read_only: true,
        });
        s
    }

    #[test]
    fn mounts_follow_the_design_order() {
        let files = EtcFiles::in_dir(Path::new("/state/sandbox/runs/1"));
        let plan = mount_plan(&spec(), &files);
        let targets: Vec<_> = plan.iter().map(MountOp::target).collect();
        assert_eq!(
            targets,
            [
                "/proc",
                "/sys",
                "/dev",
                "/etc/resolv.conf",
                "/etc/hosts",
                "/etc/hostname",
                "/root/.cache/go"
            ]
            .map(Path::new)
        );
        assert_eq!(
            plan[3],
            MountOp::File {
                source: "/state/sandbox/runs/1/resolv.conf".into(),
                target: "/etc/resolv.conf".into()
            }
        );
        assert!(matches!(
            &plan[6],
            MountOp::Bind {
                read_only: true,
                ..
            }
        ));
        let stubs: Vec<_> = mount_targets(&spec()).into_iter().map(|(p, _)| p).collect();
        assert_eq!(stubs, targets);
    }

    #[test]
    fn read_only_remount_keeps_locked_flags() {
        let flags =
            read_only_remount_flags(StatVfsMountFlags::NOSUID | StatVfsMountFlags::RELATIME);
        assert_eq!(
            flags,
            MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::RELATIME
        );
        assert!(!flags.contains(MountFlags::NODEV));
    }

    #[test]
    fn environment_gets_path_and_home() {
        let env = environment(&[("A".into(), "1".into())], Path::new("/home/u"));
        assert_eq!(
            env,
            [
                ("A".into(), "1".into()),
                ("PATH".into(), DEFAULT_PATH.into()),
                ("HOME".into(), "/home/u".into())
            ]
        );
        let env = environment(
            &[("PATH".into(), "/x".into()), ("HOME".into(), "/h".into())],
            Path::new("/home/u"),
        );
        assert_eq!(env.len(), 2);
        assert_eq!(path_var(&env), Some(OsStr::new("/x")));
    }

    #[test]
    fn candidates_search_path_unless_program_has_a_slash() {
        let c = exec_candidates(OsStr::new("sh"), Some(OsStr::new("/usr/bin::/bin/")));
        assert_eq!(c, ["/usr/bin/sh", "./sh", "/bin/sh"].map(OsString::from));
        let c = exec_candidates(OsStr::new("./x"), Some(OsStr::new("/usr/bin")));
        assert_eq!(c, [OsString::from("./x")]);
        let c = exec_candidates(OsStr::new("ls"), None);
        assert_eq!(c.len(), 6);
        assert_eq!(c[0], "/usr/local/sbin/ls");
    }

    #[test]
    fn hosts_files() {
        let none = hosts_file(Network::None, "h", Some(b"10.0.0.1 db"));
        let none = String::from_utf8(none).unwrap();
        assert!(none.starts_with("127.0.0.1\tlocalhost\n"), "{none}");
        assert!(!none.contains("db"));
        assert!(none.ends_with("127.0.0.1\th\n"));

        let host = hosts_file(Network::Host, "h", Some(b"10.0.0.1 db"));
        assert_eq!(host, b"10.0.0.1 db\n127.0.0.1\th\n");
        let missing = hosts_file(Network::Host, "h", None);
        assert!(missing.starts_with(b"127.0.0.1\tlocalhost\n"));
        assert_eq!(hostname_file("h"), b"h\n");
    }

    #[test]
    fn dev_tables_are_well_formed() {
        for (name, source) in DEV_NODES {
            assert!(!name.to_bytes().contains(&b'/'));
            assert!(source.to_bytes().starts_with(b"/dev/"));
        }
        for (link, _) in DEV_SYMLINKS {
            assert!(!link.to_bytes().contains(&b'/'));
        }
    }
}
