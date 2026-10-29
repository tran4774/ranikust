//! `USER` and `--chown` names, looked up in the root's `/etc/passwd` and
//! `/etc/group`, never in the host's.
//!
//! Both files are read through [`RootPath`], so a symlinked `/etc/passwd`
//! resolves inside the root. A missing file is an empty database. Malformed
//! lines are skipped rather than read as id 0.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::root::RootPath;

/// Largest `/etc/passwd` or `/etc/group` read; a bigger one is refused.
const DB_LIMIT: u64 = 16 << 20;

/// Largest id accepted, as runc: `chown(2)` reads `u32::MAX` as "unchanged",
/// and ids above `i32::MAX` break tools that use signed ids.
const MAX_ID: u32 = 0x7fff_ffff;

/// The credentials of a `USER`, resolved in the root, for `rnk-sandbox`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    /// User id.
    pub uid: u32,
    /// Primary group id.
    pub gid: u32,
    /// Supplementary groups: the groups listing the user as a member, when
    /// no group was given and the user is in `/etc/passwd`.
    pub supplementary_gids: Vec<u32>,
    /// Home directory from `/etc/passwd`, `/` when unknown or empty.
    pub home: PathBuf,
}

/// The owner a `--chown` sets, resolved in the root, for `rnk-builder`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    /// User id.
    pub uid: u32,
    /// Group id.
    pub gid: u32,
}

/// Resolves a `USER` value, `user[:group]`, as Docker and runc do.
///
/// - `user` is a name in `/etc/passwd`, else a numeric uid (looked up by uid
///   for its group and home; a uid with no entry gets gid 0 and home `/`).
/// - `group` is a name in `/etc/group`, else a numeric gid. Without it, the
///   gid is the passwd entry's, and the supplementary groups are those that
///   list the user.
///
/// # Errors
///
/// [`Error::User`] for an unknown name, an empty part or an id above
/// `2147483647`; [`Error::Io`] when a database cannot be read.
pub fn lookup_user(root: &RootPath, spec: &str) -> Result<User> {
    let (user, group) = split_spec(spec)?;
    let passwd = read_db(root, "/etc/passwd")?;
    let entry = find_user(&passwd, user)?;
    let (uid, primary, home, name) = match entry {
        Some(e) => (e.uid, e.gid, e.home, Some(e.name)),
        None => match parse_id(user)? {
            Some(uid) => (uid, 0, &b""[..], None),
            None => return Err(unknown("user", user, "/etc/passwd")),
        },
    };
    let home = if home.is_empty() {
        PathBuf::from("/")
    } else {
        PathBuf::from(OsStr::from_bytes(home))
    };
    let (gid, supplementary_gids) = match group {
        Some(g) => {
            let groups = read_db(root, "/etc/group")?;
            (resolve_group(&groups, g)?, Vec::new())
        }
        None => {
            let mut gids = Vec::new();
            if let Some(name) = name {
                let groups = read_db(root, "/etc/group")?;
                for g in groups_of(&groups) {
                    if g.members().any(|m| m == name) && !gids.contains(&g.gid) {
                        gids.push(g.gid);
                    }
                }
            }
            (primary, gids)
        }
    };
    Ok(User {
        uid,
        gid,
        supplementary_gids,
        home,
    })
}

/// Resolves a `--chown` value, `user[:group]`, as kaniko and Docker do.
///
/// `user` and `group` are names in the root's databases, else numeric ids.
/// Without a group, the gid is the uid (Docker's documented `--chown` rule,
/// `kaniko@44aa7ddc0 pkg/util/command_util.go:495`), not the user's
/// primary group.
///
/// # Errors
///
/// As [`lookup_user`].
pub fn lookup_owner(root: &RootPath, spec: &str) -> Result<Owner> {
    let (user, group) = split_spec(spec)?;
    let passwd = read_db(root, "/etc/passwd")?;
    let uid = match passwd_of(&passwd).find(|e| e.name == user.as_bytes()) {
        Some(e) => e.uid,
        None => parse_id(user)?.ok_or_else(|| unknown("user", user, "/etc/passwd"))?,
    };
    let gid = match group {
        Some(g) => resolve_group(&read_db(root, "/etc/group")?, g)?,
        None => uid,
    };
    Ok(Owner { uid, gid })
}

/// Splits `user[:group]`; both parts must be non-empty.
fn split_spec(spec: &str) -> Result<(&str, Option<&str>)> {
    let (user, group) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    if user.is_empty() || group.is_some_and(str::is_empty) {
        return Err(Error::User(format!(
            "invalid user specification {spec:?}: expected user[:group]"
        )));
    }
    Ok((user, group))
}

/// A decimal id, `None` when `s` is not all digits.
fn parse_id(s: &str) -> Result<Option<u32>> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    match s.parse::<u32>() {
        Ok(id) if id <= MAX_ID => Ok(Some(id)),
        _ => Err(Error::User(format!(
            "id {s} is out of range (0 to {MAX_ID})"
        ))),
    }
}

fn unknown(kind: &str, name: &str, db: &str) -> Error {
    Error::User(format!(
        "{kind} {name:?} is neither in the root's {db} nor numeric"
    ))
}

/// Reads `p` through the root; a missing file is empty.
fn read_db(root: &RootPath, p: &str) -> Result<Vec<u8>> {
    let fd = match root.open_regular(p) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NOENT.raw_os_error()) => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };
    let mut data = Vec::new();
    std::fs::File::from(fd)
        .take(DB_LIMIT + 1)
        .read_to_end(&mut data)
        .map_err(|source| Error::Io {
            op: "read",
            path: Path::new(p).to_owned(),
            source,
        })?;
    if u64::try_from(data.len()).unwrap_or(u64::MAX) > DB_LIMIT {
        return Err(Error::User(format!(
            "{p} in the root is larger than {DB_LIMIT} bytes"
        )));
    }
    Ok(data)
}

struct PasswdEntry<'a> {
    name: &'a [u8],
    uid: u32,
    gid: u32,
    home: &'a [u8],
}

struct GroupEntry<'a> {
    name: &'a [u8],
    gid: u32,
    members: &'a [u8],
}

impl<'a> GroupEntry<'a> {
    fn members(&self) -> impl Iterator<Item = &'a [u8]> {
        self.members
            .split(|&b| b == b',')
            .map(<[u8]>::trim_ascii)
            .filter(|m| !m.is_empty())
    }
}

/// An id field of a database line; out-of-range or malformed ids skip the
/// line.
fn db_id(field: Option<&[u8]>) -> Option<u32> {
    let s = std::str::from_utf8(field?).ok()?;
    parse_id(s).ok().flatten()
}

/// Lines worth parsing: not empty, not a comment, not NIS `+`/`-` syntax.
fn db_lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.is_empty() && !matches!(l.first(), Some(b'#' | b'+' | b'-')))
}

/// `name:password:uid:gid:gecos:home:shell`.
fn passwd_of(data: &[u8]) -> impl Iterator<Item = PasswdEntry<'_>> {
    db_lines(data).filter_map(|line| {
        let mut f = line.split(|&b| b == b':');
        let name = f.next()?;
        f.next()?;
        let uid = db_id(f.next())?;
        let gid = db_id(f.next())?;
        f.next();
        let home = f.next().unwrap_or_default();
        (!name.is_empty()).then_some(PasswdEntry {
            name,
            uid,
            gid,
            home,
        })
    })
}

/// `name:password:gid:member,member`.
fn groups_of(data: &[u8]) -> impl Iterator<Item = GroupEntry<'_>> {
    db_lines(data).filter_map(|line| {
        let mut f = line.split(|&b| b == b':');
        let name = f.next()?;
        f.next()?;
        let gid = db_id(f.next())?;
        let members = f.next().unwrap_or_default();
        (!name.is_empty()).then_some(GroupEntry { name, gid, members })
    })
}

/// The passwd entry named `user`, else the first with that numeric uid.
fn find_user<'a>(passwd: &'a [u8], user: &str) -> Result<Option<PasswdEntry<'a>>> {
    if let Some(e) = passwd_of(passwd).find(|e| e.name == user.as_bytes()) {
        return Ok(Some(e));
    }
    Ok(match parse_id(user)? {
        Some(uid) => passwd_of(passwd).find(|e| e.uid == uid),
        None => None,
    })
}

/// A group name in `/etc/group`, else a numeric gid.
fn resolve_group(groups: &[u8], g: &str) -> Result<u32> {
    if let Some(e) = groups_of(groups).find(|e| e.name == g.as_bytes()) {
        return Ok(e.gid);
    }
    parse_id(g)?.ok_or_else(|| unknown("group", g, "/etc/group"))
}

#[cfg(test)]
mod tests;
