//! Golden keys and tags: they pin the algorithm, so a change to what a key
//! or a tag hashes, or how, fails here.

use rnk_cache::{
    CacheKey, CopyFrom, Encoding, File, FileKind, KEY_VERSION, KeyInputs, Mount, Network, Sharing,
    TimePolicy,
};
use rnk_oci::{Compression, Digest, Platform};

use crate::support::{EPOCH, Spec, content, digest, file, full, linux_amd64, root};

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
    let empty = root().next("RUN true", KeyInputs::new(TimePolicy::Kept));
    golden(
        "step with empty inputs",
        &hex(&empty),
        "a8bcfad0a78ac2177b0cc738cc2507ce8f51ee226780e03796ec642986db3529",
    );
    golden(
        "step with every input",
        &hex(&full().key(&root())),
        "a6f3004f53b56028173198b4e295c2e55cc907c27ac7f619391c095c30a12055",
    );
    let mut inputs = KeyInputs::new(TimePolicy::Kept);
    inputs.copy_from(CopyFrom::Stage(empty));
    golden(
        "COPY --from=<stage> step",
        &hex(&root().next("COPY --from=build /out /", inputs)),
        "8f6939bab18a88c37a726427e7148abc691780763a76077017bf5b1a43bd5e5f",
    );
    // The network and sharing names `full` does not use.
    let mut inputs = KeyInputs::new(TimePolicy::Kept);
    inputs.network(Network::Default).mount(Mount::Cache {
        id: "go".to_owned(),
        sharing: Sharing::Private,
        dropped: false,
    });
    golden(
        "RUN step with the default network and a private cache",
        &hex(&root().next(
            "RUN --mount=type=cache,id=go,sharing=private go build",
            inputs,
        )),
        "8e348f6b209e57f9edc39b6cb793d5e88d15ead23897273307e1ba307e1a26aa",
    );
    let mut inputs = KeyInputs::new(TimePolicy::Kept);
    inputs.network(Network::Host);
    golden(
        "RUN step with the host network",
        &hex(&root().next("RUN --network=host true", inputs)),
        "13a30b9e8e4acd874dcf74b1ea4f76dff5d597f980936b1a12b0ff8d8a762c92",
    );
}

#[test]
fn copy_keys() {
    let copy = |epoch, files| Spec {
        epoch,
        instruction: "COPY . /".to_owned(),
        files,
        ..Spec::default()
    };
    let hello = |path| file(path, FileKind::Regular(content(b"hello")));
    golden(
        "COPY step",
        &hex(&copy(None, vec![hello("a.txt")]).key(&root())),
        "6e2dd52a966ca983c7d7b79dc873c0230a6f9be911e80c8b647234cefcc173e3",
    );
    golden(
        "COPY step with the file renamed",
        &hex(&copy(None, vec![hello("b.txt")]).key(&root())),
        "14941e9be42acb99dadc9dc198d496aec98174f9b5a2587c21f0ed1bc214c540",
    );
    // One mtime later than the epoch, clamped, and one earlier, kept.
    let files = vec![
        File {
            mtime: EPOCH + 1000,
            ..hello("new")
        },
        File {
            mtime: EPOCH - 1000,
            ..file("old", FileKind::Directory)
        },
    ];
    golden(
        "COPY step under --reproducible",
        &hex(&copy(Some(EPOCH), files).key(&root())),
        "f8e857ea4ac3d6a0364f8dd16ee784b467f07389b462b75170bcabb16c91ae32",
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
        "rnk-v1-bef53ce32b702cb50fca05750380ccfd7bf08cf3b0e6e0c95d8d06dee2bee59e",
    );
    golden(
        "zstd reproducible tag",
        &tag(Compression::Zstd, 3, TimePolicy::Clamped { epoch: 0 }),
        "rnk-v1-8cbc428fb8592e18fb87c528583b8b23694a9584fdfc1ade3d6874afc5cdb9c3",
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
        "rnk-v1-0e97749319b93b0a381b183a5c5676d710398d8919c49641b38d0d913dd8ab5d",
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
