//! Red team of the owner scan's heap (ADR-0098, T15): a layer of a great
//! many entries, each of an owner of its own, with long names and PAX
//! records near the `tar-header` limit, made as it is read. The scan keeps
//! sixteen names and a count, whatever the layer holds. A test binary of
//! its own, with one test: the counting allocator sees the whole process.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};

use rnk_rootfs::{DEFAULT_EXTENSION_HEADER_LIMIT, MappedIds, Unmapped, scan_owners};

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

const MIB: usize = 1 << 20;

/// Entries of short names, each of a uid and gid of its own.
const MANY: u64 = 200_000;
/// Entries whose PAX records come near the `tar-header` limit.
const HEAVY: u64 = 64;
/// The first id; none of them is mapped.
const FIRST: u64 = 100_000;

fn header(name: &str, kind: tar::EntryType, size: usize, id: u64) -> [u8; 512] {
    let mut h = tar::Header::new_gnu();
    h.set_path(name).unwrap();
    h.set_entry_type(kind);
    h.set_size(u64::try_from(size).unwrap());
    h.set_mode(0o644);
    h.set_uid(id);
    h.set_gid(id);
    h.set_mtime(0);
    h.set_cksum();
    *h.as_bytes()
}

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

fn padded(mut v: Vec<u8>) -> Vec<u8> {
    v.extend(std::iter::repeat_n(0, (512 - v.len() % 512) % 512));
    v
}

/// The heavy entry: a PAX `path` of about 4 KiB, a `uid`, a `gid`, and a
/// comment that fills the record set to just under the limit; with where
/// the path's last 200 bytes and the ids' digits are, to be rewritten in
/// place.
struct Heavy {
    bytes: Vec<u8>,
    name_at: usize,
    ids_at: [usize; 2],
}

fn heavy() -> Heavy {
    let dir = format!("{}/", "d".repeat(255)).repeat(15);
    let mut pax = record("path", format!("{dir}{:0>200}", 0).as_bytes());
    let name_at = 512 + pax.len() - 201;
    let mut ids_at = [0; 2];
    for (key, at) in ["uid", "gid"].into_iter().zip(&mut ids_at) {
        let id = record(key, FIRST.to_string().as_bytes());
        *at = 512 + pax.len() + id.len() - 7;
        pax.extend(id);
    }
    let limit = usize::try_from(DEFAULT_EXTENSION_HEADER_LIMIT).unwrap();
    let fill = limit - pax.len() - 64;
    pax.extend(record("comment", &vec![b'c'; fill]));
    let mut bytes = header("pax", tar::EntryType::XHeader, pax.len(), 0).to_vec();
    bytes.extend(padded(pax));
    bytes.extend(header("x", tar::EntryType::Regular, 0, 0));
    Heavy {
        bytes,
        name_at,
        ids_at,
    }
}

/// Writes the `n`-th entry into `buf`: first the heavy ones, then the
/// short ones, then the end-of-archive blocks; in place, so the
/// generator's own heap stays out of the count.
fn entry(n: u64, heavy: &Heavy, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    let id = FIRST + n;
    if n < HEAVY {
        buf.extend_from_slice(&heavy.bytes);
        let name = format!("{n:0>200}");
        buf[heavy.name_at..heavy.name_at + 200].copy_from_slice(name.as_bytes());
        for at in heavy.ids_at {
            buf[at..at + 6].copy_from_slice(id.to_string().as_bytes());
        }
        return true;
    }
    if n < HEAVY + MANY {
        buf.extend_from_slice(&header(&format!("f{n}"), tar::EntryType::Regular, 0, id));
        return true;
    }
    if n == HEAVY + MANY {
        buf.extend_from_slice(&[0; 1024]);
        return true;
    }
    false
}

/// A tar made entry by entry as it is read.
struct Made {
    n: u64,
    heavy: Heavy,
    buf: Vec<u8>,
    at: usize,
}

impl Read for Made {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.buf.len() {
            if !entry(self.n, &self.heavy, &mut self.buf) {
                return Ok(0);
            }
            self.n += 1;
            self.at = 0;
        }
        let k = out.len().min(self.buf.len() - self.at);
        out[..k].copy_from_slice(&self.buf[self.at..self.at + k]);
        self.at += k;
        Ok(k)
    }
}

#[test]
fn red_team_the_owner_scan_holds_sixteen_names_and_a_count_whatever_the_layer_holds() {
    let map = MappedIds::parse("0 1000 1\n", "0 1000 1\n").unwrap();
    let mut found = Unmapped::owners(map);
    let heavy = heavy();
    let tar = Made {
        n: 0,
        buf: Vec::with_capacity(heavy.bytes.len()),
        heavy,
        at: 0,
    };
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    scan_owners(tar, &mut found).unwrap();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    // One PAX record set at a time (the limit, read whole by the `tar`
    // crate, and its parse), sixteen paths of about 4 KiB, buffers.
    assert!(peak < 4 * MIB, "peak {peak} bytes");
    let text = found.to_string();
    let total = HEAVY + MANY;
    assert!(
        text.ends_with(&format!(
            ", and {} more entries with other ids; it maps uid 0 and gid 0",
            total - 8
        )),
        "{}",
        &text[text.len().saturating_sub(200)..]
    );
    assert_eq!(text.matches(" (/dddd").count(), 16);
    assert!(text.len() < 16 * 4_400 + 256, "{} bytes", text.len());
    assert_eq!(
        found.highest_uid(None),
        Some(u32::try_from(FIRST + total - 1).unwrap())
    );
}
