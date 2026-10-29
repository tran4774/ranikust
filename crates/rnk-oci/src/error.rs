//! The crate's error type.

use crate::media_type::{Compression, ImageFormat, MediaType};

/// Everything that can go wrong in `rnk-oci`.
///
/// Inputs quoted in a message are untrusted (they come from registries,
/// Dockerfiles and flags), so they are cut to a short excerpt.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A digest is not `sha256:<64 hex>` or `sha512:<128 hex>`.
    #[error("invalid digest {input:?}: {reason}")]
    InvalidDigest {
        /// The start of the rejected input.
        input: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The image format has no layer media type for this compression:
    /// Docker v2 schema 2 has no zstd.
    #[error("{format} images cannot carry {compression} layers; zstd needs the OCI format")]
    UnsupportedCompression {
        /// The requested image format.
        format: ImageFormat,
        /// The requested compression.
        compression: Compression,
    },
    /// A media type has no equivalent in the other image format.
    #[error("media type {} has no {format} equivalent", excerpt(.media_type.as_str()))]
    NoEquivalentMediaType {
        /// The media type to convert.
        media_type: MediaType,
        /// The target format.
        format: ImageFormat,
    },
    /// A manifest or index is not schema version 2 (Docker v2 schema 1 is
    /// not supported).
    #[error("unsupported schemaVersion {0}; only 2 is supported")]
    UnsupportedSchemaVersion(u32),
    /// A document's own media type says it is something else.
    #[error("expected {expected}, found media type {}", excerpt(.found.as_str()))]
    UnexpectedMediaType {
        /// What the caller asked to parse.
        expected: &'static str,
        /// The document's `mediaType`.
        found: MediaType,
    },
    /// A manifest or index mixes OCI and Docker media types.
    #[error("the {document} mixes OCI and Docker media types")]
    MixedFormats {
        /// `manifest` or `index`.
        document: &'static str,
    },
    /// JSON that does not parse into the expected type, or a value that
    /// cannot be written as JSON.
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A value that has no canonical JSON form.
    #[error("not canonical JSON: {0}")]
    NotCanonical(&'static str),
}

/// Longest excerpt of an untrusted input kept in an error, in characters.
const EXCERPT_CHARS: usize = 80;

/// The first [`EXCERPT_CHARS`] characters of `input`, with `…` when cut.
pub(crate) fn excerpt(input: &str) -> String {
    match input.char_indices().nth(EXCERPT_CHARS) {
        Some((end, _)) => format!("{}…", input.get(..end).unwrap_or(input)),
        None => input.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_keeps_short_input() {
        assert_eq!(excerpt("sha256:abc"), "sha256:abc");
        assert_eq!(excerpt(""), "");
    }

    #[test]
    fn excerpt_cuts_long_input_on_a_char_boundary() {
        let long = "é".repeat(200);
        let cut = excerpt(&long);
        assert_eq!(cut.chars().count(), EXCERPT_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn media_types_in_messages_are_cut() {
        let err = Error::UnexpectedMediaType {
            expected: "an image manifest",
            found: MediaType::new("x".repeat(10_000)),
        };
        assert!(err.to_string().len() < 200);
    }
}
