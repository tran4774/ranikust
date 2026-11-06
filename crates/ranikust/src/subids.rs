//! The subordinate ids of the user ranikust runs as, for the build's user
//! namespace (`rnk_sandbox::enter_build_user_namespace`): the first range
//! `/etc/subuid` and `/etc/subgid` give the user, by name or by uid, as
//! `newuidmap` and `newgidmap` read them. Read on the host before the
//! namespace is entered. The name comes from `/etc/passwd` only: a static
//! binary has no NSS, so a user known only to a directory service is
//! matched by uid.

use rnk_sandbox::{IdRange, SubordinateIds};

/// The files read, in a test's directory or `/etc`.
const PASSWD: &str = "passwd";
const SUBUID: &str = "subuid";
const SUBGID: &str = "subgid";

/// The subordinate ids of `uid` under `/etc`, `None` for root, which maps
/// its own ids, or where either file has no usable range for the user.
///
/// # Errors
///
/// A file there that cannot be read, but for one that does not exist,
/// which gives no range: the file and why.
pub(crate) fn lookup(uid: u32) -> Result<Option<SubordinateIds>, String> {
    lookup_in(std::path::Path::new("/etc"), uid)
}

/// [`lookup`] with the files in `etc`.
fn lookup_in(etc: &std::path::Path, uid: u32) -> Result<Option<SubordinateIds>, String> {
    if uid == 0 {
        return Ok(None);
    }
    let read = |name: &str| {
        let path = etc.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(valid_lines(&bytes)),
            // A missing file gives no range: the build maps one uid and says so.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    };
    let passwd = read(PASSWD)?;
    let name = user_name(&passwd, uid);
    let (subuid, subgid) = (read(SUBUID)?, read(SUBGID)?);
    Ok(first_range(&subuid, name, uid)
        .zip(first_range(&subgid, name, uid))
        .map(|(uids, gids)| SubordinateIds { uids, gids }))
}

/// The lines of `bytes` that are UTF-8: another (a Latin-1 comment field
/// of `/etc/passwd`) names no one this lookup could match.
fn valid_lines(bytes: &[u8]) -> String {
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The name of `uid` in `passwd` (`name:x:uid:...`), its first entry.
fn user_name(passwd: &str, uid: u32) -> Option<&str> {
    passwd.lines().find_map(|line| {
        let mut f = line.split(':');
        let (name, _, id) = (f.next()?, f.next()?, f.next()?);
        (!name.is_empty() && id.parse::<u32>().ok()? == uid).then_some(name)
    })
}

/// The first well-formed range (`owner:start:count`) of `text` whose owner
/// is `name` or `uid` in decimal: a count of at least one, whose end
/// (`start + count`) fits in 32 bits, as the kernel takes a map's line.
fn first_range(text: &str, name: Option<&str>, uid: u32) -> Option<IdRange> {
    let uid = uid.to_string();
    text.lines().find_map(|line| {
        let mut f = line.split(':');
        let (owner, start, count) = (f.next()?, f.next()?, f.next()?);
        if f.next().is_some() || (Some(owner) != name && owner != uid) {
            return None;
        }
        let (start, count) = (decimal(start)?, decimal(count)?);
        (count >= 1 && start.checked_add(count).is_some()).then_some(IdRange { start, count })
    })
}

/// A plain decimal `u32`, without the sign or spaces `str::parse` takes.
fn decimal(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests;
