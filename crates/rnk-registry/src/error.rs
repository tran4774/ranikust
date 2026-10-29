//! The crate's error type.
//!
//! Messages name the registry host and the object (manifest or blob digest)
//! but never a credential: `oci-client` errors are reduced to their status
//! and registry message before they reach a caller.

use std::io;
use std::path::PathBuf;

use oci_client::errors::{DigestError, OciDistributionError, OciErrorCode};

/// Everything that can go wrong talking to a registry.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The trust store or the TLS crypto provider could not be set up.
    #[error("TLS set-up: {0}")]
    Tls(String),

    /// A docker `config.json` could not be read or parsed.
    #[error("registry credentials in {path}: {reason}")]
    Credentials {
        /// The file that was read.
        path: PathBuf,
        /// What was wrong with it.
        reason: String,
    },

    /// A reference could not be handed to the distribution client.
    #[error("reference {reference}: {reason}")]
    Reference {
        /// The reference, as written.
        reference: String,
        /// Why it was rejected.
        reason: String,
    },

    /// Authentication with a registry failed.
    #[error("authentication with {host} failed: {reason}")]
    Auth {
        /// The registry host.
        host: String,
        /// The registry's answer or the local reason.
        reason: String,
    },

    /// The registry answered with an error or could not be reached.
    #[error("{operation} {target} on {host}: {message}")]
    Registry {
        /// What was attempted: `pull manifest`, `push blob`, ...
        operation: &'static str,
        /// The registry host.
        host: String,
        /// The manifest reference or blob digest.
        target: String,
        /// The status and the registry's message, without credentials.
        message: String,
        /// Whether trying again may succeed (network errors, 5xx, 429).
        retryable: bool,
    },

    /// Received content does not match the digest it was asked for.
    #[error("{target} from {host}: digest mismatch, expected {expected}, got {actual}")]
    DigestMismatch {
        /// The registry host.
        host: String,
        /// The manifest reference or blob digest.
        target: String,
        /// The digest asked for.
        expected: String,
        /// The digest of what arrived.
        actual: String,
    },

    /// A blob was longer or shorter than its descriptor says.
    #[error("blob {digest} from {host}: expected {expected} bytes, got {actual}")]
    SizeMismatch {
        /// The registry host.
        host: String,
        /// The blob digest.
        digest: String,
        /// The size in the descriptor.
        expected: u64,
        /// The bytes received, or at least this many when the stream was cut
        /// at the limit.
        actual: u64,
    },

    /// A manifest could not be understood, or an index has no manifest for
    /// the requested platform.
    #[error("manifest {target} from {host}: {reason}")]
    Manifest {
        /// The registry host.
        host: String,
        /// The manifest reference.
        target: String,
        /// What was wrong.
        reason: String,
    },

    /// Reading a blob to push, or writing a pulled one, failed locally.
    #[error("{what}: {source}")]
    Io {
        /// The local object being read or written.
        what: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl Error {
    /// Whether the failed operation may succeed if repeated.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Registry {
                retryable: true,
                ..
            }
        )
    }
}

/// Context for turning an `oci-client` error into an [`Error`].
pub(crate) struct Op<'a> {
    pub(crate) operation: &'static str,
    pub(crate) host: &'a str,
    pub(crate) target: &'a str,
}

impl Op<'_> {
    /// Converts an `oci-client` error, classifying it as retryable or not.
    pub(crate) fn error(&self, err: OciDistributionError) -> Error {
        let (message, retryable) = match err {
            OciDistributionError::DigestError(DigestError::VerificationError {
                expected,
                actual,
            }) => {
                return Error::DigestMismatch {
                    host: self.host.to_owned(),
                    target: self.target.to_owned(),
                    expected,
                    actual,
                };
            }
            OciDistributionError::AuthenticationFailure(reason) => {
                return Error::Auth {
                    host: self.host.to_owned(),
                    reason: excerpt(&reason),
                };
            }
            OciDistributionError::UnauthorizedError { .. } => {
                ("401 unauthorized".to_owned(), false)
            }
            OciDistributionError::ServerError { code, message, .. } => (
                format!("HTTP {code}: {}", excerpt(&message)),
                retryable_status(code),
            ),
            OciDistributionError::RegistryError { envelope, .. } => {
                let retryable = envelope
                    .errors
                    .iter()
                    .any(|e| matches!(e.code, OciErrorCode::Toomanyrequests));
                let codes: Vec<String> = envelope
                    .errors
                    .iter()
                    .map(|e| format!("{:?}: {}", e.code, excerpt(&e.message)))
                    .collect();
                (codes.join("; "), retryable)
            }
            OciDistributionError::RequestError(e) => {
                let retryable = e.is_connect() || e.is_timeout() || e.is_body() || e.is_request();
                let status = e.status().map(|s| s.as_u16());
                let retryable = retryable || status.is_some_and(retryable_status);
                (request_error_message(&e), retryable)
            }
            OciDistributionError::IoError(e) => (format!("I/O: {e}"), false),
            other => (excerpt(&other.to_string()), false),
        };
        Error::Registry {
            operation: self.operation,
            host: self.host.to_owned(),
            target: self.target.to_owned(),
            message,
            retryable,
        }
    }
}

/// The statuses go-containerregistry retries (`retry.rs`): 408, 429, 500,
/// 502, 503 and 504.
pub(crate) fn retryable_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
}

/// A `reqwest` error without its URL, which may carry a pre-signed query.
fn request_error_message(e: &reqwest::Error) -> String {
    let kind = if e.is_connect() {
        "connection failed"
    } else if e.is_timeout() {
        "timed out"
    } else if e.is_body() || e.is_decode() {
        "response body failed"
    } else {
        "request failed"
    };
    // Not `e.to_string()`, which names the URL; the source chain names the
    // underlying I/O or TLS error.
    let mut message = kind.to_owned();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        message.push_str(": ");
        message.push_str(&excerpt(&s.to_string()));
        source = s.source();
    }
    message
}

/// At most 200 characters of an untrusted message, on one line.
pub(crate) fn excerpt(text: &str) -> String {
    const MAX: usize = 200;
    let single: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = single.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_owned();
    }
    let mut cut: String = trimmed.chars().take(MAX).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op() -> Op<'static> {
        Op {
            operation: "pull blob",
            host: "r.example",
            target: "sha256:00",
        }
    }

    #[test]
    fn server_errors_retry_only_when_transient() {
        for (code, retryable) in [
            (500, true),
            (503, true),
            (429, true),
            (408, true),
            (501, false),
            (404, false),
            (400, false),
        ] {
            let err = op().error(OciDistributionError::ServerError {
                code,
                url: "https://r.example/v2/a/blobs/sha256:00".to_owned(),
                message: "nope".to_owned(),
            });
            assert_eq!(err.is_retryable(), retryable, "status {code}");
        }
    }

    #[test]
    fn digest_errors_map_to_mismatch() {
        let err = op().error(OciDistributionError::DigestError(
            DigestError::VerificationError {
                expected: "sha256:aa".to_owned(),
                actual: "sha256:bb".to_owned(),
            },
        ));
        assert!(matches!(err, Error::DigestMismatch { .. }));
        assert!(!err.is_retryable());
    }

    #[test]
    fn excerpts_are_bounded_single_lines() {
        let long = "x".repeat(500);
        assert_eq!(excerpt(&long).chars().count(), 201);
        assert_eq!(excerpt("a\nb\r\tc"), "a b  c");
    }
}
