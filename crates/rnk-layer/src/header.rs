//! Tar headers in the PAX format, built by hand.
//!
//! The layout is POSIX.1-2001 pax: a ustar header, preceded by an extended
//! header (`x`) whose records carry what ustar cannot hold. The choices
//! where POSIX leaves room follow what Go's `archive/tar` writes with
//! `Format = FormatPAX`, which kaniko (`kaniko@44aa7ddc0
//! pkg/util/tar_util.go:61-129`), moby and containerd use, so the readers
//! layers are made for see familiar headers:
//!
//! - numeric fields are zero-padded octal ending in NUL; a value that does
//!   not fit is written as 0 and carried by a PAX record (`size`, `uid`,
//!   `gid`, `mtime`);
//! - a name or link target that is longer than 100 bytes or not ASCII gets a
//!   `path` or `linkpath` record; the ustar field keeps its ASCII bytes,
//!   truncated; the ustar prefix field is never used;
//! - user and group names are empty; device numbers are written for every
//!   entry;
//! - PAX records are sorted by key; the extended header is named
//!   `<dir>/PaxHeaders.0/<name>`, with mode, owner and mtime 0.

use std::collections::BTreeMap;
use std::ops::Range;

/// Size of a tar block.
pub(crate) const BLOCK_SIZE: usize = 512;

const NAME: Range<usize> = 0..100;
const MODE: Range<usize> = 100..108;
const UID: Range<usize> = 108..116;
const GID: Range<usize> = 116..124;
const SIZE: Range<usize> = 124..136;
const MTIME: Range<usize> = 136..148;
const CHECKSUM: Range<usize> = 148..156;
const TYPEFLAG: usize = 156;
const LINKNAME: Range<usize> = 157..257;
const MAGIC: Range<usize> = 257..263;
const VERSION: Range<usize> = 263..265;
const DEVMAJOR: Range<usize> = 329..337;
const DEVMINOR: Range<usize> = 337..345;

/// Largest value of an 8-byte octal field (7 digits).
const MAX_OCTAL_8: u64 = 0o7_777_777;
/// Largest PAX extended header Go's `archive/tar` reads (`maxSpecialFileSize`).
const MAX_PAX_SIZE: usize = 1 << 20;

/// The type of a tar entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypeFlag {
    Regular,
    HardLink,
    Symlink,
    Char,
    Block,
    Dir,
    Fifo,
}

impl TypeFlag {
    fn byte(self) -> u8 {
        match self {
            Self::Regular => b'0',
            Self::HardLink => b'1',
            Self::Symlink => b'2',
            Self::Char => b'3',
            Self::Block => b'4',
            Self::Dir => b'5',
            Self::Fifo => b'6',
        }
    }
}

/// The fields of one tar entry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header<'a> {
    /// Full name, with a trailing `/` for directories.
    pub(crate) name: &'a [u8],
    pub(crate) typeflag: TypeFlag,
    /// Permission, setuid, setgid and sticky bits only.
    pub(crate) mode: u32,
    pub(crate) uid: u64,
    pub(crate) gid: u64,
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    /// Symlink target or hardlink name; empty otherwise.
    pub(crate) linkname: &'a [u8],
    /// `(major, minor)`; `(0, 0)` unless a device.
    pub(crate) device: (u32, u32),
    pub(crate) xattrs: Option<&'a BTreeMap<Vec<u8>, Vec<u8>>>,
}

/// A header that the PAX format cannot carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderError {
    /// A device number above 2,097,151, which has no PAX record.
    DeviceTooLarge,
    /// PAX records above 1 MiB, which Go's `archive/tar` refuses to read.
    PaxTooLarge,
}

impl HeaderError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::DeviceTooLarge => "a device number does not fit a tar header",
            Self::PaxTooLarge => "the xattrs need more than 1 MiB of PAX records",
        }
    }
}

/// Appends the encoded header to `out`: an extended header block and its
/// records when needed, then the ustar block.
pub(crate) fn encode(h: &Header<'_>, out: &mut Vec<u8>) -> Result<(), HeaderError> {
    let (major, minor) = h.device;
    if u64::from(major) > MAX_OCTAL_8 || u64::from(minor) > MAX_OCTAL_8 {
        return Err(HeaderError::DeviceTooLarge);
    }

    let mut pax: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    if needs_pax_string(h.name, NAME.len()) {
        pax.insert(b"path".to_vec(), h.name.to_vec());
    }
    if needs_pax_string(h.linkname, LINKNAME.len()) {
        pax.insert(b"linkpath".to_vec(), h.linkname.to_vec());
    }
    let numbers = [
        (&b"size"[..], h.size, SIZE.len()),
        (b"uid", h.uid, UID.len()),
        (b"gid", h.gid, GID.len()),
    ];
    for (key, value, width) in numbers {
        if !fits_octal(value, width) {
            pax.insert(key.to_vec(), value.to_string().into_bytes());
        }
    }
    let mtime = u64::try_from(h.mtime)
        .ok()
        .filter(|&t| fits_octal(t, MTIME.len()));
    if mtime.is_none() {
        pax.insert(b"mtime".to_vec(), h.mtime.to_string().into_bytes());
    }
    for (name, value) in h.xattrs.into_iter().flatten() {
        let mut key = b"SCHILY.xattr.".to_vec();
        key.extend_from_slice(name);
        pax.insert(key, value.clone());
    }

    if !pax.is_empty() {
        let mut records = Vec::new();
        for (key, value) in &pax {
            push_pax_record(&mut records, key, value);
        }
        if records.len() > MAX_PAX_SIZE {
            return Err(HeaderError::PaxTooLarge);
        }
        let mut block = [0u8; BLOCK_SIZE];
        put_string(&mut block[NAME], &pax_header_name(h.name));
        put_octal(&mut block[MODE], 0);
        put_octal(&mut block[UID], 0);
        put_octal(&mut block[GID], 0);
        put_octal(&mut block[SIZE], usize_to_u64(records.len()));
        put_octal(&mut block[MTIME], 0);
        block[TYPEFLAG] = b'x';
        finish_block(&mut block);
        out.extend_from_slice(&block);
        out.extend_from_slice(&records);
        out.resize(out.len() + padding(usize_to_u64(records.len())), 0);
    }

    let mut block = [0u8; BLOCK_SIZE];
    put_string(&mut block[NAME], &to_ascii(h.name));
    put_octal(&mut block[MODE], u64::from(h.mode & 0o7777));
    put_octal(&mut block[UID], h.uid);
    put_octal(&mut block[GID], h.gid);
    put_octal(&mut block[SIZE], h.size);
    put_octal(&mut block[MTIME], mtime.unwrap_or(0));
    block[TYPEFLAG] = h.typeflag.byte();
    put_string(&mut block[LINKNAME], &to_ascii(h.linkname));
    put_octal(&mut block[DEVMAJOR], u64::from(major));
    put_octal(&mut block[DEVMINOR], u64::from(minor));
    finish_block(&mut block);
    out.extend_from_slice(&block);
    Ok(())
}

/// Zero bytes needed after `len` bytes of content to fill the last block.
pub(crate) fn padding(len: u64) -> usize {
    let rest = len % 512;
    if rest == 0 {
        0
    } else {
        // 512 - rest < 512 always fits.
        usize::from(u16::try_from(512 - rest).unwrap_or(0))
    }
}

fn usize_to_u64(n: usize) -> u64 {
    // usize is at most 64 bits on every supported target.
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn needs_pax_string(s: &[u8], width: usize) -> bool {
    s.len() > width || !s.is_ascii()
}

/// Whether `value` fits a NUL-terminated octal field of `width` bytes.
fn fits_octal(value: u64, width: usize) -> bool {
    let digits = width - 1;
    digits >= 22 || value < 1u64 << (3 * digits)
}

/// Writes `value` as zero-padded octal and a NUL, or 0 if it does not fit.
fn put_octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let mut v = if fits_octal(value, field.len()) {
        value
    } else {
        0
    };
    for slot in field[..digits].iter_mut().rev() {
        // Clippy sees that the mask makes the cast lossless.
        *slot = b'0' + (v & 7) as u8;
        v >>= 3;
    }
    field[digits] = 0;
}

/// Copies `s` into a NUL-filled field, truncating. Like Go, a truncated
/// value never ends in `/`, which old readers take for a directory.
fn put_string(field: &mut [u8], s: &[u8]) {
    let n = s.len().min(field.len());
    field[..n].copy_from_slice(&s[..n]);
    let last = field.len() - 1;
    if s.len() > field.len() && field[last] == b'/' {
        let kept = trim_trailing_slashes(&s[..last]).len();
        field[kept] = 0;
    }
}

fn trim_trailing_slashes(s: &[u8]) -> &[u8] {
    let end = s.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
    &s[..end]
}

/// The ASCII bytes of `s` without NUL, as Go's `toASCII`.
fn to_ascii(s: &[u8]) -> Vec<u8> {
    s.iter()
        .copied()
        .filter(|&b| b != 0 && b.is_ascii())
        .collect()
}

/// `<dir>/PaxHeaders.0/<file>` for `<dir>/<file>`, as Go names it.
fn pax_header_name(name: &[u8]) -> Vec<u8> {
    let split = name.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    let (dir, file) = name.split_at(split);
    let mut out = dir.to_vec();
    out.extend_from_slice(b"PaxHeaders.0");
    if !file.is_empty() {
        out.push(b'/');
        out.extend_from_slice(file);
    }
    let mut out = to_ascii(&out);
    out.truncate(NAME.len());
    let kept = trim_trailing_slashes(&out).len();
    out.truncate(kept);
    out
}

/// Appends `"<len> <key>=<value>\n"`, where `<len>` counts the whole record.
fn push_pax_record(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    let base = key.len() + value.len() + 3;
    let mut len = base + decimal_digits(base);
    if decimal_digits(len) != decimal_digits(base) {
        len = base + decimal_digits(len);
    }
    out.extend_from_slice(len.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(key);
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
}

fn decimal_digits(n: usize) -> usize {
    n.to_string().len()
}

/// Sets the ustar magic and version, then the checksum.
fn finish_block(block: &mut [u8; BLOCK_SIZE]) {
    block[MAGIC].copy_from_slice(b"ustar\0");
    block[VERSION].copy_from_slice(b"00");
    block[CHECKSUM].fill(b' ');
    let sum: u64 = block.iter().map(|&b| u64::from(b)).sum();
    // At most 512 * 255, six octal digits, then NUL and a space.
    put_octal(&mut block[CHECKSUM.start..CHECKSUM.end - 1], sum);
    block[CHECKSUM.end - 1] = b' ';
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &[u8]) -> Header<'_> {
        Header {
            name,
            typeflag: TypeFlag::Regular,
            mode: 0o644,
            uid: 0,
            gid: 0,
            size: 0,
            mtime: 0,
            linkname: b"",
            device: (0, 0),
            xattrs: None,
        }
    }

    fn encode_one(h: &Header<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        encode(h, &mut out).unwrap();
        out
    }

    /// Parses `"<len> <key>=<value>\n"` records back into pairs.
    fn parse_records(mut data: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut records = Vec::new();
        while !data.is_empty() {
            let space = data.iter().position(|&b| b == b' ').unwrap();
            let len: usize = std::str::from_utf8(&data[..space])
                .unwrap()
                .parse()
                .unwrap();
            let record = &data[space + 1..len];
            assert_eq!(data[len - 1], b'\n', "record length is off");
            let eq = record.iter().position(|&b| b == b'=').unwrap();
            records.push((
                String::from_utf8(record[..eq].to_vec()).unwrap(),
                record[eq + 1..record.len() - 1].to_vec(),
            ));
            data = &data[len..];
        }
        records
    }

    #[test]
    fn plain_header_matches_the_ustar_layout() {
        let mut h = header(b"etc/passwd");
        h.mode = 0o104_755; // S_IFREG | 04755: the type bits are dropped
        h.uid = 1000;
        h.gid = 100;
        h.size = 1234;
        h.mtime = 1_700_000_000;
        let block = encode_one(&h);
        assert_eq!(block.len(), BLOCK_SIZE);
        assert_eq!(&block[..11], b"etc/passwd\0");
        assert_eq!(&block[MODE], b"0004755\0");
        assert_eq!(&block[UID], b"0001750\0");
        assert_eq!(&block[GID], b"0000144\0");
        assert_eq!(&block[SIZE], b"00000002322\0");
        assert_eq!(&block[MTIME], b"14524770400\0");
        assert_eq!(block[TYPEFLAG], b'0');
        assert_eq!(&block[MAGIC], b"ustar\0");
        assert_eq!(&block[VERSION], b"00");
        assert_eq!(&block[DEVMAJOR], b"0000000\0");
        assert!(block[265..329].iter().all(|&b| b == 0), "uname/gname set");
        let mut check = block.clone();
        check[CHECKSUM].fill(b' ');
        let sum: u64 = check.iter().map(|&b| u64::from(b)).sum();
        assert_eq!(&block[CHECKSUM], format!("{sum:06o}\0 ").as_bytes());
    }

    #[test]
    fn out_of_range_fields_go_to_pax_records() {
        let long_name = [b'a'; 150];
        let mut xattrs = BTreeMap::new();
        xattrs.insert(b"security.capability".to_vec(), vec![1, 0, 0, 2, 0xff]);
        let mut h = header(&long_name);
        h.size = 9 << 30;
        h.uid = 3_000_000;
        h.gid = 2_097_151; // the largest that still fits
        h.mtime = -1;
        h.linkname = "ü".as_bytes();
        h.typeflag = TypeFlag::Symlink;
        h.xattrs = Some(&xattrs);
        let out = encode_one(&h);

        let pax = &out[..BLOCK_SIZE];
        assert_eq!(pax[TYPEFLAG], b'x');
        assert_eq!(&pax[..16], b"PaxHeaders.0/aaa");
        let size = usize::from_str_radix(std::str::from_utf8(&pax[SIZE][..11]).unwrap(), 8);
        let size = size.unwrap();
        let records = parse_records(&out[BLOCK_SIZE..BLOCK_SIZE + size]);
        let keys: Vec<&str> = records.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "SCHILY.xattr.security.capability",
                "linkpath",
                "mtime",
                "path",
                "size",
                "uid"
            ]
        );
        assert_eq!(records[0].1, [1, 0, 0, 2, 0xff]);
        assert_eq!(records[1].1, "ü".as_bytes());
        assert_eq!(records[2].1, b"-1");
        assert_eq!(records[3].1, long_name);
        assert_eq!(records[4].1, b"9663676416");
        assert_eq!(records[5].1, b"3000000");

        let main = &out[out.len() - BLOCK_SIZE..];
        assert_eq!(&main[..100], &long_name[..100]);
        assert_eq!(&main[SIZE], b"00000000000\0");
        assert_eq!(&main[UID], b"0000000\0");
        assert_eq!(&main[GID], b"7777777\0");
        assert_eq!(&main[MTIME], b"00000000000\0");
        assert_eq!(main[LINKNAME.start], 0, "non-ASCII bytes are dropped");
    }

    #[test]
    fn pax_record_length_counts_itself() {
        let mut out = Vec::new();
        // 4 + 90 + 3 = 97 bytes plus 2 digits: 99.
        push_pax_record(&mut out, b"path", &[b'x'; 90]);
        assert_eq!(&out[..3], b"99 ");
        assert_eq!(out.len(), 99);
        // 98 bytes plus 2 digits would be 100, which needs 3 digits: 101.
        out.clear();
        push_pax_record(&mut out, b"path", &[b'x'; 91]);
        assert_eq!(&out[..4], b"101 ");
        assert_eq!(out.len(), 101);
    }

    #[test]
    fn truncated_names_do_not_end_in_a_slash() {
        let mut name = vec![b'a'; 99];
        name.extend_from_slice(b"/bcd");
        let out = encode_one(&header(&name));
        let main = &out[out.len() - BLOCK_SIZE..];
        assert_eq!(&main[97..100], b"aa\0");
        assert_eq!(pax_header_name(b"a/b/"), b"a/b/PaxHeaders.0");
        assert_eq!(pax_header_name(b"x"), b"PaxHeaders.0/x");
        assert_eq!(pax_header_name(b"d/x"), b"d/PaxHeaders.0/x");
    }

    #[test]
    fn rejects_what_pax_cannot_carry() {
        let mut h = header(b"dev/big");
        h.typeflag = TypeFlag::Char;
        h.device = (1, 2_097_152);
        assert_eq!(
            encode(&h, &mut Vec::new()),
            Err(HeaderError::DeviceTooLarge)
        );
        let mut xattrs = BTreeMap::new();
        xattrs.insert(b"user.big".to_vec(), vec![0; MAX_PAX_SIZE]);
        let mut h = header(b"big");
        h.xattrs = Some(&xattrs);
        assert_eq!(encode(&h, &mut Vec::new()), Err(HeaderError::PaxTooLarge));
    }

    #[test]
    fn pads_to_the_block_size() {
        assert_eq!(padding(0), 0);
        assert_eq!(padding(1), 511);
        assert_eq!(padding(512), 0);
        assert_eq!(padding(513), 511);
    }
}
