//! Property tests of the tar writer on random trees.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;
use rnk_layer::{Entry, EntryKind};

use crate::support::{Tree, entry, plain, read_tar};

/// Components chosen to stress the order: `-` and `.` sort before `/`.
fn names() -> Vec<String> {
    let mut names: Vec<String> = ["a", "b", "a-b", "a.b", "-", "ü", "Z"]
        .map(String::from)
        .to_vec();
    names.push("y".repeat(124));
    names
}

const IDS: [u64; 5] = [0, 1000, 2_097_151, 2_097_152, 4_294_967_295];
const MTIMES: [i64; 5] = [-5, 0, 1_700_000_000, 8_589_934_591, 8_589_934_592];

#[derive(Clone, Debug)]
enum Leaf {
    File(Vec<u8>),
    Dir,
    Symlink(usize),
    Fifo,
    Char(u32, u32),
    Whiteout,
    Opaque,
    /// A hardlink to the nearest earlier file.
    Link,
}

#[derive(Clone, Debug)]
struct Spec {
    path: Vec<usize>,
    leaf: Leaf,
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: i64,
}

fn leaf() -> impl Strategy<Value = Leaf> {
    prop_oneof![
        3 => vec(any::<u8>(), 0..1500).prop_map(Leaf::File),
        1 => Just(Leaf::Dir),
        1 => (0..names().len()).prop_map(Leaf::Symlink),
        1 => Just(Leaf::Fifo),
        1 => (0u32..300, 0u32..300).prop_map(|(a, b)| Leaf::Char(a, b)),
        1 => Just(Leaf::Whiteout),
        1 => Just(Leaf::Opaque),
        2 => Just(Leaf::Link),
    ]
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        vec(0..names().len(), 1..4),
        leaf(),
        0u32..0o10000,
        select(&IDS[..]),
        select(&IDS[..]),
        select(&MTIMES[..]),
    )
        .prop_map(|(path, leaf, mode, uid, gid, mtime)| Spec {
            path,
            leaf,
            mode,
            uid,
            gid,
            mtime,
        })
}

/// FNV-1a, to make seeded choices that depend only on the path.
fn hash(bytes: &[u8], seed: u64) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325 ^ seed, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Builds a valid tree from the specs: ancestors become directories, fed or
/// left to the parent lookup depending on `seed`.
fn build(specs: &[Spec], seed: u64) -> Tree {
    let names = names();
    let mut chosen: BTreeMap<String, Spec> = BTreeMap::new();
    for s in specs {
        let p: Vec<&str> = s.path.iter().map(|&i| names[i].as_str()).collect();
        chosen.entry(p.join("/")).or_insert_with(|| s.clone());
    }
    let mut ancestors = BTreeSet::new();
    for p in chosen.keys() {
        let mut prefix = String::new();
        for part in p.split('/').collect::<Vec<_>>().split_last().unwrap().1 {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            ancestors.insert(prefix.clone());
        }
    }

    let mut t = Tree::default();
    let mut last_file: Option<(String, Vec<u8>, (u64, u64))> = None;
    for (i, (p, s)) in chosen.iter().enumerate() {
        let leaf = if ancestors.contains(p) {
            match s.leaf {
                Leaf::Opaque => Leaf::Opaque,
                _ => Leaf::Dir,
            }
        } else {
            s.leaf.clone()
        };
        let mut e = match leaf {
            Leaf::File(data) => {
                let inode = (1, u64::try_from(i).unwrap());
                t.source.files.insert(crate::support::path(p), data.clone());
                last_file = Some((p.clone(), data.clone(), inode));
                let mut e = entry(
                    p,
                    EntryKind::File {
                        size: u64::try_from(data.len()).unwrap(),
                    },
                );
                e.inode = Some(inode);
                e
            }
            Leaf::Link => {
                let (data, inode) = last_file
                    .as_ref()
                    .map_or((Vec::new(), None), |(_, d, n)| (d.clone(), Some(*n)));
                t.source.files.insert(crate::support::path(p), data.clone());
                let mut e = entry(
                    p,
                    EntryKind::File {
                        size: u64::try_from(data.len()).unwrap(),
                    },
                );
                e.inode = inode;
                e
            }
            Leaf::Dir => entry(p, EntryKind::Dir),
            Leaf::Symlink(target) => {
                entry(p, EntryKind::Symlink(names[target].clone().into_bytes()))
            }
            Leaf::Fifo => entry(p, EntryKind::Fifo),
            Leaf::Char(a, b) => entry(p, EntryKind::Char(a, b)),
            Leaf::Whiteout => entry(p, EntryKind::Whiteout),
            Leaf::Opaque => entry(p, EntryKind::OpaqueDir),
        };
        e.mode = s.mode;
        e.uid = s.uid;
        e.gid = s.gid;
        e.mtime = s.mtime;
        t.add(e);
    }
    for a in ancestors.iter().filter(|a| !chosen.contains_key(*a)) {
        if hash(a.as_bytes(), seed) & 1 == 0 {
            t.dir(a, 0o755);
        } else {
            t.known_parent(a, 0o711);
        }
    }
    t
}

/// What the archive must hold for each name.
#[derive(Debug)]
struct Expected<'a> {
    entry: &'a Entry,
    marker: bool,
}

fn expected(t: &Tree) -> BTreeMap<Vec<u8>, Expected<'_>> {
    let mut out = BTreeMap::new();
    for e in t.entries.iter().chain(t.source.parents.values()) {
        let mut name = e.path.as_bytes().to_vec();
        match e.kind {
            EntryKind::Whiteout => {
                let at = name.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
                name.splice(at..at, b".wh.".iter().copied());
                out.insert(
                    name,
                    Expected {
                        entry: e,
                        marker: true,
                    },
                );
                continue;
            }
            EntryKind::OpaqueDir => {
                let mut marker = name.clone();
                marker.extend_from_slice(b"/.wh..wh..opq");
                out.insert(
                    marker,
                    Expected {
                        entry: e,
                        marker: true,
                    },
                );
                name.push(b'/');
            }
            EntryKind::Dir => name.push(b'/'),
            EntryKind::File { .. }
            | EntryKind::Symlink(_)
            | EntryKind::Char(..)
            | EntryKind::Block(..)
            | EntryKind::Fifo => {}
        }
        out.insert(
            name,
            Expected {
                entry: e,
                marker: false,
            },
        );
    }
    out
}

fn parent_of(name: &[u8]) -> Option<&[u8]> {
    let trimmed = name.strip_suffix(b"/").unwrap_or(name);
    trimmed
        .iter()
        .rposition(|&b| b == b'/')
        .map(|i| &trimmed[..=i])
}

fn is_marker(name: &[u8]) -> bool {
    let trimmed = name.strip_suffix(b"/").unwrap_or(name);
    let base = trimmed
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(trimmed, |i| &trimmed[i + 1..]);
    base.starts_with(b".wh.")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn feed_order_never_changes_the_bytes(specs in vec(spec(), 0..24), seed: u64, order: u64) {
        let mut t = build(&specs, seed);
        let sorted = t.encode(plain()).unwrap();
        t.entries.sort_by_key(|e| hash(e.path.as_bytes(), order));
        prop_assert_eq!(t.encode(plain()).unwrap(), sorted);
    }

    #[test]
    fn every_entry_reads_back(specs in vec(spec(), 0..24), seed: u64) {
        let mut t = build(&specs, seed);
        let bytes = t.encode(plain()).unwrap();
        let read = read_tar(&bytes);
        let want = expected(&t);

        let got_names: Vec<&[u8]> = read.iter().map(|r| r.name.as_slice()).collect();
        let want_names: Vec<&[u8]> = want.keys().map(Vec::as_slice).collect();
        let mut sorted = got_names.clone();
        sorted.sort_unstable();
        prop_assert_eq!(&sorted, &want_names, "names differ");

        let position: HashMap<&[u8], usize> = got_names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
        let mut first_of_inode: HashMap<(u64, u64), &[u8]> = HashMap::new();
        for (i, r) in read.iter().enumerate() {
            let name = r.name.as_slice();
            if let Some(parent) = parent_of(name) {
                prop_assert!(position[parent] < i, "{} before its parent", r.name());
            }
            if is_marker(name) {
                let earlier_sibling = read[..i].iter().any(|o| parent_of(&o.name) == parent_of(name) && !is_marker(&o.name));
                prop_assert!(!earlier_sibling, "{} after a sibling", r.name());
            }

            let Expected { entry: e, marker } = &want[name];
            prop_assert_eq!(r.mtime, e.mtime);
            if *marker {
                prop_assert_eq!((r.typeflag, r.mode, r.uid, r.gid), (b'0', 0, 0, 0));
                continue;
            }
            prop_assert_eq!((r.mode, r.uid, r.gid), (e.mode & 0o7777, e.uid, e.gid));
            match &e.kind {
                EntryKind::File { .. } => {
                    let first = e.inode.map(|n| *first_of_inode.entry(n).or_insert(name));
                    match first {
                        Some(first) if first != name => {
                            prop_assert_eq!(r.typeflag, b'1');
                            prop_assert_eq!(r.link.as_deref(), Some(first));
                        }
                        _ => {
                            prop_assert_eq!(r.typeflag, b'0');
                            prop_assert_eq!(&r.content, &t.source.files[&e.path]);
                        }
                    }
                }
                EntryKind::Dir | EntryKind::OpaqueDir => prop_assert_eq!(r.typeflag, b'5'),
                EntryKind::Symlink(target) => {
                    prop_assert_eq!(r.typeflag, b'2');
                    prop_assert_eq!(r.link.as_deref(), Some(target.as_slice()));
                }
                EntryKind::Char(a, b) => prop_assert_eq!((r.typeflag, r.device), (b'3', (*a, *b))),
                EntryKind::Block(a, b) => prop_assert_eq!((r.typeflag, r.device), (b'4', (*a, *b))),
                EntryKind::Fifo => prop_assert_eq!(r.typeflag, b'6'),
                EntryKind::Whiteout => prop_assert!(false, "a whiteout is a marker"),
            }
        }
    }
}
