//! Blob sources and sinks: where a pull writes and what a push reads.
//!
//! Both must allow starting over, because a retried transfer begins at the
//! first byte: a sink is truncated, a source reopened.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::stream::{self, BoxStream, StreamExt as _};
use oci_client::errors::OciDistributionError;
use tokio::io::{AsyncSeekExt as _, AsyncWrite, AsyncWriteExt as _};

/// Where a pulled blob is written: typically an `rnk-store` temporary file.
///
/// Bytes arrive before the digest is checked; on any error the caller
/// discards what was written.
pub trait BlobSink: AsyncWrite + Unpin + Send {
    /// Discards everything written so far, before a retry.
    fn reset(&mut self) -> impl Future<Output = io::Result<()>> + Send;

    /// Whether [`BlobSink::reset`] can still succeed: a transfer into a
    /// sink that cannot restart is not retried, and its first error stands.
    fn can_restart(&self) -> bool {
        true
    }
}

impl BlobSink for tokio::fs::File {
    async fn reset(&mut self) -> io::Result<()> {
        self.flush().await?;
        self.set_len(0).await?;
        self.seek(io::SeekFrom::Start(0)).await?;
        Ok(())
    }
}

/// The writing end of a bounded in-memory pipe, read as the blob arrives
/// (ADR-0055). What was already read cannot be taken back: a transfer is
/// retried only while no byte has gone into the pipe (a refused or broken
/// request), and `reset` fails after that.
#[derive(Debug)]
struct PipeSink {
    pipe: tokio::io::DuplexStream,
    written: u64,
}

impl AsyncWrite for PipeSink {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.pipe).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = polled {
            self.written += n as u64;
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_shutdown(cx)
    }
}

impl BlobSink for PipeSink {
    fn can_restart(&self) -> bool {
        self.written == 0
    }

    async fn reset(&mut self) -> io::Result<()> {
        if self.can_restart() {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a streamed blob cannot be restarted once bytes were read",
        ))
    }
}

impl BlobSink for Vec<u8> {
    async fn reset(&mut self) -> io::Result<()> {
        self.clear();
        Ok(())
    }
}

/// The content of a blob to push, readable once per attempt.
#[derive(Clone, Debug)]
pub enum BlobBody {
    /// Content in memory: a config, a small layer.
    Bytes(Bytes),
    /// A file reopened through a handle for each attempt, never by a path:
    /// an anonymous file whose descriptor number could name another file
    /// once it is released (ADR-0055).
    Opened(Arc<dyn BlobFile>),
    /// A file, streamed: a layer in the store.
    File(PathBuf),
    /// The same blob in another repository, possibly on another registry,
    /// pulled while it is pushed through a pipe of [`PIPE_BYTES`]: a base
    /// layer that was never kept (ADR-0055).
    Remote(Box<RemoteBlob>),
}

/// A file that [`BlobBody::Opened`] reopens: a new descriptor read from the
/// start, or an error once the file is gone.
pub trait BlobFile: std::fmt::Debug + Send + Sync {
    /// A new descriptor of the file, at its start.
    ///
    /// # Errors
    ///
    /// When the file is gone or cannot be reopened.
    fn open(&self) -> io::Result<std::fs::File>;
}

/// A blob pushed from another repository: [`BlobBody::Remote`].
#[derive(Clone, Debug)]
pub struct RemoteBlob {
    /// The client that pulls it.
    pub client: Arc<crate::Client>,
    /// Where it is pulled from.
    pub from: rnk_oci::Reference,
    /// The blob, verified while it streams.
    pub blob: rnk_oci::Descriptor,
}

/// Bytes held in the pipe of a streamed blob: what one pulled blob may keep
/// in memory at a time.
pub const PIPE_BYTES: usize = 256 * 1024;

/// Bytes read per chunk from a file body.
const FILE_CHUNK: usize = 256 * 1024;

/// A push body as `oci-client` takes it.
pub(crate) type PushStream = BoxStream<'static, Result<Bytes, OciDistributionError>>;

impl BlobBody {
    /// Opens the body, counting the bytes it yields into `sent`.
    ///
    /// Returns the stream and the body's length.
    pub(crate) async fn open(&self, sent: Arc<AtomicU64>) -> io::Result<(PushStream, u64)> {
        let count = move |chunk: &Bytes| {
            sent.fetch_add(
                u64::try_from(chunk.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        };
        match self {
            Self::Bytes(bytes) => {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let bytes = bytes.clone();
                let body = stream::once(async move { Ok(bytes) })
                    .inspect(move |r: &Result<Bytes, OciDistributionError>| {
                        if let Ok(chunk) = r {
                            count(chunk);
                        }
                    })
                    .boxed();
                Ok((body, len))
            }
            Self::File(_) | Self::Opened(_) => {
                let file = if let Self::File(path) = self {
                    tokio::fs::File::open(path).await?
                } else if let Self::Opened(handle) = self {
                    tokio::fs::File::from_std(handle.open()?)
                } else {
                    return Err(io::Error::other("not a file body"));
                };
                let len = file.metadata().await?.len();
                let body = tokio_util::io::ReaderStream::with_capacity(file, FILE_CHUNK)
                    .map(move |r| {
                        let chunk = r.map_err(OciDistributionError::IoError)?;
                        count(&chunk);
                        Ok(chunk)
                    })
                    .boxed();
                Ok((body, len))
            }
            Self::Remote(remote) => {
                let RemoteBlob { client, from, blob } = &**remote;
                let handle = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
                let (reader, finished, pull) = pull_into_pipe(&handle, client, from, blob)?;
                // A failed pull (digest, size, connection) ends the body with
                // that error after its bytes, never with a clean end. The
                // body owns the pull: dropped, it stops it.
                let pulled = stream::once(finished).filter_map(move |r| {
                    let _owned = &pull;
                    async move {
                        outcome(r)
                            .err()
                            .map(|e| Err(OciDistributionError::IoError(e)))
                    }
                });
                let bytes =
                    tokio_util::io::ReaderStream::with_capacity(reader, FILE_CHUNK).map(move |r| {
                        let chunk = r.map_err(OciDistributionError::IoError)?;
                        count(&chunk);
                        Ok(chunk)
                    });
                Ok((bytes.chain(pulled).boxed(), blob.size))
            }
        }
    }

    /// A name for error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Bytes(_) => "in-memory blob".to_owned(),
            Self::File(path) => path.display().to_string(),
            Self::Opened(_) => "anonymous file".to_owned(),
            Self::Remote(r) => format!("{} from {}", r.blob.digest, r.from),
        }
    }
}

/// The outcome of a pull into a pipe, as the reading end reports it.
type PullDone = tokio::sync::oneshot::Receiver<Result<(), crate::Error>>;

/// Starts pulling `blob` of `from` into a pipe of [`PIPE_BYTES`] on a
/// thread of its own, driven by `handle`'s runtime; returns the pipe's
/// reading end and the pull's outcome.
///
/// The pull's future is not `Send` (its retry closures), so it can be
/// neither a task nor part of a `Send` stream; a thread blocking on it is.
/// The pull is retried only until the first byte enters the pipe: what
/// was read cannot be taken back. The returned [`PipePull`] owns the
/// thread: dropped, it stops the pull and waits for the thread, so no pull
/// outlives its reader, nor the runtime the reader lives under.
fn pull_into_pipe(
    handle: &tokio::runtime::Handle,
    client: &Arc<crate::Client>,
    from: &rnk_oci::Reference,
    blob: &rnk_oci::Descriptor,
) -> io::Result<(tokio::io::DuplexStream, PullDone, PipePull)> {
    let (pipe, reader) = tokio::io::duplex(PIPE_BYTES);
    let mut writer = PipeSink { pipe, written: 0 };
    let (done, finished) = tokio::sync::oneshot::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (client, from, blob) = (Arc::clone(client), from.clone(), blob.clone());
    let handle = handle.clone();
    let thread = std::thread::Builder::new()
        .name("rnk-pull-pipe".to_owned())
        .spawn(move || {
            let r = handle.block_on(async {
                tokio::select! {
                    biased;
                    // Sent or dropped: the reader is gone.
                    _ = stopped => None,
                    r = client.pull_blob(&from, &blob, &mut writer) => Some(r),
                }
            });
            drop(writer);
            if let Some(r) = r {
                // The reader may have given up and gone: nobody waits then.
                let _unread = done.send(r);
            }
        })?;
    Ok((
        reader,
        finished,
        PipePull {
            stop: Some(stop),
            thread: Some(thread),
        },
    ))
}

/// The thread of a pull into a pipe: stopped and joined when dropped.
#[derive(Debug)]
struct PipePull {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for PipePull {
    fn drop(&mut self) {
        // Closing the channel wakes the thread, which drops the pull at its
        // next poll; a finished pull has nothing left to stop.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("the thread of a streamed pull panicked");
        }
    }
}

/// What a finished pull into a pipe means to its reader.
fn outcome(
    r: Result<Result<(), crate::Error>, tokio::sync::oneshot::error::RecvError>,
) -> io::Result<()> {
    match r {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(io::Error::other(e.to_string())),
        Err(_) => Err(io::Error::other("the pull of the blob stopped")),
    }
}

/// A blob read as it is pulled, for blocking code: the reading end of
/// [`crate::Client::open_blob`]. It ends cleanly only once the pull has
/// verified the blob's size and digest; any failure is a read error, after
/// the bytes that arrived.
#[derive(Debug)]
pub struct BlobReader {
    handle: tokio::runtime::Handle,
    reader: tokio::io::DuplexStream,
    done: Option<PullDone>,
    /// The pull's failure, reported again by every later read: a reader
    /// that failed never reads to a clean end.
    failed: Option<(io::ErrorKind, String)>,
    /// Declared last: dropped after the pipe's reading end, which unblocks
    /// a pull waiting to write.
    _pull: PipePull,
}

impl BlobReader {
    pub(crate) fn open(
        handle: &tokio::runtime::Handle,
        client: &Arc<crate::Client>,
        from: &rnk_oci::Reference,
        blob: &rnk_oci::Descriptor,
    ) -> io::Result<Self> {
        let (reader, done, pull) = pull_into_pipe(handle, client, from, blob)?;
        Ok(Self {
            handle: handle.clone(),
            reader,
            done: Some(done),
            failed: None,
            _pull: pull,
        })
    }

    fn read_once(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use tokio::io::AsyncReadExt as _;
        // Blocking inside a runtime would panic: refuse instead.
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(io::Error::other(
                "a streamed blob is read from a thread inside the async runtime",
            ));
        }
        let n = self.handle.block_on(self.reader.read(buf))?;
        if n == 0
            && let Some(done) = self.done.take()
        {
            outcome(done.blocking_recv())?;
        }
        Ok(n)
    }
}

impl std::io::Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some((kind, message)) = &self.failed {
            return Err(io::Error::new(*kind, message.clone()));
        }
        self.read_once(buf).inspect_err(|e| {
            self.failed = Some((e.kind(), e.to_string()));
        })
    }
}

/// Wraps a sink, counting what is written and refusing more than `limit`
/// bytes, so a hostile registry cannot fill the disk past a blob's size.
pub(crate) struct Bounded<'a, W> {
    inner: &'a mut W,
    written: u64,
    /// `written` plus the refused write, once the limit is hit.
    received: Option<u64>,
    limit: u64,
}

impl<'a, W> Bounded<'a, W> {
    pub(crate) fn new(inner: &'a mut W, limit: u64) -> Self {
        Self {
            inner,
            written: 0,
            received: None,
            limit,
        }
    }

    /// Bytes written so far.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// The bytes that had arrived when a write went past the limit, if one
    /// did.
    pub(crate) fn overflow(&self) -> Option<u64> {
        self.received
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Bounded<'_, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        let received = self.written.saturating_add(len);
        if received > self.limit {
            self.received = Some(received);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "blob is larger than its descriptor",
            )));
        }
        let this = &mut *self;
        let poll = Pin::new(&mut *this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = poll {
            this.written += u64::try_from(n).unwrap_or(u64::MAX);
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt as _;

    use super::*;

    #[tokio::test]
    async fn bounded_refuses_extra_bytes() {
        let mut out = Vec::new();
        let mut bounded = Bounded::new(&mut out, 4);
        bounded.write_all(b"abc").await.unwrap();
        assert!(bounded.write_all(b"de").await.is_err());
        assert_eq!(bounded.overflow(), Some(5));
        assert_eq!(bounded.written(), 3);
        assert_eq!(out, b"abc");
    }

    #[tokio::test]
    async fn file_sinks_reset_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        let mut file = tokio::fs::File::create(&path).await.unwrap();
        file.write_all(b"first attempt").await.unwrap();
        BlobSink::reset(&mut file).await.unwrap();
        file.write_all(b"ok").await.unwrap();
        file.flush().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"ok");
    }

    #[tokio::test]
    async fn bodies_reopen_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layer");
        std::fs::write(&path, vec![7u8; 600 * 1024]).unwrap();
        let sent = Arc::new(AtomicU64::new(0));
        for body in [
            BlobBody::File(path),
            BlobBody::Bytes(Bytes::from_static(b"cfg")),
        ] {
            for _ in 0..2 {
                let (stream, len) = body.open(Arc::clone(&sent)).await.unwrap();
                let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
                let total: usize = chunks.iter().map(Bytes::len).sum();
                assert_eq!(u64::try_from(total).unwrap(), len);
            }
        }
        assert_eq!(sent.load(Ordering::Relaxed), 2 * (600 * 1024 + 3));
    }
}
