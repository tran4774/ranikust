//! pigz-style parallel gzip: one member, the same bytes for any thread count.
//!
//! The input is cut into fixed 128 KiB blocks by byte offset, whatever the
//! sizes of the writes. Each block is raw-deflated on its own by a fresh
//! compressor, primed with the last 32 KiB of the previous block as a preset
//! dictionary, and ended with a sync flush, so the blocks concatenate into
//! one deflate stream; an empty final block closes it. The trailer's CRC-32
//! is combined from the blocks' CRCs. A block's bytes depend only on the
//! block, the previous block's tail and the level, so threads change the
//! speed and nothing else (`exp-05` Q3; pigz does the same). The worker
//! threads and the hashing are [`crate::parallel`]'s.

use std::io;

use flate2::{Compress, Crc, FlushCompress};

use crate::parallel::{BlockEncoder, Codec};

/// Uncompressed bytes per block; part of every gzip layer digest.
pub(crate) const BLOCK_SIZE: usize = 128 * 1024;
/// Bytes of the previous block used as the preset dictionary.
pub(crate) const DICT_SIZE: usize = 32 * 1024;
/// A final, empty, fixed-Huffman deflate block: `BFINAL=1`, `BTYPE=01`, EOB.
const FINAL_EMPTY_BLOCK: [u8; 2] = [0x03, 0x00];

/// A gzip writer: one member, compressed block by block on worker threads.
pub(crate) type GzipEncoder<W> = BlockEncoder<W, Gzip>;

/// The gzip codec at one compression level (0 to 9).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Gzip {
    pub(crate) level: u32,
}

/// One compressed block and the CRC of its input.
pub(crate) struct Compressed {
    deflate: Vec<u8>,
    crc: Crc,
}

impl Codec for Gzip {
    type Block = Compressed;
    type State = Crc;
    // Each block starts a fresh deflate stream.
    type Scratch = ();
    const BLOCK_SIZE: usize = BLOCK_SIZE;
    const NAME: &'static str = "rnk-gzip";
    const BLOCK_FOR_EMPTY_INPUT: bool = false;

    fn header(&self) -> Vec<u8> {
        // RFC 1952: no name, no mtime, XFL from the level as Go's
        // compress/gzip sets it, OS unknown.
        let xfl = match self.level {
            9 => 2,
            1 => 4,
            _ => 0,
        };
        vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 0xff]
    }

    fn carry(&self, block: &[u8]) -> Vec<u8> {
        block[block.len().saturating_sub(DICT_SIZE)..].to_vec()
    }

    fn scratch(&self) -> io::Result<()> {
        Ok(())
    }

    fn compress(&self, (): &mut (), carried: &[u8], data: &[u8]) -> io::Result<Compressed> {
        compress_block(self.level, carried, data)
    }

    fn absorb(crc: &mut Crc, block: Compressed) -> Vec<u8> {
        crc.combine(&block.crc);
        block.deflate
    }

    fn trailer(&self, crc: Crc) -> Vec<u8> {
        let mut end = FINAL_EMPTY_BLOCK.to_vec();
        end.extend_from_slice(&crc.sum().to_le_bytes());
        end.extend_from_slice(&crc.amount().to_le_bytes());
        end
    }
}

/// Raw deflate of one block, primed with `dict`, ended by a sync flush.
fn compress_block(level: u32, dict: &[u8], data: &[u8]) -> io::Result<Compressed> {
    let mut deflate = Compress::new(flate2::Compression::new(level), false);
    if !dict.is_empty() {
        deflate.set_dictionary(dict).map_err(io::Error::other)?;
    }
    let mut out = Vec::with_capacity(data.len() + data.len() / 8 + 64);
    let mut consumed = 0;
    loop {
        // zlib: keep more than six bytes free for a sync flush, or the flush
        // marker repeats.
        if out.capacity() - out.len() < 64 {
            out.reserve(BLOCK_SIZE / 4);
        }
        let before = (deflate.total_in(), deflate.total_out());
        let input = data
            .get(consumed..)
            .ok_or_else(|| io::Error::other("deflate overran its input"))?;
        deflate
            .compress_vec(input, &mut out, FlushCompress::Sync)
            .map_err(io::Error::other)?;
        consumed = usize::try_from(deflate.total_in()).map_err(io::Error::other)?;
        if consumed == data.len() && out.len() < out.capacity() {
            break;
        }
        if (deflate.total_in(), deflate.total_out()) == before {
            return Err(io::Error::other("deflate made no progress"));
        }
    }
    let mut crc = Crc::new();
    crc.update(data);
    Ok(Compressed { deflate: out, crc })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Read, Write as _};
    use std::num::NonZeroUsize;

    use proptest::prelude::*;

    use super::*;

    fn gzip(data: &[u8], level: u32, threads: usize, chunk: usize) -> Vec<u8> {
        let threads = NonZeroUsize::new(threads).unwrap();
        let mut enc = GzipEncoder::new(Vec::new(), Gzip { level }, threads).unwrap();
        for piece in data.chunks(chunk.max(1)) {
            enc.write_all(piece).unwrap();
        }
        let (gz, (_, size), (digest, gz_size)) = enc.finish().unwrap();
        assert_eq!(size, u64::try_from(data.len()).unwrap());
        assert_eq!(gz_size, u64::try_from(gz.len()).unwrap());
        let mut check = crate::digest::Hasher::default();
        check.update(&gz);
        assert_eq!(check.finish().0, digest);
        gz
    }

    /// Decodes exactly one gzip member and checks nothing follows it.
    fn gunzip_one_member(gz: &[u8]) -> Vec<u8> {
        let mut decoder = flate2::bufread::GzDecoder::new(gz);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).unwrap();
        assert!(
            decoder.into_inner().is_empty(),
            "bytes after the first member"
        );
        out
    }

    /// Compressible but not trivial: text-like runs mixed with noise.
    pub(crate) fn sample(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let noise = x.to_le_bytes()[0];
                if i % 97 < 60 {
                    b"layer tar bytes "[i % 16]
                } else {
                    noise
                }
            })
            .collect()
    }

    #[test]
    fn empty_input_is_a_valid_member() {
        let gz = gzip(b"", 6, 1, 1);
        assert_eq!(gz.len(), 10 + 2 + 8);
        assert!(gunzip_one_member(&gz).is_empty());
        assert_eq!(gzip(b"", 6, 4, 1), gz);
    }

    #[test]
    fn block_boundaries_round_trip() {
        for len in [
            1,
            BLOCK_SIZE - 1,
            BLOCK_SIZE,
            BLOCK_SIZE + 1,
            3 * BLOCK_SIZE,
        ] {
            let data = sample(len, 7);
            let one = gzip(&data, 6, 1, 4096);
            assert_eq!(gunzip_one_member(&one), data, "len {len}");
            for threads in [2, 3, 8] {
                assert_eq!(
                    gzip(&data, 6, threads, 999),
                    one,
                    "len {len}, {threads} threads"
                );
            }
        }
    }

    #[test]
    fn every_level_round_trips() {
        let data = sample(2 * BLOCK_SIZE + 5, 3);
        for level in 0..=9 {
            let gz = gzip(&data, level, 3, 50_000);
            assert_eq!(gunzip_one_member(&gz), data, "level {level}");
            assert_eq!(gz, gzip(&data, level, 1, data.len()), "level {level}");
        }
    }

    #[test]
    fn the_dictionary_helps() {
        // A 16 KiB pattern repeated across a block boundary: with the previous
        // block's tail as dictionary, the second block is nearly free.
        let unit = sample(DICT_SIZE / 2, 11);
        let data: Vec<u8> = unit
            .iter()
            .copied()
            .cycle()
            .take(BLOCK_SIZE + DICT_SIZE / 2)
            .collect();
        let gz = gzip(&data, 6, 2, 8192);
        assert_eq!(gunzip_one_member(&gz), data);
        let alone = gzip(&data[BLOCK_SIZE..], 6, 1, 8192);
        let first = gzip(&data[..BLOCK_SIZE], 6, 1, 8192);
        assert!(gz.len() < first.len() + alone.len() - 1000);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn threads_and_chunking_never_change_the_bytes(
            len in 0usize..(4 * BLOCK_SIZE + 3),
            seed: u64,
            threads in 2usize..9,
            chunk in 1usize..(BLOCK_SIZE + 9),
        ) {
            let data = sample(len, seed);
            let reference = gzip(&data, 6, 1, data.len());
            prop_assert_eq!(gzip(&data, 6, threads, chunk), reference.clone());
            prop_assert_eq!(gunzip_one_member(&reference), data);
        }

        #[test]
        fn arbitrary_bytes_round_trip(data in proptest::collection::vec(any::<u8>(), 0..20_000), threads in 1usize..5) {
            let gz = gzip(&data, 6, threads, 333);
            prop_assert_eq!(gunzip_one_member(&gz), data);
        }
    }
}
