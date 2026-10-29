//! pigz-style parallel gzip: one member, the same bytes for any thread count.
//!
//! The input is cut into fixed 128 KiB blocks by byte offset, whatever the
//! sizes of the writes. Each block is raw-deflated on its own by a fresh
//! compressor, primed with the last 32 KiB of the previous block as a preset
//! dictionary, and ended with a sync flush, so the blocks concatenate into
//! one deflate stream; an empty final block closes it. The trailer's CRC-32
//! is combined from the blocks' CRCs. A block's bytes depend only on the
//! block, the previous block's tail and the level, so threads change the
//! speed and nothing else (`exp-05` Q3; pigz does the same).

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::mem;
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use flate2::{Compress, Crc, FlushCompress};

/// Uncompressed bytes per block; part of every gzip layer digest.
pub(crate) const BLOCK_SIZE: usize = 128 * 1024;
/// Bytes of the previous block used as the preset dictionary.
pub(crate) const DICT_SIZE: usize = 32 * 1024;
/// A final, empty, fixed-Huffman deflate block: `BFINAL=1`, `BTYPE=01`, EOB.
const FINAL_EMPTY_BLOCK: [u8; 2] = [0x03, 0x00];
/// Blocks queued or being compressed per worker thread.
const QUEUE_PER_THREAD: usize = 2;

/// A gzip writer compressing 128 KiB blocks on `threads` threads (in the
/// caller's thread when `threads` is 1).
///
/// [`flush`](Write::flush) flushes the inner writer only: cutting a block
/// early would change the output. Call [`finish`](Self::finish) to end the
/// stream; dropping the encoder discards the unfinished stream.
pub(crate) struct GzipEncoder<W: Write> {
    out: W,
    level: u32,
    /// The block being filled.
    block: Vec<u8>,
    /// The tail of the previous block.
    dict: Vec<u8>,
    /// Index of the next block handed to a compressor.
    next_submit: u64,
    /// Index of the next block written to `out`.
    next_write: u64,
    /// Compressed blocks that came back ahead of `next_write`.
    ready: BTreeMap<u64, Compressed>,
    /// CRC of everything written so far.
    crc: Crc,
    pool: Option<Pool>,
}

/// One compressed block and the CRC of its input.
struct Compressed {
    deflate: Vec<u8>,
    crc: Crc,
}

impl<W: Write> GzipEncoder<W> {
    /// Writes the gzip header and starts the worker threads.
    pub(crate) fn new(mut out: W, level: u32, threads: NonZeroUsize) -> io::Result<Self> {
        // RFC 1952: no name, no mtime, XFL from the level as Go's
        // compress/gzip sets it, OS unknown.
        let xfl = match level {
            9 => 2,
            1 => 4,
            _ => 0,
        };
        out.write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 0xff])?;
        let pool = match threads.get() {
            1 => None,
            n => Some(Pool::start(level, n)?),
        };
        Ok(Self {
            out,
            level,
            block: Vec::with_capacity(BLOCK_SIZE),
            dict: Vec::new(),
            next_submit: 0,
            next_write: 0,
            ready: BTreeMap::new(),
            crc: Crc::new(),
            pool,
        })
    }

    /// Compresses what is left, writes the final block and the trailer, and
    /// returns the inner writer.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        if !self.block.is_empty() {
            self.submit()?;
        }
        while self.next_write < self.next_submit {
            self.wait_one()?;
        }
        if let Some(pool) = self.pool.take() {
            pool.stop()?;
        }
        self.out.write_all(&FINAL_EMPTY_BLOCK)?;
        self.out.write_all(&self.crc.sum().to_le_bytes())?;
        self.out.write_all(&self.crc.amount().to_le_bytes())?;
        Ok(self.out)
    }

    /// Hands the full (or last) block to a compressor.
    fn submit(&mut self) -> io::Result<()> {
        let data = mem::replace(&mut self.block, Vec::with_capacity(BLOCK_SIZE));
        let tail = &data[data.len().saturating_sub(DICT_SIZE)..];
        let dict = mem::replace(&mut self.dict, tail.to_vec());
        let index = self.next_submit;
        self.next_submit += 1;
        let Some(pool) = &mut self.pool else {
            let block = compress_block(self.level, &dict, &data)?;
            return self.write_block(block);
        };
        pool.send(Job { index, data, dict })?;
        while let Some(done) = pool.try_recv()? {
            self.ready.insert(done.index, done.result?);
        }
        self.write_ready()?;
        let limit = pool_limit(self.pool.as_ref());
        while self.next_submit - self.next_write >= limit {
            self.wait_one()?;
        }
        Ok(())
    }

    /// Waits for one block from the workers, then writes what is ready.
    fn wait_one(&mut self) -> io::Result<()> {
        let Some(pool) = &self.pool else {
            return Err(io::Error::other("gzip: no block in flight"));
        };
        let done = pool.recv()?;
        self.ready.insert(done.index, done.result?);
        self.write_ready()
    }

    fn write_ready(&mut self) -> io::Result<()> {
        while let Some(block) = self.ready.remove(&self.next_write) {
            self.write_block(block)?;
        }
        Ok(())
    }

    fn write_block(&mut self, block: Compressed) -> io::Result<()> {
        self.out.write_all(&block.deflate)?;
        self.crc.combine(&block.crc);
        self.next_write += 1;
        Ok(())
    }
}

/// Blocks allowed between submission and writing, written ones excluded.
fn pool_limit(pool: Option<&Pool>) -> u64 {
    let workers = pool.map_or(1, |p| p.workers.len());
    u64::try_from(workers * QUEUE_PER_THREAD).unwrap_or(u64::MAX)
}

impl<W: Write> Write for GzipEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        while !rest.is_empty() {
            let room = BLOCK_SIZE - self.block.len();
            let (now, later) = rest.split_at(room.min(rest.len()));
            self.block.extend_from_slice(now);
            rest = later;
            if self.block.len() == BLOCK_SIZE {
                self.submit()?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
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

struct Job {
    index: u64,
    data: Vec<u8>,
    dict: Vec<u8>,
}

struct Done {
    index: u64,
    result: io::Result<Compressed>,
}

/// Worker threads, each with its own queue; block `i` goes to worker
/// `i % n`.
struct Pool {
    workers: Vec<SyncSender<Job>>,
    results: Receiver<Done>,
    handles: Vec<JoinHandle<()>>,
}

impl Pool {
    fn start(level: u32, threads: usize) -> io::Result<Self> {
        let (done_tx, results) = mpsc::channel::<Done>();
        let mut workers = Vec::with_capacity(threads);
        let mut handles = Vec::with_capacity(threads);
        for i in 0..threads {
            let (job_tx, jobs) = mpsc::sync_channel::<Job>(QUEUE_PER_THREAD);
            let done = done_tx.clone();
            let handle = thread::Builder::new()
                .name(format!("rnk-gzip-{i}"))
                .spawn(move || {
                    for job in jobs {
                        let result = compress_block(level, &job.dict, &job.data);
                        if done
                            .send(Done {
                                index: job.index,
                                result,
                            })
                            .is_err()
                        {
                            // The encoder is gone; nothing waits for blocks.
                            break;
                        }
                    }
                })?;
            workers.push(job_tx);
            handles.push(handle);
        }
        Ok(Self {
            workers,
            results,
            handles,
        })
    }

    fn send(&self, job: Job) -> io::Result<()> {
        let worker = usize::try_from(
            job.index % u64::try_from(self.workers.len()).map_err(io::Error::other)?,
        )
        .map_err(io::Error::other)?;
        self.workers[worker].send(job).map_err(|_| stopped())
    }

    fn recv(&self) -> io::Result<Done> {
        self.results.recv().map_err(|_| stopped())
    }

    fn try_recv(&self) -> io::Result<Option<Done>> {
        match self.results.try_recv() {
            Ok(done) => Ok(Some(done)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(stopped()),
        }
    }

    /// Closes the queues and joins the workers.
    fn stop(self) -> io::Result<()> {
        drop(self.workers);
        for handle in self.handles {
            handle.join().map_err(|_| stopped())?;
        }
        Ok(())
    }
}

fn stopped() -> io::Error {
    io::Error::other("a gzip worker thread stopped")
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use proptest::prelude::*;

    use super::*;

    fn gzip(data: &[u8], level: u32, threads: usize, chunk: usize) -> Vec<u8> {
        let threads = NonZeroUsize::new(threads).unwrap();
        let mut enc = GzipEncoder::new(Vec::new(), level, threads).unwrap();
        for piece in data.chunks(chunk.max(1)) {
            enc.write_all(piece).unwrap();
        }
        enc.finish().unwrap()
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
    fn sample(len: usize, seed: u64) -> Vec<u8> {
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
