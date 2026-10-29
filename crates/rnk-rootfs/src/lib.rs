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
