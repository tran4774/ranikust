use proptest::prelude::*;

use super::*;

fn header_of(mut h: tar::Header, name: &str, kind: tar::EntryType, size: u64) -> Vec<u8> {
    h.set_path(name).unwrap();
    h.set_entry_type(kind);
    h.set_size(size);
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_cksum();
    h.as_bytes().to_vec()
}

fn header(name: &str, kind: tar::EntryType, size: u64) -> Vec<u8> {
    header_of(tar::Header::new_gnu(), name, kind, size)
}

fn padded(body: &[u8]) -> Vec<u8> {
    let mut v = body.to_vec();
    v.resize(body.len().div_ceil(BLOCK) * BLOCK, 0);
    v
}

/// Reads all of `archive` through [`Framed`] in `chunk`-byte reads.
fn read_all(archive: &[u8], limit: u64, chunk: usize) -> io::Result<()> {
    let mut framed = Framed::new(archive, limit);
    let mut buf = vec![0; chunk];
    while framed.read(&mut buf)? > 0 {}
    Ok(())
}

fn too_large(e: &io::Error) -> Option<&TooLarge> {
    e.get_ref().and_then(|i| i.downcast_ref::<TooLarge>())
}

#[test]
fn an_extension_header_over_the_limit_is_refused_at_its_header() {
    for kind in [
        tar::EntryType::GNULongName,
        tar::EntryType::GNULongLink,
        tar::EntryType::XHeader,
    ] {
        let archive = header("././@LongLink", kind, 2048);
        for chunk in [1, 100, 512, 4096] {
            let err = read_all(&archive, 1024, chunk).unwrap_err();
            assert!(
                too_large(&err).is_some(),
                "{kind:?}, {chunk}-byte reads: {err}"
            );
        }
    }
}

#[test]
fn extension_headers_within_the_limit_and_other_entries_pass() {
    let mut archive = Vec::new();
    archive.extend(header("././@LongLink", tar::EntryType::GNULongName, 600));
    archive.extend(padded(&[b'n'; 600]));
    // A file far larger than the limit is not an extension header.
    archive.extend(header("big", tar::EntryType::Regular, 5000));
    archive.extend(padded(&[0; 5000]));
    // A global PAX header is handed to the caller and streamed.
    archive.extend(header("g", tar::EntryType::XGlobalHeader, 5000));
    archive.extend(padded(&[b'g'; 5000]));
    archive.extend([0; BLOCK]);
    for chunk in [1, 7, 512] {
        read_all(&archive, 1024, chunk).unwrap();
    }
}

#[test]
fn the_size_a_pax_record_gives_the_next_entry_keeps_the_framing() {
    // The PAX set says the next file holds 2048 bytes; its header says 0.
    // Inside those 2048 bytes sits what looks like an oversized long
    // name: it is file data, not a header.
    let record = b"13 size=2048\n";
    let mut archive = Vec::new();
    archive.extend(header("pax", tar::EntryType::XHeader, 13));
    archive.extend(padded(record));
    archive.extend(header("f", tar::EntryType::Regular, 0));
    let mut data = header("././@LongLink", tar::EntryType::GNULongName, 1 << 30);
    data.resize(2048, 0);
    archive.extend(&data);
    // The real next header is an oversized long name, after the data.
    archive.extend(header("././@LongLink", tar::EntryType::GNULongName, 4096));
    for chunk in [1, 512, 1 << 16] {
        let err = read_all(&archive, 1024, chunk).unwrap_err();
        let TooLarge { size, .. } = too_large(&err).expect("refused for its size");
        assert_eq!(
            *size, 4096,
            "{chunk}: the header after the data, not inside it"
        );
    }
}

#[test]
fn every_stop_is_sticky() {
    let mut buf = [0; BLOCK];
    // Over the limit.
    let archive = header("x", tar::EntryType::XHeader, 4096);
    let mut framed = Framed::new(&archive[..], 1024);
    assert!(framed.read(&mut buf).is_err());
    assert!(framed.read(&mut buf).is_err());

    // After a sparse header: the header itself reaches the caller, nothing
    // after it does.
    let mut archive = header("s", tar::EntryType::GNUSparse, 0);
    archive.extend(header(
        "././@LongLink",
        tar::EntryType::GNULongName,
        1 << 40,
    ));
    let mut framed = Framed::new(&archive[..], 1024);
    assert_eq!(framed.read(&mut buf).unwrap(), BLOCK);
    assert!(framed.read(&mut buf).is_err());
    assert!(framed.read(&mut buf).is_err());

    // A size that does not parse.
    let mut bad = header("f", tar::EntryType::Regular, 0);
    bad[124..136].copy_from_slice(b"zzzzzzzzzzz\0");
    let mut framed = Framed::new(&bad[..], 1024);
    assert!(framed.read(&mut buf).is_err());
    assert!(framed.read(&mut buf).is_err());

    // Past the end-of-archive block.
    let mut ended = vec![0; BLOCK];
    ended.extend(header(
        "././@LongLink",
        tar::EntryType::GNULongName,
        1 << 40,
    ));
    let mut framed = Framed::new(&ended[..], 1024);
    assert_eq!(framed.read(&mut buf).unwrap(), BLOCK);
    assert!(framed.read(&mut buf).is_err());
    assert!(framed.read(&mut buf).is_err());
}

#[test]
fn a_huge_raised_limit_reserves_little_up_front() {
    // The PAX set declares a body far past what arrives: the reader must
    // not reserve it before the bytes come.
    let archive = header("pax", tar::EntryType::XHeader, 1 << 40);
    let mut framed = Framed::new(&archive[..], u64::MAX);
    let mut buf = [0; BLOCK];
    assert_eq!(framed.read(&mut buf).unwrap(), BLOCK);
    let (body, _) = framed.pax.as_ref().unwrap();
    assert!(body.capacity() <= PAX_RESERVE);
}

/// One member of a generated archive: optional extension headers, then an
/// entry handed to the caller.
#[derive(Clone, Debug)]
struct Member {
    long_name: Option<usize>,
    long_link: Option<usize>,
    /// A PAX record set, with the `size` it gives the next entry, if any.
    pax: Option<Option<usize>>,
    /// 0 a file, 1 a directory, 2 a global PAX header, 3 a v7 header typed
    /// `x`, which the `tar` crate does not treat as an extension.
    kind: u8,
    size: usize,
}

fn member() -> impl Strategy<Value = Member> {
    (
        proptest::option::of(1usize..1500),
        proptest::option::of(1usize..1500),
        proptest::option::of(proptest::option::of(0usize..3000)),
        0u8..4,
        0usize..3000,
    )
        .prop_map(|(long_name, long_link, pax, kind, size)| Member {
            long_name,
            long_link,
            pax,
            kind,
            size,
        })
}

/// One PAX record, `<length> <key>=<value>\n`, its length counting itself.
fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    loop {
        let next = len.to_string().len() + body.len();
        if next == len {
            return format!("{len}{body}");
        }
        len = next;
    }
}

/// The archive, and whether any PAX record set gives a `size`.
fn archive_of(members: &[Member]) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut sized = false;
    for m in members {
        if let Some(n) = m.long_name {
            out.extend(header(
                "././@LongLink",
                tar::EntryType::GNULongName,
                n as u64,
            ));
            out.extend(padded(&vec![b'n'; n]));
        }
        if let Some(n) = m.long_link {
            out.extend(header(
                "././@LongLink",
                tar::EntryType::GNULongLink,
                n as u64,
            ));
            out.extend(padded(&vec![b'k'; n]));
        }
        let mut body_size = m.size;
        if let Some(pax_size) = m.pax {
            let mut records = b"14 comment=ab\n".to_vec();
            if let Some(s) = pax_size {
                records.extend(pax_record("size", &s.to_string()).into_bytes());
                // Only an entry that is not an extension header takes it.
                if m.kind == 0 || m.kind == 1 {
                    body_size = s;
                    sized = true;
                }
            }
            out.extend(header("pax", tar::EntryType::XHeader, records.len() as u64));
            out.extend(padded(&records));
        }
        let h = match m.kind {
            0 => header("f", tar::EntryType::Regular, m.size as u64),
            1 => header("d/", tar::EntryType::Directory, m.size as u64),
            2 => header("g", tar::EntryType::XGlobalHeader, m.size as u64),
            _ => header_of(
                tar::Header::new_old(),
                "v7",
                tar::EntryType::XHeader,
                m.size as u64,
            ),
        };
        out.extend(h);
        out.extend(padded(&vec![b'b'; body_size]));
    }
    out.extend([0; 2 * BLOCK]);
    // Bytes after the end, which the `tar` crate never reads.
    out.extend([b'z'; 700]);
    (out, sized)
}

/// The `tar` crate this reader mirrors; a change of version must re-check
/// the framing (`src/archive.rs:285-460`).
#[test]
fn the_tar_crate_is_the_version_the_framing_follows() {
    let lock = include_str!("../../../../../Cargo.lock");
    assert!(
        lock.contains("name = \"tar\"\nversion = \"0.4.46\"\n"),
        "the tar crate changed version: re-check limit.rs against it"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The headers [`Framed`] sees are the ones the `tar` crate reads:
    /// those it hands on (with PAX `size` overrides), and in raw mode every
    /// header (where no PAX record gives a `size`, which raw mode ignores).
    #[test]
    fn framed_follows_the_tar_crate(members in proptest::collection::vec(member(), 0..8)) {
        let (bytes, sized) = archive_of(&members);

        let mut archive = tar::Archive::new(Framed::new(&bytes[..], u64::MAX));
        let mut handed = Vec::new();
        for entry in archive.entries().unwrap() {
            handed.push(entry.unwrap().raw_header_position());
        }
        let framed = archive.into_inner();
        let seen: Vec<u64> = framed.headers.iter().filter(|(_, h)| *h).map(|(p, _)| *p).collect();
        prop_assert_eq!(&handed, &seen);

        if !sized {
            let mut archive = tar::Archive::new(Framed::new(&bytes[..], u64::MAX));
            let mut raw = Vec::new();
            for entry in archive.entries().unwrap().raw(true) {
                raw.push(entry.unwrap().raw_header_position());
            }
            let framed = archive.into_inner();
            let all: Vec<u64> = framed.headers.iter().map(|(p, _)| *p).collect();
            prop_assert_eq!(raw, all);
        }
    }
}

/// Red team (ADR-0079, T15): one block of a hostile archive, laid out
/// with no regard for what the `tar` crate will make of it, so that a
/// header may land where the crate reads data and data where it reads a
/// header.
#[derive(Clone, Debug)]
enum Hostile {
    /// A header: an index into [`HOSTILE_TYPES`], its format (0 GNU,
    /// 1 ustar, 2 v7, which the `tar` crate does not count as an extension
    /// even when typed `L`, `K` or `x`), the size it declares, and whether
    /// a body of that size follows.
    Header {
        kind: usize,
        format: u8,
        size: u64,
        body: bool,
    },
    /// A PAX record set (`x`) in a GNU or ustar header, then its body.
    Pax { format: u8, records: Vec<PaxRec> },
    /// Blocks of data, the first a well-formed oversized long name header:
    /// a reader that takes it for a header is out of step.
    Filler(usize),
    /// One block of zeros: the end of the archive to the `tar` crate.
    Zeros,
}

/// One record of a hostile PAX record set.
#[derive(Clone, Debug)]
enum PaxRec {
    /// `size=<n>`, which the next entry takes as its size.
    Size(u64),
    /// `size=<text>` that does not parse.
    BadSize,
    /// A record whose length prefix lies.
    LyingLength,
    /// A record without `=`.
    NoEquals,
    /// An empty line, which ends the iteration.
    Empty,
    /// `comment=` of this many bytes.
    Comment(usize),
    /// A key that is not UTF-8, then `size`.
    BinaryKey,
}

/// Every typeflag the generator writes: those the `tar` crate treats as
/// extensions, ordinary ones, and ones it hands on without a second look
/// (GNU volume, multi-volume, old GNU long name, contiguous, unknown).
/// `S` (GNU sparse) is left out: [`Framed`] stops at it by design.
const HOSTILE_TYPES: [u8; 15] = *b"LKxg0\x005216VMN7Z";

fn hostile_rec() -> impl Strategy<Value = PaxRec> {
    prop_oneof![
        4 => (0u64..2600).prop_map(PaxRec::Size),
        1 => Just(PaxRec::BadSize),
        1 => Just(PaxRec::LyingLength),
        1 => Just(PaxRec::NoEquals),
        1 => Just(PaxRec::Empty),
        2 => (0usize..1500).prop_map(PaxRec::Comment),
        1 => Just(PaxRec::BinaryKey),
    ]
}

fn hostile_block() -> impl Strategy<Value = Hostile> {
    prop_oneof![
        6 => (0..HOSTILE_TYPES.len(), 0u8..3, 0u64..2600, proptest::bool::weighted(0.8))
            .prop_map(|(kind, format, size, body)| Hostile::Header { kind, format, size, body }),
        3 => (0u8..2, proptest::collection::vec(hostile_rec(), 0..4))
            .prop_map(|(format, records)| Hostile::Pax { format, records }),
        1 => (1usize..4).prop_map(Hostile::Filler),
        1 => Just(Hostile::Zeros),
    ]
}

fn hostile_header(format: u8, kind: u8, size: u64) -> Vec<u8> {
    let h = match format {
        0 => tar::Header::new_gnu(),
        1 => tar::Header::new_ustar(),
        _ => tar::Header::new_old(),
    };
    header_of(h, "n", tar::EntryType::new(kind), size)
}

fn hostile_records(records: &[PaxRec]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        match r {
            PaxRec::Size(n) => out.extend(pax_record("size", &n.to_string()).into_bytes()),
            PaxRec::BadSize => out.extend(pax_record("size", "12x").into_bytes()),
            PaxRec::LyingLength => out.extend(b"99 size=1\n"),
            PaxRec::NoEquals => out.extend(b"9 sizeab\n"),
            PaxRec::Empty => out.push(b'\n'),
            PaxRec::Comment(n) => {
                out.extend(pax_record("comment", &"c".repeat(*n)).into_bytes());
            }
            PaxRec::BinaryKey => {
                out.extend(b"12 \xffsize=77\n");
                out.extend(pax_record("size", "1").into_bytes());
            }
        }
    }
    out
}

fn hostile_archive(blocks: &[Hostile]) -> Vec<u8> {
    let mut out = Vec::new();
    for b in blocks {
        match b {
            Hostile::Header {
                kind,
                format,
                size,
                body,
            } => {
                out.extend(hostile_header(*format, HOSTILE_TYPES[*kind], *size));
                if *body {
                    out.extend(padded(&vec![b'b'; usize::try_from(*size).unwrap()]));
                }
            }
            Hostile::Pax { format, records } => {
                let body = hostile_records(records);
                out.extend(hostile_header(*format, b'x', body.len() as u64));
                out.extend(padded(&body));
            }
            Hostile::Filler(n) => {
                out.extend(header(
                    "././@LongLink",
                    tar::EntryType::GNULongName,
                    1 << 30,
                ));
                out.extend(vec![b'f'; (n - 1) * BLOCK]);
            }
            Hostile::Zeros => out.extend([0; BLOCK]),
        }
    }
    out.extend([0; 2 * BLOCK]);
    out.extend([b'z'; 700]);
    out
}

/// What the `tar` crate hands on from an archive read through [`Framed`]:
/// each entry's header position, the longest name, link name or PAX
/// record any entry carries, whether iteration ended in an error, and
/// whether that error was [`Framed`]'s.
#[derive(Debug)]
struct Handed {
    positions: Vec<u64>,
    longest: usize,
    error: bool,
    framed_error: bool,
}

fn hand_on(bytes: &[u8], limit: u64) -> (Handed, Vec<(u64, bool)>) {
    let mut archive = tar::Archive::new(Framed::new(bytes, limit));
    let mut handed = Handed {
        positions: Vec::new(),
        longest: 0,
        error: false,
        framed_error: false,
    };
    for entry in archive.entries().unwrap() {
        match entry {
            Ok(mut e) => {
                handed.positions.push(e.raw_header_position());
                let mut longest = e.path_bytes().len();
                longest = longest.max(e.link_name_bytes().map_or(0, |l| l.len()));
                // On an `x` or `g` entry handed on, `pax_extensions` reads
                // the entry's own body, which the caller streams by choice
                // (the extractor skips `g` and refuses a v7 `x`): only the
                // record set attached to an entry is the crate's buffer.
                let kind = e.header().entry_type();
                let own_body = kind.is_pax_global_extensions() || kind.is_pax_local_extensions();
                if !own_body && let Ok(Some(pax)) = e.pax_extensions() {
                    for rec in pax.flatten() {
                        longest = longest.max(rec.key_bytes().len() + rec.value_bytes().len());
                    }
                }
                handed.longest = handed.longest.max(longest);
            }
            Err(e) => {
                handed.error = true;
                // The `tar` crate wraps the reader's error: look through
                // the chain for one of `Framed`'s.
                let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
                while let Some(s) = source {
                    let text = s.to_string();
                    if s.downcast_ref::<TooLarge>().is_some()
                        || text.contains("past the end")
                        || text.contains("invalid size")
                        || text.contains("sparse")
                    {
                        handed.framed_error = true;
                    }
                    source = s.source();
                }
                break;
            }
        }
    }
    let framed = archive.into_inner();
    (handed, framed.headers)
}

/// The same archive read by the `tar` crate on its own.
fn hand_on_bare(bytes: &[u8]) -> (Vec<u64>, bool) {
    let mut archive = tar::Archive::new(bytes);
    let mut positions = Vec::new();
    for entry in archive.entries().unwrap() {
        match entry {
            Ok(e) => positions.push(e.raw_header_position()),
            Err(_) => return (positions, true),
        }
    }
    (positions, false)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// Red team (ADR-0079, T15): an archive laid out to put [`Framed`] out
    /// of step with the `tar` crate (headers where data goes, PAX record
    /// sets with lying, unparsable, repeated or binary `size` records, v7
    /// headers typed as extensions, unknown typeflags, early zero blocks)
    /// never does. With no bound, [`Framed`] changes nothing the `tar`
    /// crate hands on and fails no archive the crate reads, and the
    /// entries it counts as handed on are the crate's. With a bound, no
    /// entry the crate hands on carries a name, link name or PAX record
    /// longer than the bound, so no extension body over it was read.
    #[test]
    fn red_team_framed_never_falls_out_of_step_with_the_tar_crate(
        blocks in proptest::collection::vec(hostile_block(), 0..12),
    ) {
        let bytes = hostile_archive(&blocks);
        let (bare, bare_error) = hand_on_bare(&bytes);
        let (handed, seen) = hand_on(&bytes, u64::MAX);
        prop_assert_eq!(&handed.positions, &bare, "unbounded: what the crate hands on changed");
        prop_assert_eq!(handed.error, bare_error, "unbounded: an error appeared or went");
        // Framed may be the first to see a header the crate would refuse
        // (a size that does not parse), never one the crate reads.
        prop_assert!(
            !handed.framed_error || bare_error,
            "unbounded: Framed failed an archive the crate reads: {:?}",
            handed
        );
        let counted: Vec<u64> = seen.iter().filter(|(_, h)| *h).map(|(p, _)| *p).collect();
        // The crate may stop at a header it read and refused (a bad
        // checksum), which Framed saw: at most that one more.
        prop_assert!(
            counted.starts_with(&handed.positions)
                && counted.len() <= handed.positions.len() + usize::from(handed.error),
            "Framed's handed headers {:?} against the crate's {:?}",
            counted,
            handed.positions
        );

        for bound in [1u64, 100, 700, 1500] {
            let (limited, _) = hand_on(&bytes, bound);
            prop_assert!(
                limited.longest as u64 <= bound,
                "bound {}: an entry carries {} bytes of extension text",
                bound,
                limited.longest
            );
            if !limited.error {
                prop_assert_eq!(&limited.positions, &bare, "bound {}: entries changed", bound);
            }
        }
    }
}
