//! Block-parallel compression: the same bytes for any thread count.
//!
//! The input is cut into fixed blocks by byte offset, whatever the sizes of
//! the writes. Each block is compressed on its own by a [`Codec`], at most
//! with a little context carried over from the block before it (gzip's
//! preset dictionary), and the compressed blocks are written in input order.
//! A block's bytes depend only on the block, what the previous block
//! carried and the codec's settings, so threads change the speed and nothing
//! else (`exp-05` Q3).
//!
//! The encoder also hashes its input (the diff ID) and its output (the blob
//! digest): in the caller's thread when it compresses there, otherwise in one
//! thread per stream fed in order, so that the caller only copies and
//! writes.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::mem;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use crate::digest::{Hasher, Sha256Digest};

/// Blocks queued or being compressed per worker thread.
const QUEUE_PER_THREAD: usize = 2;

/// A compression format whose stream is a header, blocks compressed one by
/// one, and a trailer.
///
/// The codec value is cloned into every worker thread, each with its own
/// [`Codec::Scratch`]; the running state of one stream (gzip's CRC) lives in
/// [`Codec::State`], owned by the encoder.
pub(crate) trait Codec: Clone + Send + 'static {
    /// One compressed block, with whatever the stream state needs from it.
    type Block: Send + 'static;
    /// What the encoder folds every block into, in order.
    type State: Default;
    /// Working memory one thread reuses from block to block (zstd's
    /// context); it never changes the output.
    type Scratch: Send + 'static;
    /// Uncompressed bytes per block: part of every layer digest.
    const BLOCK_SIZE: usize;
    /// Prefix of the worker threads' names.
    const NAME: &'static str;
    /// Whether an empty input still compresses one (empty) block.
    const BLOCK_FOR_EMPTY_INPUT: bool;

    /// Bytes written before the first block.
    fn header(&self) -> Vec<u8>;
    /// What the next block needs from `block` (gzip: its tail as a preset
    /// dictionary); empty when blocks are independent.
    fn carry(&self, block: &[u8]) -> Vec<u8>;
    /// Fresh working memory for one thread.
    fn scratch(&self) -> io::Result<Self::Scratch>;
    /// Compresses one block, given what the previous block carried.
    fn compress(
        &self,
        scratch: &mut Self::Scratch,
        carried: &[u8],
        data: &[u8],
    ) -> io::Result<Self::Block>;
    /// Folds a block into the stream state and returns its bytes.
    fn absorb(state: &mut Self::State, block: Self::Block) -> Vec<u8>;
    /// Bytes written after the last block.
    fn trailer(&self, state: Self::State) -> Vec<u8>;
}

/// A sha256 and the number of bytes it covers.
pub(crate) type Digested = (Sha256Digest, u64);

/// A writer compressing fixed blocks with `C` on `threads` threads (in the
/// caller's thread when `threads` is 1), hashing its input and output.
///
/// [`flush`](Write::flush) flushes the inner writer only: cutting a block
/// early would change the output. Call [`finish`](Self::finish) to end the
/// stream; dropping the encoder discards the unfinished stream.
pub(crate) struct BlockEncoder<W: Write, C: Codec> {
    out: W,
    codec: C,
    /// The block being filled.
    block: Vec<u8>,
    /// What the previous block carried over.
    carried: Vec<u8>,
    /// Index of the next block handed to a compressor.
    next_submit: u64,
    /// Index of the next block written to `out`.
    next_write: u64,
    /// Compressed blocks that came back ahead of `next_write`.
    ready: BTreeMap<u64, C::Block>,
    state: C::State,
    /// The hash of the input.
    input_hash: StreamHash,
    /// The hash of everything written to `out`.
    output_hash: StreamHash,
    /// The caller thread's scratch, when it compresses (no pool).
    inline: Option<C::Scratch>,
    pool: Option<Pool<C>>,
}

impl<W: Write, C: Codec> BlockEncoder<W, C> {
    /// Writes the codec's header and starts the worker threads.
    pub(crate) fn new(mut out: W, codec: C, threads: NonZeroUsize) -> io::Result<Self> {
        let header = codec.header();
        out.write_all(&header)?;
        let (inline, pool, input_hash, mut output_hash) = match threads.get() {
            1 => (
                Some(codec.scratch()?),
                None,
                StreamHash::inline(),
                StreamHash::inline(),
            ),
            n => (
                None,
                Some(Pool::start(&codec, n)?),
                StreamHash::start(&format!("{}-hash-in", C::NAME), n)?,
                StreamHash::start(&format!("{}-hash-out", C::NAME), n)?,
            ),
        };
        if !header.is_empty() {
            output_hash.update(Arc::new(header))?;
        }
        Ok(Self {
            out,
            codec,
            block: Vec::with_capacity(C::BLOCK_SIZE),
            carried: Vec::new(),
            next_submit: 0,
            next_write: 0,
            ready: BTreeMap::new(),
            state: C::State::default(),
            input_hash,
            output_hash,
            inline,
            pool,
        })
    }

    /// Compresses what is left, writes the trailer, and returns the inner
    /// writer with the sha256 and size of the input, then of the output.
    pub(crate) fn finish(mut self) -> io::Result<(W, Digested, Digested)> {
        if !self.block.is_empty() || (C::BLOCK_FOR_EMPTY_INPUT && self.next_submit == 0) {
            self.submit()?;
        }
        while self.next_write < self.next_submit {
            self.wait_one()?;
        }
        if let Some(pool) = self.pool.take() {
            pool.stop()?;
        }
        let end = self.codec.trailer(mem::take(&mut self.state));
        if !end.is_empty() {
            self.out.write_all(&end)?;
            self.output_hash.update(Arc::new(end))?;
        }
        let input = self.input_hash.finish()?;
        let output = self.output_hash.finish()?;
        Ok((self.out, input, output))
    }

    /// Hands the full (or last) block to a compressor.
    fn submit(&mut self) -> io::Result<()> {
        let data = Arc::new(mem::replace(
            &mut self.block,
            Vec::with_capacity(C::BLOCK_SIZE),
        ));
        let carried = mem::replace(&mut self.carried, self.codec.carry(&data));
        self.input_hash.update(Arc::clone(&data))?;
        let index = self.next_submit;
        self.next_submit += 1;
        let Some(pool) = &mut self.pool else {
            let scratch = self
                .inline
                .as_mut()
                .ok_or_else(|| io::Error::other(format!("{}: no compressor", C::NAME)))?;
            let block = self.codec.compress(scratch, &carried, &data)?;
            return self.write_block(block);
        };
        pool.send(Job {
            index,
            data,
            carried,
        })?;
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
            return Err(io::Error::other(format!("{}: no block in flight", C::NAME)));
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

    fn write_block(&mut self, block: C::Block) -> io::Result<()> {
        let bytes = C::absorb(&mut self.state, block);
        self.out.write_all(&bytes)?;
        self.output_hash.update(Arc::new(bytes))?;
        self.next_write += 1;
        Ok(())
    }
}

/// Blocks allowed between submission and writing, written ones excluded.
fn pool_limit<C: Codec>(pool: Option<&Pool<C>>) -> u64 {
    let workers = pool.map_or(1, |p| p.workers.len());
    u64::try_from(workers * QUEUE_PER_THREAD).unwrap_or(u64::MAX)
}

impl<W: Write, C: Codec> Write for BlockEncoder<W, C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        while !rest.is_empty() {
            let room = C::BLOCK_SIZE - self.block.len();
            let (now, later) = rest.split_at(room.min(rest.len()));
            self.block.extend_from_slice(now);
            rest = later;
            if self.block.len() == C::BLOCK_SIZE {
                self.submit()?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

struct Job {
    index: u64,
    data: Arc<Vec<u8>>,
    carried: Vec<u8>,
}

struct Done<B> {
    index: u64,
    result: io::Result<B>,
}

/// Worker threads, each with its own queue; block `i` goes to worker
/// `i % n`.
struct Pool<C: Codec> {
    workers: Vec<SyncSender<Job>>,
    results: Receiver<Done<C::Block>>,
    handles: Vec<JoinHandle<()>>,
}

impl<C: Codec> Pool<C> {
    fn start(codec: &C, threads: usize) -> io::Result<Self> {
        let (done_tx, results) = mpsc::channel::<Done<C::Block>>();
        let mut workers = Vec::with_capacity(threads);
        let mut handles = Vec::with_capacity(threads);
        for i in 0..threads {
            let (job_tx, jobs) = mpsc::sync_channel::<Job>(QUEUE_PER_THREAD);
            let done = done_tx.clone();
            let codec = codec.clone();
            let mut scratch = codec.scratch()?;
            let handle = thread::Builder::new()
                .name(format!("{}-{i}", C::NAME))
                .spawn(move || {
                    for job in jobs {
                        let result = codec.compress(&mut scratch, &job.carried, &job.data);
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

    fn recv(&self) -> io::Result<Done<C::Block>> {
        self.results.recv().map_err(|_| stopped())
    }

    fn try_recv(&self) -> io::Result<Option<Done<C::Block>>> {
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

/// The running hash of one stream.
enum StreamHash {
    Inline(Hasher),
    /// Pieces go, in order, to a thread that returns the hash when the
    /// queue closes.
    Thread {
        blocks: SyncSender<Arc<Vec<u8>>>,
        handle: JoinHandle<Hasher>,
    },
}

impl StreamHash {
    fn inline() -> Self {
        Self::Inline(Hasher::default())
    }

    fn start(name: &str, threads: usize) -> io::Result<Self> {
        let (blocks, queue) = mpsc::sync_channel::<Arc<Vec<u8>>>(threads * QUEUE_PER_THREAD);
        let handle = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let mut hasher = Hasher::default();
                for block in queue {
                    hasher.update(&block);
                }
                hasher
            })?;
        Ok(Self::Thread { blocks, handle })
    }

    fn update(&mut self, block: Arc<Vec<u8>>) -> io::Result<()> {
        match self {
            Self::Inline(hasher) => {
                hasher.update(&block);
                Ok(())
            }
            Self::Thread { blocks, .. } => blocks.send(block).map_err(|_| stopped()),
        }
    }

    fn finish(self) -> io::Result<Digested> {
        match self {
            Self::Inline(hasher) => Ok(hasher.finish()),
            Self::Thread { blocks, handle } => {
                drop(blocks);
                handle.join().map(Hasher::finish).map_err(|_| stopped())
            }
        }
    }
}

fn stopped() -> io::Error {
    io::Error::other("a compression worker thread stopped")
}
