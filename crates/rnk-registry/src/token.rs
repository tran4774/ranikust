//! The identity-token exchange `oci-client` lacks.
//!
//! A docker `identitytoken` is an OAuth2 refresh token. It is traded for a
//! short-lived access token per repository and operation at the realm the
//! registry names in its `WWW-Authenticate` challenge, as the distribution
//! token specification describes and go-containerregistry does
//! (`kaniko@44aa7ddc0 vendor/github.com/google/go-containerregistry/pkg/v1/remote/transport/bearer.go:442-497`):
//! a form `POST` with `grant_type=refresh_token`.

use std::time::{Duration, Instant};

use serde::Deserialize;

/// A `Bearer` challenge from `WWW-Authenticate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Challenge {
    /// The token endpoint.
    pub(crate) realm: String,
    /// The `service` to name in token requests.
    pub(crate) service: Option<String>,
}

/// The first `Bearer` challenge in a `WWW-Authenticate` header value.
pub(crate) fn parse_challenge(header: &str) -> Option<Challenge> {
    http_auth::parser::ChallengeParser::new(header)
        .filter_map(Result::ok)
        .find(|c| c.scheme.eq_ignore_ascii_case("bearer"))
        .and_then(|c| {
            let param = |name: &str| {
                c.params
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.to_unescaped())
            };
            Some(Challenge {
                realm: param("realm")?,
                service: param("service"),
            })
        })
}

/// An access token and when to stop using it.
pub(crate) struct AccessToken {
    pub(crate) token: String,
    pub(crate) refresh_after: Instant,
}

/// How long a token is used when the response gives no `expires_in`: the
/// distribution token specification's default.
const DEFAULT_EXPIRES_IN: u64 = 60;
/// Tokens are refreshed this long before they expire.
const MARGIN: Duration = Duration::from_secs(10);
/// Upper bound on a token response body.
const MAX_BODY: usize = 1 << 20;
/// The `client_id` sent with token requests.
const CLIENT_ID: &str = "ranikust";

/// Trades `refresh_token` for an access token with `scope`
/// (`repository:<name>:pull` or `repository:<name>:pull,push`).
///
/// The error is a short reason without the token or the response body,
/// which may echo it.
pub(crate) async fn exchange(
    http: &reqwest::Client,
    challenge: &Challenge,
    refresh_token: &str,
    scope: &str,
) -> Result<AccessToken, ExchangeError> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
        ("scope", scope),
    ];
    if let Some(service) = &challenge.service {
        form.push(("service", service));
    }
    let started = Instant::now();
    let mut response = http
        .post(&challenge.realm)
        .form(&form)
        .send()
        .await
        .map_err(ExchangeError::Request)?;
    let status = response.status();
    if !status.is_success() {
        return Err(ExchangeError::Status(status.as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ExchangeError::Request)? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(ExchangeError::Invalid("token response larger than 1 MiB"));
        }
        body.extend_from_slice(&chunk);
    }
    let parsed: TokenResponse = serde_json::from_slice(&body)
        .map_err(|_| ExchangeError::Invalid("token response is not JSON"))?;
    let token = parsed
        .access_token
        .or(parsed.token)
        .filter(|t| !t.is_empty())
        .ok_or(ExchangeError::Invalid("no token in the token response"))?;
    // `oci-client` unwraps the header it builds from the token; a token that
    // is not a valid header value would abort the process.
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ExchangeError::Invalid(
            "the token has characters a header cannot carry",
        ));
    }
    let lifetime = Duration::from_secs(parsed.expires_in.unwrap_or(DEFAULT_EXPIRES_IN));
    Ok(AccessToken {
        token,
        refresh_after: started + lifetime.saturating_sub(MARGIN),
    })
}

/// Why an exchange failed; never holds the token.
#[derive(Debug)]
pub(crate) enum ExchangeError {
    /// The request failed before a status arrived.
    Request(reqwest::Error),
    /// The realm answered with this status.
    Status(u16),
    /// The response was not a usable token.
    Invalid(&'static str),
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_challenges_are_parsed() {
        let header = r#"Basic realm="x", Bearer realm="https://auth.example/token",service="registry.example",scope="repository:a:pull""#;
        assert_eq!(
            parse_challenge(header),
            Some(Challenge {
                realm: "https://auth.example/token".to_owned(),
                service: Some("registry.example".to_owned()),
            })
        );
        assert_eq!(parse_challenge(r#"Basic realm="registry""#), None);
        assert_eq!(parse_challenge(r#"Bearer service="no-realm""#), None);
    }
}
