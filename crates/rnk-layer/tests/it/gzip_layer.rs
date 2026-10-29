//! gzip layers: goldens, thread-count independence, one member.

use std::io::Read;

use proptest::collection::vec;
use proptest::prelude::*;
use rnk_layer::{Compression, EntryKind, Error, LayerWriter};

use crate::support::{Tree, entry, options, plain, sample_tree, sha256_hex};
use crate::tar_props::{build, spec};

/// sha256 of the sample tree as gzip level 6. Checked with GNU gzip 1.14
/// (`gzip -t`; `zcat` gives the plain golden) when it was pinned. It changes
/// with the tar bytes, zlib-rs, the level or the 128 KiB block size.
const SAMPLE_GZIP_SHA256: &str = "6897f043dc0d706aa58b3249a23a57f49a47cf404b9985802dc658712f04e88a";

/// Decodes exactly one gzip member and checks nothing follows it.
pub(crate) fn gunzip_one_member(gz: &[u8]) -> Vec<u8> {
    let mut decoder = flate2::bufread::GzDecoder::new(gz);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap();
    assert!(
        decoder.into_inner().is_empty(),
        "bytes after the first member"
    );
    out
}

/// A tree of a few MiB, so that layers span many gzip blocks and several
/// zstd frames.
pub(crate) fn big_tree() -> Tree {
    let mut t = Tree::default();
    t.dir("data", 0o755);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in 0..12 {
        let len = 100_000 + i * 37_000;
        let content: Vec<u8> = (0..len)
            .map(|j| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if j % 5 == 0 {
                    x.to_le_bytes()[0]
                } else {
                    b"ranikust"[j % 8]
                }
            })
            .collect();
        t.file(&format!("data/f{i:02}"), &content);
    }
    t
}

#[test]
fn sample_tree_matches_its_golden_gzip_digest() {
    let gz = sample_tree().encode(options(Compression::Gzip, 1)).unwrap();
    assert_eq!(sha256_hex(&gz), SAMPLE_GZIP_SHA256);
}

#[test]
fn gzip_layers_are_one_member_holding_the_plain_tar() {
    let tar = sample_tree().encode(plain()).unwrap();
    let gz = sample_tree().encode(options(Compression::Gzip, 3)).unwrap();
    assert_eq!(gunzip_one_member(&gz), tar);
}

#[test]
fn thread_count_never_changes_the_bytes() {
    let mut t = big_tree();
    let tar = t.encode(plain()).unwrap();
    assert!(
        tar.len() > 20 * 128 * 1024,
        "the tree must span many blocks"
    );
    let one = t.encode(options(Compression::Gzip, 1)).unwrap();
    for threads in [2, 3, 4, 7, 8, 16] {
        assert_eq!(
            t.encode(options(Compression::Gzip, threads)).unwrap(),
            one,
            "{threads} threads"
        );
    }
    assert_eq!(gunzip_one_member(&one), tar);
}

#[test]
fn repeated_runs_at_many_threads_give_the_same_bytes() {
    let mut t = big_tree();
    let first = t.encode(options(Compression::Gzip, 12)).unwrap();
    for _ in 0..5 {
        assert_eq!(t.encode(options(Compression::Gzip, 12)).unwrap(), first);
    }
}

#[test]
fn gzip_levels_are_checked() {
    for level in [-1, 10] {
        let mut opts = options(Compression::Gzip, 1);
        opts.level = level;
        let err = LayerWriter::new(opts).unwrap_err();
        assert!(
            matches!(err, Error::InvalidLevel { level: l, .. } if l == level),
            "{err}"
        );
    }
    let mut t = Tree::default();
    t.add(entry("x", EntryKind::Fifo));
    for level in [0, 1, 9] {
        let mut opts = options(Compression::Gzip, 2);
        opts.level = level;
        let gz = t.encode(opts).unwrap();
        assert_eq!(gunzip_one_member(&gz), t.encode(plain()).unwrap());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn random_layers_round_trip_at_any_thread_count(
        specs in vec(spec(), 0..16),
        seed: u64,
        threads in 2usize..9,
    ) {
        let mut t = build(&specs, seed);
        let tar = t.encode(plain()).unwrap();
        let one = t.encode(options(Compression::Gzip, 1)).unwrap();
        prop_assert_eq!(t.encode(options(Compression::Gzip, threads)).unwrap(), one.clone());
        prop_assert_eq!(gunzip_one_member(&one), tar);
    }
}
