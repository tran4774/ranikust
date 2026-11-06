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
        /// Whether the registry refused access: a 401 or 403, or an
        /// `UNAUTHORIZED` or `DENIED` code in its error envelope. Decided
        /// from the status and codes, never from `message`, which holds only
        /// an excerpt of what the registry said.
        refused: bool,
        /// Whether the registry answered a manifest or blob `GET` with 404:
        /// it holds no such manifest, blob or repository. Decided from the
        /// status only; never retried.
        not_found: bool,
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

    /// A response, or the size a manifest declares for a blob, is larger
    /// than one of the `--limit` limits (ADR-0079); never retried.
    #[error(
        "{what} {target} from {host} is larger than {} (limit \"{limit}\"); \
         raise it with --limit {limit}=<size>",
        size_text(*max)
    )]
    TooLarge {
        /// What was read: `manifest`, `image config`, `token response`.
        what: &'static str,
        /// The registry host.
        host: String,
        /// The manifest reference, blob digest or repository.
        target: String,
        /// The limit's name, as `--limit` spells it.
        limit: &'static str,
        /// The limit, in bytes.
        max: u64,
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
    /// This error, never retried: what it interrupted cannot restart.
    pub(crate) fn not_retryable(mut self) -> Self {
        if let Self::Registry { retryable, .. } = &mut self {
            *retryable = false;
        }
        self
    }

    /// Whether the registry answered 404 to a manifest or blob `GET`: it
    /// holds no such manifest, blob or repository.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Registry {
                not_found: true,
                ..
            }
        )
    }

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
        let (message, retryable, refused) = match err {
            OciDistributionError::DigestError(DigestError::VerificationError {
                expected,
                actual,
            }) => {
                // `expected` may be a registry's `Docker-Content-Digest`, of
                // any length: shown whole only when it is a digest.
                return Error::DigestMismatch {
                    host: self.host.to_owned(),
                    target: self.target.to_owned(),
                    expected: shown_digest(&expected),
                    actual: shown_digest(&actual),
                };
            }
            OciDistributionError::AuthenticationFailure(reason) => {
                return Error::Auth {
                    host: self.host.to_owned(),
                    reason: excerpt(&reason),
                };
            }
            OciDistributionError::UnauthorizedError { .. } => {
                ("401 unauthorized".to_owned(), false, true)
            }
            OciDistributionError::ServerError { code, message, .. } => (
                format!("HTTP {code}: {}", excerpt(&message)),
                retryable_status(code),
                refusal_status(code),
            ),
            OciDistributionError::RegistryError { envelope, .. } => {
                let retryable = envelope
                    .errors
                    .iter()
                    .any(|e| matches!(e.code, OciErrorCode::Toomanyrequests));
                let refused = envelope
                    .errors
                    .iter()
                    .any(|e| matches!(e.code, OciErrorCode::Unauthorized | OciErrorCode::Denied));
                (envelope_text(&envelope.errors), retryable, refused)
            }
            OciDistributionError::RequestError(e) => {
                let retryable = e.is_connect() || e.is_timeout() || e.is_body() || e.is_request();
                let status = e.status().map(|s| s.as_u16());
                let retryable = retryable || status.is_some_and(retryable_status);
                let refused = status.is_some_and(refusal_status);
                (request_error_message(&e), retryable, refused)
            }
            OciDistributionError::IoError(e) => (format!("I/O: {e}"), false, false),
            other => (excerpt(&other.to_string()), false, false),
        };
        Error::Registry {
            operation: self.operation,
            host: self.host.to_owned(),
            target: self.target.to_owned(),
            message,
            retryable,
            refused,
            not_found: false,
        }
    }
}

/// The statuses go-containerregistry retries (`retry.rs`): 408, 429, 500,
/// 502, 503 and 504.
pub(crate) fn retryable_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
}

impl Op<'_> {
    /// [`Op::error`] for an answer whose status is known: a 401 or 403 is a
    /// refusal whatever its body said, and a 404 is "not found".
    pub(crate) fn error_for_status(&self, status: u16, err: OciDistributionError) -> Error {
        let mut e = self.error(err);
        if let Error::Registry {
            refused, not_found, ..
        } = &mut e
        {
            *refused |= refusal_status(status);
            *not_found = status == 404;
        }
        e
    }
}

/// Whether `code` is a registry refusing access: 401 or 403.
pub(crate) fn refusal_status(code: u16) -> bool {
    matches!(code, 401 | 403)
}

/// A registry that sent nothing for the `registry-idle` limit, `idle`:
/// retried, as any request that did not complete.
pub(crate) fn idle_error(ctx: &Op<'_>, idle: Option<std::time::Duration>) -> Error {
    let waited = idle.map_or_else(String::new, |d| format!(" for {} s", d.as_secs_f64()));
    Error::Registry {
        operation: ctx.operation,
        host: ctx.host.to_owned(),
        target: ctx.target.to_owned(),
        message: format!(
            "no data{waited} (limit \"registry-idle\"); raise it with \
             --limit registry-idle=<duration>"
        ),
        retryable: true,
        refused: false,
        not_found: false,
    }
}

/// `bytes` as a size `--limit` accepts: `4 MiB`, `512 KiB` or `100 bytes`.
pub(crate) fn size_text(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
    for (unit, name) in UNITS {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{} {name}", bytes / unit);
        }
    }
    format!("{bytes} bytes")
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

/// A registry's error envelope as one excerpt: its entries, `code: message`,
/// joined until the excerpt's 200 characters are used, then the number of
/// entries. A registry can send thousands of entries within the
/// `registry-body` limit; only the excerpt reaches a message.
fn envelope_text(errors: &[oci_client::errors::OciError]) -> String {
    let mut joined = String::new();
    for e in errors {
        if joined.chars().count() > EXCERPT_CHARS {
            break;
        }
        if !joined.is_empty() {
            joined.push_str("; ");
        }
        joined.push_str(&format!("{:?}: {}", e.code, excerpt(&e.message)));
    }
    let text = excerpt(&joined);
    if errors.len() > 1 {
        format!("{text} ({} errors)", errors.len())
    } else {
        text
    }
}

/// A digest as a message shows it: whole when it is `<algorithm>:<hex>` of
/// a length a digest has, else an excerpt (a registry's
/// `Docker-Content-Digest` header can be of any length).
fn shown_digest(text: &str) -> String {
    let well_formed = text.split_once(':').is_some_and(|(algorithm, hex)| {
        let len = match algorithm {
            "sha256" => 64,
            "sha384" => 96,
            "sha512" => 128,
            _ => return false,
        };
        hex.len() == len && hex.bytes().all(|b| b.is_ascii_hexdigit())
    });
    if well_formed {
        text.to_owned()
    } else {
        excerpt(text)
    }
}

/// How many characters of an untrusted message reach an error.
const EXCERPT_CHARS: usize = 200;

/// At most 200 characters of an untrusted message, on one line.
pub(crate) fn excerpt(text: &str) -> String {
    const MAX: usize = EXCERPT_CHARS;
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

    #[test]
    fn an_envelope_of_many_entries_is_one_excerpt_and_a_count() {
        let one = format!(r#"{{"code":"UNKNOWN","message":"{}"}}"#, "m".repeat(199));
        let many: Vec<oci_client::errors::OciError> =
            serde_json::from_str(&format!("[{}]", vec![one; 2000].join(","))).unwrap();
        let text = envelope_text(&many);
        assert!(text.chars().count() < 240, "{}", text.len());
        assert!(text.ends_with("(2000 errors)"), "{text}");
        let single = envelope_text(&many[..1]);
        assert!(single.contains(": mmm"), "{single}");
        assert!(!single.contains("errors)"), "{single}");
    }

    #[test]
    fn a_digest_is_shown_whole_only_when_well_formed() {
        let good = format!("sha256:{}", "a".repeat(64));
        assert_eq!(shown_digest(&good), good);
        let long = format!("sha256:{}", "e".repeat(300_000));
        assert_eq!(shown_digest(&long).chars().count(), 201);
        assert_eq!(shown_digest("md5:abc"), "md5:abc");
        let err = op().error(OciDistributionError::DigestError(
            DigestError::VerificationError {
                expected: long,
                actual: good,
            },
        ));
        assert!(err.to_string().len() < 400, "{}", err.to_string().len());
    }
}
