//! Pulled layer blobs back to plain tar streams.

use std::io::{self, BufReader, Chain, Cursor, ErrorKind, Read};

use flate2::read::MultiGzDecoder;

use crate::digest::{Hasher, Sha256Digest};
use crate::{Compression, Error};

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// The blob with the sniffed bytes put back in front.
type Sniffed<R> = Chain<Cursor<Vec<u8>>, Counted<R>>;

/// Turns a layer blob into its plain tar stream, as a [`Read`], and hashes
/// that stream for the diff ID.
///
/// The compression is sniffed from the first bytes: gzip (any number of
/// members, as other tools write them), zstd (any number of frames), or else
/// a plain tar. Memory stays bounded whatever the blob holds (libzstd refuses
/// frames that need a window above 128 MiB); a corrupt blob gives an
/// [`io::Error`], never a panic. Decompressing is sequential: gzip has no
/// index to split it by, and other tools write zstd as one long frame.
///
/// Verifying the blob's own digest is the caller's job (`rnk-registry`,
/// `rnk-store`): this type sees only what comes out. Each decoder opens an
/// `rnk_layer.decode` span that closes when it is dropped.
pub struct Decoder<R: Read> {
    inner: Inner<R>,
    compression: Compression,
    hasher: Option<Hasher>,
    /// Set once the end of the tar stream was read.
    diff_id: Option<Sha256Digest>,
    size: u64,
    span: tracing::Span,
}

enum Inner<R: Read> {
    Plain(Sniffed<R>),
    Gzip(Box<MultiGzDecoder<Sniffed<R>>>),
    Zstd(Box<zstd::stream::read::Decoder<'static, BufReader<Sniffed<R>>>>),
}

/// The blob, counting the bytes read from it.
struct Counted<R> {
    inner: R,
    count: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count += u64::try_from(n).map_err(io::Error::other)?;
        Ok(n)
    }
}

impl<R: Read> Decoder<R> {
    /// Sniffs the compression of `blob`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if reading the first bytes fails or the zstd decoder
    /// cannot be set up.
    pub fn new(blob: R) -> Result<Self, Error> {
        let mut blob = Counted {
            inner: blob,
            count: 0,
        };
        let mut magic = [0u8; 4];
        let mut have = 0;
        while have < magic.len() {
            match blob.read(&mut magic[have..]) {
                Ok(0) => break,
                Ok(n) => have += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let magic = &magic[..have];
        let compression = if magic.starts_with(&GZIP_MAGIC) {
            Compression::Gzip
        } else if magic == ZSTD_MAGIC {
            Compression::Zstd
        } else {
            Compression::None
        };
        let sniffed = Cursor::new(magic.to_vec()).chain(blob);
        let inner = match compression {
            Compression::None => Inner::Plain(sniffed),
            Compression::Gzip => Inner::Gzip(Box::new(MultiGzDecoder::new(sniffed))),
            Compression::Zstd => Inner::Zstd(Box::new(zstd::stream::read::Decoder::new(sniffed)?)),
        };
        let span = tracing::debug_span!(
            "rnk_layer.decode",
            bytes_in = tracing::field::Empty,
            bytes_out = tracing::field::Empty,
        );
        Ok(Self {
            inner,
            compression,
            hasher: Some(Hasher::default()),
            diff_id: None,
            size: 0,
            span,
        })
    }

    /// Bytes read from the blob so far, the sniffed ones included.
    fn blob_bytes(&self) -> u64 {
        let sniffed = match &self.inner {
            Inner::Plain(r) => r,
            Inner::Gzip(r) => r.get_ref(),
            Inner::Zstd(r) => r.get_ref().get_ref(),
        };
        sniffed.get_ref().1.count
    }

    /// The compression found in the blob.
    #[must_use]
    pub fn compression(&self) -> Compression {
        self.compression
    }

    /// The diff ID, once the end of the tar stream has been read.
    #[must_use]
    pub fn diff_id(&self) -> Option<Sha256Digest> {
        self.diff_id
    }

    /// Bytes of tar stream read so far.
    #[must_use]
    pub fn tar_size(&self) -> u64 {
        self.size
    }

    /// Reads whatever is left (a tar reader stops at the end-of-archive
    /// marker, before any padding) and returns the diff ID.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if reading or decompressing fails.
    pub fn finish(mut self) -> Result<Sha256Digest, Error> {
        io::copy(&mut self, &mut io::sink())?;
        self.diff_id
            .ok_or_else(|| Error::Io(io::Error::other("the tar stream did not end")))
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = match &mut self.inner {
            Inner::Plain(r) => r.read(buf)?,
            Inner::Gzip(r) => r.read(buf)?,
            Inner::Zstd(r) => r.read(buf)?,
        };
        if n > 0 {
            if let Some(hasher) = &mut self.hasher {
                hasher.update(&buf[..n]);
            }
            self.size += u64::try_from(n).map_err(io::Error::other)?;
        } else if !buf.is_empty()
            && let Some(hasher) = self.hasher.take()
        {
            self.diff_id = Some(hasher.finish().0);
        }
        Ok(n)
    }
}

impl<R: Read> Drop for Decoder<R> {
    fn drop(&mut self) {
        // Recorded when the decoder goes away, so a blob read only partly
        // (a failed extraction) still shows what was consumed.
        self.span.record("bytes_in", self.blob_bytes());
        self.span.record("bytes_out", self.size);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn decode_all(blob: &[u8]) -> io::Result<Vec<u8>> {
        let mut d = Decoder::new(blob).map_err(io::Error::other)?;
        let mut out = Vec::new();
        d.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn short_blobs_are_plain() {
        for blob in [&b""[..], b"\x1f", b"ab", b"\x28\xb5\x2f"] {
            let d = Decoder::new(blob).unwrap();
            assert_eq!(d.compression(), Compression::None, "{blob:?}");
            assert_eq!(decode_all(blob).unwrap(), blob);
        }
    }

    #[test]
    fn a_truncated_zstd_frame_is_an_error() {
        let d = Decoder::new(&b"\x28\xb5\x2f\xfd\x00"[..]).unwrap();
        assert_eq!(d.compression(), Compression::Zstd);
        assert!(decode_all(b"\x28\xb5\x2f\xfd\x00").is_err());
    }

    #[test]
    fn frames_needing_a_huge_window_are_refused() {
        // A frame header announcing a 2 GiB window (Window_Descriptor
        // exponent 21, RFC 8878 3.1.1.1.2) and no content size.
        let blob = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 21 << 3, 0, 0, 0];
        let err = decode_all(&blob).unwrap_err();
        // libzstd: "Frame requires too much memory for decoding".
        assert!(err.to_string().contains("memory"), "{err}");
    }

    #[test]
    fn bytes_read_from_the_blob_are_counted() {
        let blob = zstd::bulk::compress(&[7; 10_000], 3).unwrap();
        let mut d = Decoder::new(&blob[..]).unwrap();
        assert_eq!(d.blob_bytes(), 4, "the sniffed magic");
        io::copy(&mut d, &mut io::sink()).unwrap();
        assert_eq!(d.blob_bytes(), u64::try_from(blob.len()).unwrap());
        assert_eq!(d.tar_size(), 10_000);
    }

    proptest! {
        #[test]
        fn hostile_gzip_never_panics(tail in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let mut blob = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
            blob.extend_from_slice(&tail);
            // Any outcome but a panic is fine.
            let _ = decode_all(&blob);
        }

        #[test]
        fn hostile_zstd_never_panics(tail in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let mut blob = vec![0x28, 0xb5, 0x2f, 0xfd];
            blob.extend_from_slice(&tail);
            let _ = decode_all(&blob);
        }
    }
}
