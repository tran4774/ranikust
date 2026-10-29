//! Layer encoding: deterministic tar, parallel compression, digests.
//!
//! Writes layer tar streams with fully controlled headers: the writer sorts the
//! entries it is fed and emits missing parent directory headers, with
//! hardlinks, PAX xattrs and fixed timestamps for reproducible builds.
//! Compresses gzip (one pigz-style member) and zstd (independent 1 MiB
//! frames) in parallel with the same bytes for any thread count,
//! decompresses pulled gzip and zstd blobs into a plain tar stream, and
//! computes digests while streaming.
//!
//! The exporter feeds [`Entry`] values to a [`LayerWriter`], then calls
//! [`LayerWriter::finish`] with an [`EntrySource`] that opens file contents
//! and answers lookups for parent directories it did not feed; it gets
//! [`LayerDigests`] back. A pulled blob goes through a [`Decoder`], which
//! reads as the plain tar stream and gives its diff ID.
//!
//! # Boundaries
//!
//! - Same input, same bytes: output must not depend on thread count or on how
//!   input is chunked.
//! - Does not decide what goes into a layer (`rnk-snapshot`) or where it is
//!   sent (`rnk-registry`), and does not write extracted files to disk
//!   (`rnk-rootfs`).

mod decoder;
mod digest;
mod entry;
mod error;
mod gzip;
mod header;
mod parallel;
mod writer;
mod zstd_frames;

pub use decoder::Decoder;
pub use digest::Sha256Digest;
pub use entry::{Entry, EntryKind, EntryPath};
pub use error::Error;
pub use writer::{Compression, EncodeOptions, EntrySource, LayerDigests, LayerWriter};
