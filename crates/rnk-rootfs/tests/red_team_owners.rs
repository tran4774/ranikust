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
        Form::Live => check_headers_and_owners(tar, &mut found),
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
    /// Extracted too, not only scanned.
    extract: bool,
}

fn case(what: &'static str, t: &Tar, overlay: Expect, live: Expect) -> Case {
    Case {
        what,
        tar: t.bytes(),
        lower: None,
        overlay,
        live,
        extract: true,
    }
}

/// Scanned only: a debug build's `Uid::from_raw` and `Gid::from_raw`
/// assert on -1, which the extraction hands `chown` as "unchanged".
fn scan_only(c: Case) -> Case {
    Case {
        extract: false,
        ..c
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
        scan_only(both(
            "a PAX uid of u32::MAX, which chown leaves unchanged",
            Tar::new().pax(&[("uid", b"4294967295")]).file("f", o),
            Expect::Pass,
        )),
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
        scan_only(both(
            "a base-256 uid of u32::MAX",
            Tar::new().raw_uid("f", [0x80, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]),
            Expect::Pass,
        )),
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
        extract: true,
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
            if !unprivileged || !c.extract {
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

/// An ignored path of a far owner: the scan of a live root does not know
/// the ignore list and names it, while `extract_live` skips the entry.
/// Stricter than the extraction, never laxer.
#[test]
fn red_team_an_ignored_path_s_owner_is_still_named() {
    let o = own();
    let tar = Tar::new().dir("proc/", o).file("proc/x", F).bytes();
    assert!(
        matches!(scanned(Form::Live, &tar), Seen::Unmapped(t) if t.contains("uid 3000000000 (/proc/x)"))
    );
    if !rustix::process::geteuid().is_root() {
        let r = root();
        let mut keep = IgnoreList::new();
        keep.insert("/proc").unwrap();
        extract_live(&r.root, tar.as_slice(), &keep).unwrap();
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
