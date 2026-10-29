//! Layer encoding: deterministic tar, parallel compression, digests.
//!
//! Writes layer tar streams with fully controlled headers: the writer sorts the
//! entries it is fed and emits missing parent directory headers, with
//! hardlinks, PAX xattrs and fixed timestamps for reproducible builds.
//! Compresses gzip and zstd in parallel, decompresses pulled gzip and zstd
//! blobs into a plain tar stream, and computes digests while streaming.
//!
//! # Boundaries
//!
//! - Same input, same bytes: output must not depend on thread count or on how
//!   input is chunked.
//! - Does not decide what goes into a layer (`rnk-snapshot`) or where it is
//!   sent (`rnk-registry`), and does not write extracted files to disk
//!   (`rnk-rootfs`).
