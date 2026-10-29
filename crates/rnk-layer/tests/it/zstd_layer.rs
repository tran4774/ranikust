//! zstd layers: goldens, thread-count independence, independent frames.

use std::io::Read;

use proptest::collection::vec;
use proptest::prelude::*;
use rnk_layer::{Compression, EntryKind, Error, LayerWriter};

use crate::gzip_layer::big_tree;
use crate::support::{Tree, entry, options, plain, sample_tree, sha256_hex};
use crate::tar_props::{build, spec};

/// sha256 of the sample tree as zstd level 6 (libzstd 1.5.7). Checked with
/// the zstd 1.5.2 CLI (`zstd -t`; `zstd -dc` gives the plain golden) when it
/// was pinned. It changes with the tar bytes, libzstd, the level or the
/// 1 MiB frame size.
const SAMPLE_ZSTD_SHA256: &str = "9678199da88123e65e7901a2e53b2c43ca408011ea16e191174fb9601b7872fc";

/// Decodes every frame with libzstd's streaming decoder.
fn unzstd(blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    zstd::stream::read::Decoder::new(blob)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

/// The number of frames in `blob`.
fn frames(blob: &[u8]) -> usize {
    let mut rest = blob;
    let mut n = 0;
    while !rest.is_empty() {
        let len = zstd::zstd_safe::find_frame_compressed_size(rest).unwrap();
        rest = &rest[len..];
        n += 1;
    }
    n
}

#[test]
fn sample_tree_matches_its_golden_zstd_digest() {
    let blob = sample_tree().encode(options(Compression::Zstd, 1)).unwrap();
    assert_eq!(sha256_hex(&blob), SAMPLE_ZSTD_SHA256);
}

#[test]
fn zstd_layers_are_frames_holding_the_plain_tar() {
    let tar = sample_tree().encode(plain()).unwrap();
    let blob = sample_tree().encode(options(Compression::Zstd, 3)).unwrap();
    assert_eq!(frames(&blob), 1);
    assert_eq!(unzstd(&blob), tar);
}

#[test]
fn thread_count_never_changes_the_bytes() {
    let mut t = big_tree();
    let tar = t.encode(plain()).unwrap();
    let one = t.encode(options(Compression::Zstd, 1)).unwrap();
    assert_eq!(frames(&one), tar.len().div_ceil(1024 * 1024));
    assert!(frames(&one) > 2, "the tree must span several frames");
    for threads in [2, 3, 4, 7, 16] {
        assert_eq!(
            t.encode(options(Compression::Zstd, threads)).unwrap(),
            one,
            "{threads} threads"
        );
    }
    assert_eq!(unzstd(&one), tar);
}

#[test]
fn the_empty_layer_is_one_frame() {
    let mut t = Tree::default();
    let tar = t.encode(plain()).unwrap();
    let blob = t.encode(options(Compression::Zstd, 4)).unwrap();
    assert_eq!(frames(&blob), 1);
    assert_eq!(unzstd(&blob), tar);
}

#[test]
fn zstd_levels_are_checked() {
    for level in [-1, 0, 20, 22] {
        let mut opts = options(Compression::Zstd, 1);
        opts.level = level;
        let err = LayerWriter::new(opts).unwrap_err();
        assert!(
            matches!(
                err,
                Error::InvalidLevel { compression: Compression::Zstd, level: l } if l == level
            ),
            "{err}"
        );
    }
    let mut t = Tree::default();
    t.add(entry("x", EntryKind::Fifo));
    for level in [1, 3, 19] {
        let mut opts = options(Compression::Zstd, 2);
        opts.level = level;
        let blob = t.encode(opts).unwrap();
        assert_eq!(unzstd(&blob), t.encode(plain()).unwrap());
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
        let one = t.encode(options(Compression::Zstd, 1)).unwrap();
        prop_assert_eq!(t.encode(options(Compression::Zstd, threads)).unwrap(), one.clone());
        prop_assert_eq!(unzstd(&one), tar);
    }
}
