//! Red team (T15, ADR-0079, ADR-0123): the heap and the `tar-header` limit
//! of `check_headers`, the pass on `/` that reads a streamed layer's
//! headers before any entry reaches the root (T0). Layers are made as they
//! are read, never held. A test binary of its own, with one test: the
//! counting allocator sees the whole process and the limit is process-wide.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};

use rnk_rootfs::{
    DEFAULT_EXTENSION_HEADER_LIMIT, Error, check_headers, set_extension_header_limit,
};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call is forwarded to `System` unchanged; the counters only
// observe sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` is passed on as is.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's contract for `dealloc` is passed on as is.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc` is passed on as is.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: u64 = 1 << 20;

/// How much more memory than at the start the process held at its peak
/// while `f` ran.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

fn header(name: &str, kind: tar::EntryType, size: u64, link: &str) -> Vec<u8> {
    let mut h = tar::Header::new_gnu();
    h.set_path(name).unwrap();
    h.set_entry_type(kind);
    h.set_size(size);
    h.set_mode(0o644);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    if !link.is_empty() {
        h.set_link_name(link).unwrap();
    }
    h.set_cksum();
    h.as_bytes().to_vec()
}

/// A layer made piece by piece as it is read: piece `i` is asked for once
/// the one before it is used up, and the layer ends at the first `None`.
struct Made<F> {
    next: F,
    i: u64,
    piece: Option<Box<dyn Read>>,
}

impl<F: FnMut(u64) -> Option<Box<dyn Read>>> Read for Made<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let piece = match &mut self.piece {
                Some(piece) => piece,
                None => {
                    let Some(piece) = (self.next)(self.i) else {
                        return Ok(0);
                    };
                    self.i += 1;
                    self.piece.insert(piece)
                }
            };
            let n = piece.read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            self.piece = None;
        }
    }
}

fn made(next: impl FnMut(u64) -> Option<Box<dyn Read>>) -> impl Read {
    Made {
        next,
        i: 0,
        piece: None,
    }
}

/// An extension header (or a global PAX header) of `size` bytes of `fill`,
/// its padding included.
fn extension(kind: tar::EntryType, size: u64, fill: u8) -> Box<dyn Read> {
    let pad = (512 - size % 512) % 512;
    Box::new(
        io::Cursor::new(header("././@LongLink", kind, size, ""))
            .chain(io::repeat(fill).take(size))
            .chain(io::repeat(0).take(pad)),
    )
}

/// A PAX record set of exactly `size` bytes: one `key` record whose value
/// fills it.
fn pax_set(key: &str, size: u64) -> Box<dyn Read> {
    let digits = size.to_string();
    let head = format!("{digits} {key}=");
    let value = size - u64::try_from(head.len() + 1).unwrap();
    let pad = (512 - size % 512) % 512;
    Box::new(
        io::Cursor::new(header("pax", tar::EntryType::XHeader, size, ""))
            .chain(io::Cursor::new(head))
            .chain(io::repeat(b'v').take(value))
            .chain(io::Cursor::new(b"\n".to_vec()))
            .chain(io::repeat(0).take(pad)),
    )
}

/// A GNU long name or long link of `size` bytes that names `first`: the
/// slashes after it clean away, so the kernel would take it (#179).
fn long_path(kind: tar::EntryType, size: u64, first: u8) -> Box<dyn Read> {
    let pad = (512 - size % 512) % 512;
    Box::new(
        io::Cursor::new(header("././@LongLink", kind, size, ""))
            .chain(io::Cursor::new([first]))
            .chain(io::repeat(b'/').take(size - 1))
            .chain(io::repeat(0).take(pad)),
    )
}

/// The length of a PAX record of a `key` and a value of `value` bytes, its
/// own digits included.
fn record_len(key: &str, value: u64) -> u64 {
    let body = u64::try_from(key.len()).unwrap() + value + 3;
    let digits = |n: u64| u64::try_from(n.to_string().len()).unwrap();
    let mut len = body + 1;
    while len != body + digits(len) {
        len = body + digits(len);
    }
    len
}

/// A PAX record set of exactly `size` bytes the kernel would take (#179):
/// `user.*` xattrs of 64 KiB (`XATTR_SIZE_MAX`), as many as fit, and a
/// `comment` record that fills the rest.
fn pax_xattrs(size: u64) -> Box<dyn Read> {
    let record = |key: &str, value: u64| -> Box<dyn Read> {
        let len = record_len(key, value);
        Box::new(
            io::Cursor::new(format!("{len} {key}="))
                .chain(io::repeat(b'v').take(value))
                .chain(io::Cursor::new(b"\n")),
        )
    };
    let mut body: Box<dyn Read> = Box::new(io::empty());
    let mut left = size;
    for i in 0.. {
        let key = format!("SCHILY.xattr.user.v{i:02}");
        let len = record_len(&key, 1 << 16);
        // Room for the `comment` record after it.
        if len + 64 > left {
            break;
        }
        body = Box::new(body.chain(record(&key, 1 << 16)));
        left -= len;
    }
    // `<left> comment=<value>\n`
    let value = left - u64::try_from(format!("{left} comment=\n").len()).unwrap();
    assert_eq!(record_len("comment", value), left);
    let pad = (512 - size % 512) % 512;
    Box::new(
        io::Cursor::new(header("pax", tar::EntryType::XHeader, size, ""))
            .chain(body)
            .chain(record("comment", value))
            .chain(io::repeat(0).take(pad)),
    )
}

fn piece(bytes: Vec<u8>) -> Option<Box<dyn Read>> {
    Some(Box::new(io::Cursor::new(bytes)))
}

/// `body` (pieces), then a one-byte file and the end of the archive.
fn layer(body: Vec<Box<dyn Read>>) -> impl Read {
    let mut pieces = body.into_iter();
    let mut tail = header("f", tar::EntryType::Regular, 1, "");
    tail.push(b'x');
    tail.resize(tail.len() + 511, 0);
    tail.extend([0; 1024]);
    let mut tail = Some(tail);
    made(move |_| pieces.next().or_else(|| tail.take().and_then(piece)))
}

#[test]
fn red_team_check_headers_holds_one_entry_at_a_time_within_the_tar_header_limit() {
    assert_eq!(DEFAULT_EXTENSION_HEADER_LIMIT, MIB);

    // An extension header that declares 64 MiB is refused at its header.
    for kind in [
        tar::EntryType::GNULongName,
        tar::EntryType::GNULongLink,
        tar::EntryType::XHeader,
    ] {
        let tar = layer(vec![extension(kind, 64 * MIB, b'a')]);
        let (checked, peak) = peak_of(|| check_headers(tar));
        let e = checked.unwrap_err();
        assert!(
            matches!(e, Error::ExtensionHeader { size, max } if size == 64 * MIB && max == MIB),
            "{kind:?}: {e}"
        );
        assert!(peak < 2 << 20, "{kind:?}: held {peak} bytes");
    }

    // Entries each with a GNU long name, a GNU long link and a PAX record
    // set at the limit, of values the kernel takes: what one entry needs is
    // let go before the next.
    const AT_LIMIT: u64 = 16;
    let tar = made(|i| match (i, i % 4) {
        (i, _) if i == AT_LIMIT * 4 => piece(vec![0; 1024]),
        (i, _) if i > AT_LIMIT * 4 => None,
        (_, 0) => Some(long_path(tar::EntryType::GNULongName, MIB, b'n')),
        (_, 1) => Some(long_path(tar::EntryType::GNULongLink, MIB, b't')),
        (_, 2) => Some(pax_xattrs(MIB)),
        _ => piece(header("s", tar::EntryType::Link, 0, "l")),
    });
    let (checked, at_limit) = peak_of(|| check_headers(tar));
    checked.unwrap();

    // Many tiny entries hold nothing from one to the next.
    const TINY: u64 = 100_000;
    let one = header("f", tar::EntryType::Regular, 0, "");
    let tar = made(move |i| match i {
        _ if i < TINY => piece(one.clone()),
        _ if i == TINY => piece(vec![0; 1024]),
        _ => None,
    });
    let (checked, tiny) = peak_of(|| check_headers(tar));
    checked.unwrap();

    // A global PAX header is skipped, never buffered, whatever its size.
    let tar = layer(vec![extension(
        tar::EntryType::XGlobalHeader,
        256 * MIB,
        b'g',
    )]);
    let (checked, global) = peak_of(|| check_headers(tar));
    checked.unwrap();

    println!(
        "check_headers peaks: {AT_LIMIT} entries at the limit {at_limit}, {TINY} tiny \
         entries {tiny}, a 256 MiB global header {global}"
    );
    assert!(
        at_limit < 16 << 20,
        "{AT_LIMIT} entries at the limit held {at_limit} bytes"
    );
    assert!(tiny < 1 << 20, "{TINY} tiny entries held {tiny} bytes");
    assert!(global < 2 << 20, "a global header held {global} bytes");

    // The check follows `--limit tar-header`, lowered and raised, as the
    // extraction does.
    let pax = |size| layer(vec![pax_set("comment", size)]);
    check_headers(pax(16 << 10)).unwrap();
    set_extension_header_limit(8 << 10);
    let lowered = check_headers(pax(16 << 10));
    set_extension_header_limit(4 * MIB);
    let raised = check_headers(pax(2 * MIB));
    set_extension_header_limit(DEFAULT_EXTENSION_HEADER_LIMIT);
    assert!(
        matches!(lowered, Err(Error::ExtensionHeader { size, max: 8192 }) if size == 16 << 10),
        "{lowered:?}"
    );
    raised.unwrap();
    let e = check_headers(pax(2 * MIB)).unwrap_err();
    assert!(matches!(e, Error::ExtensionHeader { .. }), "{e}");
}
