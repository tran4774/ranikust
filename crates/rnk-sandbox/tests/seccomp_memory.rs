//! The heap a seccomp profile takes to compile, measured by a counting
//! allocator (T0).
//!
//! A profile comes from the operator. One rule that names a call over and
//! over compiles to millions of instructions, far past the kernel's 4096:
//! the profile is refused with how many, while what the compiler holds grows
//! with the profile's size, not with the program it describes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use rnk_sandbox::{Error, SeccompProfile};

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

/// The peak heap `f` adds above what was allocated when it started.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

/// One rule refusing `read` when six masked arguments match, with `read`
/// named `names` times.
fn profile(names: usize) -> String {
    let args: Vec<String> = (0..6)
        .map(|i| format!(r#"{{"index":{i},"value":255,"valueTwo":{i},"op":"SCMP_CMP_MASKED_EQ"}}"#))
        .collect();
    format!(
        r#"{{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{{"names":[{}],"action":"SCMP_ACT_ERRNO","args":[{}]}}]}}"#,
        vec![r#""read""#; names].join(","),
        args.join(",")
    )
}

/// The instruction count a refusal gives.
fn refused_at(json: &str) -> usize {
    let m = match SeccompProfile::from_json(json.as_bytes()) {
        Err(Error::SeccompProfile(m)) => m,
        other => panic!("not refused: {other:?}"),
    };
    let n = m
        .strip_prefix("the profile compiles to ")
        .and_then(|m| m.strip_suffix(" instructions, more than the kernel's 4096"))
        .unwrap_or_else(|| panic!("{m}"));
    n.parse().unwrap()
}

/// A profile just under the 1 MiB a file may have, one rule naming `read`
/// 149,700 times, each copy 37 instructions (six masked conditions of six,
/// then the action), is refused with the length the whole program would
/// have, holding under 8 MiB (each name costs a few words while the profile
/// is read).
#[test]
fn a_profile_far_past_the_kernel_limit_is_refused_in_bounded_memory() {
    const NAMES: usize = 149_700;
    let small = refused_at(&profile(200));
    let json = profile(NAMES);
    assert!(
        json.len() <= MIB && json.len() > MIB - 1024,
        "{}",
        json.len()
    );
    let (n, peak) = peak_of(|| refused_at(&json));
    println!("{} bytes of profile: peak {peak} bytes", json.len());
    assert_eq!(n, small + (NAMES - 200) * 37, "the whole program's length");
    assert!(peak < 8 * MIB, "peak {peak} bytes");
}
