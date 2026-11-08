//! The advice for a user other than root who gets no usable user namespace
//! (`design/modes-and-detection.md#no-usable-user-namespace`), in order:
//! `sudo`; where `kernel.apparmor_restrict_unprivileged_userns` is 1 and
//! AppArmor may be what refused, ranikust's AppArmor profile
//! (`contrib/apparmor/ranikust`), for the binary's own path where only root
//! may change it, else after installing the binary where only root may; a
//! container.

use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rnk_sandbox::{StepFailure, StepResult};
use rustix::fs::{CWD, FileType, FsWord, Mode, OFlags, StatVfsMountFlags};
use rustix::io::Errno;

/// The path the repository's profile binds.
const PROFILE_PATH: &str = "/usr/local/bin/ranikust";

/// The directory of [`PROFILE_PATH`].
const PROFILE_DIR: &str = "/usr/local/bin";

/// The profile, and where to read it.
const PROFILE: &str = "ranikust's AppArmor profile (contrib/apparmor/ranikust in its \
     repository; the book's \"Choosing where to run it\" shows it)";

/// The `f_type`s of the local filesystems whose owners and modes only what
/// root mounted sets. Any other (FUSE, NFS, SMB, 9p, Ceph, AFS…) may show
/// what its server, or the user who mounted it, says.
const LOCAL: [u32; 8] = [
    0xEF53,      // ext2, ext3, ext4
    0x5846_5342, // xfs
    0x9123_683E, // btrfs
    0x7371_7368, // squashfs
    0xE0F5_E1E2, // erofs
    0x0102_1994, // tmpfs
    0x794C_7630, // overlayfs
    0x2FC1_2FC1, // zfs
];

/// rnk-sandbox's name for the `unshare` step's own operation, which a step
/// before it (the probe's `fchdir`) may stand in for: another name, or a
/// renamed one, offers no profile.
const UNSHARE_USER: &str = "unshare(CLONE_NEWUSER)";

/// What stopped a user other than root, as the advice weighs it: AppArmor's
/// switch leaves a user namespace without capabilities, and a profile
/// without `userns` refuses `unshare` with `EACCES`; what else refuses
/// `unshare` (a seccomp filter, `kernel.unprivileged_userns_clone`,
/// `user.max_user_namespaces`) no profile lifts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// `unshare(CLONE_NEWUSER)` refused, with this errno.
    Unshare(i32),
    /// A step refused in the user namespace `unshare` made: the map,
    /// `chown`, the mount namespace.
    InNamespace,
    /// Not known: `unshare`'s step refused by another operation or with no
    /// errno, a step that did not run (a child cut short), a namespace
    /// shown to have capabilities, or a refusal once the probe passed.
    Unknown,
}

impl Stop {
    /// The stop named `step`, as [`rnk_sandbox::UsernsReport::stop`] names
    /// it, from how `unshare(CLONE_NEWUSER)` went (`unshare`) and how that
    /// step went (`result`; `unshare` again for `unshare`): a later step
    /// counts only where `unshare` worked and the step itself was refused.
    pub(crate) fn at(step: &str, unshare: &StepResult, result: &StepResult) -> Self {
        match (step, unshare, result) {
            (
                "unshare",
                _,
                StepResult::Failed(StepFailure {
                    op: UNSHARE_USER,
                    errno: Some(errno),
                    ..
                }),
            ) => Self::Unshare(*errno),
            ("map" | "chown" | "mount namespace", StepResult::Ok, StepResult::Failed(_)) => {
                Self::InNamespace
            }
            _ => Self::Unknown,
        }
    }

    /// Whether AppArmor may be what refused.
    fn offers_profile(self) -> bool {
        match self {
            Self::InNamespace => true,
            Self::Unshare(errno) => errno == Errno::ACCESS.raw_os_error(),
            Self::Unknown => false,
        }
    }
}

/// The advice for this host, for `binary`, the path ranikust started from
/// (`None` where it could not be read), stopped by `stop`: the profile only
/// where AppArmor restricts user namespaces and may be what refused, for
/// that path only where root alone may change it.
pub(crate) fn guidance(binary: Option<&Path>, stop: Stop) -> String {
    if !(stop.offers_profile() && restricted()) {
        return advice(None);
    }
    let bound = match binary {
        None => Err("ranikust cannot read the path it runs from".to_owned()),
        Some(path) => match only_root_changes(path, FileType::RegularFile) {
            Err(why) => Err(format!(
                "{} is not a path only root can change ({why})",
                path.display()
            )),
            Ok(()) if !plain(path) => Err(format!(
                "{} has characters a profile reads as a pattern or a separator",
                path.display()
            )),
            Ok(()) => Ok(path),
        },
    };
    let dir = only_root_changes(Path::new(PROFILE_DIR), FileType::Directory);
    advice(Some(Checked { bound, dir }))
}

/// Whether `kernel.apparmor_restrict_unprivileged_userns` reads 1: any
/// other value, or none, offers no profile.
fn restricted() -> bool {
    std::fs::read(super::APPARMOR_USERNS).is_ok_and(|v| v.trim_ascii() == b"1")
}

/// Whether a profile may name `path` as it is: letters, digits and `._+-/`
/// alone, none of which AppArmor reads as a pattern.
fn plain(path: &Path) -> bool {
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b"._+-/".contains(&b))
}

/// What the advice weighs where AppArmor restricts user namespaces.
struct Checked<'a> {
    /// The binary's path, where a profile may be bound to it, else why not.
    bound: Result<&'a Path, String>,
    /// Whether only root may change [`PROFILE_DIR`], else why not.
    dir: Result<(), String>,
}

/// The advice's text; `profile` is `None` where AppArmor does not restrict
/// user namespaces.
fn advice(profile: Option<Checked<'_>>) -> String {
    const SUDO: &str = "run ranikust as root, with sudo";
    const CONTAINER: &str = "or run ranikust in a container";
    const LOAD: &str = "apparmor_parser -r /etc/apparmor.d/ranikust";
    const INSTALL: &str = "owned by root and written by no one else (install -o root -g root -m \
                           0755)";
    const ANY: &str = "a profile lets whatever is at its path make user namespaces";
    let Some(Checked { bound, dir }) = profile else {
        return format!(
            "a user other than root builds in a user namespace of its own, which this user \
             cannot get here: {SUDO}; {CONTAINER}"
        );
    };
    let profile = match (bound, dir) {
        (Ok(path), _) if path == Path::new(PROFILE_PATH) => {
            format!("or, as root, copy {PROFILE} to /etc/apparmor.d/ranikust and run {LOAD}")
        }
        (Ok(path), _) => format!(
            "or, as root, copy {PROFILE} to /etc/apparmor.d/ranikust with its path \
             {PROFILE_PATH} changed to {}, and run {LOAD}",
            path.display()
        ),
        (Err(why), Ok(())) => format!(
            "or, as root, install ranikust as {PROFILE_PATH} first, {INSTALL}, copy {PROFILE} \
             to /etc/apparmor.d/ranikust and run {LOAD}, then run ranikust from \
             {PROFILE_PATH}: {why}, and {ANY}"
        ),
        (Err(why), Err(dir)) => format!(
            "or, as root, install ranikust first in a directory only root can change, which \
             {PROFILE_DIR} is not here ({dir}), {INSTALL}, copy {PROFILE} to \
             /etc/apparmor.d/ranikust with its path {PROFILE_PATH} changed to the binary's, \
             and run {LOAD}, then run ranikust from there: {why}, and {ANY}"
        ),
    };
    format!(
        "a user other than root builds in a user namespace of its own, and where \
         kernel.apparmor_restrict_unprivileged_userns is 1, as here, only a program an AppArmor \
         profile allows gets a usable one: {SUDO}; {profile}; {CONTAINER}"
    )
}

/// Whether only root may change what `path` names, so that a profile bound
/// to it gives a user namespace to no one else's program: `path` absolute,
/// with no `..`; `/`, each directory below it and the node itself, of kind
/// `leaf`, owned by root and not writable by group or others (an ACL that
/// grants writing shows in the group bits), none a symlink, each on a
/// mount without `nosuid` of a local filesystem ([`LOCAL`]): elsewhere the
/// owners and modes are what a user or a server says.
///
/// # Errors
///
/// The first node that fails and why, or the error that stopped the walk:
/// any error says no.
fn only_root_changes(path: &Path, leaf: FileType) -> Result<(), String> {
    let nodes = walk(path).map_err(|(at, e)| format!("{}: {e}", at.display()))?;
    verdict(&nodes, leaf)
}

/// What [`walk`] found of one component of a path.
#[derive(Clone, Copy, Debug)]
struct Node {
    kind: FileType,
    uid: u32,
    /// The permission bits, sticky included.
    mode: u32,
    /// Its filesystem's `f_type`.
    fs_type: FsWord,
    /// Whether its mount is `nosuid`.
    nosuid: bool,
}

/// `/` and each component of `path` below it, each with the path it is
/// at, opened `O_PATH` from the one above without following a symlink: a
/// symlink is a node of its own, and nothing is found below it. The paths
/// are only shown, never opened.
///
/// # Errors
///
/// The path where the walk stopped and why: `EINVAL` for a relative path
/// or a `..`.
fn walk(path: &Path) -> Result<Vec<(PathBuf, Node)>, (PathBuf, Errno)> {
    let mut components = path.components();
    let mut at = PathBuf::from("/");
    if components.next() != Some(Component::RootDir) {
        return Err((path.to_owned(), Errno::INVAL));
    }
    let how = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let stop = |at: &Path| {
        let at = at.to_owned();
        move |e| (at, e)
    };
    let mut fd =
        rustix::fs::openat(CWD, "/", how | OFlags::DIRECTORY, Mode::empty()).map_err(stop(&at))?;
    let mut nodes = vec![(at.clone(), node(&fd).map_err(stop(&at))?)];
    for component in components {
        let Component::Normal(name) = component else {
            return Err((path.to_owned(), Errno::INVAL));
        };
        at.push(name);
        fd = rustix::fs::openat(&fd, name, how, Mode::empty()).map_err(stop(&at))?;
        nodes.push((at.clone(), node(&fd).map_err(stop(&at))?));
    }
    Ok(nodes)
}

fn node(fd: &OwnedFd) -> rustix::io::Result<Node> {
    let st = rustix::fs::fstat(fd)?;
    let fs = rustix::fs::fstatfs(fd)?;
    let mount = rustix::fs::fstatvfs(fd)?;
    Ok(Node {
        kind: FileType::from_raw_mode(st.st_mode),
        uid: st.st_uid,
        mode: st.st_mode & 0o7777,
        fs_type: fs.f_type,
        nosuid: mount.f_flag.contains(StatVfsMountFlags::NOSUID),
    })
}

/// Whether `fs_type` is one of [`LOCAL`].
fn local(fs_type: FsWord) -> bool {
    u32::try_from(fs_type).is_ok_and(|t| LOCAL.contains(&t))
}

/// [`only_root_changes`]'s rule on what [`walk`] found, `/` first: the
/// last node of kind `leaf`, the others directories.
fn verdict(nodes: &[(PathBuf, Node)], leaf: FileType) -> Result<(), String> {
    let Some(last) = nodes.len().checked_sub(1) else {
        return Err("nothing was found".to_owned());
    };
    for (i, (at, n)) in nodes.iter().enumerate() {
        let at = at.display();
        let want = if i == last { leaf } else { FileType::Directory };
        if n.kind == FileType::Symlink {
            return Err(format!("{at} is a symlink"));
        }
        if n.kind != want {
            let what = if want == FileType::Directory {
                "a directory"
            } else {
                "a regular file"
            };
            return Err(format!("{at} is not {what}"));
        }
        if n.uid != 0 {
            return Err(format!("{at} is owned by uid {}", n.uid));
        }
        if n.mode & 0o022 != 0 {
            return Err(format!(
                "{at} is writable by its group or others (mode {:o})",
                n.mode
            ));
        }
        // udisks2 and fusermount always mount `nosuid`, fstab `user` by default.
        if n.nosuid {
            return Err(format!("{at} is on a nosuid mount, as a user's mount is"));
        }
        if !local(n.fs_type) {
            return Err(format!(
                "{at} is not on a local filesystem ranikust knows (f_type {:#x})",
                n.fs_type
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
