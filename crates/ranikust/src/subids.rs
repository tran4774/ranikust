//! The subordinate ids of the user ranikust runs as, for the build's user
//! namespace (`rnk_sandbox::enter_build_user_namespace`): the first range
//! `/etc/subuid` and `/etc/subgid` give the user, by name or by uid, in
//! decimal (`newuidmap` and `newgidmap` read a leading 0 as octal, so such
//! a line is passed over); for root's `--userns`, the `ranikust` entry
//! alone ([`root_range`], ADR-0104). Read on the host before the namespace
//! is entered. The name comes from `/etc/passwd` only: a static binary has
//! no NSS, so a user known only through NSS is matched by uid.

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
/// `/etc/subgid` (ADR-0104): an entry of ranikust's own. Podman allocates
/// its containers' ranges from `containers`, other tools from `root`, but
/// `newuidmap` and `newgidmap` give a line to the account its owner names,
/// so [`root_range`] refuses where `/etc/passwd` or systemd's userdb has
/// one of this name.
pub(crate) const ROOT_ENTRY: &str = "ranikust";

/// What to do where root has no range for `--userns` (ADR-0104): the
/// lines to add, the start left to the admin.
pub(crate) const ROOT_RANGE_GUIDANCE: &str = "add the line ranikust:<start>:65536 to both \
     /etc/subuid and /etc/subgid, in decimal with no leading 0, with a <start> above 0 that \
     no account, no other range and no other allocator of ids (an NSS subid module, the \
     kubelet, systemd) uses, and no account named ranikust (ADR-0104)";

/// Root's range for `--userns` (ADR-0104): in each of `/etc/subuid` and
/// `/etc/subgid`, the first well-formed range whose owner is [`ROOT_ENTRY`],
/// by that name only, never a uid, sharing no id with the host's accounts or
/// other ranges (ADR-0139): no account named [`ROOT_ENTRY`] in `/etc/passwd`
/// or as a userdb record, which `newuidmap` and `newgidmap` would let map the
/// range; the uid range none of `/etc/passwd`'s uids, the gid range none of
/// `/etc/group`'s gids or of `/etc/passwd`'s primary gids; and neither any
/// range another line of its file may give, as [`ranges_shadow_may_read`]
/// counts them. Those local files only, and the names of userdb's drop-in
/// records: a static binary has no NSS, so an account or id only NSS knows
/// (LDAP, SSSD, systemd's userdb records' ids, `systemd-homed` and
/// `DynamicUser` accounts) is not seen, nor are the ranges another allocator
/// hands out (an NSS `subid:` module, which `newuidmap` then reads instead of
/// the files and of which a warning is logged; the kubelet's for pods;
/// systemd's `DynamicUser` and `systemd-nspawn --private-users=pick`). Lines
/// end at `\n` alone, as shadow's own parser reads them: a `ranikust` line
/// ending in `\r` is not well-formed, so not found, unlike [`lookup`], which
/// takes `\r\n` as a line end too. Read on the host before anything is
/// written.
///
/// # Errors
///
/// Why there is none to use: a file without one (a file that does not
/// exist has none), one whose range starts at 0, which would map the
/// host's root, an account named [`ROOT_ENTRY`], one that holds an
/// account's id, overlaps another line or sits beside a line whose numbers
/// cannot be read as shadow's tools may read them, naming it, a file that
/// cannot be read, or a userdb record of which it cannot tell whether it
/// exists. [`ROOT_RANGE_GUIDANCE`] says what to do.
pub(crate) fn root_range() -> Result<SubordinateIds, String> {
    let etc = std::path::Path::new("/etc");
    let userdb = USERDB_DIRS.map(std::path::Path::new);
    if let Some(record) = userdb_record(&userdb)? {
        return Err(format!(
            "{} is a file named like a systemd userdb record of an account named \
             {ROOT_ENTRY}, to which newuidmap and newgidmap would give root's {ROOT_ENTRY} \
             range",
            record.display()
        ));
    }
    let ids = root_range_in(etc)?;
    match read_in(etc, NSSWITCH) {
        Ok(text) => {
            if let Some(module) = subid_module(&text) {
                tracing::warn!(
                    "ranikust: {} names the subid module {module:?}: newuidmap and newgidmap \
                     take ranges from it, not from /etc/subuid and /etc/subgid, so --userns \
                     cannot tell whether it hands out ids of the ranikust range; give that \
                     range a <start> it does not use",
                    etc.join(NSSWITCH).display()
                );
            }
        }
        Err(e) => tracing::warn!(
            "ranikust: cannot tell whether an NSS subid module hands out ids of the ranikust \
             range: {e}"
        ),
    }
    Ok(ids)
}

/// The NSS configuration, whose `subid:` line libsubid reads.
const NSSWITCH: &str = "nsswitch.conf";

/// Where systemd's userdb looks for an account's drop-in record,
/// `<name>.user` (`nss-systemd`, ADR-0139), in its order, with `/lib/userdb`
/// of a split `/usr`.
const USERDB_DIRS: [&str; 6] = [
    "/etc/userdb",
    "/run/userdb",
    "/run/host/userdb",
    "/usr/local/lib/userdb",
    "/usr/lib/userdb",
    "/lib/userdb",
];

/// The first of `dirs` to hold a userdb record named for [`ROOT_ENTRY`],
/// of any type: through `nss-systemd`, `newuidmap` would find that account.
///
/// # Errors
///
/// Where whether one is there cannot be told.
fn userdb_record(dirs: &[&std::path::Path]) -> Result<Option<std::path::PathBuf>, String> {
    for dir in dirs {
        let record = dir.join(format!("{ROOT_ENTRY}.user"));
        match std::fs::symlink_metadata(&record) {
            Ok(_) => return Ok(Some(record)),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(e) => {
                return Err(format!(
                    "cannot tell whether {} exists: {e}",
                    record.display()
                ));
            }
        }
    }
    Ok(None)
}

/// The module the first `subid:` line of `nsswitch` that names one names
/// first, where it is not `files`, as libsubid reads the file: from column
/// 0, in any case, `#` lines passed over.
fn subid_module(nsswitch: &[u8]) -> Option<String> {
    nsswitch.split(|b| *b == b'\n').find_map(|line| {
        let rest = line
            .get(..6)
            .filter(|key| key.eq_ignore_ascii_case(b"subid:"))
            .map(|_| &line[6..])?;
        let module = without_c_space(rest)
            .split(|b| matches!(b, b' ' | b'\t'))
            .next()
            .filter(|t| !t.is_empty())?;
        Some((module != b"files").then(|| String::from_utf8_lossy(module).into_owned()))
    })?
}

/// [`root_range`] with the files in `etc`.
fn root_range_in(etc: &std::path::Path) -> Result<SubordinateIds, String> {
    let passwd = read_in(etc, PASSWD)?;
    let group = read_in(etc, GROUP)?;
    let path = |name: &str| etc.join(name).display().to_string();
    let (passwd_path, group_path) = (path(PASSWD), path(GROUP));
    // glibc passes over white space before a line's name; `+name` is NIS
    // compat's way to bring in that account.
    let named = passwd.split(|b| *b == b'\n').position(|line| {
        let name = without_c_space(line).split(|b| *b == b':').next();
        let name = name.map(|n| n.strip_prefix(b"+").unwrap_or(n));
        name == Some(ROOT_ENTRY.as_bytes())
    });
    if let Some(i) = named {
        return Err(format!(
            "line {} of {passwd_path} is an account named {ROOT_ENTRY}, to which newuidmap and \
             newgidmap would give root's {ROOT_ENTRY} range",
            i + 1
        ));
    }
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
        let (start, end) = (
            u128::from(r.start),
            u128::from(r.start) + u128::from(r.count),
        );
        for (i, line) in lines.iter().filter(|(i, _)| *i != at) {
            let n = i + 1;
            let ranges = ranges_shadow_may_read(line).map_err(|why| {
                format!(
                    "line {n} of {path}, {}, is one that versions of newuidmap and newgidmap may \
                     read differently ({why}), so whether it overlaps {ours} is not known: write \
                     its start and count in decimal, or remove it",
                    shown(line)
                )
            })?;
            if ranges.iter().any(|&(s, e)| start < e && s < end) {
                return Err(format!(
                    "{ours} overlaps line {n} of {path}, {}: the build's ids would be another's",
                    shown(line)
                ));
            }
        }
        if let Some(h) = held.iter().find(|h| (start..end).contains(&h.id.into())) {
            let id = match &h.written {
                Some(written) => format!("{written} ({} as musl reads it)", h.id),
                None => h.id.to_string(),
            };
            return Err(format!(
                "{ours} holds {} {id} of {} in {}: the build's ids would be that account's",
                h.kind,
                shown(h.name.as_bytes()),
                h.file
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
    /// The field as written, where musl wraps it into `id`.
    written: Option<String>,
    name: String,
    file: String,
}

/// `line` for a message: lossy UTF-8 cut after 80 characters, what is not
/// printable escaped.
fn shown(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    match text.char_indices().nth(80) {
        Some((cut, _)) => format!("{}...", text[..cut].escape_debug()),
        None => text.escape_debug().to_string(),
    }
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
        let written = f.nth(field - 1)?;
        let (id, wrapped) = loose(written)?;
        Some(Held {
            kind,
            id,
            written: wrapped.then(|| shown(without_c_space(written))),
            name: String::from_utf8_lossy(name).into_owned(),
            file: file.to_owned(),
        })
    })
}

/// The ids, from the first to one past the last, that some version of
/// shadow's tools may take a line of `/etc/subuid` or `/etc/subgid` to
/// give, whatever its owner, for the overlap check: a range another tool
/// may honour counts even where it is not well-formed here, and so does
/// each reading of it. Shadow 4.13 and 4.17 honour the first three fields
/// of a line with more, a trailing `:` and a count past 32 bits, which
/// later versions refuse; all read the numbers as `strtoul` in base 0. So
/// the first three fields of a line with at least three count, a trailing
/// `\r` trimmed (no version honours such a line, so counting it is the
/// stricter reading), each number read by [`readings`], and the ids in
/// 128 bits, never cut. A count of 0 from 0 is every id, as shadow's
/// `start + count - 1` makes it. A line of fewer fields gives none: every
/// version skips it.
///
/// # Errors
///
/// What makes the line one shadow's versions read differently: a NUL byte
/// (where it ends a line differs), or a start or count [`readings`] cannot
/// read.
fn ranges_shadow_may_read(line: &[u8]) -> Result<Vec<(u128, u128)>, &'static str> {
    if line.contains(&0) {
        return Err("a NUL byte");
    }
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut f = line.split(|b| *b == b':');
    let (Some(_), Some(start), Some(count)) = (f.next(), f.next(), f.next()) else {
        return Ok(Vec::new());
    };
    let (starts, counts) = (readings(start)?, readings(count)?);
    Ok(starts
        .iter()
        .flat_map(|&s| counts.iter().map(move |&c| (s, c)))
        .filter_map(|(s, c)| match (s, c) {
            (0, 0) => Some((0, u128::MAX)),
            (_, 0) => None,
            _ => Some((s, s.saturating_add(c))),
        })
        .collect())
}

/// The values a start or count of a subordinate id line may be read as:
/// as `strtoul` reads one in base 0 where the whole field must be the
/// number (C white space and a `+` before it, `0x` or `0X` for hex, a
/// leading 0 for octal), and, for a leading 0, in decimal too, as other
/// tools read it. Each fits in 64 bits but a decimal reading, which may be
/// more.
///
/// # Errors
///
/// Why it is not such a number: a sign, which `strtoul` takes and the
/// versions read differently; anything else in the field, an empty one
/// included, and `0b` or `0B`, binary to glibc 2.38 and later; a value
/// past 64 bits, which `strtoul` refuses.
fn readings(field: &[u8]) -> Result<Vec<u128>, &'static str> {
    const NOT: &str = "not a number";
    let s = without_c_space(field);
    let s = match s.first() {
        Some(b'+') => &s[1..],
        Some(b'-') => return Err("a sign"),
        _ => s,
    };
    let digits = |s: &[u8], radix: u32| -> Result<u128, &'static str> {
        let mut v: u128 = 0;
        for b in s {
            let d = char::from(*b).to_digit(radix).ok_or(NOT)?;
            v = v * u128::from(radix) + u128::from(d);
            if v > u128::from(u64::MAX) {
                return Err("past 64 bits");
            }
        }
        Ok(v)
    };
    if s.is_empty() {
        return Err(NOT);
    }
    if let Some(hex) = s.strip_prefix(b"0x").or_else(|| s.strip_prefix(b"0X")) {
        // `strtoul` reads a bare `0x` as 0 followed by junk.
        return if hex.is_empty() {
            Err(NOT)
        } else {
            digits(hex, 16).map(|v| vec![v])
        };
    }
    if s.len() > 1 && s[0] == b'0' {
        let octal = digits(s, 8)?;
        let decimal = s.iter().fold(0u128, |v, b| {
            v.saturating_mul(10).saturating_add(u128::from(b - b'0'))
        });
        return Ok(vec![octal, decimal]);
    }
    digits(s, 10).map(|v| vec![v])
}

/// `bytes` without the white space before it, as `isspace` takes it in
/// the C locale, which `newuidmap` and `newgidmap` keep.
fn without_c_space(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

/// A `/etc/passwd` or `/etc/group` id in base 10, as glibc and musl read
/// one: the whole field the number, C white space and a `+` before it
/// allowed (glibc), and past 32 bits wrapped, as musl reads it (64-bit
/// glibc makes it `u32::MAX`, which is never in root's range), with whether
/// it was. None for a `-`, which 64-bit glibc makes `u32::MAX` (`-0` aside,
/// which is 0).
fn loose(field: &[u8]) -> Option<(u32, bool)> {
    let s = without_c_space(field);
    let s = s.strip_prefix(b"+").unwrap_or(s);
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let (mut id, mut wrapped) = (0u32, false);
    for b in s {
        let (times, o1) = id.overflowing_mul(10);
        let (sum, o2) = times.overflowing_add(u32::from(b - b'0'));
        (id, wrapped) = (sum, wrapped || o1 || o2);
    }
    Some((id, wrapped))
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
/// is `name` or `uid` in decimal: numbers in decimal with no leading 0
/// (`newuidmap` and `newgidmap` read one as octal), a count of at least
/// one, whose end (`start + count`) fits in 32 bits, as the kernel takes a
/// map's line.
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
    let (start, count) = (range_number(start)?, range_number(count)?);
    (count >= 1 && start.checked_add(count).is_some()).then_some((owner, IdRange { start, count }))
}

/// A start or count of a range: [`decimal`], with no leading 0.
fn range_number(s: &str) -> Option<u32> {
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    decimal(s)
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
