//! The `tar-header` limit (ADR-0079), end to end through `extract` (T0):
//! a layer that declares a huge GNU long name, GNU long link or PAX record
//! set is refused at its header, with memory bounded, measured by a
//! counting allocator. A test binary of its own, with one test: the
//! allocator counts the whole process and the limit is process-wide.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};

use rnk_rootfs::{
    DEFAULT_EXTENSION_HEADER_LIMIT, Error, RootPath, Whiteouts, extract, set_extension_header_limit,
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

/// How much more memory than at the start the process held at its peak
/// while `f` ran.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// What a hostile layer declares: far more than the limit.
const HOSTILE: u64 = 64 << 20;

fn header(name: &str, kind: tar::EntryType, size: u64) -> Vec<u8> {
    let mut h = tar::Header::new_gnu();
    h.set_path(name).unwrap();
    h.set_entry_type(kind);
    h.set_size(size);
    h.set_mode(0o644);
    h.set_uid(u64::from(rustix::process::getuid().as_raw()));
    h.set_gid(u64::from(rustix::process::getgid().as_raw()));
    h.set_mtime(0);
    if let Some(gnu) = h.as_gnu_mut() {
        gnu.set_real_size(size);
    }
    h.set_cksum();
    h.as_bytes().to_vec()
}

/// An extension header of `size` bytes, its body made as it is read, then
/// a small file and the end of the archive: the layer is never in memory.
fn layer(kind: tar::EntryType, size: u64, body: u8) -> impl Read {
    let pad = (512 - size % 512) % 512;
    let mut tail = header("f", tar::EntryType::Regular, 1);
    tail.push(b'x');
    tail.resize(tail.len() + 511, 0);
    tail.extend([0; 1024]);
    io::Cursor::new(header("././@LongLink", kind, size))
        .chain(io::repeat(body).take(size))
        .chain(io::repeat(0).take(pad))
        .chain(io::Cursor::new(tail))
}

fn new_root() -> (tempfile::TempDir, RootPath) {
    let tmp = tempfile::tempdir().unwrap();
    let host = tmp.path().join("root");
    std::fs::create_dir(&host).unwrap();
    let root = RootPath::open(&host).unwrap();
    (tmp, root)
}

#[test]
fn extension_headers_are_bounded_by_the_tar_header_limit() {
    assert_eq!(DEFAULT_EXTENSION_HEADER_LIMIT, 1 << 20);
    for kind in [
        tar::EntryType::GNULongName,
        tar::EntryType::GNULongLink,
        tar::EntryType::XHeader,
    ] {
        let (_tmp, root) = new_root();
        let (result, peak) =
            peak_of(|| extract(&root, layer(kind, HOSTILE, b'a'), Whiteouts::Overlay));
        let err = result.unwrap_err();
        assert!(
            matches!(err, Error::ExtensionHeader { size: HOSTILE, max } if max == 1 << 20),
            "{kind:?}: {err}"
        );
        let text = err.to_string();
        assert!(
            text.contains("1 MiB") && text.contains("--limit tar-header=<size>"),
            "{text}"
        );
        assert!(peak < 2 << 20, "{kind:?}: held {peak} bytes");
    }

    // A GNU sparse entry named as a whiteout is refused, not skipped as a
    // whiteout would be, so the oversized long name after it is never read.
    for wh in [Whiteouts::Apply, Whiteouts::Overlay] {
        let mut archive = header("a/.wh..wh.x", tar::EntryType::GNUSparse, 0);
        archive.extend(header(
            "././@LongLink",
            tar::EntryType::GNULongName,
            1 << 40,
        ));
        archive.extend([0; 1024]);
        let (_tmp, root) = new_root();
        let (result, peak) = peak_of(|| extract(&root, &archive[..], wh));
        let err = result.unwrap_err();
        assert!(
            matches!(&err, Error::Entry { reason, .. } if reason.contains("sparse")),
            "{wh:?}: {err}"
        );
        assert!(peak < 2 << 20, "{wh:?}: held {peak} bytes");
    }

    // Control: without a working bound, the `tar` crate holds the whole
    // header, and the allocator sees it.
    set_extension_header_limit(u64::MAX);
    let (_tmp, root) = new_root();
    let (result, peak) = peak_of(|| {
        extract(
            &root,
            layer(tar::EntryType::GNULongName, HOSTILE, b'a'),
            Whiteouts::Overlay,
        )
    });
    assert!(!matches!(result, Err(Error::ExtensionHeader { .. })));
    assert!(
        peak >= usize::try_from(HOSTILE).unwrap(),
        "the control held only {peak} bytes"
    );
    set_extension_header_limit(DEFAULT_EXTENSION_HEADER_LIMIT);

    // A PAX record set of 2 MiB (one comment record) passes once the limit
    // is raised, and the file after it is extracted.
    let comment = {
        let value = "c".repeat((2 << 20) - 17);
        let record = format!(" comment={value}\n");
        let len = record.len() + 7; // `2097152` has 7 digits.
        format!("{len}{record}")
    };
    assert_eq!(comment.len(), 2 << 20);
    let pax_layer = || {
        let size = u64::try_from(comment.len()).unwrap();
        let mut tail = header("f", tar::EntryType::Regular, 1);
        tail.push(b'x');
        tail.resize(tail.len() + 511, 0);
        tail.extend([0; 1024]);
        io::Cursor::new(header("pax", tar::EntryType::XHeader, size))
            .chain(io::Cursor::new(comment.clone().into_bytes()))
            .chain(io::Cursor::new(tail))
    };
    let (_tmp, root) = new_root();
    let err = extract(&root, pax_layer(), Whiteouts::Overlay).unwrap_err();
    assert!(matches!(err, Error::ExtensionHeader { .. }), "{err}");
    set_extension_header_limit(4 << 20);
    let (tmp, root) = new_root();
    extract(&root, pax_layer(), Whiteouts::Overlay).unwrap();
    assert!(tmp.path().join("root/f").exists());
    set_extension_header_limit(DEFAULT_EXTENSION_HEADER_LIMIT);
}
