//! The ids a user namespace maps, and the ones a build would give a file or
//! a process that it does not (ADR-0098): no flag lets them pass.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::meta::chown_id;

/// How many ids of each kind [`Unmapped`] names, and how many of a `RUN`'s
/// supplementary groups; entries or groups past them are counted.
const NAMED: usize = 8;

/// The uids and gids a user namespace maps: the ids inside it that the lines
/// of its `uid_map` and `gid_map` give.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedIds {
    /// Inclusive ranges, sorted, neither overlapping nor adjacent.
    uids: Vec<(u32, u32)>,
    gids: Vec<(u32, u32)>,
}

impl MappedIds {
    /// The ids the maps `uid_map` and `gid_map` give, each line `inside
    /// outside count` as the kernel shows it. `None` unless each map has a
    /// line, and every line is three numbers, a count above zero and a range
    /// that ends at `u32::MAX` at most.
    #[must_use]
    pub fn parse(uid_map: &str, gid_map: &str) -> Option<Self> {
        Some(Self {
            uids: ranges(uid_map)?,
            gids: ranges(gid_map)?,
        })
    }

    /// Whether the namespace maps the uid `id`.
    #[must_use]
    pub fn maps_uid(&self, id: u32) -> bool {
        holds(&self.uids, id)
    }

    /// Whether the namespace maps the gid `id`.
    #[must_use]
    pub fn maps_gid(&self, id: u32) -> bool {
        holds(&self.gids, id)
    }

    /// Whether the uid map or the gid map gives one id only, as a map written
    /// without `newuidmap` and `newgidmap` does.
    #[must_use]
    pub fn is_one_id(&self) -> bool {
        [&self.uids, &self.gids]
            .iter()
            .any(|r| matches!(r.as_slice(), [(a, b)] if a == b))
    }
}

impl fmt::Display for MappedIds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} and {}",
            ranges_text("uid", &self.uids),
            ranges_text("gid", &self.gids)
        )
    }
}

/// A map's lines as ranges of ids inside, merged.
fn ranges(map: &str) -> Option<Vec<(u32, u32)>> {
    let mut out = map
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut n = line.split_ascii_whitespace().map(|w| w.parse::<u32>().ok());
            match (n.next(), n.next(), n.next(), n.next()) {
                (Some(Some(inside)), Some(Some(_)), Some(Some(count)), None) if count > 0 => {
                    Some((inside, inside.checked_add(count - 1)?))
                }
                _ => None,
            }
        })
        .collect::<Option<Vec<_>>>()?;
    if out.is_empty() {
        return None;
    }
    out.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(out.len());
    for (a, b) in out {
        match merged.last_mut() {
            Some(last) if u64::from(a) <= u64::from(last.1) + 1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    Some(merged)
}

fn holds(ranges: &[(u32, u32)], id: u32) -> bool {
    ranges.iter().any(|&(a, b)| (a..=b).contains(&id))
}

/// `uid 0`, or `uids 0-65536, 70000`.
fn ranges_text(kind: &str, ranges: &[(u32, u32)]) -> String {
    let each: Vec<String> = ranges
        .iter()
        .map(|&(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            }
        })
        .collect();
    match ranges {
        [(a, b)] if a == b => format!("{kind} {a}"),
        _ => format!("{kind}s {}", each.join(", ")),
    }
}

/// Ids a user namespace does not map, met where a build would give them to
/// a file or a process: the owners of base layers' entries, read from their
/// tar headers before anything of them is extracted
/// ([`crate::scan_owners`]); the owner of a `chown` refused with `EINVAL`;
/// the user or group a `RUN` could not switch to. An error of its own
/// ([`crate::Error::Unmapped`]), so that the binary finds it in an error's
/// sources and says what to do (ADR-0098).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unmapped(Box<Found>);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Found {
    /// What a scan of layers found against `map`.
    Owners {
        map: MappedIds,
        uids: BTreeMap<u32, PathBuf>,
        gids: BTreeMap<u32, PathBuf>,
        /// Entries with an id past those named.
        more: u64,
        highest: [Option<u32>; 2],
    },
    /// A `chown` of `path` refused with `EINVAL`.
    Chown {
        path: PathBuf,
        uid: Option<u32>,
        gid: Option<u32>,
    },
    /// A `RUN`'s switch to its user refused with `EINVAL` at `step`.
    User {
        step: String,
        uid: u32,
        gid: u32,
        groups: Vec<u32>,
    },
}

impl Unmapped {
    /// No owner found yet, for a scan against `map`.
    #[must_use]
    pub fn owners(map: MappedIds) -> Self {
        Self(Box::new(Found::Owners {
            map,
            uids: BTreeMap::new(),
            gids: BTreeMap::new(),
            more: 0,
            highest: [None; 2],
        }))
    }

    /// A `chown` of `path` (in the root) to `uid` and `gid` refused with
    /// `EINVAL`: one of them is not mapped. `u32::MAX`, which `chown` reads
    /// as "unchanged", is left out.
    #[must_use]
    pub fn chown(path: &Path, uid: Option<u32>, gid: Option<u32>) -> Self {
        let id = |id: Option<u32>| id.and_then(chown_id);
        Self(Box::new(Found::Chown {
            path: path.to_owned(),
            uid: id(uid),
            gid: id(gid),
        }))
    }

    /// A `RUN`'s switch to `uid`, `gid` and the supplementary `groups`
    /// refused with `EINVAL` at `step` (`set the user id`, ...): one of them
    /// is not mapped.
    #[must_use]
    pub fn user(step: impl fmt::Display, uid: u32, gid: u32, groups: &[u32]) -> Self {
        Self(Box::new(Found::User {
            step: step.to_string(),
            uid,
            gid,
            groups: groups.to_vec(),
        }))
    }

    /// Whether a scan found nothing: always false for a refused `chown` or
    /// user switch.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match &*self.0 {
            Found::Owners { highest, .. } => highest.iter().all(Option::is_none),
            Found::Chown { .. } | Found::User { .. } => false,
        }
    }

    /// The highest uid met that `map` does not hold: the one a map must
    /// reach. A refused call does not tell which of its ids failed, so
    /// without `map` each of them counts; a scan's own map always applies.
    /// `u32::MAX`, which `chown` reads as "unchanged", is none.
    #[must_use]
    pub fn highest_uid(&self, map: Option<&MappedIds>) -> Option<u32> {
        let unmapped = |id: &u32| chown_id(*id).is_some() && map.is_none_or(|m| !m.maps_uid(*id));
        match &*self.0 {
            Found::Owners { highest, .. } => highest[0],
            Found::Chown { uid, .. } => uid.filter(unmapped),
            Found::User { uid, .. } => Some(*uid).filter(unmapped),
        }
    }

    /// The highest gid met that `map` does not hold, as
    /// [`Unmapped::highest_uid`].
    #[must_use]
    pub fn highest_gid(&self, map: Option<&MappedIds>) -> Option<u32> {
        let unmapped = |id: &u32| chown_id(*id).is_some() && map.is_none_or(|m| !m.maps_gid(*id));
        match &*self.0 {
            Found::Owners { highest, .. } => highest[1],
            Found::Chown { gid, .. } => gid.filter(unmapped),
            Found::User { gid, groups, .. } => {
                groups.iter().copied().chain([*gid]).filter(unmapped).max()
            }
        }
    }

    /// The map a scan read against; `None` for a refused call.
    #[must_use]
    pub fn map(&self) -> Option<&MappedIds> {
        match &*self.0 {
            Found::Owners { map, .. } => Some(map),
            Found::Chown { .. } | Found::User { .. } => None,
        }
    }

    /// Notes the owner `uid`:`gid` of the entry `path` of a layer, each id
    /// the map does not hold. `u32::MAX`, which `chown` reads as
    /// "unchanged", is no owner.
    pub(crate) fn note(&mut self, uid: Option<u32>, gid: Option<u32>, path: &Path) {
        let Found::Owners {
            map,
            uids,
            gids,
            more,
            highest,
        } = &mut *self.0
        else {
            return;
        };
        let ids = [
            (
                uid,
                &mut *uids,
                MappedIds::maps_uid as fn(&MappedIds, u32) -> bool,
            ),
            (gid, &mut *gids, MappedIds::maps_gid),
        ];
        let mut unnamed = false;
        for ((id, named, maps), top) in ids.into_iter().zip(highest.iter_mut()) {
            let Some(id) = id.and_then(chown_id).filter(|&id| !maps(map, id)) else {
                continue;
            };
            *top = Some(top.map_or(id, |t| t.max(id)));
            if named.contains_key(&id) {
                continue;
            }
            if named.len() < NAMED {
                named.insert(id, path.to_owned());
            } else {
                unnamed = true;
            }
        }
        // An entry counts once, whether its uid, its gid or both are unnamed.
        if unnamed {
            *more += 1;
        }
    }
}

impl fmt::Display for Unmapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.0 {
            Found::Owners {
                map,
                uids,
                gids,
                more,
                ..
            } => {
                let named: Vec<String> = [("uid", uids), ("gid", gids)]
                    .into_iter()
                    .flat_map(|(kind, ids)| {
                        ids.iter()
                            .map(move |(id, at)| format!("{kind} {id} (/{})", at.display()))
                    })
                    .collect();
                write!(
                    f,
                    "owners this user namespace does not map: {}",
                    named.join(", ")
                )?;
                if *more > 0 {
                    write!(f, ", and {more} more entries with other ids")?;
                }
                write!(f, "; it maps {map}")
            }
            Found::Chown { path, uid, gid } => write!(
                f,
                "chown {} to {}: an id this user namespace does not map",
                path.display(),
                owner_text(*uid, *gid)
            ),
            Found::User {
                step,
                uid,
                gid,
                groups,
            } => {
                write!(f, "{step} for uid {uid}, gid {gid}")?;
                if !groups.is_empty() {
                    let list: Vec<String> = groups.iter().take(NAMED).map(u32::to_string).collect();
                    write!(f, ", groups {}", list.join(" "))?;
                    if groups.len() > NAMED {
                        write!(f, " and {} more", groups.len() - NAMED)?;
                    }
                }
                f.write_str(": an id this user namespace does not map")
            }
        }
    }
}

impl std::error::Error for Unmapped {}

/// `1000:1000`, `uid 1000` or `gid 42`.
fn owner_text(uid: Option<u32>, gid: Option<u32>) -> String {
    match (uid, gid) {
        (Some(u), Some(g)) => format!("{u}:{g}"),
        (Some(u), None) => format!("uid {u}"),
        (None, Some(g)) => format!("gid {g}"),
        (None, None) => "no owner".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(uid_map: &str, gid_map: &str) -> MappedIds {
        MappedIds::parse(uid_map, gid_map).unwrap()
    }

    #[test]
    fn a_map_gives_the_ids_inside_merged_whatever_its_order_or_padding() {
        let ranged = map(
            "         1     100000      65536\n         0       1000          1\n",
            "0 1000 1\n1 100000 65536\n",
        );
        assert_eq!(ranged.to_string(), "uids 0-65536 and gids 0-65536");
        assert!(ranged.maps_uid(0) && ranged.maps_uid(65536));
        assert!(!ranged.maps_uid(65537) && !ranged.maps_gid(u32::MAX));
        assert!(!ranged.is_one_id());
        let one = map("0 1000 1\n", "0 1000 1\n");
        assert_eq!(one.to_string(), "uid 0 and gid 0");
        assert!(one.is_one_id() && one.maps_gid(0) && !one.maps_gid(1));
        // One gid alone makes a one-id map: a rename of a `0555` directory
        // of an unmapped gid fails as a chown does (ADR-0098).
        assert!(map("0 1000 1\n1 100000 65536\n", "0 1000 1\n").is_one_id());
        let gaps = map("0 0 1\n5 5 2\n100 100 1\n", "0 0 4294967295\n");
        assert_eq!(gaps.to_string(), "uids 0, 5-6, 100 and gids 0-4294967294");
        for bad in [
            "",
            "\n",
            "0 1000\n",
            "0 1000 1 2\n",
            "0 1000 0\n",
            "0 -1 1\n",
            "x 1 1\n",
            "4294967295 0 2\n",
        ] {
            assert_eq!(MappedIds::parse(bad, "0 0 1\n"), None, "{bad:?}");
            assert_eq!(MappedIds::parse("0 0 1\n", bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_scan_names_each_unmapped_id_once_with_its_first_entry_and_counts_the_rest() {
        let mut found = Unmapped::owners(map("0 1000 1\n", "0 1000 1\n"));
        assert!(found.is_empty());
        found.note(Some(0), Some(0), Path::new("etc/passwd"));
        assert!(found.is_empty());
        found.note(Some(0), Some(42), Path::new("etc/shadow"));
        found.note(Some(0), Some(42), Path::new("etc/gshadow"));
        // `chown` reads `u32::MAX` as "unchanged".
        found.note(Some(u32::MAX), None, Path::new("odd"));
        found.note(Some(100), Some(101), Path::new("var/lib/app"));
        assert!(!found.is_empty());
        assert_eq!(
            found.to_string(),
            "owners this user namespace does not map: uid 100 (/var/lib/app), gid 42 \
             (/etc/shadow), gid 101 (/var/lib/app); it maps uid 0 and gid 0"
        );
        let named = u32::try_from(NAMED).unwrap();
        for id in 1000..1000 + 2 * named {
            found.note(Some(id), None, Path::new("many"));
        }
        assert_eq!(found.highest_uid(None), Some(1000 + 2 * named - 1));
        assert_eq!(found.highest_gid(None), Some(101));
        assert!(
            found
                .to_string()
                .contains(", and 9 more entries with other ids; it maps"),
            "{found}"
        );
        // Six more gids fill the names, two are counted; then an entry whose
        // uid and gid are both unnamed counts once.
        for id in 2000..2008 {
            found.note(None, Some(id), Path::new("gids"));
        }
        found.note(Some(5000), Some(5000), Path::new("both"));
        assert!(
            found
                .to_string()
                .contains(", and 12 more entries with other ids; it maps"),
            "{found}"
        );
        assert_eq!(found.map(), Some(&map("0 1000 1\n", "0 1000 1\n")));
    }

    #[test]
    fn a_refused_chown_or_user_switch_names_its_ids() {
        let c = Unmapped::chown(Path::new("/app"), Some(1000), Some(1000));
        assert_eq!(
            c.to_string(),
            "chown /app to 1000:1000: an id this user namespace does not map"
        );
        assert!(!c.is_empty() && c.map().is_none());
        assert_eq!(
            Unmapped::chown(Path::new("a"), None, Some(42)).to_string(),
            "chown a to gid 42: an id this user namespace does not map"
        );
        let u = Unmapped::user("set the group id", 1000, 1000, &[27, 4]);
        assert_eq!(
            u.to_string(),
            "set the group id for uid 1000, gid 1000, groups 27 4: an id this user namespace \
             does not map"
        );
        assert_eq!(
            (u.highest_uid(None), u.highest_gid(None)),
            (Some(1000), Some(1000))
        );
        let u = Unmapped::user("set supplementary groups", 0, 0, &[5000]);
        assert_eq!(u.highest_gid(None), Some(5000));
        // With the map, only the ids it lacks: the call does not tell which.
        let ranges = map("0 1000 1\n1 100000 65536\n", "0 1000 1\n1 100000 65536\n");
        let u = Unmapped::user("set the user id", 70000, 0, &[27, 80000]);
        assert_eq!(
            (u.highest_uid(Some(&ranges)), u.highest_gid(Some(&ranges))),
            (Some(70000), Some(80000))
        );
        let c = Unmapped::chown(Path::new("a"), Some(70000), Some(42));
        assert_eq!(
            (c.highest_uid(Some(&ranges)), c.highest_gid(Some(&ranges))),
            (Some(70000), None)
        );
        // `chown` reads `u32::MAX` as "unchanged": no id a map must reach.
        let c = Unmapped::chown(Path::new("a"), Some(u32::MAX), Some(42));
        assert_eq!((c.highest_uid(None), c.highest_gid(None)), (None, Some(42)));
        assert_eq!(
            c.to_string(),
            "chown a to gid 42: an id this user namespace does not map"
        );
        // Eight groups are named, the rest counted, all of them weighed.
        let many: Vec<u32> = (100..111).collect();
        let u = Unmapped::user("set supplementary groups", 1000, 1000, &many);
        assert_eq!(
            u.to_string(),
            "set supplementary groups for uid 1000, gid 1000, groups 100 101 102 103 104 105 106 \
             107 and 3 more: an id this user namespace does not map"
        );
        assert_eq!(u.highest_gid(None), Some(1000));
        assert_eq!(u.highest_gid(Some(&ranges)), None);
        let u = Unmapped::user(
            "set supplementary groups",
            0,
            0,
            &[70000, 70001, 2, 3, 4, 5, 6, 7, 80000],
        );
        assert_eq!(u.highest_gid(Some(&ranges)), Some(80000));
    }
}
