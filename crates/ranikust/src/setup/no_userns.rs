//! The advice for a user other than root who gets no usable user namespace
//! (`design/modes-and-detection.md#no-usable-user-namespace`), in order:
//! `sudo`; where `kernel.apparmor_restrict_unprivileged_userns` is 1,
//! ranikust's AppArmor profile (`contrib/apparmor/ranikust`), for the
//! binary's own path where only root may change it, else after installing
//! the binary where only root may; a container.

use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{CWD, FileType, FsWord, Mode, OFlags};
use rustix::io::Errno;

/// The path the repository's profile binds.
const PROFILE_PATH: &str = "/usr/local/bin/ranikust";

/// The directory of [`PROFILE_PATH`].
const PROFILE_DIR: &str = "/usr/local/bin";

/// The profile, and where to read it.
const PROFILE: &str = "ranikust's AppArmor profile (contrib/apparmor/ranikust in its \
     repository; the book's \"Choosing where to run it\" shows it)";

/// FUSE's `f_type`: its server, which any user may run, reports the owners
/// and modes.
const FUSE_SUPER_MAGIC: FsWord = 0x6573_5546;

/// The advice for this host, for `binary`, the path ranikust started from
/// (`None` where it could not be read): the profile only where AppArmor
/// restricts user namespaces, for that path only where root alone may
/// change it.
pub(crate) fn guidance(binary: Option<&Path>) -> String {
    if !restricted() {
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
/// grants writing shows in the group bits), none a symlink or on FUSE.
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
    fuse: bool,
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
    Ok(Node {
        kind: FileType::from_raw_mode(st.st_mode),
        uid: st.st_uid,
        mode: st.st_mode & 0o7777,
        fuse: fs.f_type == FUSE_SUPER_MAGIC,
    })
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
        if n.fuse {
            return Err(format!("{at} is on FUSE, whose server reports its owners"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
