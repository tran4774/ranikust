//! Digests computed while streaming.

use rnk_layer::{Compression, LayerDigests, Sha256Digest};

use crate::support::{Tree, options, plain, sample_tree, sha256_hex};

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

#[test]
fn a_plain_layer_digest_is_its_diff_id() {
    let (tar, d) = sample_tree().encode_with_digests(plain()).unwrap();
    assert_eq!(d.diff_id.to_string(), sha256(&tar));
    assert_eq!(d.digest, d.diff_id);
    assert_eq!(d.size, u64::try_from(tar.len()).unwrap());
    assert_eq!(d.compression, Compression::None);
}

#[test]
fn a_gzip_layer_hashes_both_streams() {
    let tar = sample_tree().encode(plain()).unwrap();
    let (gz, d) = sample_tree()
        .encode_with_digests(options(Compression::Gzip, 4))
        .unwrap();
    assert_eq!(d.diff_id.to_string(), sha256(&tar));
    assert_eq!(d.digest.to_string(), sha256(&gz));
    assert_eq!(d.size, u64::try_from(gz.len()).unwrap());
    assert_eq!(d.compression, Compression::Gzip);
}

#[test]
fn digests_do_not_depend_on_threads() {
    let digests: Vec<LayerDigests> = (1..=6)
        .map(|t| {
            sample_tree()
                .encode_with_digests(options(Compression::Gzip, t))
                .unwrap()
                .1
        })
        .collect();
    assert!(digests.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn the_empty_layer_has_the_well_known_diff_id() {
    // 1024 zero bytes: the diff ID of every empty layer.
    let (_, d) = Tree::default().encode_with_digests(plain()).unwrap();
    assert_eq!(
        d.diff_id.hex(),
        "5f70bf18a086007016e948b04aed3b82103a36bea41755b6cddfaf10ace3c6ef"
    );
    assert_eq!(Sha256Digest::from(*d.diff_id.as_bytes()), d.diff_id);
}
