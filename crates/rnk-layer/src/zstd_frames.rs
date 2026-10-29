//! zstd as independent frames: the same bytes for any thread count.
//!
//! The input is cut into fixed 1 MiB blocks by byte offset, whatever the
//! sizes of the writes, and each block becomes one complete zstd frame
//! (content size in the frame header, no checksum), compressed on its own;
//! the frames are written in input order. RFC 8878 allows concatenated
//! frames, and the `zstd` CLI, libzstd's streaming decoder and Go's
//! `klauspost/compress` read them as one stream.
//!
//! libzstd's own multithreading is not used: its output changed with the
//! thread count and with how data was fed in (`exp-05` Table 9). Independent
//! frames are deterministic by construction, at the price of a slightly
//! worse ratio than one long frame. The worker threads and the hashing are
//! [`crate::parallel`]'s.

use std::io;

use crate::parallel::{BlockEncoder, Codec};

/// Uncompressed bytes per frame; part of every zstd layer digest.
pub(crate) const BLOCK_SIZE: usize = 1024 * 1024;

/// A zstd writer: one frame per block, compressed on worker threads.
pub(crate) type ZstdEncoder<W> = BlockEncoder<W, Zstd>;

/// The zstd codec at one compression level.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Zstd {
    pub(crate) level: i32,
}

impl Codec for Zstd {
    type Block = Vec<u8>;
    type State = ();
    // One context per thread, reset by libzstd for every frame: a new
    // context per block would allocate its tables each time, with the C
    // allocator (musl's on the static build). Same frames either way.
    type Scratch = zstd::bulk::Compressor<'static>;
    const BLOCK_SIZE: usize = BLOCK_SIZE;
    const NAME: &'static str = "rnk-zstd";
    // An empty input still gets one (empty) frame, so the blob is a valid
    // zstd stream rather than zero bytes.
    const BLOCK_FOR_EMPTY_INPUT: bool = true;

    fn header(&self) -> Vec<u8> {
        Vec::new()
    }

    fn carry(&self, _block: &[u8]) -> Vec<u8> {
        Vec::new()
    }

    fn scratch(&self) -> io::Result<Self::Scratch> {
        zstd::bulk::Compressor::new(self.level)
    }

    fn compress(
        &self,
        context: &mut Self::Scratch,
        _carried: &[u8],
        data: &[u8],
    ) -> io::Result<Vec<u8>> {
        context.compress(data)
    }

    fn absorb(_state: &mut (), frame: Vec<u8>) -> Vec<u8> {
        frame
    }

    fn trailer(&self, _state: ()) -> Vec<u8> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write as _};
    use std::num::NonZeroUsize;
    use std::process::{Command, Stdio};

    use proptest::prelude::*;

    use super::*;
    use crate::gzip::tests::sample;

    fn zstd(data: &[u8], level: i32, threads: usize, chunk: usize) -> Vec<u8> {
        let threads = NonZeroUsize::new(threads).unwrap();
        let mut enc = ZstdEncoder::new(Vec::new(), Zstd { level }, threads).unwrap();
        for piece in data.chunks(chunk.max(1)) {
            enc.write_all(piece).unwrap();
        }
        let (blob, (_, size), (digest, blob_size)) = enc.finish().unwrap();
        assert_eq!(size, u64::try_from(data.len()).unwrap());
        assert_eq!(blob_size, u64::try_from(blob.len()).unwrap());
        let mut check = crate::digest::Hasher::default();
        check.update(&blob);
        assert_eq!(check.finish().0, digest);
        blob
    }

    /// Decodes every frame with libzstd's streaming decoder.
    fn unzstd(blob: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        zstd::stream::read::Decoder::new(blob)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn empty_input_is_one_empty_frame() {
        let blob = zstd(b"", 3, 1, 1);
        assert!(blob.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]));
        assert!(unzstd(&blob).is_empty());
        assert_eq!(zstd(b"", 3, 4, 1), blob);
    }

    #[test]
    fn one_frame_per_block_each_decodable_alone() {
        let data = sample(2 * BLOCK_SIZE + 17, 5);
        for threads in [1, 3] {
            let blob = zstd(&data, 3, threads, 70_000);
            assert_eq!(unzstd(&blob), data);
            let mut rest = &blob[..];
            for (i, block) in data.chunks(BLOCK_SIZE).enumerate() {
                let len = zstd::zstd_safe::find_frame_compressed_size(rest).unwrap();
                let frame = &rest[..len];
                // A reused context gives what a fresh one gives.
                assert_eq!(frame, zstd::bulk::compress(block, 3).unwrap(), "frame {i}");
                assert_eq!(
                    zstd::zstd_safe::get_frame_content_size(frame).ok(),
                    Some(Some(u64::try_from(block.len()).unwrap())),
                    "frame {i} announces its size"
                );
                assert_eq!(zstd::bulk::decompress(frame, BLOCK_SIZE).unwrap(), block);
                rest = &rest[len..];
            }
            assert!(rest.is_empty(), "{threads} threads: three frames only");
        }
    }

    #[test]
    fn block_boundaries_and_threads_give_one_output() {
        for len in [1, BLOCK_SIZE - 1, BLOCK_SIZE, BLOCK_SIZE + 1] {
            let data = sample(len, 9);
            let one = zstd(&data, 3, 1, len.max(1));
            assert_eq!(unzstd(&one), data, "len {len}");
            for threads in [2, 5] {
                assert_eq!(
                    zstd(&data, 3, threads, 4099),
                    one,
                    "len {len}, {threads} threads"
                );
            }
        }
    }

    #[test]
    fn the_zstd_cli_reads_the_frames() {
        // Interoperability with the reference implementation, when installed.
        let Ok(mut child) = Command::new("zstd")
            .args(["-d", "-c", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        else {
            eprintln!("skipped: no zstd command");
            return;
        };
        let data = sample(BLOCK_SIZE + BLOCK_SIZE / 2, 13);
        let blob = zstd(&data, 3, 2, 50_000);
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(&blob).unwrap());
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, data);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(12))]

        #[test]
        fn threads_and_chunking_never_change_the_bytes(
            len in 0usize..(2 * BLOCK_SIZE + 3),
            seed: u64,
            threads in 2usize..7,
            chunk in 1usize..(BLOCK_SIZE + 9),
        ) {
            let data = sample(len, seed);
            let reference = zstd(&data, 3, 1, data.len());
            prop_assert_eq!(zstd(&data, 3, threads, chunk), reference.clone());
            prop_assert_eq!(unzstd(&reference), data);
        }

        #[test]
        fn arbitrary_bytes_round_trip(data in proptest::collection::vec(any::<u8>(), 0..20_000), threads in 1usize..4) {
            let blob = zstd(&data, 3, threads, 333);
            prop_assert_eq!(unzstd(&blob), data);
        }
    }
}
