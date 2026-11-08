//! The advice for a user other than root who gets no usable user namespace
//! (`design/modes-and-detection.md#refusal-and-guidance`), in order: `sudo`;
//! where `kernel.apparmor_restrict_unprivileged_userns` is 1, ranikust's
//! AppArmor profile (`contrib/apparmor/ranikust`), for the binary's own
//! path where only root may change it, else after installing the binary
//! under `/usr/local/bin`; a container.

use std::os::fd::OwnedFd;
use std::path::{Component, Path};

use rustix::fs::{CWD, FileType, FsWord, Mode, OFlags};
use rustix::io::Errno;

/// The path the repository's profile binds.
const PROFILE_PATH: &str = "/usr/local/bin/ranikust";

/// The profile, and where to read it.
const PROFILE: &str = "ranikust's AppArmor profile (contrib/apparmor/ranikust in its \
     repository; the book's \"Choosing where to run it\" shows it)";

/// FUSE's `f_type`: its server, which any user may run, reports the owners
/// and modes.
const FUSE_SUPER_MAGIC: FsWord = 0x6573_5546;

/// The advice for this host: whether AppArmor restricts user namespaces
/// here, and whether only root may change `binary`, the path ranikust
/// started from (`None` where it could not be read), weighed only then.
pub(crate) fn guidance(binary: Option<&Path>) -> String {
    let restricted = restricted();
    let root_only = restricted && binary.is_some_and(only_root_changes);
    advice(restricted, binary, root_only)
}

/// Whether `kernel.apparmor_restrict_unprivileged_userns` reads 1: any
/// other value, or none, offers no profile.
fn restricted() -> bool {
    std::fs::read(super::APPARMOR_USERNS).is_ok_and(|v| v.trim_ascii() == b"1")
}

/// The advice's text: `restricted` as [`restricted`] says, `root_only` as
/// [`only_root_changes`] says of `binary`.
fn advice(restricted: bool, binary: Option<&Path>, root_only: bool) -> String {
    const SUDO: &str = "run ranikust as root, with sudo";
    const CONTAINER: &str = "or run ranikust in a container";
    const LOAD: &str = "apparmor_parser -r /etc/apparmor.d/ranikust";
    if !restricted {
        return format!(
            "a user other than root builds in a user namespace of its own, which this user \
             cannot get here: {SUDO}; {CONTAINER}"
        );
    }
    let profile = match binary {
        Some(path) if root_only && path == Path::new(PROFILE_PATH) => {
            format!("or, as root, copy {PROFILE} to /etc/apparmor.d/ranikust and run {LOAD}")
        }
        Some(path) if root_only => format!(
            "or, as root, copy {PROFILE} to /etc/apparmor.d/ranikust with its path \
             {PROFILE_PATH} changed to {}, and run {LOAD}",
            path.display()
        ),
        _ => {
            let why = binary.map_or_else(
                || "ranikust cannot read the path it runs from".to_owned(),
                |path| {
                    format!(
                        "{} is not a path only root can change, or ranikust cannot tell that it \
                         is",
                        path.display()
                    )
                },
            );
            format!(
                "or, as root, install ranikust as {PROFILE_PATH} first, copy {PROFILE} to \
                 /etc/apparmor.d/ranikust and run {LOAD}, then run ranikust from \
                 {PROFILE_PATH}: {why}, and a profile lets whatever is at its path make user \
                 namespaces"
            )
        }
    };
    format!(
        "a user other than root builds in a user namespace of its own, and where \
         kernel.apparmor_restrict_unprivileged_userns is 1, as here, only a program an AppArmor \
         profile allows gets a usable one: {SUDO}; {profile}; {CONTAINER}"
    )
}

/// Whether only root may change what `path` names, so that a profile bound
/// to it gives a user namespace to no one else's program: `path` absolute,
/// with no `..`; `/`, each directory below it and the file itself owned by
/// root and not writable by group or others (an ACL that grants writing
/// shows in the group bits), none a symlink or on FUSE; the file a regular
/// one. Any error says no.
fn only_root_changes(path: &Path) -> bool {
    walk(path).is_ok_and(|nodes| root_only(&nodes))
}

/// What [`walk`] found of one component of a path.
#[derive(Clone, Copy, Debug)]
struct Node {
    kind: FileType,
    uid: u32,
    /// The permission bits, sticky included.
    mode: u32,
    fuse: bool,
}

/// `/` and each component of `path` below it, opened `O_PATH` from the one
/// above without following a symlink: a symlink is a node of its own, and
/// nothing is found below it.
fn walk(path: &Path) -> rustix::io::Result<Vec<Node>> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(Errno::INVAL);
    }
    let how = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut at = rustix::fs::openat(CWD, "/", how | OFlags::DIRECTORY, Mode::empty())?;
    let mut nodes = vec![node(&at)?];
    for component in components {
        let Component::Normal(name) = component else {
            return Err(Errno::INVAL);
        };
        at = rustix::fs::openat(&at, name, how, Mode::empty())?;
        nodes.push(node(&at)?);
    }
    Ok(nodes)
}

fn node(fd: &OwnedFd) -> rustix::io::Result<Node> {
    let st = rustix::fs::fstat(fd)?;
    let fs = rustix::fs::fstatfs(fd)?;
    Ok(Node {
        kind: FileType::from_raw_mode(st.st_mode),
        uid: st.st_uid,
        mode: st.st_mode & 0o7777,
        fuse: fs.f_type == FUSE_SUPER_MAGIC,
    })
}

/// [`only_root_changes`]'s rule on what [`walk`] found, `/` first.
fn root_only(nodes: &[Node]) -> bool {
    let Some((file, dirs)) = nodes.split_last() else {
        return false;
    };
    !dirs.is_empty()
        && file.kind == FileType::RegularFile
        && dirs.iter().all(|d| d.kind == FileType::Directory)
        && nodes
            .iter()
            .all(|n| n.uid == 0 && n.mode & 0o022 == 0 && !n.fuse)
}

#[cfg(test)]
mod tests;
