//! Golden keys and tags: they pin the algorithm, so a change to what a key
//! or a tag hashes, or how, fails here.

use rnk_cache::{
    CacheKey, CopyFrom, Encoding, File, FileKind, KEY_VERSION, KeyInputs, Mount, Network, Sharing,
    TimePolicy,
};
use rnk_oci::{Compression, Digest, Platform};

use crate::support::{EPOCH, Spec, content, digest, encoding, file, full, linux_amd64, root};

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
    golden("KEY_VERSION", &KEY_VERSION.to_string(), "2");
}

#[test]
fn root_keys() {
    golden(
        "scratch root",
        &hex(&CacheKey::root(None, &linux_amd64())),
        "48207fdaf7bfb8b8226df64a7c68c029cddc50c760033e851d36175379f95f21",
    );
    golden(
        "image root",
        &hex(&root()),
        "4d669e4d32682f1268681f288cbeb8fe5145c27877827ab1351b80e28622f6b2",
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
        "2e85a0034c248a85db3f24625780aa059cb873ab6f1a8e683e8f1c8e72864faa",
    );
}

#[test]
fn step_keys() {
    let empty = root().next("RUN true", KeyInputs::new(&encoding(None)));
    golden(
        "step with empty inputs",
        &hex(&empty),
        "86d41f6f6d1119c3ef45f38a81e32376f9fcc12f398b27c358376350776a9981",
    );
    golden(
        "step with every input",
        &hex(&full().key(&root())),
        "a949f4ffdf8c45cdd972d636face070a22bc8e7090b6d40f0cf971a06fdee1bf",
    );
    let mut inputs = KeyInputs::new(&encoding(None));
    inputs.copy_from(CopyFrom::Stage(empty));
    golden(
        "COPY --from=<stage> step",
        &hex(&root().next("COPY --from=build /out /", inputs)),
        "c1d2b83e308f988402fa5d9ecc66fdb927acf54befd87690d3a2d6919d787270",
    );
    // The network and sharing names `full` does not use.
    let mut inputs = KeyInputs::new(&encoding(None));
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
        "c4e70a7c1d27863397cb8de41af6e685f48c7c6a8b0dbd2e4cfe35b9a20556a1",
    );
    let mut inputs = KeyInputs::new(&encoding(None));
    inputs.network(Network::Host);
    golden(
        "RUN step with the host network",
        &hex(&root().next("RUN --network=host true", inputs)),
        "744fa5b5038f2f6889712660dd182502cda1c71ecb67a98771c6ba19d20de3b0",
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
        "141f69c2e97d39f95f3aa3738528882951b0d67ed2994a020ecf9fbf3fa0c29b",
    );
    golden(
        "COPY step with the file renamed",
        &hex(&copy(None, vec![hello("b.txt")]).key(&root())),
        "c022ee1e9bf8850e6ca34ef38f811a9d98bba6fe1a6f980b337232a22116fcc2",
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
        "91aad27c42e4e5f0ef254fde9cad0db4e5f28a34090506dcb8da0654276e42a7",
    );
}

/// Each tag's key holds `full`'s inputs made for the tag's encoding, as
/// an entry's always does.
#[test]
fn tags() {
    let spec = full();
    let tag = |compression, level, times| {
        let encoding = Encoding {
            compression,
            level,
            times,
        };
        root()
            .next_entry(&spec.instruction, spec.encoded_as(&encoding))
            .tag()
    };
    golden(
        "gzip tag",
        &tag(Compression::Gzip, 6, TimePolicy::Kept),
        "rnk-v2-94a5f7718b77018ac1be744ed70692af43c77d2716dcf501d6615008216e38a8",
    );
    golden(
        "zstd reproducible tag",
        &tag(Compression::Zstd, 3, TimePolicy::Clamped { epoch: 0 }),
        "rnk-v2-d2b40a1044e564a8368137771e1dbc91191e0e44dbbf88f4a5bacfbfd8c2cc44",
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
        "rnk-v2-5f48e7cee48743c54873ffe8fb557c3fa933d8f1af017e65137a77bad6d5a7b5",
    );
}

/// The encoding as a cache entry's annotation names it. A change here
/// bumps `KEY_VERSION`: without the bump, entries already pushed would be
/// refused, with a warning, instead of missed.
#[test]
fn encoding_annotations() {
    let annotation = |compression, level, times| {
        Encoding {
            compression,
            level,
            times,
        }
        .to_string()
    };
    golden(
        "gzip",
        &annotation(Compression::Gzip, 6, TimePolicy::Kept),
        "gzip;level=6;times=kept",
    );
    golden(
        "zstd reproducible",
        &annotation(Compression::Zstd, 3, TimePolicy::Clamped { epoch: 0 }),
        "zstd;level=3;times=clamped;epoch=0",
    );
    golden(
        "uncompressed",
        &annotation(
            Compression::Uncompressed,
            0,
            TimePolicy::Clamped {
                epoch: -1_700_000_000,
            },
        ),
        "uncompressed;times=clamped;epoch=-1700000000",
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
