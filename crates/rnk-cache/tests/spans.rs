//! The `rnk_cache.key`, `rnk_cache.lookup` and `rnk_cache.push` spans the
//! metrics report reads (ADR-0025).
//!
//! A test binary of its own, and every test creates its [`Capture`] before
//! it reaches a span: tracing caches whether a span is wanted once per
//! process, and a test reaching it with no subscriber alive could race a
//! `Capture` into caching "never".

#[path = "../../rnk-registry/tests/registry/fake.rs"]
mod fake;
#[path = "registry/support.rs"]
mod support;

use fake::{Auth, FakeRegistry};
use rnk_cache::{
    CacheKey, ContentHasher, Encoding, File, FileKind, KeyInputs, LayerCache as _, Lookup, Pushed,
    TimePolicy,
};
use rnk_metrics::Capture;
use rnk_oci::{Compression, Platform};
use tracing::instrument::WithSubscriber as _;

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
        // A second step with files, so the report's sum shows; a heredoc's
        // body counts in the bytes, not in the files.
        let mut inputs = KeyInputs::new(&ENCODING);
        inputs.file(regular("c", b"abc")).heredoc("EOF", b"ab");
        (copy, root.next("COPY c /", inputs))
    });
    let report = capture.report();
    let key = &report["phases"]["rnk_cache.key"];
    assert_eq!(key["count"], 2, "{report:#}");
    assert_eq!(key["files_hashed"], 3, "{report:#}");
    assert_eq!(key["bytes_hashed"], 20, "{report:#}");
}

#[tokio::test]
async fn lookups_count_their_hits_and_a_push_its_bytes_out() {
    let capture = Capture::new();
    let registry = FakeRegistry::start(Auth::Open).await;
    let cache = support::cache(&registry, support::WEEK);
    let entry = support::entry("RUN make");
    let layer = support::layer(&[7; 1000]);
    async {
        assert!(matches!(cache.lookup(&entry).await, Lookup::Miss(_)));
        assert!(matches!(cache.push(&entry, &layer).await, Pushed::Done(_)));
        support::hit(cache.lookup(&entry).await);
    }
    .with_subscriber(capture.dispatch().clone())
    .await;

    // The layer, the config and the manifest, all uploaded.
    let manifest = {
        let state = registry.state();
        let (_, bytes) = &state.manifests[&(support::REPO.to_owned(), entry.tag())];
        bytes.len() as u64
    };
    let (stored, _) = support::stored(&registry, &entry.tag());
    let config = stored["config"]["size"].as_u64().unwrap();
    let report = capture.report();
    let phases = &report["phases"];
    assert_eq!(phases["rnk_cache.lookup"]["count"], 2, "{report:#}");
    assert_eq!(phases["rnk_cache.lookup"]["hit"], 1, "{report:#}");
    assert_eq!(phases["rnk_cache.push"]["count"], 1, "{report:#}");
    assert_eq!(
        phases["rnk_cache.push"]["bytes_out"],
        1000 + config + manifest,
        "{report:#}"
    );
}
