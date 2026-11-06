//! Golden keys and tags: they pin the algorithm, so a change to what a key
//! or a tag hashes, or how, fails here.

use rnk_cache::{CacheKey, CopyFrom, Encoding, KEY_VERSION, KeyInputs, TimePolicy};
use rnk_oci::{Compression, Digest, Platform};

use crate::support::{content, digest, full, linux_amd64, root};

#[track_caller]
fn golden(what: &str, got: &str, want: &str) {
    assert_eq!(
        got, want,
        "the golden {what} changed, so the key algorithm changed: bump KEY_VERSION \
         (crates/rnk-cache/src/key.rs, now {KEY_VERSION}), then update every golden \
         here; never update a golden without a new KEY_VERSION, or old cache entries \
         would match the new keys"
    );
}

fn hex(key: &CacheKey) -> String {
    Digest::from_sha256_bytes(key.hash()).encoded().to_owned()
}

#[test]
fn the_version_is_pinned_with_the_goldens() {
    golden("KEY_VERSION", &KEY_VERSION.to_string(), "1");
}

#[test]
fn root_keys() {
    golden(
        "scratch root",
        &hex(&CacheKey::root(None, &linux_amd64())),
        "ab8e094e90ab8bc439529a735f850510873e7c8291795d410f331454fb85d805",
    );
    golden(
        "image root",
        &hex(&root()),
        "75e3a7f9230e90b2f22cbd68926efb70731c31c5d4f4a0ef9789b7ff7c9021da",
    );
    let platform = Platform {
        architecture: "arm64".to_owned(),
        os: "linux".to_owned(),
        os_version: Some("10.0".to_owned()),
        os_features: Some(vec!["win32k".to_owned()]),
        variant: Some("v8".to_owned()),
        features: Some(vec!["sse4".to_owned()]),
    };
    golden(
        "root with every platform field",
        &hex(&CacheKey::root(Some(&digest(1)), &platform)),
        "e858411304d2a3c251eec227722b20e0608190a4755ea980123665e087bc3d2b",
    );
}

#[test]
fn step_keys() {
    let empty = root().next("RUN true", KeyInputs::new());
    golden(
        "step with empty inputs",
        &hex(&empty),
        "a8bcfad0a78ac2177b0cc738cc2507ce8f51ee226780e03796ec642986db3529",
    );
    golden(
        "step with every input",
        &hex(&full().key(&root())),
        "c377f4087830580a656708a7adfc42d88d5855f4f5796f58b0a06454771be4cb",
    );
    let mut inputs = KeyInputs::new();
    inputs.copy_from(CopyFrom::Stage(empty));
    golden(
        "COPY --from=<stage> step",
        &hex(&root().next("COPY --from=build /out /", inputs)),
        "8f6939bab18a88c37a726427e7148abc691780763a76077017bf5b1a43bd5e5f",
    );
}

#[test]
fn tags() {
    let key = full().key(&root());
    let tag = |compression, level, times| {
        key.tag(&Encoding {
            compression,
            level,
            times,
        })
    };
    golden(
        "gzip tag",
        &tag(Compression::Gzip, 6, TimePolicy::Kept),
        "rnk-v1-48e68c3c35ae4d1d3e39a87cc7095c8e89cc6b14ade4fa27df19941d25080ac0",
    );
    golden(
        "zstd reproducible tag",
        &tag(Compression::Zstd, 3, TimePolicy::Clamped { epoch: 0 }),
        "rnk-v1-81ff1445263bb5ccb281bcb3db4d0025e3f62ef0d556efbefc8bc334c3928f47",
    );
    golden(
        "uncompressed tag",
        &tag(
            Compression::Uncompressed,
            0,
            TimePolicy::Clamped {
                epoch: 1_700_000_000,
            },
        ),
        "rnk-v1-d6c673f1453a9c68b2ae99ecffb460f02682cf1613083f9e1402282da44ef7de",
    );
}

#[test]
fn content_is_the_sha256_of_the_bytes() {
    let sha = |bytes: &[u8]| {
        Digest::from_sha256_bytes(content(bytes).sha256())
            .encoded()
            .to_owned()
    };
    // The test vectors of FIPS 180-2.
    assert_eq!(
        sha(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(content(b"abc").size(), 3);
}
