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
}

impl BlobSink for tokio::fs::File {
    async fn reset(&mut self) -> io::Result<()> {
        self.flush().await?;
        self.set_len(0).await?;
        self.seek(io::SeekFrom::Start(0)).await?;
        Ok(())
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
    /// A file, streamed: a layer in the store.
    File(PathBuf),
}

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
            Self::File(path) => {
                let file = tokio::fs::File::open(path).await?;
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
        }
    }

    /// A name for error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Bytes(_) => "in-memory blob".to_owned(),
            Self::File(path) => path.display().to_string(),
        }
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
