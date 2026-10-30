//! Errors of the stage builder.

use std::io;
use std::path::PathBuf;

/// A builder result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why building, assembling or writing an image failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Encoding a layer failed.
    #[error("encoding a layer: {0}")]
    Layer(#[from] rnk_layer::Error),

    /// An image type could not be built or serialized.
    #[error(transparent)]
    Oci(#[from] rnk_oci::Error),

    /// A registry request failed.
    #[error(transparent)]
    Registry(#[from] rnk_registry::Error),

    /// A host file operation on a build-private file or an output failed.
    #[error("{op} {}: {source}", path.display())]
    Io {
        /// The operation, for example `create`.
        op: &'static str,
        /// The host path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The image config does not match the layers it is assembled with: a
    /// bug in the caller, never caused by user input.
    #[error("the image config does not match its layers: {0}")]
    Inconsistent(String),

    /// `SOURCE_DATE_EPOCH` is not a number of seconds since the Unix epoch.
    #[error("SOURCE_DATE_EPOCH={value:?} is not a non-negative number of seconds")]
    SourceDateEpoch {
        /// The value, as given.
        value: String,
    },

    /// A destination cannot be pushed to.
    #[error("destination {destination}: {reason}")]
    Destination {
        /// The destination, as given.
        destination: String,
        /// Why it is refused.
        reason: &'static str,
    },

    /// A layer's media type, from a base image's manifest, is not one of
    /// the layer types an output can name (a Docker tar needs to know its
    /// compression).
    #[error("a layer has the media type {media_type}, which is not a known layer type")]
    UnknownLayerType {
        /// The media type, as the base manifest gave it.
        media_type: rnk_oci::MediaType,
    },

    /// An instruction failed; `source` says why.
    #[error("{stage}: line {line}: {instruction}: {source}")]
    Instruction {
        /// The stage's display name.
        stage: String,
        /// The instruction's first line in the Dockerfile.
        line: usize,
        /// The instruction as written, shortened.
        instruction: String,
        /// What failed.
        #[source]
        source: Box<Error>,
    },

    /// The Dockerfile asks for something this build refuses: a feature of
    /// a later phase, or an invalid value (a missing source, a bad
    /// `--chmod`).
    #[error("{0}")]
    Invalid(String),

    /// A `RUN` command exited with a failure.
    #[error("the command returned a non-zero code: {code}")]
    RunFailed {
        /// Exit code, or `128 + signal` when a signal killed it.
        code: i32,
    },

    /// The build was cancelled.
    #[error("cancelled")]
    Cancelled,

    /// Expanding or resolving the Dockerfile failed.
    #[error(transparent)]
    Dockerfile(#[from] rnk_dockerfile::Error),

    /// The build context could not be read.
    #[error(transparent)]
    Context(#[from] rnk_context::Error),

    /// A root filesystem operation failed.
    #[error(transparent)]
    Rootfs(#[from] rnk_rootfs::Error),

    /// A stage root could not be made, mounted or diffed.
    #[error(transparent)]
    Snapshot(#[from] rnk_snapshot::Error),

    /// The sandbox could not run a `RUN`.
    #[error(transparent)]
    Sandbox(#[from] rnk_sandbox::Error),

    /// The store failed.
    #[error(transparent)]
    Store(#[from] rnk_store::Error),

    /// Pushing to one destination failed. The destinations in `pushed` were
    /// pushed completely before it and stay so; later ones were not tried.
    #[error("pushing to {destination}: {source}")]
    Push {
        /// The destination that failed.
        destination: Box<rnk_oci::Reference>,
        /// The destinations pushed before it, with the image's digest.
        pushed: Vec<(rnk_oci::Reference, rnk_oci::Digest)>,
        /// What failed.
        #[source]
        source: Box<Error>,
    },
}

impl Error {
    pub(crate) fn io(op: &'static str, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { op, path, source }
    }
}
