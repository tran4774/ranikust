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
