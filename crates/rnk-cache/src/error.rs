//! Why a lookup missed or a push failed: a warning for the caller, never a
//! failed build (ADR-0113).

use std::fmt;

use crate::key::Encoding;

/// A lookup or push that failed. The caller logs it as a warning; the
/// build's exit status and image do not change (invariant 6 of #123).
///
/// Messages hold no credential: registry errors never carry one, and text
/// read from an entry reaches a message only as a short excerpt.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The cache repository is not a repository: it has a tag or a digest.
    #[error("cache repository {reference}: {reason}")]
    Repository {
        /// The reference, as given.
        reference: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// The registry failed: unreachable, a 5xx after its retries, a
    /// refusal, a timeout, or a document over a `--limit` limit.
    #[error("cache entry {entry}: {source}")]
    Registry {
        /// The entry's reference.
        entry: String,
        /// What the registry client reported.
        #[source]
        source: Box<rnk_registry::Error>,
    },
    /// The entry under the tag is not what this build would have pushed.
    #[error("cache entry {entry} refused: {refusal}")]
    Refused {
        /// The entry's reference.
        entry: String,
        /// What was wrong with it.
        refusal: Refusal,
    },
    /// The layer to push is not encoded as its entry says.
    #[error(
        "cache entry {entry}: the layer to push is {media_type}, not of the entry's encoding \
         {encoding}"
    )]
    Encoding {
        /// The entry's reference.
        entry: String,
        /// The layer's media type, cut to an excerpt.
        media_type: String,
        /// The entry's encoding.
        encoding: Encoding,
    },
    /// The entry's tagged reference or one of its documents could not be
    /// made.
    #[error("cache entry {entry}: {source}")]
    Document {
        /// The entry's reference.
        entry: String,
        /// Why.
        #[source]
        source: rnk_oci::Error,
    },
}

/// Why an entry found under its tag was refused: it does not have the form
/// of [ADR-0113]'s entries, or names another key, version or encoding.
///
/// [ADR-0113]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0113-the-cache-key-holds-buildkits-fields-and-a-hit-is-looked-up-with-its-encoding.md
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// Under the tag is not an OCI image manifest: an index, a Docker
    /// manifest.
    NotAnImageManifest {
        /// Its media type, cut to an excerpt; empty when it names none.
        media_type: String,
    },
    /// The manifest or its config does not parse.
    Malformed {
        /// `manifest` or `config`.
        document: &'static str,
        /// Why, cut to an excerpt.
        reason: String,
    },
    /// The manifest does not hold exactly one layer.
    Layers {
        /// How many it holds.
        count: usize,
    },
    /// The layer's media type is not the entry encoding's.
    LayerType {
        /// The layer's media type, cut to an excerpt; empty for
        /// `"mediaType": ""` (a missing one is [`Refusal::Malformed`]).
        media_type: String,
    },
    /// The config's media type is not an OCI image config.
    ConfigType {
        /// The config's media type, cut to an excerpt; empty for
        /// `"mediaType": ""` (a missing one is [`Refusal::Malformed`]).
        media_type: String,
    },
    /// The manifest is an artifact or refers to another manifest.
    NotAnImage,
    /// An annotation is missing, or names another key, version or
    /// encoding.
    Annotation {
        /// The annotation's name.
        name: &'static str,
    },
    /// The config's `rootfs` does not name exactly one layer.
    DiffIds,
    /// The config's `created` is missing or not an RFC 3339 time.
    Created {
        /// Why: `missing`, or the parser's message, which quotes the value
        /// as an excerpt.
        reason: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnImageManifest { media_type } if media_type.is_empty() => {
                f.write_str("a document with no media type, not an OCI image manifest")
            }
            Self::NotAnImageManifest { media_type } => {
                write!(f, "a {media_type}, not an OCI image manifest")
            }
            Self::Malformed { document, reason } => write!(f, "its {document}: {reason}"),
            Self::Layers { count } => write!(f, "{count} layers, not one"),
            Self::LayerType { media_type } => {
                write!(f, "a layer {}, not of its encoding", typed(media_type))
            }
            Self::ConfigType { media_type } => {
                write!(f, "a config {}, not an OCI image config", typed(media_type))
            }
            Self::NotAnImage => f.write_str("an artifact or a referrer, not an image"),
            Self::Annotation { name } => {
                write!(f, "annotation {name} missing or naming another entry")
            }
            Self::DiffIds => f.write_str("its config does not name exactly one layer"),
            Self::Created { reason } => write!(f, "its config's created time: {reason}"),
        }
    }
}

/// `of type <media_type>`, or `with no media type` when it is empty.
fn typed(media_type: &str) -> String {
    if media_type.is_empty() {
        "with no media type".to_owned()
    } else {
        format!("of type {media_type}")
    }
}

/// Longest excerpt of an entry's text kept in a message, in characters.
const EXCERPT_CHARS: usize = 80;

/// The first [`EXCERPT_CHARS`] characters of `text` on one line, with `…`
/// when cut.
pub(crate) fn excerpt(text: &str) -> String {
    let mut out: String = text
        .chars()
        .take(EXCERPT_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.chars().nth(EXCERPT_CHARS).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_a_missing_media_type_in_words() {
        let none = String::new;
        let shown = [
            Refusal::NotAnImageManifest { media_type: none() },
            Refusal::LayerType { media_type: none() },
            Refusal::ConfigType { media_type: none() },
        ]
        .map(|r| r.to_string());
        assert_eq!(
            shown,
            [
                "a document with no media type, not an OCI image manifest",
                "a layer with no media type, not of its encoding",
                "a config with no media type, not an OCI image config",
            ]
        );
        let zstd = Refusal::LayerType {
            media_type: "application/vnd.oci.image.layer.v1.tar+zstd".to_owned(),
        };
        assert_eq!(
            zstd.to_string(),
            "a layer of type application/vnd.oci.image.layer.v1.tar+zstd, not of its encoding"
        );
    }

    #[test]
    fn excerpts_are_short_single_lines() {
        assert_eq!(excerpt("a\nb"), "a b");
        let long = excerpt(&"é".repeat(500));
        assert_eq!(long.chars().count(), EXCERPT_CHARS + 1);
        assert!(long.ends_with('…'));
        assert_eq!(excerpt(&"x".repeat(EXCERPT_CHARS)).chars().count(), 80);
    }
}
