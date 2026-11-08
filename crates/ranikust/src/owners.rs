//! What to do when a build met an id its user namespace does not map: a
//! base layer's owner, a `chown` or a `RUN`'s user (ADR-0098). The libraries
//! refuse with [`rnk_rootfs::Unmapped`], which names the ids; this module
//! finds it among a failure's sources and says the ways out, which depend
//! on who made the namespace and how. No flag lets a refused `chown` pass.

use std::error::Error;
use std::fs::File;
use std::io::Read as _;

use rnk_rootfs::{MappedIds, Unmapped};
use rustix::fs::{Mode, OFlags};

/// Read no more of a map: the kernel writes at most 340 lines.
const MAP_MAX: u64 = 64 * 1024;

/// The user namespace the build runs in, as the ways out tell it apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Namespace {
    /// ranikust made it for a user other than root, with one id: no
    /// `newuidmap` and `newgidmap`, or no ranges of the user's own.
    UserOneId,
    /// ranikust made it for a user other than root, with the user's first
    /// ranges in `/etc/subuid` and `/etc/subgid`.
    UserRanges,
    /// ranikust made it for root with a single uid or gid: the namespace it
    /// was started in maps no more, or its ranges could not be mapped.
    RootOneId,
    /// Root's own ranges in a container, or a namespace ranikust was
    /// started in (a rootless container's), or one that cannot be read.
    Container,
}

/// The [`Unmapped`] among `e` and its sources, if any.
pub(crate) fn find<'a>(e: &'a (dyn Error + 'static)) -> Option<&'a Unmapped> {
    let mut next = Some(e);
    while let Some(e) = next {
        if let Some(found) = e.downcast_ref::<Unmapped>() {
            return Some(found);
        }
        next = e.source();
    }
    None
}

/// The ways out of `found`, in the user namespace this process runs in;
/// `made` when ranikust made it (rootless overlay mode).
pub(crate) fn guidance(found: &Unmapped, made: bool) -> String {
    let maps = match read_maps() {
        Ok(maps) => maps,
        Err(e) => {
            tracing::debug!("the user namespace's maps cannot be read: {e}");
            None
        }
    };
    let ids = maps
        .as_ref()
        .and_then(|(uid_map, gid_map)| MappedIds::parse(uid_map, gid_map));
    ways_out(found, namespace(made, maps.as_ref()), ids.as_ref())
}

/// The process's `uid_map` and `gid_map`, or `None` in the initial user
/// namespace.
fn read_maps() -> Result<Option<(String, String)>, String> {
    let Some(dir) = rnk_store::user_namespace_proc().map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let read = |name: &str| -> Result<String, String> {
        let fd = rustix::fs::openat(&dir, name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| format!("{name}: {e}"))?;
        let mut text = String::new();
        File::from(fd)
            .take(MAP_MAX)
            .read_to_string(&mut text)
            .map_err(|e| format!("{name}: {e}"))?;
        Ok(text)
    };
    Ok(Some((read("uid_map")?, read("gid_map")?)))
}

/// Who made the namespace whose maps are `maps` and how: by the uid its
/// root is outside, a user's own unless 0.
fn namespace(made: bool, maps: Option<&(String, String)>) -> Namespace {
    let Some((uid_map, gid_map)) = maps.filter(|_| made) else {
        return Namespace::Container;
    };
    let Some(ids) = MappedIds::parse(uid_map, gid_map) else {
        return Namespace::Container;
    };
    let root_outside = uid_map.lines().find_map(|line| {
        let mut n = line.split_ascii_whitespace().map(|w| w.parse::<u32>().ok());
        match (n.next(), n.next(), n.next()) {
            (Some(Some(0)), Some(Some(outside)), Some(Some(1..))) => Some(outside),
            _ => None,
        }
    });
    match (root_outside, ids.is_one_id()) {
        (None, _) => Namespace::Container,
        (Some(0), true) => Namespace::RootOneId,
        (Some(0), false) => Namespace::Container,
        (Some(_), true) => Namespace::UserOneId,
        (Some(_), false) => Namespace::UserRanges,
    }
}

/// The highest ids of `found` that `ids` does not map; all of them where
/// the maps are unknown, or hold every one (the call failed with `EINVAL`
/// for another reason).
fn highest(found: &Unmapped, ids: Option<&MappedIds>) -> (Option<u32>, Option<u32>) {
    match (found.highest_uid(ids), found.highest_gid(ids)) {
        (None, None) => (found.highest_uid(None), found.highest_gid(None)),
        some => some,
    }
}

/// `uid 1000 and gid 42`.
fn ids_text(uid: Option<u32>, gid: Option<u32>) -> String {
    match (uid, gid) {
        (Some(u), Some(g)) => format!("uid {u} and gid {g}"),
        (Some(u), None) => format!("uid {u}"),
        (None, Some(g)) => format!("gid {g}"),
        (None, None) => "every id the image uses".to_owned(),
    }
}

/// The subordinate ids a user's first range needs to map `uid` and `gid`:
/// in the map ranikust writes (`0 <user> 1`, then `1 <first> <count>`), id
/// N is the N-th of the range, and 0 is the user's own.
fn needed_text(uid: Option<u32>, gid: Option<u32>) -> String {
    match (uid.filter(|&u| u > 0), gid.filter(|&g| g > 0)) {
        (Some(u), Some(g)) => format!("{u} subordinate uids and {g} subordinate gids"),
        (Some(u), None) => format!("{u} subordinate uids"),
        (None, Some(g)) => format!("{g} subordinate gids"),
        (None, None) => "as many ids as the image uses".to_owned(),
    }
}

/// The widest range the ways out ask an administrator for: `useradd`'s
/// default count. A wider one would likely take ids another user or a real
/// account holds, so past it they ask for root or a container instead.
const RANGE_MAX: u32 = 65536;

/// What every way out that names a range says of it.
const NO_OVERLAP: &str =
    "a range must not overlap another user's range or the ids of a real account";

fn ways_out(found: &Unmapped, ns: Namespace, ids: Option<&MappedIds>) -> String {
    let (uid, gid) = highest(found, ids);
    let named = ids_text(uid, gid);
    let far = |id: Option<u32>| id.filter(|&id| id > RANGE_MAX);
    let too_few = (far(uid).is_some() || far(gid).is_some()).then(|| {
        format!(
            "a range in /etc/subuid and /etc/subgid usually gives {RANGE_MAX} ids, too few for \
             {}, and {NO_OVERLAP}",
            ids_text(far(uid), far(gid))
        )
    });
    match (ns, too_few) {
        (Namespace::UserOneId, None) => format!(
            "the build's user namespace maps one uid and one gid, as for a user without \
             newuidmap and newgidmap or without ranges of its own: install uidmap (Ubuntu \
             does not by default) and give this user a first range in /etc/subuid and \
             /etc/subgid of at least {} ({NO_OVERLAP}), run ranikust as root, or run it in a \
             container",
            needed_text(uid, gid)
        ),
        (Namespace::UserOneId, Some(too_few)) => format!(
            "the build's user namespace maps one uid and one gid, as for a user without \
             newuidmap and newgidmap or without ranges of its own; {too_few}: run ranikust as \
             root, or run it in a container"
        ),
        (Namespace::UserRanges, None) => format!(
            "the build's user namespace maps this user's first range in /etc/subuid and \
             /etc/subgid from id 1 on: widen that range to at least {} ({NO_OVERLAP}), run \
             ranikust as root, or run it in a container",
            needed_text(uid, gid)
        ),
        (Namespace::UserRanges, Some(too_few)) => format!(
            "the build's user namespace maps this user's first range in /etc/subuid and \
             /etc/subgid from id 1 on; {too_few}: run ranikust as root, or run it in a \
             container"
        ),
        (Namespace::RootOneId, None) => format!(
            "the build's user namespace maps a single uid or gid, not {named}: the user \
             namespace ranikust was started in maps no more, or its ranges could not be \
             mapped into the build's. Widen the id ranges of the user namespace ranikust was \
             started in (for a rootless Podman container, those of the user running it in \
             /etc/subuid and /etc/subgid, then `podman system migrate`; {NO_OVERLAP}), or run \
             ranikust as root on a host or in a rootful container"
        ),
        (Namespace::RootOneId, Some(too_few)) => format!(
            "the build's user namespace maps a single uid or gid, not {named}: the user \
             namespace ranikust was started in maps no more, or its ranges could not be \
             mapped into the build's; {too_few}: run ranikust as root on a host or in a \
             rootful container"
        ),
        (Namespace::Container, None) => format!(
            "the user namespace ranikust was started in does not map {named}: widen its id \
             ranges (for a rootless Podman container, those of the user running it in \
             /etc/subuid and /etc/subgid, then `podman system migrate`; {NO_OVERLAP}), or run \
             ranikust as root on a host or in a rootful container"
        ),
        (Namespace::Container, Some(too_few)) => format!(
            "the user namespace ranikust was started in does not map {named}; {too_few}: run \
             ranikust as root on a host or in a rootful container"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn maps(uid_map: &str, gid_map: &str) -> Option<(String, String)> {
        Some((uid_map.to_owned(), gid_map.to_owned()))
    }

    /// Who made the namespace, and with which map, picks the ways out: a
    /// user's one id or ranges where ranikust made it, the container's
    /// otherwise.
    #[test]
    fn the_ways_out_follow_who_made_the_namespace_and_its_map() {
        let one = maps("0 1000 1\n", "0 1000 1\n");
        let gid_one = maps("0 1000 1\n1 100000 65536\n", "0 1000 1\n");
        let ranges = maps(
            "         0       1000          1\n         1     100000      65536\n",
            "0 1000 1\n1 100000 65536\n",
        );
        let root = maps("0 0 1\n1 100000 65536\n", "0 0 1\n1 100000 65536\n");
        let root_one = maps("0 0 1\n", "0 0 1\n");
        assert_eq!(namespace(true, one.as_ref()), Namespace::UserOneId);
        assert_eq!(namespace(true, gid_one.as_ref()), Namespace::UserOneId);
        assert_eq!(namespace(true, ranges.as_ref()), Namespace::UserRanges);
        assert_eq!(namespace(true, root.as_ref()), Namespace::Container);
        assert_eq!(namespace(true, root_one.as_ref()), Namespace::RootOneId);
        assert_eq!(namespace(false, root_one.as_ref()), Namespace::Container);
        assert_eq!(namespace(false, one.as_ref()), Namespace::Container);
        assert_eq!(namespace(false, ranges.as_ref()), Namespace::Container);
        assert_eq!(namespace(true, None), Namespace::Container);
        assert_eq!(
            namespace(true, maps("x", "0 0 1\n").as_ref()),
            Namespace::Container
        );

        let parsed = |m: &Option<(String, String)>| {
            m.as_ref()
                .and_then(|(u, g)| MappedIds::parse(u, g))
                .unwrap()
        };
        let chown = Unmapped::chown(Path::new("/app"), Some(1000), Some(1000));
        let one_id = ways_out(&chown, Namespace::UserOneId, Some(&parsed(&one)));
        for part in [
            "maps one uid and one gid,",
            "install uidmap",
            "Ubuntu does not by default",
            "a first range in /etc/subuid and /etc/subgid of at least 1000 subordinate uids \
             and 1000 subordinate gids",
            "run ranikust as root",
            "in a container",
        ] {
            assert!(one_id.contains(part), "{part}: {one_id}");
        }
        // The primary gid 0 is mapped: only the uid is named.
        let narrow = maps("0 1000 1\n1 100000 1000\n", "0 1000 1\n1 100000 1000\n");
        let user = Unmapped::user("set the user id", 5000, 0, &[]);
        let widen = ways_out(&user, Namespace::UserRanges, Some(&parsed(&narrow)));
        assert!(
            widen.contains("widen that range to at least 5000 subordinate uids (a range must"),
            "{widen}"
        );
        // Maps that hold every id named (an EINVAL of another cause) name
        // them all, but 0, which is the user's own.
        let all = maps("0 0 4294967295\n", "0 0 4294967295\n");
        let unknown = ways_out(&user, Namespace::UserRanges, Some(&parsed(&all)));
        assert!(
            unknown.contains("at least 5000 subordinate uids (a range must"),
            "{unknown}"
        );
        let root_only = ways_out(&user, Namespace::RootOneId, Some(&parsed(&root_one)));
        assert!(
            root_only.contains(
                "maps a single uid or gid, not uid 5000: the user namespace ranikust was started \
                 in maps no more"
            ),
            "{root_only}"
        );
        let gid = Unmapped::chown(Path::new("a"), None, Some(42));
        let container = ways_out(&gid, Namespace::Container, None);
        assert!(
            container.contains("started in does not map gid 42: widen its id ranges"),
            "{container}"
        );
        assert!(container.contains("podman system migrate"), "{container}");
        // A range asked for must not take another's ids.
        for text in [&one_id, &widen, &root_only, &container] {
            assert!(text.contains(NO_OVERLAP), "{text}");
        }
        // No flag is a way out.
        for text in [one_id, widen, root_only, container] {
            assert!(!text.contains("--"), "{text}");
        }
    }

    /// Past the count `useradd` gives a range, the ways out name the id
    /// but ask no range that wide: root or a container instead. An id of
    /// `u32::MAX`, which `chown` reads as "unchanged", is no id to map.
    #[test]
    fn an_id_past_a_usual_range_asks_for_root_or_a_container_not_a_wider_range() {
        let far = Unmapped::chown(Path::new("far"), Some(3_000_000_000), Some(42));
        let edge = Unmapped::chown(Path::new("edge"), Some(65536), None);
        let past = Unmapped::chown(Path::new("past"), Some(65537), None);
        for ns in [
            Namespace::UserOneId,
            Namespace::UserRanges,
            Namespace::RootOneId,
            Namespace::Container,
        ] {
            let text = ways_out(&far, ns, None);
            assert!(text.contains("uid 3000000000"), "{ns:?}: {text}");
            // Gid 42 fits a range: only the uid is too far for one.
            assert!(
                text.contains("usually gives 65536 ids, too few for uid 3000000000, and"),
                "{ns:?}: {text}"
            );
            assert!(!text.contains("subordinate uids"), "{ns:?}: {text}");
            assert!(!text.contains("widen"), "{ns:?}: {text}");
            assert!(!text.contains("install uidmap"), "{ns:?}: {text}");
            assert!(text.contains("run ranikust as root"), "{ns:?}: {text}");
            assert!(text.contains(NO_OVERLAP), "{ns:?}: {text}");
            let edge = ways_out(&edge, ns, None);
            assert!(!edge.contains("too few"), "{ns:?}: {edge}");
            let past = ways_out(&past, ns, None);
            assert!(past.contains("too few for uid 65537"), "{ns:?}: {past}");
        }
        assert!(
            ways_out(&edge, Namespace::UserRanges, None)
                .contains("widen that range to at least 65536 subordinate uids")
        );
        // `-1:42` asks for gid 42 alone.
        let unchanged = Unmapped::chown(Path::new("a"), Some(u32::MAX), Some(42));
        let text = ways_out(&unchanged, Namespace::UserOneId, None);
        assert!(text.contains("of at least 42 subordinate gids ("), "{text}");
        assert!(!text.contains("4294967295"), "{text}");
    }

    /// The refusal is found however deep the libraries wrap it, through
    /// transparent and boxed sources alike.
    #[test]
    fn the_refusal_is_found_among_the_sources() {
        let found = Unmapped::chown(Path::new("./far"), Some(3), Some(3));
        let rootfs = rnk_rootfs::Error::Unmapped(found.clone());
        let snapshot = rnk_snapshot::Error::from(rootfs);
        let builder = rnk_builder::Error::Image {
            reference: "busybox:latest".to_owned(),
            source: Box::new(rnk_builder::Error::Snapshot(snapshot)),
        };
        let sched: rnk_sched::Error<rnk_builder::Error> = rnk_sched::Error::Prepare {
            stage: 0,
            name: "0".to_owned(),
            source: builder,
        };
        assert_eq!(find(&sched), Some(&found));
        assert_eq!(
            sched.to_string(),
            "stage 0: preparing: image busybox:latest: chown ./far to 3:3: an id this user \
             namespace does not map"
        );
        let sandbox = rnk_sandbox::Error::Unmapped(found.clone());
        let builder = rnk_builder::Error::Instruction {
            stage: "0".to_owned(),
            line: 2,
            instruction: "RUN id".to_owned(),
            source: Box::new(rnk_builder::Error::Sandbox(sandbox)),
        };
        assert_eq!(find(&builder), Some(&found));
        let store = rnk_store::Error::Rootfs(rnk_rootfs::Error::Unmapped(found.clone()));
        assert_eq!(find(&store), Some(&found));
        assert_eq!(find(&rnk_rootfs::Error::User("x".to_owned())), None);
    }
}
