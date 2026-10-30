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
}

impl Error {
    pub(crate) fn io(op: &'static str, path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { op, path, source }
    }
}
