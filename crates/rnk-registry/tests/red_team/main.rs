//! Red team (T11, T15, ADR-0070, ADR-0079): a hostile registry scripted
//! byte for byte, on loopback, plain HTTP (T0).
//!
//! The fake registry of the other test binaries speaks the protocol
//! well; this one answers each request with raw bytes a test chooses, so
//! that headers and bodies can be what no well-behaved server sends: an
//! error envelope with thousands of entries, a `Docker-Content-Digest` of
//! hundreds of kilobytes, a token that would break a header, redirects
//! that leave the registry's origin. Each test says which promise it
//! checks.

use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use rnk_oci::{Descriptor, Digest, MediaType, Reference};
use rnk_registry::{Client, ClientOptions, Credentials, RetryPolicy, TrustStore};
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A request as the scripted server saw it: the request line and the
/// header lines, lower-cased names.
#[derive(Clone, Debug)]
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn path(&self) -> &str {
        self.line.split(' ').nth(1).unwrap_or("")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

type Script = dyn Fn(&Seen) -> Vec<u8> + Send + Sync;

/// A server that answers every request with the bytes `script` makes of
/// it, then closes the connection, and records what it saw.
struct Scripted {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Scripted {
    async fn start(script: impl Fn(&Seen) -> Vec<u8> + Send + Sync + 'static) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<Script> = Arc::new(script);
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let script = Arc::clone(&script);
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                        // A TLS handshake (HTTPS tried first for a listed
                        // registry): plain HTTP only, closed at once.
                        if buf.first() == Some(&0x16) {
                            return;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let mut lines = head.split("\r\n");
                    let line = lines.next().unwrap_or("").to_owned();
                    let headers = lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                        .collect();
                    let seen = Seen { line, headers };
                    let answer = script(&seen);
                    log.lock().unwrap().push(seen);
                    let _ = sock.write_all(&answer).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self { addr, seen }
    }

    fn host(&self) -> String {
        self.addr.to_string()
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// An HTTP/1.1 response with `Connection: close`, its `Content-Length`
/// counted unless `extra` names one.
fn response(status: &str, extra: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write!(out, "HTTP/1.1 {status}\r\nConnection: close\r\n").unwrap();
    let mut has_length = false;
    for (k, v) in extra {
        has_length |= k.eq_ignore_ascii_case("content-length");
        write!(out, "{k}: {v}\r\n").unwrap();
    }
    if !has_length {
        write!(out, "Content-Length: {}\r\n", body.len()).unwrap();
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn options(hosts: &[String]) -> ClientOptions {
    let mut options = ClientOptions::default();
    options.insecure = hosts.iter().cloned().collect();
    options.http_credentials = options.insecure.clone();
    options.retry = RetryPolicy {
        attempts: 1,
        initial_delay: Duration::from_millis(1),
        factor: 2,
    };
    options.read_timeout = Some(Duration::from_secs(10));
    options
}

/// Basic credentials `u`/`dummy-password` for `host`, from a `config.json`.
fn basic(host: &str) -> Credentials {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let auth = base64::engine::general_purpose::STANDARD.encode("u:dummy-password");
    std::fs::write(
        &path,
        format!(r#"{{"auths":{{"{host}":{{"auth":"{auth}"}}}}}}"#),
    )
    .unwrap();
    Credentials::load(&path).unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    let mut out = String::from("sha256:");
    for b in sha2::Sha256::digest(bytes) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// What a client may put of a registry's text in one error: the
/// operation, the host and target, and an excerpt of 200 characters, with
/// room to spare.
const EXCERPT_BOUND: usize = 2048;

/// Red team (T15, threat model section 6, Q-447): a registry's error text
/// reaches a build's error only as an excerpt of 200 characters. Each
/// answer below is a registry's own text, far longer than that, in a
/// place where it reaches the error: an error envelope of many entries,
/// each under 200 characters; a plain error body; a `Docker-Content-Digest`
/// that does not match. Each error must stay near the excerpt's size.
#[tokio::test]
async fn red_team_a_registrys_text_reaches_an_error_only_as_an_excerpt() {
    const ENTRIES: usize = 2000;
    let message = "m".repeat(199);
    let envelope = {
        let one = format!(r#"{{"code":"UNKNOWN","message":"{message}"}}"#);
        format!(r#"{{"errors":[{}]}}"#, vec![one; ENTRIES].join(","))
    };
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":2},"layers":[]}"#;
    let long_digest = format!("sha256:{}", "e".repeat(300_000));
    let server = {
        let envelope = envelope.clone();
        let long_digest = long_digest.clone();
        Scripted::start(move |req| match req.path() {
            "/v2/" => response("200 OK", &[], b""),
            p if p.contains("/manifests/envelope") => response(
                "404 Not Found",
                &[("Content-Type", "application/json")],
                envelope.as_bytes(),
            ),
            p if p.contains("/manifests/plain") => response(
                "500 Internal Server Error",
                &[],
                "p".repeat(500_000).as_bytes(),
            ),
            p if p.contains("/manifests/digest") => response(
                "200 OK",
                &[
                    ("Content-Type", "application/vnd.oci.image.manifest.v1+json"),
                    ("Docker-Content-Digest", long_digest.as_str()),
                ],
                manifest,
            ),
            p if p.contains("/blobs/") => response(
                "404 Not Found",
                &[("Content-Type", "application/json")],
                envelope.as_bytes(),
            ),
            _ => response("404 Not Found", &[], b""),
        })
        .await
    };
    let host = server.host();
    assert!(envelope.len() < 1 << 20, "within the registry-body limit");
    let client = Client::new(
        options(std::slice::from_ref(&host)),
        TrustStore::load(),
        Credentials::none(),
    )
    .unwrap();
    let mut lengths = Vec::new();
    for tag in ["envelope", "plain", "digest"] {
        let reference = Reference::parse(&format!("{host}/app:{tag}")).unwrap();
        let err = client.pull_manifest(&reference).await.unwrap_err();
        lengths.push((format!("manifest {tag}"), err.to_string().len()));
    }
    let blob = Descriptor::new(
        MediaType::OCI_LAYER_GZIP,
        Digest::parse(&sha256(b"anything")).unwrap(),
        8,
    );
    let reference = Reference::parse(&format!("{host}/app")).unwrap();
    let err = client
        .pull_blob(&reference, &blob, &mut Vec::new())
        .await
        .unwrap_err();
    lengths.push(("blob envelope".to_owned(), err.to_string().len()));
    println!("error lengths: {lengths:?}");
    let long: Vec<_> = lengths.iter().filter(|(_, n)| *n > EXCERPT_BOUND).collect();
    assert!(
        long.is_empty(),
        "a registry's text reached an error far past a 200-character excerpt: {long:?}"
    );
}

/// Red team (T11, ADR-0070): a registry's credentials go only to it. A
/// manifest or blob `GET` the registry redirects to another origin (here
/// another port of the same address, which `reqwest` treats as another
/// host) arrives there without the `Authorization` header; the
/// registry's own requests carry it.
#[tokio::test]
async fn red_team_a_redirect_off_the_registry_carries_no_credential() {
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":2},"layers":[]}"#.to_vec();
    let layer = b"dummy layer".to_vec();
    let elsewhere = {
        let (manifest, layer) = (manifest.clone(), layer.clone());
        Scripted::start(move |req| {
            if req.path().contains("/manifests/") {
                response(
                    "200 OK",
                    &[("Content-Type", "application/vnd.oci.image.manifest.v1+json")],
                    &manifest,
                )
            } else {
                response("200 OK", &[], &layer)
            }
        })
        .await
    };
    let other = elsewhere.host();
    let registry = Scripted::start(move |req| {
        if req.header("authorization").is_none() {
            return response(
                "401 Unauthorized",
                &[("WWW-Authenticate", r#"Basic realm="r""#)],
                b"",
            );
        }
        match req.path() {
            "/v2/" => response("200 OK", &[], b""),
            p => response(
                "307 Temporary Redirect",
                &[("Location", &format!("http://{other}{p}"))],
                b"",
            ),
        }
    })
    .await;
    let host = registry.host();
    let client = Client::new(
        options(&[host.clone(), elsewhere.host()]),
        TrustStore::load(),
        basic(&host),
    )
    .unwrap();
    let reference = Reference::parse(&format!("{host}/app:1")).unwrap();
    let pulled = client.pull_manifest(&reference).await;
    let blob = Descriptor::new(
        MediaType::OCI_LAYER_GZIP,
        Digest::parse(&sha256(&layer)).unwrap(),
        layer.len() as u64,
    );
    let mut out = Vec::new();
    let blob_pulled = client
        .pull_blob(
            &Reference::parse(&format!("{host}/app")).unwrap(),
            &blob,
            &mut out,
        )
        .await;
    let at_registry = registry.seen();
    let at_elsewhere = elsewhere.seen();
    println!("registry: {at_registry:?}\nelsewhere: {at_elsewhere:?}");
    pulled.unwrap();
    blob_pulled.unwrap();
    assert_eq!(out, layer);
    assert!(
        at_registry
            .iter()
            .any(|r| r.path() != "/v2/" && r.header("authorization").is_some()),
        "control: the registry's own requests carry the credential"
    );
    assert_eq!(at_elsewhere.len(), 2, "both redirects were followed");
    let leaked: Vec<_> = at_elsewhere
        .iter()
        .filter(|r| r.header("authorization").is_some())
        .collect();
    assert!(
        leaked.is_empty(),
        "the registry's credential went to another origin: {leaked:?}"
    );
}

/// Red team (T16, T11): a token realm that answers with a token holding a
/// line break, a NUL or a space, which would split or break the header
/// it goes in, is refused with an error, never sent and never a panic; a
/// token of printable ASCII is the control.
#[tokio::test]
async fn red_team_a_token_that_would_break_a_header_is_refused() {
    for (token, usable) in [
        (r"dummy\r\nX-Injected: 1", false),
        (r"dummy\u0000x", false),
        ("dummy token", false),
        ("dummy\u{e9}", false),
        ("dummy-token.ok_1", true),
    ] {
        let body = format!(r#"{{"token":"{token}"}}"#);
        let realm_seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let registry = {
            let realm_seen = Arc::clone(&realm_seen);
            let addr = Arc::new(Mutex::new(String::new()));
            let server = Scripted::start({
                let addr = Arc::clone(&addr);
                move |req| {
                    let me = addr.lock().unwrap().clone();
                    if req.path().starts_with("/token") {
                        return response("200 OK", &[], body.as_bytes());
                    }
                    match req.header("authorization") {
                        Some(a) => {
                            realm_seen.lock().unwrap().push(a.to_owned());
                            response("404 Not Found", &[], b"")
                        }
                        None => response(
                            "401 Unauthorized",
                            &[(
                                "WWW-Authenticate",
                                &format!(r#"Bearer realm="http://{me}/token",service="s""#),
                            )],
                            b"",
                        ),
                    }
                }
            })
            .await;
            *addr.lock().unwrap() = server.host();
            server
        };
        let host = registry.host();
        let client = Client::new(
            options(std::slice::from_ref(&host)),
            TrustStore::load(),
            Credentials::none(),
        )
        .unwrap();
        let reference = Reference::parse(&format!("{host}/app:1")).unwrap();
        let err = client.pull_manifest(&reference).await.unwrap_err();
        let sent = realm_seen.lock().unwrap().clone();
        println!("{token:?}: {err}; sent {sent:?}");
        if usable {
            assert_eq!(sent, vec!["Bearer dummy-token.ok_1".to_owned()], "control");
        } else {
            assert!(sent.is_empty(), "{token:?} was sent: {sent:?}");
            assert!(err.to_string().contains("token"), "{token:?}: {err}");
        }
    }
}
