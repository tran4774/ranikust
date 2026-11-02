//! Entry keys and their names in the state directory.

use std::fmt;
use std::path::PathBuf;

use rnk_oci::Digest;

/// The layout of `layers/`, named by chain ID since version 2 (ADR-0032,
/// Q-150). Each version is a new directory, so layers extracted by an
/// earlier rule are never reused. The first layout named them by diff ID in
/// `layers/<algorithm>/`, where an upper layer may hold content copied from
/// another stack; version 2 (`layers/v2/`) may hold an image's `trusted.*`
/// xattrs, which version 3 never sets (ADR-0075). They stay on disk until
/// `prune` (Phase 5) or the operator removes them.
const LAYER_LAYOUT: &str = "v3";

/// An entry of the store, named by a digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryKey {
    /// A compressed blob, by its digest: the file
    /// `blobs/<algorithm>/<hex>`.
    Blob(Digest),
    /// An extracted layer, by the digest its extractor names it with:
    /// `rnk-snapshot` uses the layer's chain ID (its diff ID for a bottom
    /// layer), so that the gzip and zstd blobs of one layer share it and
    /// an extraction that depends on the layers below never crosses stacks
    /// (ADR-0032, Q-150): the directory `layers/v3/<algorithm>/<hex>/`
    /// [open: Q-015].
    Layer(Digest),
}

impl EntryKey {
    /// The digest naming the entry.
    #[must_use]
    pub fn digest(&self) -> &Digest {
        match self {
            Self::Blob(d) | Self::Layer(d) => d,
        }
    }

    /// The top-level directory of this kind of entry.
    fn kind_dir(&self) -> &'static str {
        match self {
            Self::Blob(_) => "blobs",
            Self::Layer(_) => "layers",
        }
    }

    /// `<kind>/<algorithm>`, or `layers/v3/<algorithm>` for a layer, the
    /// parent of the entry and of its lock file under `locks/`. Built from
    /// a validated digest only, so it never holds `..` or anything but
    /// lowercase hex and the algorithm.
    pub(crate) fn parent(&self) -> PathBuf {
        let mut p = PathBuf::from(self.kind_dir());
        if let Self::Layer(_) = self {
            p.push(LAYER_LAYOUT);
        }
        p.push(self.digest().algorithm().as_str());
        p
    }

    /// The entry, relative to the state directory.
    pub(crate) fn path(&self) -> PathBuf {
        self.parent().join(self.digest().encoded())
    }

    /// Its lock file, relative to the state directory.
    pub(crate) fn lock_path(&self) -> PathBuf {
        PathBuf::from("locks").join(self.path())
    }
}

impl fmt::Display for EntryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blob(d) => write!(f, "blob {}", d.as_str()),
            Self::Layer(d) => write!(f, "layer {}", d.as_str()),
        }
    }
}
