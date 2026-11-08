//! Red team of the owner scans (ADR-0098, #80; T15, T16): hostile tar
//! headers meant to make `scan_owners` or `check_headers_and_owners` pass
//! while the extraction they stand for gives an owner the map lacks. T0:
//! the map is the ids a regular user may `chown` to, its own uid and gid,
//! so an extraction here fails at `chown` with `EPERM` exactly where one in
//! a user namespace fails with `EINVAL`. Each case is scanned and extracted
//! into a fresh temp dir; the scan must never pass where the extraction
//! fails at a `chown`.

use std::fs;
use std::path::PathBuf;

use rnk_rootfs::{
    DeviceNodes, Error, IgnoreList, MappedIds, RootPath, Unmapped, check_headers_and_owners,
    extract_live, extract_overlay, scan_owners,
};
use tar::{EntryType, Header};

/// An id no regular user may `chown` to, and no test map holds.
const FAR: u64 = 3_000_000_000;

fn own() -> (u64, u64) {
    (
        rustix::process::geteuid().as_raw().into(),
        rustix::process::getegid().as_raw().into(),
    )
}

/// The ids this process may give a file: its own uid and gid.
fn own_map() -> MappedIds {
    let (u, g) = own();
    MappedIds::parse(&format!("{u} {u} 1\n"), &format!("{g} {g} 1\n")).unwrap()
}

/// A PAX record, its length counting itself.
fn record(key: &str, value: &[u8]) -> Vec<u8> {
    let body = key.len() + value.len() + 3;
    let mut len = body + 1;
    while len != body + len.to_string().len() {
        len = body + len.to_string().len();
    }
    let mut r = format!("{len} {key}=").into_bytes();
    r.extend_from_slice(value);
    r.push(b'\n');
    r
}

/// A tar of raw headers, each owned by this process unless it says.
struct Tar {
    out: Vec<u8>,
    own: (u64, u64),
}

impl Tar {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            own: own(),
        }
    }

    /// A header of `name` and type `t` declaring `size` bytes, then `data`
    /// padded to a block.
    fn sized(
        &mut self,
        name: &[u8],
        t: EntryType,
        size: u64,
        data: &[u8],
        edit: impl FnOnce(&mut Header),
    ) -> &mut Self {
        let mut h = Header::new_gnu();
        let old = h.as_old_mut();
        old.name = [0; 100];
        let n = name.len().min(100);
        old.name[..n].copy_from_slice(&name[..n]);
        h.set_entry_type(t);
        h.set_mode(0o755);
        h.set_uid(self.own.0);
        h.set_gid(self.own.1);
        h.set_mtime(1_000_000);
        h.set_size(size);
        edit(&mut h);
        h.set_cksum();
        self.out.extend_from_slice(h.as_bytes());
        self.out.extend_from_slice(data);
        self.out
            .extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
        self
    }

    fn push(
        &mut self,
        name: &str,
        t: EntryType,
        data: &[u8],
        edit: impl FnOnce(&mut Header),
    ) -> &mut Self {
        let size = u64::try_from(data.len()).unwrap();
        self.sized(name.as_bytes(), t, size, data, edit)
    }

    fn owned(&mut self, name: &str, t: EntryType, (uid, gid): (u64, u64)) -> &mut Self {
        self.push(name, t, b"", |h| {
            h.set_uid(uid);
            h.set_gid(gid);
        })
    }

    fn file(&mut self, name: &str, ids: (u64, u64)) -> &mut Self {
        self.owned(name, EntryType::Regular, ids)
    }

    fn dir(&mut self, name: &str, ids: (u64, u64)) -> &mut Self {
        self.owned(name, EntryType::Directory, ids)
    }

    fn link(
        &mut self,
        t: EntryType,
        name: &str,
        (uid, gid): (u64, u64),
        target: &str,
    ) -> &mut Self {
        self.push(name, t, b"", |h| {
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_link_name(target).unwrap();
        })
    }

    /// A FIFO, device or GNU sparse entry: their numeric fields set.
    fn special(&mut self, name: &str, t: EntryType, (uid, gid): (u64, u64)) -> &mut Self {
        self.push(name, t, b"", |h| {
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_device_major(1).unwrap();
            h.set_device_minor(3).unwrap();
            if let Some(gnu) = h.as_gnu_mut() {
                gnu.set_real_size(0);
            }
        })
    }

    /// The uid field of the next entry as raw bytes (base-256 and others).
    fn raw_uid(&mut self, name: &str, uid: [u8; 8]) -> &mut Self {
        self.push(name, EntryType::Regular, b"", |h| h.as_old_mut().uid = uid)
    }

    /// PAX records of the next entry (`x`), or of every later one (`g`).
    fn pax_of(&mut self, t: EntryType, records: &[(&str, &[u8])], ids: (u64, u64)) -> &mut Self {
        let data: Vec<u8> = records.iter().flat_map(|(k, v)| record(k, v)).collect();
        self.push("pax", t, &data, |h| {
            h.set_uid(ids.0);
            h.set_gid(ids.1);
        })
    }

    fn pax(&mut self, records: &[(&str, &[u8])]) -> &mut Self {
        let own = self.own;
        self.pax_of(EntryType::XHeader, records, own)
    }

    /// A GNU long name (`L`) or long link (`K`) header owned by `ids`.
    fn long(&mut self, t: EntryType, value: &str, ids: (u64, u64)) -> &mut Self {
        let mut data = value.as_bytes().to_vec();
        data.push(0);
        self.push("././@LongLink", t, &data, |h| {
            h.set_uid(ids.0);
            h.set_gid(ids.1);
        })
    }

    fn zero_block(&mut self) -> &mut Self {
        self.out.extend(std::iter::repeat_n(0, 512));
        self
    }

    fn bytes(&self) -> Vec<u8> {
        let mut v = self.out.clone();
        v.extend(std::iter::repeat_n(0, 1024));
        v
    }
}

/// What a scan or an extraction made of a layer.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Pass,
    /// The scan found owners the map lacks; the text names them.
    Unmapped(String),
    /// The extraction failed at a `chown`.
    Chown(PathBuf),
    /// Refused for anything else.
    Refused(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    Overlay,
    Live,
}

fn scanned(form: Form, tar: &[u8]) -> Seen {
    let mut found = Unmapped::owners(own_map());
    let r = match form {
        Form::Overlay => scan_owners(tar, &mut found),
        Form::Live => check_headers_and_owners(tar, &IgnoreList::new(), &mut found),
    };
    match r {
        Err(e) => Seen::Refused(e.to_string()),
        Ok(()) if found.is_empty() => Seen::Pass,
        Ok(()) => Seen::Unmapped(found.to_string()),
    }
}

/// A fresh root, removed on drop.
struct Root {
    _tmp: tempfile::TempDir,
    root: RootPath,
}

fn root() -> Root {
    let tmp = tempfile::tempdir().unwrap();
    let host = fs::canonicalize(tmp.path()).unwrap().join("root");
    fs::create_dir(&host).unwrap();
    let root = RootPath::open(&host).unwrap();
    Root { _tmp: tmp, root }
}

fn extracted(form: Form, tar: &[u8], lower: Option<&[u8]>) -> Seen {
    let r = root();
    let lower_root = lower.map(|l| {
        let lr = root();
        extract_overlay(&lr.root, l, &[], DeviceNodes::Skip).unwrap();
        lr
    });
    let lowers: Vec<&RootPath> = lower_root.iter().map(|l| &l.root).collect();
    let e = match form {
        Form::Overlay => extract_overlay(&r.root, tar, &lowers, DeviceNodes::Skip).map(drop),
        Form::Live => extract_live(&r.root, tar, &IgnoreList::new()),
    };
    match e {
        Ok(()) => Seen::Pass,
        Err(Error::Io {
            op: "chown", path, ..
        }) => Seen::Chown(path),
        Err(e) => Seen::Refused(e.to_string()),
    }
}

/// What a case expects of the scan in one form.
#[derive(Clone, Copy, Debug)]
enum Expect {
    Pass,
    /// Refused for these owners, and so is the extraction, at a `chown`.
    Owners(&'static str),
    /// Refused for a header, and so is the extraction.
    Refused(&'static str),
}

struct Case {
    what: &'static str,
    tar: Vec<u8>,
    /// A lower layer for the overlay form.
    lower: Option<Vec<u8>>,
    overlay: Expect,
    live: Expect,
}

fn case(what: &'static str, t: &Tar, overlay: Expect, live: Expect) -> Case {
    Case {
        what,
        tar: t.bytes(),
        lower: None,
        overlay,
        live,
    }
}

fn both(what: &'static str, t: &Tar, e: Expect) -> Case {
    case(what, t, e, e)
}

const F: (u64, u64) = (FAR, FAR);

fn cases() -> Vec<Case> {
    let o = own();
    let far_uid = (FAR, o.1);
    let far_gid = (o.0, FAR);
    let far_name = "f".repeat(150);
    let mut v = vec![
        both(
            "a PAX uid overrides an own ustar uid",
            Tar::new().pax(&[("uid", b"3000000000")]).file("f", o),
            Expect::Owners("uid 3000000000 (/f)"),
        ),
        both(
            "a PAX gid overrides an own ustar gid",
            Tar::new().pax(&[("gid", b"3000000000")]).file("f", o),
            Expect::Owners("gid 3000000000 (/f)"),
        ),
        both(
            "an own PAX uid over a far ustar uid",
            Tar::new()
                .pax(&[("uid", o.0.to_string().as_bytes())])
                .file("f", far_uid),
            Expect::Pass,
        ),
        both(
            "a PAX uid of -1 is no number: the far ustar uid stands",
            Tar::new().pax(&[("uid", b"-1")]).file("f", far_uid),
            Expect::Owners("uid 3000000000 (/f)"),
        ),
        both(
            "a PAX uid with a space is no number: the own ustar uid stands",
            Tar::new().pax(&[("uid", b" 3000000000")]).file("f", o),
            Expect::Pass,
        ),
        both(
            "a PAX uid with a plus sign is a number",
            Tar::new().pax(&[("uid", b"+3000000000")]).file("f", o),
            Expect::Owners("uid 3000000000 (/f)"),
        ),
        both(
            "a PAX uid past u32",
            Tar::new().pax(&[("uid", b"4294967296")]).file("f", o),
            Expect::Refused("uid 4294967296 is out of range"),
        ),
        both(
            // The `tar` crate stores a PAX id in base-256, which drops bit 63:
            // both read the id that is left.
            "a PAX uid with bit 63 set",
            Tar::new()
                .pax(&[("uid", b"9223372039854775808")])
                .file("f", o),
            Expect::Owners("uid 3000000000 (/f)"),
        ),
        both(
            "a PAX uid of u32::MAX, which chown leaves unchanged",
            Tar::new().pax(&[("uid", b"4294967295")]).file("f", o),
            Expect::Pass,
        ),
        both(
            "a PAX uid of u32::MAX - 1",
            Tar::new().pax(&[("uid", b"4294967294")]).file("f", o),
            Expect::Owners("uid 4294967294 (/f)"),
        ),
        both(
            "two PAX uid records: the first stands, an own one",
            Tar::new()
                .pax(&[("uid", o.0.to_string().as_bytes()), ("uid", b"3000000000")])
                .file("f", o),
            Expect::Pass,
        ),
        both(
            "two PAX uid records: the first stands, a far one",
            Tar::new()
                .pax(&[("uid", b"3000000000"), ("uid", o.0.to_string().as_bytes())])
                .file("f", o),
            Expect::Owners("uid 3000000000 (/f)"),
        ),
        both(
            "a base-256 far uid",
            Tar::new().raw_uid("f", [0x80, 0, 0, 0, 0xb2, 0xd0, 0x5e, 0x00]),
            Expect::Owners("uid 3000000000 (/f)"),
        ),
        both(
            "a base-256 negative uid",
            Tar::new().raw_uid("f", [0xff; 8]),
            Expect::Refused("is out of range"),
        ),
        both(
            "a base-256 uid of u32::MAX",
            Tar::new().raw_uid("f", [0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]),
            Expect::Pass,
        ),
        both(
            "an octal uid that is not a number",
            Tar::new().raw_uid("f", *b"12x4567\0"),
            Expect::Refused("numeric field was not a number"),
        ),
        both(
            "a global PAX uid applies to no entry",
            Tar::new()
                .pax_of(EntryType::XGlobalHeader, &[("uid", b"3000000000")], o)
                .file("f", o),
            Expect::Pass,
        ),
        both(
            "a global PAX header of a far owner",
            Tar::new()
                .pax_of(EntryType::XGlobalHeader, &[("comment", b"x")], F)
                .file("f", o),
            Expect::Pass,
        ),
        both(
            "a local PAX header of a far owner",
            Tar::new()
                .pax_of(EntryType::XHeader, &[("comment", b"x")], F)
                .file("f", o),
            Expect::Pass,
        ),
        both(
            "a GNU long name header of a far owner",
            Tar::new()
                .long(EntryType::GNULongName, &far_name, F)
                .file(&far_name[..100], o),
            Expect::Pass,
        ),
        both(
            "a GNU long link header of a far owner",
            Tar::new().long(EntryType::GNULongLink, &far_name, F).link(
                EntryType::Symlink,
                "s",
                o,
                "short",
            ),
            Expect::Pass,
        ),
        both(
            "a far owner under a GNU long name",
            Tar::new()
                .long(EntryType::GNULongName, &far_name, o)
                .file(&far_name[..100], F),
            Expect::Owners("uid 3000000000 (/ffffffffff"),
        ),
        both(
            "a hardlink of a far owner",
            Tar::new().file("a", o).link(EntryType::Link, "b", F, "a"),
            Expect::Pass,
        ),
        both(
            "a PAX uid on a hardlink",
            Tar::new().file("a", o).pax(&[("uid", b"3000000000")]).link(
                EntryType::Link,
                "b",
                o,
                "a",
            ),
            Expect::Pass,
        ),
        both(
            "whiteouts and an opaque marker of a far owner",
            Tar::new()
                .dir("d/", o)
                .file(".wh.x", F)
                .dir("d/.wh..wh..opq", F),
            Expect::Pass,
        ),
        both(
            "AUFS metadata of a far owner",
            Tar::new().dir(".wh..wh.plnk/", F),
            Expect::Pass,
        ),
        both(
            "a regular entry renamed to a whiteout by a PAX path",
            Tar::new().pax(&[("path", b".wh.x")]).file("f", F),
            Expect::Pass,
        ),
        both(
            "an own entry renamed by a PAX path, its far uid kept",
            Tar::new().pax(&[("path", b"g")]).file("f", F),
            Expect::Owners("uid 3000000000 (/g)"),
        ),
        both(
            "the layer's root of a far owner",
            Tar::new().dir("./", F),
            Expect::Owners("uid 3000000000 (/)"),
        ),
        both(
            "a name that cleans to the root, of a far owner",
            Tar::new().dir("a/..", F),
            Expect::Owners("uid 3000000000 (/)"),
        ),
        both(
            "a name through .. that stays in, of a far owner",
            Tar::new().dir("a/", o).file("a/../b", far_gid),
            Expect::Owners("gid 3000000000 (/b)"),
        ),
        both(
            "a symlink of a far owner",
            Tar::new().link(EntryType::Symlink, "s", F, "target"),
            Expect::Owners("uid 3000000000 (/s)"),
        ),
        case(
            "a FIFO of a far owner: made in the overlay form only",
            Tar::new().special("p", EntryType::Fifo, F),
            Expect::Owners("uid 3000000000 (/p)"),
            Expect::Pass,
        ),
        both(
            "a device of a far owner, which a user namespace leaves out",
            Tar::new().special("c", EntryType::Char, F),
            Expect::Pass,
        ),
        both(
            "a duplicate file, the later of a far owner",
            Tar::new().file("f", o).file("f", F),
            Expect::Owners("uid 3000000000 (/f)"),
        ),
        both(
            "a duplicate directory, the later of a far owner",
            Tar::new().dir("d/", o).dir("d/", F),
            Expect::Owners("uid 3000000000 (/d)"),
        ),
        both(
            "a directory of a far owner, then a file at its name",
            Tar::new().dir("d/", F).file("d", o),
            Expect::Owners("uid 3000000000 (/d)"),
        ),
        both(
            "a far owner after the end-of-archive blocks",
            Tar::new()
                .file("f", o)
                .zero_block()
                .zero_block()
                .file("g", F),
            Expect::Pass,
        ),
        both(
            "a far owner after a lone zero block",
            Tar::new().file("f", o).zero_block().file("g", F),
            Expect::Pass,
        ),
        both(
            "a GNU sparse entry of a far owner",
            Tar::new().special("s", EntryType::GNUSparse, F),
            Expect::Refused("GNU sparse entries are not supported"),
        ),
    ];

    // A PAX `size` frames the entry: the far header in its data is data.
    let mut hidden = Tar::new();
    hidden.file("h", F);
    let hidden = hidden.out;
    let mut t = Tar::new();
    t.pax(&[("size", b"1024")])
        .sized(b"f", EntryType::Regular, 0, &hidden, |_| {});
    let pad = 1024 - hidden.len();
    t.out.extend(std::iter::repeat_n(0, pad));
    v.push(both(
        "a far header inside data a PAX size frames",
        &t,
        Expect::Pass,
    ));
    // A PAX `size` of 0 frames the next block as a header.
    let mut t = Tar::new();
    t.pax(&[("size", b"0")])
        .sized(b"f", EntryType::Regular, 512, &[], |_| {})
        .file("h", F);
    v.push(both(
        "a far header a PAX size of 0 reveals",
        &t,
        Expect::Owners("uid 3000000000 (/h)"),
    ));

    let mut lower = Tar::new();
    lower.file("a", o);
    v.push(Case {
        what: "a hardlink of a far owner into a lower layer, copied with the lower owner",
        tar: Tar::new().link(EntryType::Link, "b", F, "a").bytes(),
        lower: Some(lower.bytes()),
        overlay: Expect::Pass,
        live: Expect::Refused("is neither in this layer nor in the root"),
    });
    v
}

/// Every hostile layer: the scan says what the cases expect; where it
/// passes, so does the extraction, and where it names owners, the
/// extraction fails at a `chown` (here `EPERM`, `EINVAL` in a user
/// namespace).
#[test]
fn red_team_hostile_headers_never_pass_the_owner_scan_where_the_extraction_chowns() {
    let unprivileged = !rustix::process::geteuid().is_root();
    if !unprivileged {
        eprintln!("the extractions are skipped: root may chown to any id");
    }
    let mut failures = Vec::new();
    for c in cases() {
        for (form, expect) in [(Form::Overlay, c.overlay), (Form::Live, c.live)] {
            let scan = scanned(form, &c.tar);
            let ok = match (expect, &scan) {
                (Expect::Pass, Seen::Pass) => true,
                (Expect::Owners(names), Seen::Unmapped(text))
                | (Expect::Refused(names), Seen::Refused(text)) => text.contains(names),
                // A case that needs the lower layer: only its extraction tells.
                (Expect::Refused(_), Seen::Pass) => c.lower.is_some() && form == Form::Live,
                _ => false,
            };
            if !ok {
                failures.push(format!(
                    "{form:?}: {}: scan {scan:?}, expected {expect:?}",
                    c.what
                ));
            }
            if !unprivileged {
                continue;
            }
            let lower = c.lower.as_deref().filter(|_| form == Form::Overlay);
            let extraction = extracted(form, &c.tar, lower);
            let agree = match (&scan, &extraction) {
                (Seen::Pass, Seen::Pass) | (Seen::Unmapped(_), Seen::Chown(_)) => true,
                // Refused for a header, or what only the root shows.
                (Seen::Refused(_) | Seen::Pass, Seen::Refused(_)) => {
                    matches!(expect, Expect::Refused(_))
                }
                _ => false,
            };
            if !agree {
                failures.push(format!(
                    "{form:?}: {}: scan {scan:?}, extraction {extraction:?}",
                    c.what
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// An ignored path of a far owner: the scan of a live root, given the
/// ignore list, skips it as `extract_live` skips the entry. One reached
/// through a symlink of the layer is still named, though the extraction
/// skips it too: a header does not show where it lands. Stricter than the
/// extraction, never laxer.
#[test]
fn red_team_an_ignored_path_s_owner_is_skipped_as_the_extraction_skips_it() {
    let o = own();
    let mut keep = IgnoreList::new();
    keep.insert("/proc").unwrap();
    let live = |tar: &[u8], keep: &IgnoreList| {
        let mut found = Unmapped::owners(own_map());
        check_headers_and_owners(tar, keep, &mut found).unwrap();
        found
    };
    let tar = Tar::new()
        .dir("proc/", F)
        .file("proc/x", F)
        .file("./proc/../proc/y", F)
        .bytes();
    let found = live(&tar, &keep);
    assert!(found.is_empty(), "{found}");
    // The control: without the ignore list, the same layer is refused.
    assert!(
        matches!(scanned(Form::Live, &tar), Seen::Unmapped(t) if t.contains("uid 3000000000 (/proc)"))
    );
    let through = Tar::new()
        .link(EntryType::Symlink, "p", o, "proc")
        .file("p/z", F)
        .bytes();
    let found = live(&through, &keep);
    assert!(
        found.to_string().contains("uid 3000000000 (/p/z)"),
        "{found}"
    );
    if !rustix::process::geteuid().is_root() {
        for tar in [&tar, &through] {
            let r = root();
            r.root.create_dir("proc", 0o755).unwrap();
            extract_live(&r.root, tar.as_slice(), &keep).unwrap();
        }
    }
}

/// Hostile names in a refusal: at most eight uids and eight gids are
/// named, each with the first entry that has it, the rest counted; a name
/// keeps its bytes (the binary escapes the line it prints), and the text
/// stays bounded by sixteen paths whatever the layer holds.
#[test]
fn red_team_a_refusal_names_a_bounded_number_of_hostile_names() {
    let o = own();
    let hostile = "\x1b]0;RNK\x07\n\x1b[2J\r\u{202e}";
    // Directory paths up to 4095 bytes, names up to 255: the longest an
    // overlay scan takes.
    let dir = format!("{}/", "d".repeat(255)).repeat(15);
    let mut t = Tar::new();
    let mut n = 0_u64;
    for i in 0..5_000_u64 {
        let name = format!("{dir}{hostile}{i:0>200}");
        t.long(EntryType::GNULongName, &name, o)
            .file("x", (FAR + i, FAR + i));
        n += 1;
    }
    let tar = t.bytes();
    let mut found = Unmapped::owners(own_map());
    scan_owners(tar.as_slice(), &mut found).unwrap();
    let text = found.to_string();
    assert_eq!(text.matches(hostile).count(), 16, "{}", text.len());
    assert!(
        text.contains(&format!(
            ", and {} more entries with other ids; it maps",
            n - 8
        )),
        "{}",
        &text[text.len() - 200..]
    );
    let bound = 16 * (4095 + 1 + 255 + 32) + 256;
    assert!(text.len() <= bound, "{} > {bound}", text.len());
    assert_eq!(
        found.highest_uid(None),
        Some(u32::try_from(FAR + n - 1).unwrap())
    );
}

/// A live root's ignore list as kaniko mode builds it on `/`: fixed paths,
/// a mount-point file, the below-only entry, and `--ignore-path`s, one over
/// a symlink of the root and two globs.
fn ignored_keep() -> IgnoreList {
    let mut keep = IgnoreList::new();
    for p in ["/proc", "/ranikust", "/etc/mtab", "/etc/hosts", "/var/run"] {
        keep.insert(p).unwrap();
    }
    keep.insert_below("/tmp/apt-key-gpghome").unwrap();
    keep.insert_glob("/var/cache/*").unwrap();
    keep.insert_glob("/opt/*/secret").unwrap();
    keep
}

/// A fresh live root holding what [`ignored_keep`] names, with symlinks of
/// its own: `var/run -> ../run`, as an image has it, and `lnk -> proc`.
fn ignored_root() -> Root {
    let r = root();
    for d in [
        "proc",
        "ranikust",
        "etc",
        "run",
        "var",
        "var/cache",
        "tmp",
        "tmp/apt-key-gpghome",
        "opt",
        "opt/a",
    ] {
        r.root.create_dir(d, 0o755).unwrap();
    }
    r.root.symlink("../run", "var/run").unwrap();
    r.root.symlink("proc", "lnk").unwrap();
    r
}

fn scanned_ignored(tar: &[u8], map: &MappedIds) -> Seen {
    let mut found = Unmapped::owners(map.clone());
    match check_headers_and_owners(tar, &ignored_keep(), &mut found) {
        Err(e) => Seen::Refused(e.to_string()),
        Ok(()) if found.is_empty() => Seen::Pass,
        Ok(()) => Seen::Unmapped(found.to_string()),
    }
}

/// [`extract_live`] into [`ignored_root`] with [`ignored_keep`]: a refused
/// `chown` is `EPERM` for a regular user, `EINVAL` in a user namespace.
fn extracted_ignored(tar: &[u8]) -> Seen {
    let r = ignored_root();
    match extract_live(&r.root, tar, &ignored_keep()) {
        Ok(()) => Seen::Pass,
        Err(Error::Io {
            op: "chown", path, ..
        }) => Seen::Chown(path),
        Err(Error::Unmapped(u)) => Seen::Chown(PathBuf::from(u.to_string())),
        Err(e) => Seen::Refused(e.to_string()),
    }
}

/// Hostile names of entries of a far owner on and around ignored paths:
/// what the scan given the ignore list must say of each.
fn ignored_cases() -> Vec<(&'static str, Vec<u8>, Expect)> {
    let o = own();
    let long = |dir: &str| format!("{dir}/{}", "a".repeat(150));
    let skip = Expect::Pass;
    let mut v = vec![
        (
            "an ignored path",
            Tar::new().file("proc/x", F).bytes(),
            skip,
        ),
        ("a ./ prefix", Tar::new().file("./proc/x", F).bytes(), skip),
        (
            "an absolute name",
            Tar::new().file("/proc/x", F).bytes(),
            skip,
        ),
        (
            "doubled slashes",
            Tar::new().file("//proc//x", F).bytes(),
            skip,
        ),
        (
            "a . component",
            Tar::new().file("proc/./x", F).bytes(),
            skip,
        ),
        ("a trailing /.", Tar::new().dir("proc/.", F).bytes(), skip),
        (
            ".. into an ignored path",
            Tar::new().file("etc/../proc/x", F).bytes(),
            skip,
        ),
        (
            ".. out of an ignored path",
            Tar::new().file("proc/../etc/x", F).bytes(),
            Expect::Owners("uid 3000000000 (/etc/x)"),
        ),
        (
            ".. out of an ignored path to the root",
            Tar::new().dir("proc/..", F).bytes(),
            Expect::Owners("uid 3000000000 (/)"),
        ),
        (
            "the ignored path as a file",
            Tar::new().file("proc", F).bytes(),
            skip,
        ),
        (
            "the ignored path as a symlink, then below it",
            Tar::new()
                .link(EntryType::Symlink, "proc", F, "etc")
                .file("proc/y", F)
                .bytes(),
            skip,
        ),
        (
            "another case",
            Tar::new().file("PROC/x", F).bytes(),
            Expect::Owners("uid 3000000000 (/PROC/x)"),
        ),
        (
            "a sibling the ignored name prefixes",
            Tar::new().file("proc.d", F).bytes(),
            Expect::Owners("uid 3000000000 (/proc.d)"),
        ),
        (
            "a GNU long name in an ignored path",
            Tar::new()
                .long(EntryType::GNULongName, &long("proc"), o)
                .file("etc/short", F)
                .bytes(),
            skip,
        ),
        (
            "a GNU long name out of the ignored ustar name",
            Tar::new()
                .long(EntryType::GNULongName, &long("etc"), o)
                .file("proc/x", F)
                .bytes(),
            Expect::Owners("uid 3000000000 (/etc/aaaa"),
        ),
        (
            "a PAX path out of the ignored ustar name",
            Tar::new()
                .pax(&[("path", b"etc/y")])
                .file("proc/x", F)
                .bytes(),
            Expect::Owners("uid 3000000000 (/etc/y)"),
        ),
        (
            "a PAX path into an ignored path",
            Tar::new()
                .pax(&[("path", b"proc/x")])
                .file("etc/y", F)
                .bytes(),
            skip,
        ),
        (
            "a PAX path through .. into an ignored path",
            Tar::new()
                .pax(&[("path", b"etc/../proc/z")])
                .file("etc/y", F)
                .bytes(),
            skip,
        ),
        (
            "a PAX path through .. out of an ignored path",
            Tar::new()
                .pax(&[("path", b"proc/../etc/z")])
                .file("proc/y", F)
                .bytes(),
            Expect::Owners("uid 3000000000 (/etc/z)"),
        ),
        (
            // The GNU long name stands over the PAX path, for both.
            "a PAX path out of an ignored GNU long name",
            Tar::new()
                .long(EntryType::GNULongName, &long("proc"), o)
                .pax(&[("path", b"etc/p")])
                .file("etc/x", F)
                .bytes(),
            skip,
        ),
        (
            "a hardlink of a far owner into an ignored path",
            Tar::new()
                .file("proc/x", F)
                .link(EntryType::Link, "etc/h", F, "proc/x")
                .bytes(),
            skip,
        ),
        (
            "a hardlink in an ignored path",
            Tar::new()
                .file("etc/f", o)
                .link(EntryType::Link, "proc/h", F, "etc/f")
                .bytes(),
            skip,
        ),
        (
            "a symlink of a far owner to an ignored path",
            Tar::new()
                .link(EntryType::Symlink, "etc/s", F, "/proc/x")
                .bytes(),
            Expect::Owners("uid 3000000000 (/etc/s)"),
        ),
        (
            "a symlink of the root into an ignored path",
            Tar::new().file("lnk/z", F).bytes(),
            Expect::Owners("uid 3000000000 (/lnk/z)"),
        ),
        (
            "an ignored path over a symlink of the root",
            Tar::new().file("var/run/x", F).bytes(),
            skip,
        ),
        (
            "the target of an ignored symlink of the root",
            Tar::new().file("run/x", F).bytes(),
            Expect::Owners("uid 3000000000 (/run/x)"),
        ),
        (
            "an ignored mount-point file, and out of it",
            Tar::new()
                .file("etc/hosts", F)
                .file("etc/hosts/../hostname", far_gid())
                .bytes(),
            Expect::Owners("gid 3000000000 (/etc/hostname)"),
        ),
        (
            "below-only: the entry itself",
            Tar::new().dir("tmp/apt-key-gpghome/", F).bytes(),
            Expect::Owners("uid 3000000000 (/tmp/apt-key-gpghome)"),
        ),
        (
            "below-only: below it",
            Tar::new().file("tmp/apt-key-gpghome/k", F).bytes(),
            skip,
        ),
        (
            "below-only: the entry by a trailing /.",
            Tar::new().dir("tmp/apt-key-gpghome/.", F).bytes(),
            Expect::Owners("uid 3000000000 (/tmp/apt-key-gpghome)"),
        ),
        (
            "below-only: the entry by ..",
            Tar::new().dir("tmp/apt-key-gpghome/k/..", F).bytes(),
            Expect::Owners("uid 3000000000 (/tmp/apt-key-gpghome)"),
        ),
        (
            "a glob: the directory",
            Tar::new().dir("var/cache/", F).bytes(),
            Expect::Owners("uid 3000000000 (/var/cache)"),
        ),
        (
            "a glob: in the directory, a dot name too",
            Tar::new()
                .file("var/cache/apt", F)
                .file("var/cache/.hidden", F)
                .bytes(),
            skip,
        ),
        (
            "a glob in the middle",
            Tar::new().file("opt/a/secret", F).bytes(),
            skip,
        ),
        (
            "a glob in the middle, a level deeper",
            Tar::new().file("opt/a/b/secret", F).bytes(),
            Expect::Owners("uid 3000000000 (/opt/a/b/secret)"),
        ),
        (
            "whiteouts of a far owner of and in ignored paths",
            Tar::new()
                .file(".wh.proc", F)
                .dir("proc/.wh..wh..opq", F)
                .bytes(),
            skip,
        ),
        (
            "the layer's root",
            Tar::new().dir("./", F).bytes(),
            Expect::Owners("uid 3000000000 (/)"),
        ),
        (
            "a PAX path into an ignored path with a NUL, passed over",
            Tar::new()
                .pax(&[("path", b"proc/x\0/../../etc/y")])
                .file("etc/y", F)
                .bytes(),
            Expect::Owners("uid 3000000000 (/etc/y)"),
        ),
    ];
    // Two PAX paths: whichever stands, the scan and the extraction read
    // the same one.
    v.push((
        "two PAX paths, the first into an ignored path",
        Tar::new()
            .pax(&[("path", b"proc/x"), ("path", b"etc/y")])
            .file("etc/z", F)
            .bytes(),
        skip,
    ));
    v
}

fn far_gid() -> (u64, u64) {
    (own().0, FAR)
}

/// Runs [`ignored_cases`] against `map`: the scan must say what each case
/// expects, and never pass where the extraction fails at a `chown`.
/// Stricter than the extraction is allowed (a symlink's route, which a
/// header does not show).
fn ignored_failures(map: &MappedIds, extract: bool) -> Vec<String> {
    let mut failures = Vec::new();
    for (what, tar, expect) in ignored_cases() {
        let scan = scanned_ignored(&tar, map);
        let ok = match (expect, &scan) {
            (Expect::Pass, Seen::Pass) => true,
            (Expect::Owners(names), Seen::Unmapped(text))
            | (Expect::Refused(names), Seen::Refused(text)) => text.contains(names),
            _ => false,
        };
        if !ok {
            failures.push(format!("{what}: scan {scan:?}, expected {expect:?}"));
        }
        if !extract {
            continue;
        }
        let extraction = extracted_ignored(&tar);
        if matches!(extraction, Seen::Chown(_)) && !matches!(scan, Seen::Unmapped(_)) {
            failures.push(format!("{what}: scan {scan:?}, extraction {extraction:?}"));
        }
    }
    failures
}

/// Hostile names on and around the ignored paths of a live root (ADR-0144;
/// T15, T16): the scan, given the ignore list, never passes an entry the
/// extraction with that list chowns. T0: the map is this user's own ids.
#[test]
fn red_team_hostile_names_of_ignored_paths_never_pass_the_live_scan_where_the_extraction_chowns() {
    let unprivileged = !rustix::process::geteuid().is_root();
    let failures = ignored_failures(&own_map(), unprivileged);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// As above as root of a rootless Podman container, whose map holds a far
/// owner out: the extraction's `chown` fails with `EINVAL`, as a build's
/// in a user namespace does (ADR-0098, ADR-0144).
#[test]
#[ignore = "T1: default: chown to unmapped ids fails with EINVAL in a user namespace"]
fn red_team_ignored_paths_in_a_user_namespace_never_pass_the_live_scan_where_the_extraction_chowns()
{
    let read = |p: &str| fs::read_to_string(p).unwrap();
    let map = MappedIds::parse(&read("/proc/self/uid_map"), &read("/proc/self/gid_map")).unwrap();
    let far = u32::try_from(FAR).unwrap();
    assert!(!map.maps_uid(far) && !map.maps_gid(far), "{map}");
    assert!(rustix::process::geteuid().is_root());
    let failures = ignored_failures(&map, true);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Hostile id maps (ADR-0098; T16): malformed, overflowing or zero-count
/// lines are refused in either map; padding, blank lines and line endings
/// change nothing.
#[test]
fn red_team_hostile_id_maps_are_refused() {
    let good = "0 0 1\n";
    for bad in [
        " \t \n\n",
        "0 0 0\n",
        "0 0 4294967296\n",
        "0 0 18446744073709551617\n",
        "2 0 4294967295\n",
        "4294967295 0 2\n",
        "0 0 1\0\n",
        "0\u{a0}0 1\n",
        "\u{660} 0 1\n",
        "0x0 0 1\n",
        "0 0 1e3\n",
        "0 0 1.0\n",
        "0,0,1\n",
        "0 0 1 # comment\n",
        "0 0 1\n1 1\n",
        "-0 0 1\n",
    ] {
        assert_eq!(MappedIds::parse(bad, good), None, "{bad:?}");
        assert_eq!(MappedIds::parse(good, bad), None, "{bad:?}");
    }
    let plain = MappedIds::parse("0 1000 1\n1 100000 65536\n", good).unwrap();
    for same in [
        "\t0\t1000\t1\r\n\n   1   100000   65536   \r\n",
        "1 100000 65536\n\n0 1000 1",
        "00 01000 01\n1 100000 65536\n",
    ] {
        let read = MappedIds::parse(same, good).unwrap();
        assert_eq!(read, plain, "{same:?}");
    }
    // The initial namespace's map, as the kernel prints it.
    let all = MappedIds::parse("         0          0 4294967295\n", good).unwrap();
    assert!(all.maps_uid(0) && all.maps_uid(u32::MAX - 1) && !all.maps_uid(u32::MAX));
}
