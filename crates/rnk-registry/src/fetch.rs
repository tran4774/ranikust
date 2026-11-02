//! The pulls `rnk-registry` sends itself, so every body it reads is bounded
//! while it arrives (ADR-0079): manifests up to the `manifest` limit, error
//! bodies up to the `registry-body` limit, blobs up to their descriptor's
//! size. `oci-client` collects whole bodies with `Response::bytes()`
//! before any check, so a registry could make it hold any amount.
//!
//! Status handling and digest checks follow `oci-client`'s own
//! (`oci-client-0.18.0 src/client.rs:2214-2245`, `src/digest.rs:122-160`),
//! so errors read as they did when `oci-client` sent these requests.

use bytes::{Bytes, BytesMut};
use oci_client::errors::{DigestError, OciDistributionError, OciEnvelope};
use sha2::Digest as _;

/// The `Docker-Content-Digest` header.
pub(crate) const DIGEST_HEADER: &str = "docker-content-digest";

/// Why a bounded read stopped.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// More than the limit arrived, or `Content-Length` announced more.
    TooLarge,
    /// The connection failed or timed out.
    Request(reqwest::Error),
}

/// `response`'s whole body, refusing it once it holds more than `max`
/// bytes: at once when `Content-Length` announces more, otherwise as soon
/// as the bytes that arrived go past it. Memory stays at `max` plus one
/// chunk.
pub(crate) async fn capped(mut response: reqwest::Response, max: u64) -> Result<Bytes, ReadError> {
    if response.content_length().is_some_and(|n| n > max) {
        return Err(ReadError::TooLarge);
    }
    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await.map_err(ReadError::Request)? {
        if len(&body).saturating_add(len(&chunk)) > max {
            return Err(ReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

/// At most `max` bytes of an error response's body; the rest is never
/// read. Only an excerpt of it ever reaches a message.
pub(crate) async fn error_body(mut response: reqwest::Response, max: u64) -> Bytes {
    let mut body = BytesMut::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = usize::try_from(max.saturating_sub(len(&body))).unwrap_or(usize::MAX);
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if len(&body) >= max {
            break;
        }
    }
    body.freeze()
}

fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// The error for a response whose status is not `200 OK`, as `oci-client`
/// builds it: `401` unauthorized, a distribution error envelope for other
/// client errors when the body holds one, else the status and the body.
pub(crate) fn status_error(
    status: reqwest::StatusCode,
    body: &[u8],
    url: &str,
) -> OciDistributionError {
    match status {
        reqwest::StatusCode::UNAUTHORIZED => OciDistributionError::UnauthorizedError {
            url: url.to_owned(),
        },
        s if s.is_success() => OciDistributionError::SpecViolationError(format!(
            "Expected HTTP Status {}, got {s} instead",
            reqwest::StatusCode::OK
        )),
        s if s.is_client_error() => match serde_json::from_slice::<OciEnvelope>(body) {
            Ok(envelope) => OciDistributionError::RegistryError {
                envelope,
                url: url.to_owned(),
            },
            Err(_) => OciDistributionError::ServerError {
                code: s.as_u16(),
                url: url.to_owned(),
                message: String::from_utf8_lossy(body).into_owned(),
            },
        },
        s => OciDistributionError::ServerError {
            code: s.as_u16(),
            url: url.to_owned(),
            message: String::from_utf8_lossy(body).into_owned(),
        },
    }
}

/// The `Docker-Content-Digest` of `response`, if it sent a non-empty one.
pub(crate) fn digest_header(
    headers: &reqwest::header::HeaderMap,
) -> Result<Option<String>, OciDistributionError> {
    match headers.get(DIGEST_HEADER) {
        None => Ok(None),
        Some(value) => {
            let text = value
                .to_str()
                .map_err(|e| OciDistributionError::DigestError(DigestError::InvalidHeader(e)))?;
            Ok((!text.is_empty()).then(|| text.to_owned()))
        }
    }
}

/// A running hash for one `<algorithm>:<hex>` digest.
pub(crate) enum Digester {
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
}

impl Digester {
    /// A hasher for `digest`'s algorithm.
    pub(crate) fn new(digest: &str) -> Result<Self, DigestError> {
        let (algorithm, _) = digest
            .split_once(':')
            .ok_or(DigestError::MissingAlgorithm)?;
        match algorithm {
            "sha256" => Ok(Self::Sha256(sha2::Sha256::new())),
            "sha384" => Ok(Self::Sha384(sha2::Sha384::new())),
            "sha512" => Ok(Self::Sha512(sha2::Sha512::new())),
            other => Err(DigestError::UnsupportedAlgorithm(other.to_owned())),
        }
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(d) => d.update(data),
            Self::Sha384(d) => d.update(data),
            Self::Sha512(d) => d.update(data),
        }
    }

    /// The digest of what was hashed, `<algorithm>:<hex>`.
    pub(crate) fn finish(self) -> String {
        match self {
            Self::Sha256(d) => format!("sha256:{}", hex(&d.finalize())),
            Self::Sha384(d) => format!("sha384:{}", hex(&d.finalize())),
            Self::Sha512(d) => format!("sha512:{}", hex(&d.finalize())),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Checks `body` against `expected` and returns its digest.
fn check(body: &[u8], expected: &str) -> Result<String, DigestError> {
    let mut digester = Digester::new(expected)?;
    digester.update(body);
    let actual = digester.finish();
    if actual == expected {
        Ok(actual)
    } else {
        Err(DigestError::VerificationError {
            expected: expected.to_owned(),
            actual,
        })
    }
}

/// The digest of a manifest `body`, checked against the registry's
/// `Docker-Content-Digest` (`header`) and the reference's digest, as
/// `oci-client`'s `validate_digest`: both must match when both are given,
/// the header's is the one returned, and without either the body's sha256.
pub(crate) fn manifest_digest(
    body: &[u8],
    header: Option<&str>,
    reference: Option<&str>,
) -> Result<String, DigestError> {
    match (header, reference) {
        (Some(header), Some(reference)) if header == reference => check(body, header),
        (Some(header), Some(reference)) => {
            check(body, reference)?;
            check(body, header)
        }
        (Some(header), None) => check(body, header),
        (None, Some(reference)) => check(body, reference),
        (None, None) => {
            let mut digester = Digester::Sha256(sha2::Sha256::new());
            digester.update(body);
            Ok(digester.finish())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_digests_are_checked_as_oci_client_checks_them() {
        let body = b"hello world";
        let mut d = Digester::Sha256(sha2::Sha256::new());
        d.update(body);
        let sha256 = d.finish();
        let mut d = Digester::Sha384(sha2::Sha384::new());
        d.update(body);
        let sha384 = d.finish();

        assert_eq!(manifest_digest(body, None, None).unwrap(), sha256);
        assert_eq!(manifest_digest(body, Some(&sha256), None).unwrap(), sha256);
        assert_eq!(manifest_digest(body, None, Some(&sha256)).unwrap(), sha256);
        // The header wins when both match, whatever their algorithms.
        assert_eq!(
            manifest_digest(body, Some(&sha256), Some(&sha384)).unwrap(),
            sha256
        );
        let wrong = format!("sha256:{}", "0".repeat(64));
        assert!(matches!(
            manifest_digest(body, Some(&wrong), None),
            Err(DigestError::VerificationError { .. })
        ));
        assert!(matches!(
            manifest_digest(body, Some(&sha256), Some(&wrong)),
            Err(DigestError::VerificationError { .. })
        ));
        assert!(matches!(
            manifest_digest(body, Some("md5:00"), None),
            Err(DigestError::UnsupportedAlgorithm(_))
        ));
    }

    #[test]
    fn status_errors_match_oci_client() {
        let url = "http://r/v2/a/manifests/1";
        assert!(matches!(
            status_error(reqwest::StatusCode::UNAUTHORIZED, b"", url),
            OciDistributionError::UnauthorizedError { .. }
        ));
        let envelope = br#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"x"}]}"#;
        assert!(matches!(
            status_error(reqwest::StatusCode::NOT_FOUND, envelope, url),
            OciDistributionError::RegistryError { .. }
        ));
        assert!(matches!(
            status_error(reqwest::StatusCode::NOT_FOUND, b"not json", url),
            OciDistributionError::ServerError { code: 404, .. }
        ));
        assert!(matches!(
            status_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, b"", url),
            OciDistributionError::ServerError { code: 503, .. }
        ));
        assert!(matches!(
            status_error(reqwest::StatusCode::ACCEPTED, b"", url),
            OciDistributionError::SpecViolationError(_)
        ));
    }
}
