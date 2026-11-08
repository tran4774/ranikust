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
    let read = |name: &str| read_in(etc, name);
    let passwd = read(PASSWD)?;
    let name = user_name(&passwd, uid);
    let (subuid, subgid) = (valid_lines(&read(SUBUID)?), valid_lines(&read(SUBGID)?));
    Ok(first_range(&subuid, name, uid)
        .zip(first_range(&subgid, name, uid))
        .map(|(uids, gids)| SubordinateIds { uids, gids }))
}

/// The file `name` of `etc`; one that does not exist is empty: it gives no
/// range.
fn read_in(etc: &std::path::Path, name: &str) -> Result<Vec<u8>, String> {
    let path = etc.join(name);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The owner of root's range for `--userns` in `/etc/subuid` and
/// `/etc/subgid` (ADR-0104): an entry of ranikust's own, which neither
/// podman, which allocates its containers' ranges from `containers`, nor
/// the tools that use `root` know of.
pub(crate) const ROOT_ENTRY: &str = "ranikust";

/// What to do where root has no range for `--userns` (ADR-0104): the
/// lines to add, the start left to the admin.
pub(crate) const ROOT_RANGE_GUIDANCE: &str = "add the line ranikust:<start>:65536 to both \
     /etc/subuid and /etc/subgid, with a <start> above 0 that no account and no other range \
     uses, such as one past the last range of those files (ADR-0104)";

/// Root's range for `--userns` (ADR-0104): in each of `/etc/subuid` and
/// `/etc/subgid`, the first well-formed range whose owner is
/// [`ROOT_ENTRY`], by that name only, never a uid. Read on the host before
/// anything is written.
///
/// # Errors
///
/// Why there is none to use: a file without one (a file that does not
/// exist has none), one whose range starts at 0, which would map the
/// host's root, or a file that cannot be read. [`ROOT_RANGE_GUIDANCE`]
/// says what to do.
pub(crate) fn root_range() -> Result<SubordinateIds, String> {
    root_range_in(std::path::Path::new("/etc"))
}

/// [`root_range`] with the files in `etc`.
fn root_range_in(etc: &std::path::Path) -> Result<SubordinateIds, String> {
    let range = |name: &str| -> Result<IdRange, String> {
        let text = valid_lines(&read_in(etc, name)?);
        let path = etc.join(name);
        match range_of(&text, |owner| owner == ROOT_ENTRY) {
            None => Err(format!(
                "{} has no {ROOT_ENTRY} range for root's builds",
                path.display()
            )),
            Some(r) if r.start == 0 => Err(format!(
                "the {ROOT_ENTRY} range of {} starts at 0, which would make the build's root the \
                 host's",
                path.display()
            )),
            Some(r) => Ok(r),
        }
    };
    Ok(SubordinateIds {
        uids: range(SUBUID)?,
        gids: range(SUBGID)?,
    })
}

/// The lines of `bytes` that are UTF-8: a line that is not is no
/// well-formed range of a UTF-8 name or a decimal uid.
fn valid_lines(bytes: &[u8]) -> String {
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The name of `uid` in `passwd` (`name:x:uid:...`), its first entry; only
/// the name and the uid need be UTF-8, not the comment field after them.
fn user_name(passwd: &[u8], uid: u32) -> Option<&str> {
    passwd.split(|b| *b == b'\n').find_map(|line| {
        let mut f = line.split(|b| *b == b':');
        let (name, _, id) = (f.next()?, f.next()?, f.next()?);
        let name = std::str::from_utf8(name).ok().filter(|n| !n.is_empty())?;
        (decimal(std::str::from_utf8(id).ok()?)? == uid).then_some(name)
    })
}

/// The first well-formed range (`owner:start:count`) of `text` whose owner
/// is `name` or `uid` in decimal: a count of at least one, whose end
/// (`start + count`) fits in 32 bits, as the kernel takes a map's line.
fn first_range(text: &str, name: Option<&str>, uid: u32) -> Option<IdRange> {
    let uid = uid.to_string();
    range_of(text, |owner| Some(owner) == name || owner == uid)
}

/// The first well-formed range of `text` whose owner `is_owner` takes, by
/// [`first_range`]'s rules.
fn range_of(text: &str, is_owner: impl Fn(&str) -> bool) -> Option<IdRange> {
    text.lines().find_map(|line| {
        let mut f = line.split(':');
        let (owner, start, count) = (f.next()?, f.next()?, f.next()?);
        if f.next().is_some() || !is_owner(owner) {
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
