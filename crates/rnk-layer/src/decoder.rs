//! Pulled layer blobs back to plain tar streams.

use std::io::{self, Chain, Cursor, ErrorKind, Read};

use flate2::read::MultiGzDecoder;

use crate::digest::{Hasher, Sha256Digest};
use crate::{Compression, Error};

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// The blob with the sniffed bytes put back in front.
type Sniffed<R> = Chain<Cursor<Vec<u8>>, R>;

/// Turns a layer blob into its plain tar stream, as a [`Read`], and hashes
/// that stream for the diff ID.
///
/// The compression is sniffed from the first bytes: gzip (any number of
/// members, as other tools write them), zstd ([`Error::Unsupported`] for now),
/// or else a plain tar. Memory stays bounded whatever the blob holds; a
/// corrupt blob gives an [`io::Error`], never a panic. Inflating is
/// sequential: gzip has no index to split it by.
///
/// Verifying the blob's own digest is the caller's job (`rnk-registry`,
/// `rnk-store`): this type sees only what comes out.
pub struct Decoder<R: Read> {
    inner: Inner<R>,
    compression: Compression,
    hasher: Option<Hasher>,
    /// Set once the end of the tar stream was read.
    diff_id: Option<Sha256Digest>,
    size: u64,
}

enum Inner<R: Read> {
    Plain(Sniffed<R>),
    Gzip(Box<MultiGzDecoder<Sniffed<R>>>),
}

impl<R: Read> Decoder<R> {
    /// Sniffs the compression of `blob`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for zstd; [`Error::Io`] if reading the first
    /// bytes fails.
    pub fn new(mut blob: R) -> Result<Self, Error> {
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
            // Q-052: zstd arrives with the cargo-zigbuild toolchain.
            Compression::Zstd => return Err(Error::Unsupported(Compression::Zstd)),
        };
        Ok(Self {
            inner,
            compression,
            hasher: Some(Hasher::default()),
            diff_id: None,
            size: 0,
        })
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
    fn zstd_is_recognised_but_unsupported() {
        let err = Decoder::new(&b"\x28\xb5\x2f\xfd\x00"[..]).err().unwrap();
        assert!(matches!(err, Error::Unsupported(Compression::Zstd)));
    }

    proptest! {
        #[test]
        fn hostile_gzip_never_panics(tail in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let mut blob = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
            blob.extend_from_slice(&tail);
            // Any outcome but a panic is fine.
            let _ = decode_all(&blob);
        }
    }
}
