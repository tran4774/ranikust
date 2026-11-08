//! The subordinate ids of the user ranikust runs as, for the build's user
//! namespace (`rnk_sandbox::enter_build_user_namespace`): the first range
//! `/etc/subuid` and `/etc/subgid` give the user, by name or by uid, as
//! `newuidmap` and `newgidmap` read them; for root's `--userns`, the
//! `ranikust` entry alone ([`root_range`], ADR-0104). Read on the host
//! before the namespace is entered. The name comes from `/etc/passwd` only:
//! a static binary has no NSS, so a user known only to a directory service
//! is matched by uid.

use rnk_sandbox::{IdRange, SubordinateIds};

/// The files read, in a test's directory or `/etc`.
const PASSWD: &str = "passwd";
const GROUP: &str = "group";
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
/// [`ROOT_ENTRY`], by that name only, never a uid, sharing no id with the
/// host's accounts or other ranges (ADR-0139): the uid range none of
/// `/etc/passwd`'s uids, the gid range none of `/etc/group`'s gids or of
/// `/etc/passwd`'s primary gids, and neither another well-formed line of
/// its file. Those local files only: a static binary has no NSS, so an id
/// only a directory service knows is not seen. Read on the host before
/// anything is written.
///
/// # Errors
///
/// Why there is none to use: a file without one (a file that does not
/// exist has none), one whose range starts at 0, which would map the
/// host's root, one that holds an account's id or overlaps another line,
/// naming it, or a file that cannot be read. [`ROOT_RANGE_GUIDANCE`]
/// says what to do.
pub(crate) fn root_range() -> Result<SubordinateIds, String> {
    root_range_in(std::path::Path::new("/etc"))
}

/// [`root_range`] with the files in `etc`.
fn root_range_in(etc: &std::path::Path) -> Result<SubordinateIds, String> {
    let passwd = read_in(etc, PASSWD)?;
    let group = read_in(etc, GROUP)?;
    let path = |name: &str| etc.join(name).display().to_string();
    let (passwd_path, group_path) = (path(PASSWD), path(GROUP));
    let uids = ids_of(&passwd, 2, "uid", &passwd_path).collect::<Vec<_>>();
    let gids = ids_of(&group, 2, "gid", &group_path)
        .chain(ids_of(&passwd, 3, "gid", &passwd_path))
        .collect::<Vec<_>>();
    let range = |name: &str, held: &[Held]| -> Result<IdRange, String> {
        let bytes = read_in(etc, name)?;
        let path = path(name);
        let lines = bytes.split(|b| *b == b'\n').enumerate().collect::<Vec<_>>();
        let found = lines.iter().find_map(|(i, line)| {
            parse_range(std::str::from_utf8(line).ok()?)
                .filter(|(owner, _)| *owner == ROOT_ENTRY)
                .map(|(_, r)| (*i, r))
        });
        let Some((at, r)) = found else {
            return Err(format!(
                "{path} has no {ROOT_ENTRY} range for root's builds"
            ));
        };
        if r.start == 0 {
            return Err(format!(
                "the {ROOT_ENTRY} range of {path} starts at 0, which would make the build's root \
                 the host's"
            ));
        }
        let ours = format!("the {ROOT_ENTRY} range {}:{} of {path}", r.start, r.count);
        let (start, end) = (u64::from(r.start), u64::from(r.start) + u64::from(r.count));
        let other = lines.iter().find(|(i, line)| {
            *i != at && loose_range(line).is_some_and(|(s, e)| start < e && s < end)
        });
        if let Some((i, line)) = other {
            return Err(format!(
                "{ours} overlaps line {} of {path}, {}: the build's ids would be another's",
                i + 1,
                String::from_utf8_lossy(line)
            ));
        }
        if let Some(h) = held.iter().find(|h| (start..end).contains(&h.id.into())) {
            return Err(format!(
                "{ours} holds {} {} of {} in {}: the build's ids would be that account's",
                h.kind, h.id, h.name, h.file
            ));
        }
        Ok(r)
    };
    Ok(SubordinateIds {
        uids: range(SUBUID, &uids)?,
        gids: range(SUBGID, &gids)?,
    })
}

/// An id an account or group of the host has, for [`root_range_in`]'s
/// messages.
struct Held {
    /// `uid` or `gid`.
    kind: &'static str,
    id: u32,
    name: String,
    file: String,
}

/// The ids in field `field` (from 0) of each line of `bytes`, a
/// `/etc/passwd` or `/etc/group` (`name:x:id:...`), with the line's name,
/// each read as [`loose`] reads it.
fn ids_of<'a>(
    bytes: &'a [u8],
    field: usize,
    kind: &'static str,
    file: &'a str,
) -> impl Iterator<Item = Held> + 'a {
    bytes.split(|b| *b == b'\n').filter_map(move |line| {
        let mut f = line.split(|b| *b == b':');
        let name = f.next()?;
        let id = loose(f.nth(field - 1)?)?;
        Some(Held {
            kind,
            id,
            name: String::from_utf8_lossy(name).into_owned(),
            file: file.to_owned(),
        })
    })
}

/// The ids, from the first to one past the last, that a line of
/// `/etc/subuid` or `/etc/subgid` gives whatever its owner, its numbers
/// read as [`loose`] reads them: for the overlap check, a range another
/// tool may take counts even where it is not well-formed here.
fn loose_range(line: &[u8]) -> Option<(u64, u64)> {
    let mut f = line.split(|b| *b == b':');
    let (_, start, count) = (f.next()?, f.next()?, f.next()?);
    if f.next().is_some() {
        return None;
    }
    let (start, count) = (u64::from(loose(start)?), u64::from(loose(count)?));
    (count >= 1).then_some((start, start + count))
}

/// A decimal `u32` as `strtoul` reads one in base 10 where the whole field
/// must be the number: white space and a `+` before it allowed.
fn loose(field: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(field).ok()?.trim_start();
    decimal(s.strip_prefix('+').unwrap_or(s))
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
        let (owner, range) = parse_range(line)?;
        is_owner(owner).then_some(range)
    })
}

/// The owner and range of `line` where it is well-formed, by
/// [`first_range`]'s rules.
fn parse_range(line: &str) -> Option<(&str, IdRange)> {
    let mut f = line.split(':');
    let (owner, start, count) = (f.next()?, f.next()?, f.next()?);
    if f.next().is_some() {
        return None;
    }
    let (start, count) = (decimal(start)?, decimal(count)?);
    (count >= 1 && start.checked_add(count).is_some()).then_some((owner, IdRange { start, count }))
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
