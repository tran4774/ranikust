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
//!
//! It fails closed: once it cannot follow the framing (a malformed size, a
//! GNU sparse entry, whose extension blocks it does not parse, or any read
//! past the end of the archive), every later read fails.

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

/// Why [`Framed`] stopped; every later read fails with it again.
#[derive(Clone, Copy, Debug)]
enum Stop {
    TooLarge {
        size: u64,
        max: u64,
    },
    /// After a GNU sparse entry, whose extension blocks are not followed.
    Sparse,
    /// A read after the end-of-archive block.
    PastEnd,
    /// A header whose size does not parse or overflows.
    BadSize,
}

impl Stop {
    fn error(self) -> io::Error {
        match self {
            Self::TooLarge { size, max } => {
                io::Error::new(io::ErrorKind::InvalidData, TooLarge { size, max })
            }
            Self::Sparse => io::Error::new(
                io::ErrorKind::InvalidData,
                "nothing is read after a GNU sparse entry",
            ),
            Self::PastEnd => io::Error::new(
                io::ErrorKind::InvalidData,
                "a read past the end of the archive",
            ),
            Self::BadSize => io::Error::new(
                io::ErrorKind::InvalidData,
                "a tar header with an invalid size",
            ),
        }
    }
}

const BLOCK: usize = 512;

/// How much of a PAX record set's body is reserved before it arrives: a
/// raised limit must not reserve it whole up front.
const PAX_RESERVE: usize = 64 << 10;

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
    /// The end-of-archive block was read: the `tar` crate reads no more.
    ended: bool,
    /// Set once the framing cannot be followed: every later read fails.
    stop: Option<Stop>,
    /// Offset of the next byte handed on.
    #[cfg(test)]
    offset: u64,
    /// Each header's offset, and whether the `tar` crate hands its entry
    /// to the caller (rather than keeping it as an extension).
    #[cfg(test)]
    headers: Vec<(u64, bool)>,
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
            ended: false,
            stop: None,
            #[cfg(test)]
            offset: 0,
            #[cfg(test)]
            headers: Vec::new(),
        }
    }

    /// Follows `data`, the next bytes of the archive. `Some` when what it
    /// read stops the framing.
    fn observe(&mut self, mut data: &[u8]) -> Option<Stop> {
        while !data.is_empty() {
            if self.ended {
                return Some(Stop::PastEnd);
            }
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
                self.advance(take);
                data = &data[take..];
                continue;
            }
            let take = (BLOCK - self.filled).min(data.len());
            self.header[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            self.advance(take);
            data = &data[take..];
            if self.filled == BLOCK {
                self.filled = 0;
                // A sparse header still reaches the `tar` crate, so that the
                // extraction refuses its entry by name; reads after it fail.
                match self.header_read() {
                    Ok(()) => {}
                    Err(Stop::Sparse) => {
                        self.stop = Some(Stop::Sparse);
                        return if data.is_empty() {
                            None
                        } else {
                            Some(Stop::Sparse)
                        };
                    }
                    Err(stop) => return Some(stop),
                }
            }
        }
        None
    }

    #[cfg(test)]
    fn advance(&mut self, n: usize) {
        self.offset += u64::try_from(n).unwrap_or(u64::MAX);
    }

    #[cfg(not(test))]
    fn advance(&mut self, _n: usize) {}

    /// A whole header block has been read: what the `tar` crate makes of it.
    fn header_read(&mut self) -> Result<(), Stop> {
        if self.header.iter().all(|&b| b == 0) {
            // The `tar` crate stops at the first zero block.
            self.ended = true;
            return Ok(());
        }
        let header = tar::Header::from_byte_slice(&self.header);
        let kind = header.entry_type();
        let recognized = header.as_gnu().is_some() || header.as_ustar().is_some();
        let extension_kind = kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_local_extensions()
            || kind.is_pax_global_extensions();
        let mut size = header.entry_size().map_err(|_| Stop::BadSize)?;
        if !extension_kind && let Some(Some(pax_size)) = self.pending {
            size = pax_size;
        }
        let buffered = recognized
            && (kind.is_gnu_longname() || kind.is_gnu_longlink() || kind.is_pax_local_extensions());
        #[cfg(test)]
        self.headers.push((self.offset - BLOCK as u64, !buffered));
        if buffered {
            if size > self.limit {
                return Err(Stop::TooLarge {
                    size,
                    max: self.limit,
                });
            }
            if kind.is_pax_local_extensions() {
                let len = usize::try_from(size).unwrap_or(usize::MAX);
                self.pax = Some((Vec::with_capacity(len.min(PAX_RESERVE)), len));
                if len == 0 {
                    self.finish_pax();
                }
            }
        } else {
            // An entry handed to the caller takes the pending records with it.
            self.pending = None;
            if kind.is_gnu_sparse() {
                // The `tar` crate reads extension blocks of its own for a
                // sparse entry, which this reader does not follow.
                return Err(Stop::Sparse);
            }
        }
        self.body_left = size
            .checked_add(511)
            .map(|s| s & !511)
            .ok_or(Stop::BadSize)?;
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
}

impl<R: Read> Read for Framed<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(stop) = self.stop {
            return Err(stop.error());
        }
        let n = self.inner.read(buf)?;
        if let Some(stop) = self.observe(&buf[..n]) {
            self.stop = Some(stop);
            return Err(stop.error());
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests;
