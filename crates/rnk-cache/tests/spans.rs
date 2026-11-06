//! The `rnk_cache.key` span the metrics report reads (ADR-0025).
//!
//! A test binary of its own, and every test creates its [`Capture`] before
//! it reaches a span: tracing caches whether a span is wanted once per
//! process, and a test reaching it with no subscriber alive could race a
//! `Capture` into caching "never".

use rnk_cache::{CacheKey, ContentHasher, Encoding, File, FileKind, KeyInputs, TimePolicy};
use rnk_metrics::Capture;
use rnk_oci::{Compression, Platform};

fn file(path: &str, kind: FileKind) -> File {
    File {
        path: path.as_bytes().to_vec(),
        kind,
        mode: 0o644,
        uid: 0,
        gid: 0,
        capability: None,
        mtime: 0,
    }
}

const ENCODING: Encoding = Encoding {
    compression: Compression::Gzip,
    level: 6,
    times: TimePolicy::Kept,
};

fn regular(path: &str, bytes: &[u8]) -> File {
    let mut h = ContentHasher::new();
    h.update(bytes);
    file(path, FileKind::Regular(h.finish()))
}

#[test]
fn a_key_counts_the_files_and_bytes_it_hashed() {
    let capture = Capture::new();
    let root = CacheKey::root(None, &Platform::new("linux", "amd64"));
    capture.run(|| {
        let mut inputs = KeyInputs::new(&ENCODING);
        inputs
            .file(regular("a", &[0; 10]))
            .file(regular("b", b"hello"))
            .file(file("d", FileKind::Directory))
            .file(file("l", FileKind::Symlink(b"a".to_vec())));
        let copy = root.next("COPY . /", inputs);
        // A second step with files, so the report's sum shows.
        let mut inputs = KeyInputs::new(&ENCODING);
        inputs.file(regular("c", b"abc"));
        (copy, root.next("COPY c /", inputs))
    });
    let report = capture.report();
    let key = &report["phases"]["rnk_cache.key"];
    assert_eq!(key["count"], 2, "{report:#}");
    assert_eq!(key["files_hashed"], 3, "{report:#}");
    assert_eq!(key["bytes_hashed"], 18, "{report:#}");
}
