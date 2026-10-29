//! The decoder: blobs back to the tar stream and its diff ID.

use std::io::{self, Read, Write};

use proptest::collection::vec;
use proptest::prelude::*;
use rnk_layer::{Compression, Decoder};

use crate::support::{options, plain, read_tar, sample_tree};
use crate::tar_props::{build, spec};

/// Reads through `blob` with reads of at most `chunk` bytes.
fn decode(blob: &[u8], chunk: usize) -> (Vec<u8>, Decoder<&[u8]>) {
    let mut d = Decoder::new(blob).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0; chunk];
    loop {
        let n = d.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    (out, d)
}

#[test]
fn gzip_layers_decode_to_their_tar_and_diff_id() {
    let tar = sample_tree().encode(plain()).unwrap();
    let (gz, digests) = sample_tree()
        .encode_with_digests(options(Compression::Gzip, 3))
        .unwrap();
    let (out, d) = decode(&gz, 64 * 1024);
    assert_eq!(d.compression(), Compression::Gzip);
    assert_eq!(out, tar);
    assert_eq!(d.diff_id(), Some(digests.diff_id));
    assert_eq!(d.tar_size(), u64::try_from(tar.len()).unwrap());
}

#[test]
fn plain_layers_pass_through() {
    let (tar, digests) = sample_tree().encode_with_digests(plain()).unwrap();
    let (out, d) = decode(&tar, 1000);
    assert_eq!(d.compression(), Compression::None);
    assert_eq!(out, tar);
    assert_eq!(d.diff_id(), Some(digests.diff_id));
}

#[test]
fn the_diff_id_does_not_depend_on_read_sizes() {
    let (gz, digests) = sample_tree()
        .encode_with_digests(options(Compression::Gzip, 1))
        .unwrap();
    for chunk in [1, 7, 512, 100_000] {
        assert_eq!(
            decode(&gz, chunk).1.diff_id(),
            Some(digests.diff_id),
            "{chunk}"
        );
    }
}

#[test]
fn a_tar_reader_can_consume_the_decoder() {
    let gz = sample_tree().encode(options(Compression::Gzip, 2)).unwrap();
    let tar = sample_tree().encode(plain()).unwrap();
    let mut archive = tar::Archive::new(Decoder::new(&gz[..]).unwrap());
    let names: Vec<Vec<u8>> = archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path_bytes().into_owned())
        .collect();
    let expected: Vec<Vec<u8>> = read_tar(&tar).into_iter().map(|e| e.name).collect();
    assert_eq!(names, expected);
}

#[test]
fn finish_reads_past_where_a_tar_reader_stops() {
    let (gz, digests) = sample_tree()
        .encode_with_digests(options(Compression::Gzip, 2))
        .unwrap();
    let mut d = Decoder::new(&gz[..]).unwrap();
    let mut first = [0u8; 512];
    d.read_exact(&mut first).unwrap();
    assert_eq!(d.diff_id(), None, "not at the end yet");
    assert_eq!(d.finish().unwrap(), digests.diff_id);
}

#[test]
fn several_gzip_members_are_one_stream() {
    let tar = sample_tree().encode(plain()).unwrap();
    let (a, b) = tar.split_at(tar.len() / 3);
    let mut blob = Vec::new();
    for part in [a, b] {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(part).unwrap();
        blob.extend_from_slice(&enc.finish().unwrap());
    }
    let (out, _) = decode(&blob, 4096);
    assert_eq!(out, tar);
}

#[test]
fn corrupt_gzip_is_an_error() {
    let gz = sample_tree().encode(options(Compression::Gzip, 1)).unwrap();
    let truncated = &gz[..gz.len() - 20];
    let mut out = Vec::new();
    let err = Decoder::new(truncated).unwrap().read_to_end(&mut out);
    assert!(err.is_err());

    let mut flipped = gz.clone();
    let last = flipped.len() - 6;
    flipped[last] ^= 0xff; // inside the CRC
    let err = Decoder::new(&flipped[..]).unwrap().read_to_end(&mut out);
    assert_eq!(err.unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn an_empty_blob_is_an_empty_plain_stream() {
    let (out, d) = decode(b"", 16);
    assert!(out.is_empty());
    assert_eq!(
        d.diff_id().unwrap().hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn decode_inverts_encode(
        specs in vec(spec(), 0..16),
        seed: u64,
        threads in 1usize..6,
        chunk in 1usize..70_000,
    ) {
        let mut t = build(&specs, seed);
        let tar = t.encode(plain()).unwrap();
        let (gz, digests) = t.encode_with_digests(options(Compression::Gzip, threads)).unwrap();
        let (out, d) = decode(&gz, chunk);
        prop_assert_eq!(out, tar);
        prop_assert_eq!(d.diff_id(), Some(digests.diff_id));
    }
}
