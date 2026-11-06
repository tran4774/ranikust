# rnk-registry

> **Status:** in progress · **Phase:** 1-2 (mirrors, credential helpers: 5) ·
> **Depends on:** `rnk-oci` (pulls go into sinks its caller takes from
> `rnk-store`) ·
> **Used by:** `rnk-builder`, `ranikust` (the client), `rnk-cache` (the registry cache) ·
> **Path:** `crates/rnk-registry/`

**TL;DR:** A thin wrapper over `oci-client` 0.18 adding what it lacks:
`config.json` auth, the identity-token exchange, retries, size limits, mount
fallback, fast streamed push, rustls with `ring` and an explicit trust store,
spans. Registry responses are hostile: verify every digest and size while
streaming; never log a credential.

## Owns

- Authentication (docker `config.json`; credential helpers in Phase 5); TLS:
  rustls with `ring`, trusting the host's CAs plus the embedded webpki roots,
  plus `--registry-certificate` per registry (ADR-0024).
- Retries, streamed blob pulls into a caller's sink (an `rnk-store` temporary
  file) and pushes; cross-repository mounts with a fallback to a regular
  upload (ADR-0032); platform selection from an index. Mirrors in Phase 5. A
  Range resume of a cache hit's body after a drop is planned
  ([cache keys](../../design/cache-and-store.md#cache-keys), #130).
- Manifest and blob `GET`s and anonymous or basic token requests
  (`fetch.rs`, `token.rs`), so every body read is bounded as it arrives (the
  `--limit` limits, ADR-0079); the push side goes through `oci-client`.
- Blobs streamed without being kept (ADR-0055): `Client::open_blob`, a
  blocking reader over a 256 KiB pipe (`PIPE_BYTES`) that a thread of its own
  fills, for kaniko mode's extraction; `BlobBody::Remote`, a push whose body
  is pulled from another repository through the same pipe. The reader (or
  body) owns the thread: dropped, it stops the pull and joins the thread, so
  no pull outlives the runtime. A pull into a pipe is retried only until its
  first byte; after that its first error is final, and a reader that failed
  fails on every read. Read only from a thread outside the runtime (a
  stage's): inside, the read fails.

## Must not

Boundaries: `crates/rnk-registry/src/lib.rs`. In short:

- Build, compress or inspect layer contents (`rnk-layer`), or hold caching
  policy (`rnk-cache`).
- Keep process-wide state (a global certificate pool, credentials in
  statics), or define the store layout or publish entries (`rnk-store`). The
  one exception: `ring` installed as rustls's default provider, which
  `reqwest` needs.
- Put a credential in an error, a `Debug` output, a log event or a span field.

## API

```rust
pub struct TrustStore;   // load(): host CAs + webpki-root-certs; with_pem_file(path); host_len/embedded_len/len
pub struct Credentials;  // none(); config_path(DOCKER_CONFIG, HOME); load(path); get(registry) -> Option<&Credential>; helper(registry)
    // from_env_value(DOCKER_AUTH_CONFIG bytes): `auths` only; or(fallback): asked when these say nothing (ADR-0036)
pub enum Credential { Basic { username, password }, IdentityToken(String), RegistryToken(String) } // Debug redacts
#[non_exhaustive] pub struct ClientOptions { pub insecure: BTreeSet<String>,
    pub http_credentials: BTreeSet<String>,  // may get credentials over plain HTTP: operator-typed names only (T11)
    pub verified_credentials_only: bool,     // credentials only over verified TLS, overriding both sets (kaniko mode on /, ADR-0051: a RUN may redirect names)
    pub skip_tls_verify: BTreeSet<String>,   // everything sent to the registry, token realm included
    pub registry_certificates: Vec<(String, PathBuf)>, pub retry: RetryPolicy,
    pub manifest_retries: u32,               // kaniko's --image-download-retry around pull_image_manifest
    pub connect_timeout: Option<Duration>, pub read_timeout: Option<Duration>, pub user_agent: &'static str } // plus the --limit values, defaulted
pub struct RetryPolicy { pub attempts: u32, pub initial_delay: Duration, pub factor: u32 } // default 3, 1 s, x3
pub async fn retry_kaniko<T, E: Display>(retries: u32, op: impl AsyncFn() -> Result<T, E>) -> Result<T, E>;
    // kaniko's outer Retry (1 s, doubling, any error): --push-retry, --image-download-retry
pub struct RawManifest;  // new(media_type, bytes) hashes; media_type(), digest(), bytes(), size(), descriptor()
pub struct ImageManifests { pub manifest: RawManifest, pub index: Option<RawManifest> }
pub enum PushOutcome { Existed, Mounted, Uploaded }
pub enum BlobBody { Bytes(Bytes), Opened(Arc<dyn BlobFile>), File(PathBuf), Remote(Box<RemoteBlob>) } // reopened per attempt; Remote pulled through a pipe
pub trait BlobFile: Debug + Send + Sync { fn open(&self) -> io::Result<std::fs::File>; } // a file reached by a handle, not a path (an anonymous layer)
pub trait BlobSink: AsyncWrite + Unpin + Send { fn reset(&mut self) -> impl Future<Output = io::Result<()>>; fn can_restart(&self) -> bool { true } } // File, Vec<u8>
pub fn select_platform<'a>(index: &'a ImageIndex, p: &Platform) -> Option<&'a Descriptor>;
impl Client {
    pub fn new(options: ClientOptions, trust: TrustStore, credentials: Credentials) -> Result<Client, Error>;
    pub async fn pull_manifest(&self, r: &Reference) -> Result<RawManifest, Error>;
    pub async fn find_manifest(&self, r: &Reference) -> Result<Option<RawManifest>, Error>; // None on a 404 only (the layer cache's miss)
    pub async fn pull_image_manifest(&self, r: &Reference, p: &Platform) -> Result<ImageManifests, Error>;
    pub async fn pull_blob(&self, r: &Reference, d: &Descriptor, out: &mut impl BlobSink) -> Result<(), Error>;
    pub async fn pull_config(&self, r: &Reference, d: &Descriptor, out: &mut impl BlobSink) -> Result<(), Error>; // the `config` limit first (ADR-0079)
    pub fn open_blob(self: &Arc<Self>, h: &Handle, r: &Reference, d: &Descriptor) -> io::Result<BlobReader>; // blocking Read, fails unless size and digest match
    pub async fn blob_exists(&self, r: &Reference, d: &Digest) -> Result<bool, Error>;
    pub async fn push_blob(&self, r: &Reference, d: &Descriptor, body: &BlobBody) -> Result<PushOutcome, Error>;
    pub async fn mount_or_push(&self, r: &Reference, from: &Reference, d: &Descriptor, body: &BlobBody) -> Result<PushOutcome, Error>;
    pub async fn push_manifest(&self, r: &Reference, m: &RawManifest) -> Result<Digest, Error>; // by tag only
}
pub enum Error { Tls, Credentials, Reference, Auth, Registry { retryable, refused, not_found, .. }, DigestMismatch, SizeMismatch, Manifest, Io }
    // is_not_found(): a manifest or blob GET answered 404; never retried
```

## Invariants

- No pulled byte is used before its digest is verified, except a blob read
  through `open_blob` or `BlobBody::Remote` (ADR-0055): its reader or push
  fails at the end of the body, never ends cleanly, and the caller's use of
  the bytes (an extraction, an upload session) fails with it. Otherwise a
  mismatch discards the temporary file
  ([design/cache-and-store.md#failure-handling](../../design/cache-and-store.md#failure-handling)).
  Memory stays flat whatever the blob size: stream both ways. A pull never
  writes more than the descriptor's size.
- Credentials never reach `Debug`, `Display`, logs, errors, the image or
  cache keys ([threat model T11](../../security/threat-model.md#5-threats-and-mitigations)),
  and never go to a plain-HTTP token realm of an HTTPS registry.
- References reach `oci-client` built from the parts `rnk_oci` parsed, never
  parsed again, and converted before the call connects, so a request fails
  before anything is sent if `oci-client` would address a host or repository
  other than the one the connection and the credential were chosen for
  (ADR-0070, refining ADR-0026).
- Plain HTTP: a loopback registry (`localhost`, `127.0.0.0/8`, `::1`) is
  plain HTTP unasked, as BuildKit (ADR-0032); private-network addresses are
  not special. Unlike BuildKit, credentials go over plain HTTP only to a
  registry in `ClientOptions::http_credentials`, which `ranikust` fills with
  names the operator typed (`--insecure-registry`, destinations under
  `--insecure`; ADR-0036): ranikust's `RUN` shares the host's network and
  could sniff `lo` or serve the port (T11). Any other plain-HTTP registry
  (a loopback one, or one a Dockerfile names under `--insecure-pull`) is
  accessed anonymously, and a refusal names the flags. With
  `verified_credentials_only` (kaniko mode on `/`) only verified TLS carries
  credentials: none over plain HTTP even to a registry so named, nor to one
  under `--skip-tls-verify*`, and a refusal says so (ADR-0051).

## Refs

- **Design:** [design/cache-and-store.md#ownership](../../design/cache-and-store.md#ownership) (blob transfer here), [#atomic-publication](../../design/cache-and-store.md#atomic-publication), [#locks](../../design/cache-and-store.md#locks); [design/streaming.md](../../design/streaming.md)
- **Decisions:** [ADR-0024](../../adr/0024-tls-rustls-ring.md) (`ring`, combined trust store), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (DER certs to `oci-client`; helpers external; `ring` compiles C, built with cargo-zigbuild), [ADR-0021](../../adr/0021-rnk-store-crate.md) (blobs go through `rnk-store`), [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (`--destination`, `--registry-certificate`, `--cache-repo`), [ADR-0006](../../adr/0006-persistent-store.md) (per-digest locks), [ADR-0026](../../adr/0026-rnk-oci-owns-its-types.md) (`oci-spec` types stay inside), [ADR-0070](../../adr/0070-registry-references-reach-oci-client-built-from-parts.md) (references built from parts), [ADR-0079](../../adr/0079-resource-limits-with-a-limit-flag.md) (`--limit`)
- **Open:** [#28](https://github.com/tran4774/ranikust/issues/28) (an overall pull deadline or minimum rate), [#25](https://github.com/tran4774/ranikust/issues/25) (`--network=none` and ranikust's own requests), [#23](https://github.com/tran4774/ranikust/issues/23) (encode a layer straight into its upload), [#37](https://github.com/tran4774/ranikust/issues/37) (signing). Decided, not built yet: [ADR-0120](../../adr/0120-credential-helpers-are-external-for-now-mirrors-keep-kanikos-flags-and-pushes-go-through-rnk-registry.md) (push requests move from `oci-client` to `rnk-registry`), [ADR-0115](../../adr/0115-rnk-cache-holds-keys-and-transport-and-https-downloads-live-in-rnk-registry.md) (HTTPS downloads live here), [ADR-0123](../../adr/0123-a-cache-hit-that-fails-makes-its-stage-start-again-once-with-no-cache.md) (planned with #130: a Range resume of a cache hit's body, [cache keys](../../design/cache-and-store.md#cache-keys))
- **Evidence:** [exp-05 Q4](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180) (`oci-client` behaviour, timings), [Q1](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) (CAs in `scratch`), [Q7](../../research/experiments/05-rust-ecosystem.md#q7-docker_credential-and-oci-spec) (credential lookup table); only a loopback registry was measured ([survey 00 §4](../../research/survey/00-summary.md#4-what-was-verified-and-what-was-not))
- **kaniko:** `kaniko@44aa7ddc0 pkg/image/remote/remote.go:40-129` (maps, mirrors, retries), `pkg/creds/creds.go:35-70` (keychains), `pkg/util/gcr_util.go:30-46` (`DOCKER_CONFIG`), `transport_util.go:143-196`, `pkg/executor/push.go:95-140` (permission check), `:318-366` (mount, then upload, retries); `cmd/executor/cmd/root.go:391-410` (flags); vendored go-containerregistry: `pkg/v1/remote/options.go:80-101` (retry defaults), `pkg/v1/remote/index.go:252-291` (platform match), `pkg/v1/remote/transport/bearer.go:442-497` (identity tokens), `ping.go:47-58` and `pkg/name/registry.go:88-108` (HTTP fallback)
- **External:** [oci-client v0.18.0 client.rs](https://github.com/oras-project/rust-oci-client/blob/v0.18.0/src/client.rs), [OCI distribution-spec v1.1.1](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md), [token authentication](https://distribution.github.io/distribution/spec/auth/oauth/), [docker_credential](https://crates.io/crates/docker_credential), [webpki-root-certs](https://crates.io/crates/webpki-root-certs)

## Tests

All T0 unless noted; tiers: [`docs/testing.md`](../../testing.md#tiers).

- `tests/registry/`: an in-process fake registry (hand-written on `hyper`,
  which the tree already has; it keeps blobs, manifests and upload sessions,
  which a request-matching mock would have to script): open, basic and bearer
  registries, `config.json` credentials, identity tokens per scope, digest
  and size mismatches, retries on 503 and none on 404, one-PUT push,
  existence check, mount refused (`202`) then upload, mount accepted,
  platform selection, a loopback registry over plain HTTP unlisted,
  `find_manifest` none on a 404 only. `rnk-cache` compiles the fake into its
  own tests: keep it free of anything specific to this crate's tests.
  `tests/spans/` checks the spans in a binary with one test, since tracing
  caches callsite interest per process.
- `tests/limits/`: the `--limit` limits against hostile bodies (64 MiB
  manifests, token responses and error bodies the fake streams), memory
  measured by a counting allocator, a config refused before any request, a
  stalled body failing after `registry-idle` (ADR-0079); a pull under basic
  authentication sends one `GET /v2/` only and a push carries the same
  credential (`basic_authentication`; `rnk-builder` `tests/stall.rs` runs it
  inside the store's locked fill).
- `tests/red_team/`: a registry scripted byte for byte on loopback: a
  manifest or blob `GET` redirected to another origin arrives there without
  the basic credential; a token holding CR LF, NUL, a space or non-ASCII is
  refused, never sent; a registry's text reaches an error only as an excerpt
  (a 2000-entry error envelope, a 300 KB `Docker-Content-Digest`, a plain
  error body: each error stays under 400 bytes).
- Unit tests: exp-05's Table 14 credential cases without helpers, secret
  redaction in `Debug` and errors, trust store (embedded roots, invalid host
  certificates dropped, PEM files), retry delays, the retry and not-found
  decisions from status and codes, challenge parsing.
- Ignored (need a registry): `real_registry_pull` (read-only, pulls
  `RNK_TEST_IMAGE` for linux/amd64) and `real_registry_round_trip` (pushes to
  a `k6-*` repository of `RNK_TEST_REGISTRY`, `host:port`, plain HTTP, and
  deletes the manifest; `registry:2` keeps the repository in its catalog).
  Not covered: `SSL_CERT_FILE`/`SSL_CERT_DIR` (process environment), TLS
  handshakes, htpasswd, ECR, GCR.

## Metrics

- One span per operation, each with `host`: `rnk_registry.auth` (the `/v2/`
  probe when a registry is first used, and each token fetch), `pull_manifest`,
  `pull_blob`, `push_blob` (`mounted`, `existed`), `push_manifest`
  ([rows](../../design/metrics.md#rnk-registry)); `blob_exists` has none.
- `bytes_in`/`bytes_out` are body bytes of every attempt; `requests` counts
  the requests each `oci-client` call makes, retries included, but not the
  token refreshes `oci-client` makes on its own. No span per request or
  chunk, never a credential in a field.

## Pitfalls

- `Client::pull()` holds every layer in memory, `push_blob(Vec<u8>)` peaked at
  526 MB RSS for 500 MB, and chunked push (sequential PATCHes of at most
  4 MiB) took 13.0-20.4 s against 0.9 s with `use_monolithic_push`
  ([exp-05 Q4](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180)):
  the client uses `push_blob_stream` with `use_monolithic_push`.
- A refused mount (`202` with an upload session) is a `SpecViolationError` and
  the session is dropped: upload yourself, as kaniko does (`push.go:324-334`).
  `oci-client`'s tokens cover one repository, and a token registry refuses a
  mount without pull access to the source: on a token registry
  `mount_or_push` fetches one token for both scopes itself (a `GET` to the
  realm with two `scope`s, or the identity-token exchange with both) and
  sends the mount with it, as go-containerregistry's `maybeUpdateScopes`
  does; open, basic and static-token registries keep `oci-client`'s path.
- A registry's text reaches an error only as a 200-character excerpt
  (`Error::Registry`'s `message`), so nothing is decided from it: whether the
  registry refused access (the withheld-credential hint) is
  `Error::Registry`'s `refused`, set from the status (401, 403) and the
  envelope's `UNAUTHORIZED` and `DENIED` codes
  (`a_refusal_is_decided_from_codes_and_status_not_the_excerpt`), as
  `retryable` is, and `not_found` from a pull's 404 alone, never retried
  (`find_manifest_is_none_only_for_a_404`,
  `a_404_is_not_found_and_never_retried`).
- The test fake answers `HEAD` and `GET` of a blob only in repositories that
  hold it, checks the source scope of a mount under bearer authentication,
  and stores a pushed manifest with its `Content-Type`: without these,
  mounts and per-repository reads were never exercised.
- `oci-client` panics in `push_manifest_raw` when a registry omits `Location`
  and the reference has a digest: `push_manifest` takes tags only.
- `oci-client` keeps the first authorization stored per registry and reads
  every body whole (`Response::bytes()`/`text()`), so pulls do not go through
  it: manifest and blob `GET`s and anonymous or basic token requests are sent
  by `fetch.rs`/`token.rs` with bounded reads (the `manifest` and
  `registry-body` limits, ADR-0079), with `oci-client`'s status and digest
  handling copied. It keeps the push side, whose error bodies it still reads
  whole until pushes move here ([ADR-0120](../../adr/0120-credential-helpers-are-external-for-now-mirrors-keep-kanikos-flags-and-pushes-go-through-rnk-registry.md),
  not built yet). Tokens go
  into its token cache per scope; a token that outlives `oci-client`'s 60 s
  guess is put back before each operation.
- `ClientOptions` carries the `--limit` values and defaults to them
  (`MAX_MANIFEST_BYTES`, `MAX_BODY_BYTES`, `MAX_CONFIG_BYTES`,
  `READ_TIMEOUT`): a library user is bounded too. `pull_config` checks the
  config's descriptor before the request. `read_timeout` is an idle bound
  kept by `fetch::on_socket` around each wait for a pull's bytes, never set
  as `reqwest`'s read timeout (which would cap a whole upload) and never
  running while the sink writes; it is not a deadline: a registry dripping
  bytes still holds a pull ([#28](https://github.com/tran4774/ranikust/issues/28)).
  Token requests use their own client, which follows redirects only within
  one origin.
- Without a stored authorization `oci-client` sends no credentials, and with
  one it pings `/v2/` again before each token: the client probes `/v2/` once
  per registry and stores nothing for open registries.
- `reqwest` with `rustls-no-provider` panics at build unless a crypto
  provider is installed; `Client::new` installs `ring`. Its default verifier
  fails in `scratch`, so certificates go through `tls_certs_only`, and one
  invalid root makes it refuse to build: the trust store drops certificates
  rustls rejects.
- kaniko's `--registry-certificate` (`registry=path`) joins one process-wide
  pool unless `FF_KANIKO_SCOPED_REGISTRY_CERTIFICATES` is set
  (`transport_util.go:146-170`): here it is per registry.
- kaniko tries mapped and mirror registries in order, then the original
  unless `--skip-default-registry-fallback` (`remote.go:54-98`); reads
  `DOCKER_AUTH_CONFIG` first and has built-in env, google, ecr, acr and gitlab
  helpers (`creds.go:39-55`); ranikust ships none (ADR-0020).
