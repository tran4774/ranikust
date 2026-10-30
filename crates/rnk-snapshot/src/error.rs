//! Errors of stage roots, mounts, extraction and export.

use std::path::PathBuf;

use rustix::io::Errno;

/// A `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What can go wrong in `rnk-snapshot`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An overlay mount failed. `how` is `lowerdir+` (`fsconfig`) or
    /// `mount(2)`; `step` the call that failed; `options` the length of the
    /// option string (`mount(2)` only), so a truncation is visible.
    #[error("overlay mount of {target} with {lowers} lower layers failed at {how} {step}: {errno}{}",
        .options.map(|n| format!(" (option string of {n} bytes)")).unwrap_or_default())]
    Mount {
        /// The mount point.
        target: PathBuf,
        /// How many lower layers the mount had.
        lowers: usize,
        /// `lowerdir+` or `mount(2)`.
        how: &'static str,
        /// The failing call (`fsopen`, `fsconfig upperdir`, `create`, ...).
        step: String,
        /// The kernel's answer.
        errno: Errno,
        /// The length of the `mount(2)` option string.
        options: Option<usize>,
    },
    /// The `mount(2)` option string would not fit in one page: the kernel
    /// truncates it and answers `ENOENT`.
    #[error(
        "overlay mount of {target}: the option string for {lowers} lower layers is {len} bytes, \
         above the {max}-byte limit of mount(2) on this kernel (no `lowerdir+`)"
    )]
    OptionsTooLong {
        /// The mount point.
        target: PathBuf,
        /// How many lower layers the mount had.
        lowers: usize,
        /// The option string's length.
        len: usize,
        /// The largest length `mount(2)` takes.
        max: usize,
    },
    /// A path overlay's option syntax cannot carry: `mount(2)` splits its
    /// option string on `,` and lists of lowers on `:`, with `\` and `=`
    /// special too, so such a state directory could add options or layers.
    #[error("{path}: {reason}; overlay mounts need a state directory without it (--state-dir)")]
    UnsafePath {
        /// The path.
        path: PathBuf,
        /// What it holds.
        reason: &'static str,
    },
    /// More lower layers than overlay allows [open: Q-026].
    #[error("a stage root needs {lowers} lower layers; overlay takes at most {max}")]
    TooManyLowers {
        /// How many were needed.
        lowers: usize,
        /// Overlay's limit.
        max: usize,
    },
    /// Unmounting failed.
    #[error("unmounting {target} failed: {errno}")]
    Unmount {
        /// The mount point.
        target: PathBuf,
        /// The kernel's answer.
        errno: Errno,
    },
    /// A call that needs a layer open, or none, came at the wrong time.
    #[error("stage root: {0}")]
    State(&'static str),
    /// The upper directory holds what `redirect_dir=nofollow` never
    /// creates, or what no layer can carry.
    #[error("cannot export {path}: {reason}")]
    Export {
        /// The entry, relative to the layer root.
        path: PathBuf,
        /// Why.
        reason: &'static str,
    },
    /// A pulled layer's uncompressed tar is not the diff ID the image
    /// config names.
    #[error("layer {expected}: the uncompressed tar has diff ID {actual}")]
    DiffIdMismatch {
        /// The diff ID from the image config.
        expected: String,
        /// What the tar hashed to.
        actual: String,
    },
    /// The overlay probe failed: overlay mode cannot run here.
    #[error("overlay probe: {step}: {source}")]
    Probe {
        /// The probe step (`mount`, `copy-up`, `whiteout`, ...).
        step: &'static str,
        /// What went wrong.
        #[source]
        source: Box<Error>,
    },
    /// The probe's mount worked but the upper directory did not hold what
    /// overlay should have written there.
    #[error("overlay probe: {0}")]
    ProbeCheck(&'static str),
    /// `/proc/self/mountinfo` or an ignore-list input kaniko mode cannot
    /// use.
    #[error("kaniko-mode ignore list: {0}")]
    Ignore(String),
    /// A filesystem operation through `rnk-rootfs`.
    #[error(transparent)]
    Rootfs(#[from] rnk_rootfs::Error),
    /// The store.
    #[error(transparent)]
    Store(#[from] rnk_store::Error),
    /// Decoding or encoding a layer.
    #[error(transparent)]
    Layer(#[from] rnk_layer::Error),
    /// A host-side I/O error on ranikust's own directories.
    #[error("{op} {path}: {source}")]
    Io {
        /// The operation.
        op: &'static str,
        /// The path.
        path: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// The errno behind this error, if there is one.
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        match self {
            Self::Mount { errno, .. } | Self::Unmount { errno, .. } => Some(errno.raw_os_error()),
            Self::Probe { source, .. } => source.raw_os_error(),
            Self::Rootfs(e) => e.raw_os_error(),
            Self::Io { source, .. } => source.raw_os_error(),
            Self::OptionsTooLong { .. }
            | Self::UnsafePath { .. }
            | Self::TooManyLowers { .. }
            | Self::State(_)
            | Self::Export { .. }
            | Self::DiffIdMismatch { .. }
            | Self::ProbeCheck(_)
            | Self::Ignore(_)
            | Self::Store(_)
            | Self::Layer(_) => None,
        }
    }
}
