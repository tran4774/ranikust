//! The `tar-header` limit (ADR-0079).
//!
//! The `tar` crate reads every extension header body (GNU long name `L`,
//! GNU long link `K`, PAX records `x`) whole into memory, at whatever size
//! its header declares (`tar-0.4.46 src/archive.rs:409-440`). [`Framed`]
//! sits between the decompressed layer and the `tar` crate: it follows
//! the archive's block framing exactly as the `tar` crate does
//! (`src/archive.rs:285-385`), including the `size` a PAX record set gives
//! the next entry, and fails the read that completes the header of an
//! extension larger than the limit, before the `tar` crate reads its body.
//! Go's `archive/tar`, which kaniko and BuildKit read layers with, has the
//! same 1 MiB bound (`maxSpecialFileSize`).

use std::fmt;
use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};

/// The default `tar-header` limit: Go's `archive/tar` bound.
pub const DEFAULT_EXTENSION_HEADER_LIMIT: u64 = 1 << 20;

static LIMIT: AtomicU64 = AtomicU64::new(DEFAULT_EXTENSION_HEADER_LIMIT);

/// Sets the `tar-header` limit, in bytes, for every extraction this
/// process starts afterwards: the largest body of a GNU long name, GNU
/// long link or PAX record set a layer may have. The binary sets it once,
/// from `--limit`, before any build; it is this crate's only process-wide
/// setting. A value of 0 counts as 1.
pub fn set_extension_header_limit(bytes: u64) {
    LIMIT.store(bytes.max(1), Ordering::Relaxed);
}

/// The `tar-header` limit in force.
pub(crate) fn extension_header_limit() -> u64 {
    LIMIT.load(Ordering::Relaxed)
}

/// An extension header over the limit, carried in an `io::Error` through
/// the `tar` crate.
#[derive(Debug)]
pub(crate) struct TooLarge {
    pub(crate) size: u64,
    pub(crate) max: u64,
}

impl fmt::Display for TooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "an extension header of {} bytes is larger than the limit of {}",
            self.size, self.max
        )
    }
}

impl std::error::Error for TooLarge {}

const BLOCK: usize = 512;

/// A pass-through reader that tracks the tar framing of what it hands on.
pub(crate) struct Framed<R> {
    inner: R,
    limit: u64,
    /// The header block being read.
    header: [u8; BLOCK],
    /// Bytes of `header` read so far.
    filled: usize,
    /// Bytes left of the current entry's body and padding.
    body_left: u64,
    /// The body of a PAX record set being read, and its size.
    pax: Option<(Vec<u8>, usize)>,
    /// A PAX record set waits for the next entry: the `size` it gives, if
    /// it gives one.
    pending: Option<Option<u64>>,
    /// The archive ended, or what follows is not tracked: everything is
    /// handed on as it is.
    done: bool,
    /// Every read after a refusal fails again.
    refused: Option<(u64, u64)>,
}

impl<R> Framed<R> {
    pub(crate) fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            header: [0; BLOCK],
            filled: 0,
            body_left: 0,
            pax: None,
            pending: None,
            done: false,
            refused: None,
        }
    }

    /// Follows `data`, the next bytes of the archive.
    fn observe(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() && !self.done {
            if self.body_left > 0 {
                let take = usize::try_from(self.body_left)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                if let Some((body, size)) = &mut self.pax {
                    let want = (*size - body.len()).min(take);
                    body.extend_from_slice(&data[..want]);
                    if body.len() == *size {
                        self.finish_pax();
                    }
                }
                self.body_left -= u64::try_from(take).unwrap_or(u64::MAX);
                data = &data[take..];
                continue;
            }
            let take = (BLOCK - self.filled).min(data.len());
            self.header[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == BLOCK {
                self.filled = 0;
                self.header_read()?;
            }
        }
        Ok(())
    }

    /// A whole header block has been read: what the `tar` crate makes of it.
    fn header_read(&mut self) -> io::Result<()> {
        if self.header.iter().all(|&b| b == 0) {
            // The `tar` crate stops at the first zero block.
            self.done = true;
            return Ok(());
        }
        let header = tar::Header::from_byte_slice(&self.header);
        let kind = header.entry_type();
        let recognized = header.as_gnu().is_some() || header.as_ustar().is_some();
        let extension_kind = kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_local_extensions()
            || kind.is_pax_global_extensions();
        let mut size = header.entry_size()?;
        if !extension_kind && let Some(Some(pax_size)) = self.pending {
            size = pax_size;
        }
        let buffered = recognized
            && (kind.is_gnu_longname() || kind.is_gnu_longlink() || kind.is_pax_local_extensions());
        if buffered {
            if size > self.limit {
                self.refused = Some((size, self.limit));
                return Err(self.refusal());
            }
            if kind.is_pax_local_extensions() {
                let len = usize::try_from(size).unwrap_or(usize::MAX);
                self.pax = Some((Vec::with_capacity(len), len));
                if len == 0 {
                    self.finish_pax();
                }
            }
        } else {
            // An entry handed to the caller takes the pending records with it.
            self.pending = None;
            if kind.is_gnu_sparse() {
                // The `tar` crate reads extension blocks of its own for a
                // sparse entry; the extraction refuses the entry right after,
                // so nothing past it is read.
                self.done = true;
                return Ok(());
            }
        }
        self.body_left = size
            .checked_add(511)
            .map(|s| s & !511)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "size overflow"))?;
        Ok(())
    }

    /// The `size` of a PAX record set, found as the `tar` crate finds it
    /// (`src/pax.rs:64-85`): the first `size` record, if it parses.
    fn finish_pax(&mut self) {
        if let Some((body, _)) = self.pax.take() {
            let mut size = None;
            for record in tar::PaxExtensions::new(&body) {
                let Ok(record) = record else { break };
                if record.key() != Ok("size") {
                    continue;
                }
                size = record.value().ok().and_then(|v| v.parse::<u64>().ok());
                break;
            }
            self.pending = Some(size);
        }
    }

    fn refusal(&self) -> io::Error {
        let (size, max) = self.refused.unwrap_or_default();
        io::Error::new(io::ErrorKind::InvalidData, TooLarge { size, max })
    }
}

impl<R: Read> Read for Framed<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.refused.is_some() {
            return Err(self.refusal());
        }
        let n = self.inner.read(buf)?;
        self.observe(&buf[..n])?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &[u8], kind: tar::EntryType, size: u64) -> [u8; BLOCK] {
        let mut h = tar::Header::new_gnu();
        h.set_path(std::str::from_utf8(name).unwrap()).unwrap();
        h.set_entry_type(kind);
        h.set_size(size);
        h.set_mode(0o644);
        h.set_cksum();
        let mut out = [0; BLOCK];
        out.copy_from_slice(h.as_bytes());
        out
    }

    fn padded(body: &[u8]) -> Vec<u8> {
        let mut v = body.to_vec();
        v.resize(body.len().div_ceil(BLOCK) * BLOCK, 0);
        v
    }

    /// Reads all of `archive` through [`Framed`] in `chunk`-byte reads.
    fn read_all(archive: &[u8], limit: u64, chunk: usize) -> io::Result<()> {
        let mut framed = Framed::new(archive, limit);
        let mut buf = vec![0; chunk];
        while framed.read(&mut buf)? > 0 {}
        Ok(())
    }

    fn too_large(e: &io::Error) -> bool {
        e.get_ref()
            .is_some_and(|i| i.downcast_ref::<TooLarge>().is_some())
    }

    #[test]
    fn an_extension_header_over_the_limit_is_refused_at_its_header() {
        for kind in [
            tar::EntryType::GNULongName,
            tar::EntryType::GNULongLink,
            tar::EntryType::XHeader,
        ] {
            let archive = header(b"././@LongLink", kind, 2048);
            for chunk in [1, 100, 512, 4096] {
                let err = read_all(&archive, 1024, chunk).unwrap_err();
                assert!(too_large(&err), "{kind:?}, {chunk}-byte reads: {err}");
            }
        }
    }

    #[test]
    fn extension_headers_within_the_limit_and_other_entries_pass() {
        let mut archive = Vec::new();
        archive.extend_from_slice(&header(b"././@LongLink", tar::EntryType::GNULongName, 600));
        archive.extend(padded(&[b'n'; 600]));
        // A file far larger than the limit is not an extension header.
        archive.extend_from_slice(&header(b"big", tar::EntryType::Regular, 5000));
        archive.extend(padded(&[0; 5000]));
        archive.extend([0; 2 * BLOCK]);
        // A global PAX header is handed to the caller and streamed.
        archive.extend_from_slice(&header(b"g", tar::EntryType::XGlobalHeader, 5000));
        for chunk in [1, 7, 512, 1 << 16] {
            read_all(&archive, 1024, chunk).unwrap();
        }
    }

    #[test]
    fn the_size_a_pax_record_gives_the_next_entry_keeps_the_framing() {
        // The PAX set says the next file holds 2048 bytes; its header says 0.
        // Inside those 2048 bytes sits what looks like an oversized long
        // name: it is file data, not a header.
        let record = b"13 size=2048\n";
        let mut archive = Vec::new();
        archive.extend_from_slice(&header(b"pax", tar::EntryType::XHeader, 13));
        archive.extend(padded(record));
        archive.extend_from_slice(&header(b"f", tar::EntryType::Regular, 0));
        let mut data = header(b"././@LongLink", tar::EntryType::GNULongName, 1 << 30).to_vec();
        data.resize(2048, 0);
        archive.extend(&data);
        // The real next header is an oversized long name, after the data.
        archive.extend_from_slice(&header(b"././@LongLink", tar::EntryType::GNULongName, 4096));
        for chunk in [1, 512, 1 << 16] {
            let err = read_all(&archive, 1024, chunk).unwrap_err();
            assert!(too_large(&err), "{chunk}: {err}");
            let TooLarge { size, .. } = err.get_ref().unwrap().downcast_ref().unwrap();
            assert_eq!(*size, 4096, "the header after the data, not inside it");
        }
    }

    #[test]
    fn reads_fail_again_after_a_refusal() {
        let archive = header(b"x", tar::EntryType::XHeader, 4096);
        let mut framed = Framed::new(&archive[..], 1024);
        let mut buf = [0; BLOCK];
        assert!(framed.read(&mut buf).is_err());
        assert!(framed.read(&mut buf).is_err());
    }
}
