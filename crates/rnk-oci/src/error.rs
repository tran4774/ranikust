//! The crate's error type.

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
}
