//! Filesystem access confined to a build root.
//!
//! `RootPath` resolves every path inside the root through file descriptors
//! (`openat2` with `RESOLVE_IN_ROOT`), so `..` and symlinks cannot escape it.
//! Built on it: layer extraction from an already decompressed tar stream,
//! writing whiteouts in the form the caller asks for; copy, ownership and
//! xattrs; `USER` and `--chown` name lookup in the root's `/etc/passwd` and
//! `/etc/group`, for `rnk-sandbox` and `rnk-builder`; the ignore list; and safe
//! deletion of a root's contents.
//!
//! Linux only: the crate needs `openat2` (Linux 5.6) and the `statx` mount id
//! (Linux 5.8). When either is missing or blocked by seccomp it fails with
//! [`Error::Unsupported`]; there is no string-based fallback. The binary
//! calls [`probe_support`] at start-up to refuse such a kernel before any
//! work (ADR-0028).
//!
//! # Boundaries
//!
//! - Never reaches a path in the root by joining strings onto a host path.
//! - Deletion works through file descriptors, never follows symlinks and never
//!   crosses a mount point (compared by `mnt_id`). This crate provides the
//!   mechanism; deciding what to delete and when belongs to `rnk-snapshot`
//!   (build roots) and `rnk-store` (store entries).
//! - Does not decompress layers: the caller chains `rnk-layer`'s decoder into
//!   the extraction, and this crate never depends on `rnk-layer`.
//! - Does not mount, create namespaces or run processes (`rnk-sandbox`).

mod copy;
mod delete;
mod error;
mod extract;
mod meta;
mod path;
mod root;
mod support;
#[cfg(test)]
mod test_util;
mod user;

pub use copy::{CopyOpts, copy, copy_filtered};
pub use delete::{IgnoreList, delete_contents};
pub use error::{Error, Result};
pub use extract::{Extracted, Whiteouts, extract, extract_overlay};
pub use meta::{FileTime, FileType, Meta, Stat};
pub use root::{OpenHow, RootPath};
pub use support::{Cause, Unsupported, probe_support};
pub use user::{Owner, User, lookup_owner, lookup_user};
