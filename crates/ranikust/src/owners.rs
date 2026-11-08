//! What to do when a build met an id its user namespace does not map: a
//! base layer's owner, a `chown` or a `RUN`'s user (ADR-0098). The libraries
//! refuse with [`rnk_rootfs::Unmapped`], which names the ids; this module
//! finds it among a failure's sources and says the ways out, which depend
//! on who made the namespace and how. No flag lets a refused `chown` pass.

use std::error::Error;

use rnk_rootfs::{MappedIds, Unmapped};
use rnk_snapshot::UserNamespace;

/// Who made the user namespace a build ran in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MadeBy {
    /// Not ranikust: the one it was started in, if any (kaniko mode,
    /// root's overlay mode).
    Other,
    /// ranikust, for rootless overlay mode.
    Rootless,
    /// ranikust, for root's `--userns`, with the `ranikust` range alone
    /// (ADR-0104).
    Userns,
}

impl MadeBy {
    /// The maker of rootless overlay mode's namespace: root's `--userns`
    /// where `userns`, else a user's or root's rootless build.
    pub(crate) fn rootless(userns: bool) -> Self {
        if userns { Self::Userns } else { Self::Rootless }
    }
}

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
    /// ranikust made it for root's `--userns`: its `ranikust` range of
    /// `/etc/subuid` and `/etc/subgid` from id 0 on, nothing else.
    Userns,
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

/// The ways out of `found`, in the user namespace this process runs in,
/// which `made` made.
pub(crate) fn guidance(found: &Unmapped, made: MadeBy) -> String {
    let ns = match UserNamespace::current() {
        Ok(ns) => ns,
        Err(e) => {
            tracing::debug!("the user namespace's maps cannot be read: {e}");
            None
        }
    };
    let ids = ns.as_ref().map(UserNamespace::ids);
    let root_outside = ns.as_ref().and_then(UserNamespace::root_uid_outside);
    ways_out(found, namespace(made, root_outside, ids), ids)
}

/// Who made the namespace whose maps give `ids` and how: by the uid its
/// root has outside, `root_outside`, a user's own unless 0. `--userns`
/// never writes a map other than its range's, so `ids` cannot change it.
fn namespace(made: MadeBy, root_outside: Option<u32>, ids: Option<&MappedIds>) -> Namespace {
    let ids = match made {
        MadeBy::Userns => return Namespace::Userns,
        MadeBy::Other => None,
        MadeBy::Rootless => ids,
    };
    let Some(ids) = ids else {
        return Namespace::Container;
    };
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
/// account holds, so past it they ask for root or a container instead, or,
/// under `--userns`, a build without it.
const RANGE_MAX: u32 = 65536;

/// What every way out that names a range says of it.
const NO_OVERLAP: &str =
    "a range must not overlap another user's range or the ids of a real account";

/// The counts the `ranikust` range needs in each file to map `uid` and
/// `gid`: `--userns` maps it from id 0 on, so id N is its N+1-th.
fn counts_text(uid: Option<u32>, gid: Option<u32>) -> String {
    let count = |id: u32| u64::from(id) + 1;
    match (uid, gid) {
        (Some(u), Some(g)) => format!(
            "{} uids in /etc/subuid and {} gids in /etc/subgid",
            count(u),
            count(g)
        ),
        (Some(u), None) => format!("{} uids in /etc/subuid", count(u)),
        (None, Some(g)) => format!("{} gids in /etc/subgid", count(g)),
        (None, None) => "as many ids as the image uses".to_owned(),
    }
}

/// The ways out under root's `--userns`, for the highest ids `uid` and
/// `gid` its range lacks: widen that range, unless a count would pass
/// [`RANGE_MAX`], or build without `--userns`.
fn userns_ways_out(uid: Option<u32>, gid: Option<u32>) -> String {
    // Id 0 is the range's first: a refused call names it with the others.
    let (uid, gid) = (uid.filter(|&u| u > 0), gid.filter(|&g| g > 0));
    let maps = format!(
        "the build's user namespace maps root's ranikust range of /etc/subuid and /etc/subgid \
         from id 0 on (--userns), not {}",
        ids_text(uid, gid)
    );
    let counts = counts_text(uid, gid);
    // Id N takes a count of N + 1, past `RANGE_MAX` from `RANGE_MAX` on.
    let far = |id: Option<u32>| id.filter(|&id| id >= RANGE_MAX);
    if far(uid).is_none() && far(gid).is_none() {
        format!(
            "{maps}: widen that range to at least {counts} ({NO_OVERLAP}, nor ids another \
             allocator such as an NSS subid module, the kubelet or systemd hands out), or build \
             without --userns"
        )
    } else {
        format!(
            "{maps}, which would take at least {counts}; a range usually gives {RANGE_MAX} ids, \
             too few for {}, and {NO_OVERLAP}: build without --userns",
            ids_text(far(uid), far(gid))
        )
    }
}

fn ways_out(found: &Unmapped, ns: Namespace, ids: Option<&MappedIds>) -> String {
    let (uid, gid) = highest(found, ids);
    let named = ids_text(uid, gid);
    // A user's range maps id N as its N-th: N ids, past `RANGE_MAX` above it.
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
             /etc/subgid, in decimal with no leading 0, of at least {} ({NO_OVERLAP}), run \
             ranikust as root, or run it in a container",
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
        // `too_few` is a user's range's rule: `--userns` has its own.
        (Namespace::Userns, _) => userns_ways_out(uid, gid),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn ids(uid_map: &str, gid_map: &str) -> MappedIds {
        MappedIds::parse(uid_map, gid_map).unwrap()
    }

    /// Who made the namespace, and with which map, picks the ways out: a
    /// user's one id or ranges where ranikust made it, the container's
    /// otherwise.
    #[test]
    fn the_ways_out_follow_who_made_the_namespace_and_its_map() {
        let one = ids("0 1000 1\n", "0 1000 1\n");
        let ranges = ids("0 1000 1\n1 100000 65536\n", "0 1000 1\n1 100000 65536\n");
        let root = ids("0 0 1\n1 100000 65536\n", "0 0 1\n1 100000 65536\n");
        let root_one = ids("0 0 1\n", "0 0 1\n");
        // Root's own ids mirrored, one gid among them: what root's build
        // makes in a container whose gid map gives one id.
        let root_mixed = ids("0 0 65536\n", "0 0 1\n");
        let user = Some(1000);
        assert_eq!(
            namespace(MadeBy::Rootless, user, Some(&one)),
            Namespace::UserOneId
        );
        assert_eq!(
            namespace(MadeBy::Rootless, user, Some(&ranges)),
            Namespace::UserRanges
        );
        assert_eq!(
            namespace(MadeBy::Rootless, Some(0), Some(&root)),
            Namespace::Container
        );
        assert_eq!(
            namespace(MadeBy::Rootless, Some(0), Some(&root_one)),
            Namespace::RootOneId
        );
        assert_eq!(
            namespace(MadeBy::Rootless, Some(0), Some(&root_mixed)),
            Namespace::RootOneId
        );
        assert_eq!(
            namespace(MadeBy::Other, Some(0), Some(&root_one)),
            Namespace::Container
        );
        assert_eq!(
            namespace(MadeBy::Other, user, Some(&one)),
            Namespace::Container
        );
        assert_eq!(
            namespace(MadeBy::Other, user, Some(&ranges)),
            Namespace::Container
        );
        // Maps that cannot be read, or that leave root unmapped.
        assert_eq!(
            namespace(MadeBy::Rootless, user, None),
            Namespace::Container
        );
        assert_eq!(
            namespace(MadeBy::Rootless, None, Some(&one)),
            Namespace::Container
        );

        let chown = Unmapped::chown(Path::new("/app"), Some(1000), Some(1000));
        let one_id = ways_out(&chown, Namespace::UserOneId, Some(&one));
        for part in [
            "maps one uid and one gid,",
            "install uidmap",
            "Ubuntu does not by default",
            "a first range in /etc/subuid and /etc/subgid, in decimal with no leading 0, of \
             at least 1000 subordinate uids and 1000 subordinate gids",
            "run ranikust as root",
            "in a container",
        ] {
            assert!(one_id.contains(part), "{part}: {one_id}");
        }
        // The primary gid 0 is mapped: only the uid is named.
        let narrow = ids("0 1000 1\n1 100000 1000\n", "0 1000 1\n1 100000 1000\n");
        let user = Unmapped::user("set the user id", 5000, 0, &[]);
        let widen = ways_out(&user, Namespace::UserRanges, Some(&narrow));
        assert!(
            widen.contains("widen that range to at least 5000 subordinate uids (a range must"),
            "{widen}"
        );
        // Maps that hold every id named (an EINVAL of another cause) name
        // them all, but 0, which is the user's own.
        let all = ids("0 0 4294967295\n", "0 0 4294967295\n");
        let unknown = ways_out(&user, Namespace::UserRanges, Some(&all));
        assert!(
            unknown.contains("at least 5000 subordinate uids (a range must"),
            "{unknown}"
        );
        let root_only = ways_out(&user, Namespace::RootOneId, Some(&root_one));
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

    /// Root's `--userns` maps its `ranikust` range from id 0 on, whatever
    /// the maps read: the ways out name that range and the count each file
    /// needs, id N being its N+1-th, or a build without `--userns`; never a
    /// user's range, root or a container.
    #[test]
    fn userns_ways_out_name_the_ranikust_range_and_the_count_it_needs() {
        assert_eq!(MadeBy::rootless(true), MadeBy::Userns);
        assert_eq!(MadeBy::rootless(false), MadeBy::Rootless);
        let shifted = ids("0 100000 1000\n", "0 200000 42\n");
        assert_eq!(
            namespace(MadeBy::Userns, Some(100_000), Some(&shifted)),
            Namespace::Userns
        );
        assert_eq!(namespace(MadeBy::Userns, None, None), Namespace::Userns);
        assert_eq!(
            namespace(MadeBy::Rootless, Some(100_000), Some(&shifted)),
            Namespace::UserRanges
        );
        assert_eq!(
            namespace(MadeBy::Other, Some(100_000), Some(&shifted)),
            Namespace::Container
        );

        // An image's owners, scanned against the range's map.
        let mut tar = tar::Builder::new(Vec::new());
        for (name, uid, gid) in [
            ("etc/shadow", 0, 42),
            ("home/app/", 1000, 1000),
            ("ok", 999, 41),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(if name.ends_with('/') {
                tar::EntryType::Directory
            } else {
                tar::EntryType::Regular
            });
            h.set_mode(0o755);
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_size(0);
            tar.append_data(&mut h, name, std::io::empty()).unwrap();
        }
        let mut found = Unmapped::owners(shifted.clone());
        rnk_rootfs::scan_owners(tar.into_inner().unwrap().as_slice(), &mut found).unwrap();
        let text = ways_out(&found, Namespace::Userns, Some(&shifted));
        assert_eq!(
            text,
            "the build's user namespace maps root's ranikust range of /etc/subuid and \
             /etc/subgid from id 0 on (--userns), not uid 1000 and gid 1000: widen that range \
             to at least 1001 uids in /etc/subuid and 1001 gids in /etc/subgid (a range must \
             not overlap another user's range or the ids of a real account, nor ids another \
             allocator such as an NSS subid module, the kubelet or systemd hands out), or \
             build without --userns"
        );
        // A refused call: only the ids the map lacks count.
        let chown = Unmapped::chown(Path::new("/app"), Some(5), Some(42));
        let gid = ways_out(&chown, Namespace::Userns, Some(&shifted));
        assert!(
            gid.contains("not gid 42: widen that range to at least 43 gids in /etc/subgid ("),
            "{gid}"
        );
        let user = Unmapped::user("set the user id", 4999, 0, &[]);
        let uid = ways_out(&user, Namespace::Userns, None);
        assert!(
            uid.contains("to at least 5000 uids in /etc/subuid ("),
            "{uid}"
        );
        for text in [&text, &gid, &uid] {
            assert!(text.contains(NO_OVERLAP), "{text}");
            for wrong in [
                "this user",
                "subordinate",
                "run ranikust as root",
                "container",
                "uidmap",
            ] {
                assert!(!text.contains(wrong), "{wrong}: {text}");
            }
            // The one flag named is the one to drop.
            assert_eq!(
                text.replace("--userns", "").matches("--").count(),
                0,
                "{text}"
            );
        }
    }

    /// Under `--userns`, an id whose count passes what a range usually
    /// gives is named with the count it needs, but no such range is asked
    /// for: a build without `--userns` is the way out.
    #[test]
    fn userns_past_a_usual_range_asks_for_a_build_without_userns() {
        let edge = Unmapped::chown(Path::new("edge"), Some(65535), None);
        let text = ways_out(&edge, Namespace::Userns, None);
        assert!(
            text.contains("widen that range to at least 65536 uids in /etc/subuid ("),
            "{text}"
        );
        let past = Unmapped::chown(Path::new("past"), Some(65536), Some(42));
        let text = ways_out(&past, Namespace::Userns, None);
        assert!(
            text.contains(
                "not uid 65536 and gid 42, which would take at least 65537 uids in /etc/subuid \
                 and 43 gids in /etc/subgid; a range usually gives 65536 ids, too few for uid \
                 65536, and a range must not overlap"
            ),
            "{text}"
        );
        assert!(text.ends_with(": build without --userns"), "{text}");
        assert!(!text.contains("widen"), "{text}");
        let far = Unmapped::chown(Path::new("far"), Some(4_294_967_294), None);
        let text = ways_out(&far, Namespace::Userns, None);
        assert!(text.contains("at least 4294967295 uids"), "{text}");
        // `-1:42` asks for gid 42 alone.
        let unchanged = Unmapped::chown(Path::new("a"), Some(u32::MAX), Some(42));
        let text = ways_out(&unchanged, Namespace::Userns, None);
        assert!(
            text.contains("not gid 42: widen that range to at least 43 gids in /etc/subgid ("),
            "{text}"
        );
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
