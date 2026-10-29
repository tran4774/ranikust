//! The `rnk_layer.*` spans the metrics report reads (ADR-0025).
//!
//! A test binary of its own, and every test creates its [`Capture`] before
//! it reaches a span: tracing caches whether a span is wanted once per
//! process, and a test reaching it with no subscriber alive could race a
//! `Capture` into caching "never".

use std::io::{self, Read};

use rnk_layer::{Compression, Decoder};
use rnk_metrics::Capture;

use crate::support::{options, plain, sample_tree};

#[path = "it/support.rs"]
#[allow(
    dead_code,
    reason = "shared with the `it` binary, which uses all of it"
)]
mod support;

const ALL: [Compression; 3] = [Compression::None, Compression::Gzip, Compression::Zstd];

#[test]
fn encode_counts_entries_parents_and_bytes() {
    for compression in ALL {
        let capture = Capture::new();
        // Outside `run`: not in the capture.
        let mut t = sample_tree();
        let tar = t.encode(plain()).unwrap();
        let blob = capture.run(|| t.encode(options(compression, 3)).unwrap());
        let report = capture.report();
        let encode = &report["phases"]["rnk_layer.encode"];
        // The eight `known_parent` directories of the sample tree.
        let parents = 8;
        assert_eq!(encode["count"], 1, "{compression}");
        assert_eq!(encode["parents_added"], parents, "{compression}");
        assert_eq!(
            encode["entries"],
            t.entries.len() + parents,
            "{compression}"
        );
        assert_eq!(encode["bytes_in"], tar.len(), "{compression}");
        assert_eq!(encode["bytes_out"], blob.len(), "{compression}");
        assert_eq!(encode["max_threads"], 3, "{compression}");
    }
}

#[test]
fn decode_counts_blob_and_tar_bytes_when_dropped() {
    for compression in ALL {
        let capture = Capture::new();
        let tar = sample_tree().encode(plain()).unwrap();
        let blob = sample_tree().encode(options(compression, 2)).unwrap();
        capture.run(|| {
            let mut d = Decoder::new(&blob[..]).unwrap();
            io::copy(&mut d, &mut io::sink()).unwrap();
        });
        let report = capture.report();
        let decode = &report["phases"]["rnk_layer.decode"];
        assert_eq!(decode["count"], 1, "{compression}");
        assert_eq!(decode["bytes_in"], blob.len(), "{compression}");
        assert_eq!(decode["bytes_out"], tar.len(), "{compression}");
    }
}

#[test]
fn a_decoder_dropped_early_counts_what_it_read() {
    let capture = Capture::new();
    let blob = sample_tree().encode(options(Compression::Zstd, 1)).unwrap();
    capture.run(|| {
        let mut d = Decoder::new(&blob[..]).unwrap();
        d.read_exact(&mut [0; 512]).unwrap();
    });
    let report = capture.report();
    let decode = &report["phases"]["rnk_layer.decode"];
    assert_eq!(decode["bytes_out"], 512);
    let bytes_in = decode["bytes_in"].as_u64().unwrap();
    assert!(bytes_in > 0 && bytes_in <= u64::try_from(blob.len()).unwrap());
}
