//! An in-process fake registry on loopback, plain HTTP.
//!
//! Hand-written on `hyper` (already in the tree through `reqwest`) rather
//! than a mock-server crate: the tests need a registry's state (blobs,
//! manifests, upload sessions) and the token flow, which a request-matching
//! mock would have to script request by request.
//!
//! Serves the distribution API subset `rnk-registry` uses, three
//! authentication modes, and injected faults: failing statuses, corrupt or
//! oversized blobs, refused mounts.

#![allow(dead_code, reason = "shared by two test binaries; each uses a part")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use sha2::Digest as _;

/// How the fake registry authenticates.
#[derive(Clone, Debug, Default)]
pub(crate) enum Auth {
    /// Open: `/v2/` answers 200.
    #[default]
    Open,
    /// HTTP basic authentication.
    Basic { user: String, password: String },
    /// Bearer tokens from `/token`: anonymous pulls, `user`/`password` for
    /// pushes and pulls, or the refresh token through `POST /token`.
    Bearer {
        user: String,
        password: String,
        refresh_token: String,
        anonymous_pull: bool,
    },
}

/// A failure injected into the next matching requests.
#[derive(Clone, Debug)]
pub(crate) struct Fault {
    pub(crate) method: Method,
    pub(crate) path_contains: String,
    pub(crate) status: u16,
    pub(crate) remaining: u32,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) auth: Auth,
    /// Blobs by digest; one store shared by every repository, which the
    /// tests do not need to tell apart.
    pub(crate) blobs: HashMap<String, Bytes>,
    /// Repositories that hold each blob.
    pub(crate) blob_repos: HashMap<String, Vec<String>>,
    /// `(repository, tag or digest)` → `(content type, bytes)`.
    pub(crate) manifests: HashMap<(String, String), (String, Bytes)>,
    uploads: HashMap<String, String>,
    next_upload: u32,
    pub(crate) faults: Vec<Fault>,
    /// Serve these bytes for a blob digest instead of the stored ones.
    pub(crate) tampered: HashMap<String, Bytes>,
    /// Answer mounts with `202` and an upload session.
    pub(crate) refuse_mounts: bool,
    /// `METHOD path?query` of every request, in order.
    pub(crate) log: Vec<String>,
    /// Bodies of `POST /token` requests.
    pub(crate) token_forms: Vec<String>,
}

pub(crate) struct FakeRegistry {
    pub(crate) addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl FakeRegistry {
    pub(crate) async fn start(auth: Auth) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            auth,
            ..State::default()
        }));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&shared);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |req| {
                        let state = Arc::clone(&state);
                        async move { Ok::<_, std::convert::Infallible>(handle(&state, addr, req).await) }
                    });
                    // Connections reset by a client probing with TLS end
                    // here with an error; that is expected.
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self { addr, state }
    }

    /// `127.0.0.1:<port>`, the registry part of references.
    pub(crate) fn host(&self) -> String {
        self.addr.to_string()
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Stores a blob in `repo` and returns its digest.
    pub(crate) fn add_blob(&self, repo: &str, content: &[u8]) -> String {
        let digest = sha256(content);
        let mut state = self.state();
        state
            .blobs
            .insert(digest.clone(), Bytes::copy_from_slice(content));
        state
            .blob_repos
            .entry(digest.clone())
            .or_default()
            .push(repo.to_owned());
        digest
    }

    /// Stores a manifest under `tag` and under its digest; returns the
    /// digest.
    pub(crate) fn add_manifest(
        &self,
        repo: &str,
        tag: &str,
        content_type: &str,
        content: &[u8],
    ) -> String {
        let digest = sha256(content);
        let mut state = self.state();
        let entry = (content_type.to_owned(), Bytes::copy_from_slice(content));
        state
            .manifests
            .insert((repo.to_owned(), tag.to_owned()), entry.clone());
        state
            .manifests
            .insert((repo.to_owned(), digest.clone()), entry);
        digest
    }

    /// Requests whose `METHOD path` starts with `prefix`.
    pub(crate) fn count(&self, prefix: &str) -> usize {
        self.state()
            .log
            .iter()
            .filter(|l| l.starts_with(prefix))
            .count()
    }
}

pub(crate) fn sha256(content: &[u8]) -> String {
    let hash = sha2::Sha256::digest(content);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

type Reply = Response<Full<Bytes>>;

fn reply(status: u16, body: impl Into<Bytes>) -> Reply {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    response
}

fn with_header(mut response: Reply, name: header::HeaderName, value: &str) -> Reply {
    response
        .headers_mut()
        .insert(name, HeaderValue::from_str(value).unwrap());
    response
}

fn error_envelope(status: u16, code: &str) -> Reply {
    let body = format!(r#"{{"errors":[{{"code":"{code}","message":"{code}"}}]}}"#);
    with_header(
        reply(status, body),
        header::CONTENT_TYPE,
        "application/json",
    )
}

async fn handle(state: &Mutex<State>, addr: SocketAddr, req: Request<Incoming>) -> Reply {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let authorization = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = req
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();

    let mut state = state.lock().unwrap();
    state.log.push(format!("{method} {path}?{query}"));
    let fault = state
        .faults
        .iter_mut()
        .find(|f| f.method == method && path.contains(&f.path_contains) && f.remaining > 0);
    if let Some(fault) = fault {
        fault.remaining -= 1;
        return reply(fault.status, "injected failure");
    }

    if path == "/token" {
        return token(&mut state, &method, &query, authorization.as_deref(), &body);
    }
    if path == "/v2/" {
        return match refusal(&state, addr, authorization.as_deref(), None) {
            None => reply(200, "{}"),
            Some(denied) => denied,
        };
    }
    let Some(rest) = path.strip_prefix("/v2/") else {
        return reply(404, "");
    };
    let (repo, kind, target) = if let Some((repo, target)) = rest.split_once("/blobs/uploads/") {
        (repo, "uploads", target)
    } else if let Some((repo, target)) = rest.split_once("/blobs/") {
        (repo, "blobs", target)
    } else if let Some((repo, target)) = rest.split_once("/manifests/") {
        (repo, "manifests", target)
    } else {
        return reply(404, "");
    };
    let push = !matches!(method, Method::GET | Method::HEAD);
    if let Some(denied) = refusal(&state, addr, authorization.as_deref(), Some((repo, push))) {
        return denied;
    }
    let repo = repo.to_owned();
    let target = target.to_owned();
    match (kind, method) {
        ("blobs", Method::HEAD) => match state.blobs.get(&target) {
            Some(blob) => with_header(
                reply(200, ""),
                header::CONTENT_LENGTH,
                &blob.len().to_string(),
            ),
            None => reply(404, ""),
        },
        ("blobs", Method::GET) => {
            let served = state
                .tampered
                .get(&target)
                .or_else(|| state.blobs.get(&target))
                .cloned();
            match served {
                Some(blob) => reply(200, blob),
                None => error_envelope(404, "BLOB_UNKNOWN"),
            }
        }
        ("uploads", Method::POST) => {
            let params = params(&query);
            if let (Some(digest), Some(from)) = (params.get("mount"), params.get("from")) {
                let held = state
                    .blob_repos
                    .get(digest)
                    .is_some_and(|r| r.contains(from));
                if held && !state.refuse_mounts {
                    state
                        .blob_repos
                        .entry(digest.clone())
                        .or_default()
                        .push(repo.clone());
                    let location = format!("/v2/{repo}/blobs/{digest}");
                    return with_header(reply(201, ""), header::LOCATION, &location);
                }
            }
            state.next_upload += 1;
            let id = format!("u{}", state.next_upload);
            state.uploads.insert(id.clone(), repo.clone());
            let location = format!("/v2/{repo}/blobs/uploads/{id}");
            with_header(reply(202, ""), header::LOCATION, &location)
        }
        ("uploads", Method::PUT) => {
            let Some(upload_repo) = state.uploads.remove(&target) else {
                return error_envelope(404, "BLOB_UPLOAD_UNKNOWN");
            };
            let params = params(&query);
            let Some(digest) = params.get("digest").cloned() else {
                return error_envelope(400, "DIGEST_INVALID");
            };
            if sha256(&body) != digest {
                return error_envelope(400, "DIGEST_INVALID");
            }
            state.blobs.insert(digest.clone(), body);
            state
                .blob_repos
                .entry(digest.clone())
                .or_default()
                .push(upload_repo);
            let location = format!("/v2/{repo}/blobs/{digest}");
            with_header(reply(201, ""), header::LOCATION, &location)
        }
        ("manifests", Method::GET | Method::HEAD) => {
            match state.manifests.get(&(repo.clone(), target.clone())) {
                Some((content_type, bytes)) => {
                    let digest = sha256(bytes);
                    let response = with_header(
                        reply(200, bytes.clone()),
                        header::CONTENT_TYPE,
                        content_type,
                    );
                    with_header(
                        response,
                        header::HeaderName::from_static("docker-content-digest"),
                        &digest,
                    )
                }
                None => error_envelope(404, "MANIFEST_UNKNOWN"),
            }
        }
        ("manifests", Method::PUT) => {
            let digest = sha256(&body);
            let content_type = "application/vnd.oci.image.manifest.v1+json".to_owned();
            state
                .manifests
                .insert((repo.clone(), target), (content_type.clone(), body.clone()));
            state
                .manifests
                .insert((repo.clone(), digest.clone()), (content_type, body));
            let location = format!("/v2/{repo}/manifests/{digest}");
            let response = with_header(reply(201, ""), header::LOCATION, &location);
            with_header(
                response,
                header::HeaderName::from_static("docker-content-digest"),
                &digest,
            )
        }
        _ => reply(405, ""),
    }
}

/// The refusal of a request without authorization for `scope` (repository,
/// push), or for `/v2/` when `None`: a `401` with the challenge.
fn refusal(
    state: &State,
    addr: SocketAddr,
    authorization: Option<&str>,
    scope: Option<(&str, bool)>,
) -> Option<Reply> {
    match &state.auth {
        Auth::Open => None,
        Auth::Basic { user, password } => {
            if authorization == Some(basic_header(user, password).as_str()) {
                None
            } else {
                Some(with_header(
                    reply(401, ""),
                    header::WWW_AUTHENTICATE,
                    r#"Basic realm="fake""#,
                ))
            }
        }
        Auth::Bearer { .. } => {
            let granted = authorization
                .and_then(|a| a.strip_prefix("Bearer "))
                .and_then(|t| t.strip_prefix("tok,"))
                .is_some_and(|t| match scope {
                    None => true,
                    Some((repo, push)) => {
                        let wanted = if push { "pull,push" } else { "pull" };
                        t == format!("{repo}:{wanted}")
                            || (!push && t == format!("{repo}:pull,push"))
                    }
                });
            if granted {
                None
            } else {
                let challenge = format!(r#"Bearer realm="http://{addr}/token",service="fake""#);
                Some(with_header(
                    reply(401, ""),
                    header::WWW_AUTHENTICATE,
                    &challenge,
                ))
            }
        }
    }
}

/// The token endpoint: `GET` with optional basic credentials, `POST` with a
/// refresh token. Tokens are `tok,<repository>:<actions>`.
fn token(
    state: &mut State,
    method: &Method,
    query: &str,
    authorization: Option<&str>,
    body: &[u8],
) -> Reply {
    let Auth::Bearer {
        user,
        password,
        refresh_token,
        anonymous_pull,
    } = state.auth.clone()
    else {
        return reply(404, "");
    };
    let form = if *method == Method::POST {
        let text = String::from_utf8_lossy(body).into_owned();
        state.token_forms.push(text.clone());
        params(&text)
    } else {
        params(query)
    };
    let Some(scope) = form.get("scope") else {
        return reply(400, "no scope");
    };
    let Some(repo_actions) = scope.strip_prefix("repository:") else {
        return reply(400, "bad scope");
    };
    let authorized = if *method == Method::POST {
        form.get("grant_type").map(String::as_str) == Some("refresh_token")
            && form.get("refresh_token") == Some(&refresh_token)
    } else {
        authorization == Some(basic_header(&user, &password).as_str())
            || (anonymous_pull && repo_actions.ends_with(":pull"))
    };
    if !authorized {
        return reply(401, r#"{"details":"denied"}"#);
    }
    let token = format!("tok,{repo_actions}");
    let field = if *method == Method::POST {
        "access_token"
    } else {
        "token"
    };
    reply(200, format!(r#"{{"{field}":"{token}","expires_in":300}}"#))
}

fn basic_header(user: &str, password: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Basic {encoded}")
}

/// A decoded `application/x-www-form-urlencoded` string.
fn params(text: &str) -> HashMap<String, String> {
    text.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
