# rnk-registry

> **Status:** Phase 1 done (task 6) · **Phase:** 1 ·
> **Depends on:** `rnk-oci` (pulls go into sinks its caller takes from `rnk-store`) · **Used by:** `rnk-builder`, `rnk-cache` (`ranikust` for `login`/`push` [open: Q-028]) · **Path:** `crates/rnk-registry/`

**TL;DR:** A thin wrapper over `oci-client` 0.18 adding what it lacks:
`config.json` auth, the identity-token exchange, retries, size limits,
mount fallback, fast streamed push, rustls with `ring` and an explicit trust
store, spans. Registry responses are hostile: verify every digest and size
while streaming; never log a credential.

## Owns

- Authentication (docker `config.json`; credential helpers in Phase 5); TLS:
  rustls with `ring`, trusting the host's CAs plus the embedded webpki roots,
  plus `--registry-certificate` per registry (ADR-0024).
- Retries, streamed blob pulls into a caller's sink (an `rnk-store`
  temporary file) and pushes; cross-repository mounts with a fallback to a
  regular upload; platform selection from an index. Mirrors in Phase 5.

## Must not

- Build, compress or inspect layer contents (`rnk-layer`), or hold caching
  policy (`rnk-cache`).
- Keep process-wide state (a global certificate pool, credentials in
  statics), or define the store layout or publish entries (`rnk-store`).
  The one exception: `ring` installed as rustls's default provider, which
  `reqwest` needs.

## API

```rust
pub struct TrustStore;   // load(): host CAs + webpki-root-certs; with_pem_file(path); host_len/embedded_len/len
pub struct Credentials;  // none(); config_path(DOCKER_CONFIG, HOME); load(path); get(registry) -> Option<&Credential>; helper(registry)
pub enum Credential { Basic { username, password }, IdentityToken(String), RegistryToken(String) } // Debug redacts
#[non_exhaustive] pub struct ClientOptions { pub insecure: BTreeSet<String>, pub skip_tls_verify: BTreeSet<String>,
    pub registry_certificates: Vec<(String, PathBuf)>, pub retry: RetryPolicy,
    pub connect_timeout: Option<Duration>, pub read_timeout: Option<Duration>, pub user_agent: &'static str }
pub struct RetryPolicy { pub attempts: u32, pub initial_delay: Duration, pub factor: u32 } // default 3, 1 s, ×3
pub struct RawManifest;  // new(media_type, bytes) hashes; media_type(), digest(), bytes(), size(), descriptor()
pub struct ImageManifests { pub manifest: RawManifest, pub index: Option<RawManifest> }
pub enum PushOutcome { Existed, Mounted, Uploaded }
pub enum BlobBody { Bytes(Bytes), File(PathBuf) }          // reopened per attempt
pub trait BlobSink: AsyncWrite + Unpin + Send { fn reset(&mut self) -> impl Future<Output = io::Result<()>>; } // File, Vec<u8>
pub fn select_platform<'a>(index: &'a ImageIndex, p: &Platform) -> Option<&'a Descriptor>;
impl Client {
    pub fn new(options: ClientOptions, trust: TrustStore, credentials: Credentials) -> Result<Client, Error>;
    pub async fn pull_manifest(&self, r: &Reference) -> Result<RawManifest, Error>;
    pub async fn pull_image_manifest(&self, r: &Reference, p: &Platform) -> Result<ImageManifests, Error>;
    pub async fn pull_blob(&self, r: &Reference, d: &Descriptor, out: &mut impl BlobSink) -> Result<(), Error>;
    pub async fn blob_exists(&self, r: &Reference, d: &Digest) -> Result<bool, Error>;
    pub async fn push_blob(&self, r: &Reference, d: &Descriptor, body: &BlobBody) -> Result<PushOutcome, Error>;
    pub async fn mount_or_push(&self, r: &Reference, from: &Reference, d: &Descriptor, body: &BlobBody) -> Result<PushOutcome, Error>;
    pub async fn push_manifest(&self, r: &Reference, m: &RawManifest) -> Result<Digest, Error>; // by tag only
}
pub enum Error { Tls, Credentials, Reference, Auth, Registry { retryable, .. }, DigestMismatch, SizeMismatch, Manifest, Io }
```

## Invariants

- No pulled byte is used before its digest is verified; a mismatch discards
  the temporary file ([design/cache-and-store.md#failure-handling](../../design/cache-and-store.md#failure-handling)). Memory stays flat
  whatever the blob size: stream both ways. A pull never writes more than
  the descriptor's size.
- Credentials never reach `Debug`, `Display`, logs, errors, the image or
  cache keys ([threat model T11](../../security/threat-model.md#5-threats-and-mitigations)), and never go to a plain-HTTP token
  realm of an HTTPS registry.
- References reach `oci-client` through their string form (ADR-0026).

## Refs

- **Design:** [design/cache-and-store.md#ownership](../../design/cache-and-store.md#ownership) (blob transfer here), [#atomic-publication](../../design/cache-and-store.md#atomic-publication), [#locks](../../design/cache-and-store.md#locks)
- **Decisions:** [ADR-0024](../../adr/0024-tls-rustls-ring.md) (`ring`, combined trust store), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (DER certs to `oci-client`; helpers external), [ADR-0021](../../adr/0021-rnk-store-crate.md) (blobs go through `rnk-store`), [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (`--destination`, `--registry-certificate`, `--cache-repo`), [ADR-0006](../../adr/0006-persistent-store.md) (per-digest locks), [ADR-0026](../../adr/0026-rnk-oci-owns-its-types.md) (references by string)
- **Evidence:** [exp-05 Q4](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180) (`oci-client` behaviour, timings),
  [Q1](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) (CAs in `scratch`), [Q7](../../research/experiments/05-rust-ecosystem.md#q7-docker_credential-and-oci-spec) (credential lookup table).
- **kaniko:** `kaniko@44aa7ddc0 pkg/image/remote/remote.go:40-129` (maps,
  mirrors, retries), `pkg/creds/creds.go:35-70` (keychains),
  `pkg/util/gcr_util.go:30-46` (`DOCKER_CONFIG`), `transport_util.go:143-196`,
  `pkg/executor/push.go:95-140` (permission check), `:318-366` (mount, then
  upload, retries); `cmd/executor/cmd/root.go:391-410` (flags); vendored
  go-containerregistry: `pkg/v1/remote/options.go:80-101` (retry defaults),
  `pkg/v1/remote/index.go:252-291` (platform match),
  `pkg/v1/remote/transport/bearer.go:442-497` (identity tokens),
  `ping.go:47-58` and `pkg/name/registry.go:88-108` (HTTP fallback)
- **External:** [oci-client v0.18.0 client.rs](https://github.com/oras-project/rust-oci-client/blob/v0.18.0/src/client.rs), [OCI distribution-spec v1.1.1](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md), [token authentication](https://distribution.github.io/distribution/spec/auth/oauth/), [docker_credential](https://crates.io/crates/docker_credential), [webpki-root-certs](https://crates.io/crates/webpki-root-certs)

## Tests

- T0, `tests/registry/`: an in-process fake registry (hand-written on
  `hyper`, which the tree already has; it keeps blobs, manifests and upload
  sessions, which a request-matching mock would have to script): open,
  basic and bearer registries, `config.json` credentials, identity tokens
  per scope, digest and size mismatches, retries on 503 and none on 404,
  one-PUT push, existence check, mount refused (`202`) then upload, mount
  accepted, platform selection, plain HTTP only when listed. The spans are
  checked in `tests/spans/`, a binary with one test, since tracing caches
  callsite interest per process.
- T0 unit tests: exp-05's Table 14 credential cases without helpers, secret
  redaction in `Debug` and errors, trust store (embedded roots, invalid host
  certificates dropped, PEM files), retry delays, challenge parsing.
- Ignored: `real_registry_pull`, read-only, pulls `RNK_TEST_IMAGE` for
  linux/amd64 (passed on 2026-10-30 against a local `registry:2` and
  `docker.io/library/busybox:latest`, the latter over TLS with the
  combined trust store and an anonymous token); `real_registry_round_trip`
  pushes to a `k6-*` repository of `RNK_TEST_REGISTRY` (`host:port`, plain
  HTTP) and deletes the manifest, but `registry:2` keeps the repository
  in its catalog. Not covered in T0: `SSL_CERT_FILE`/`SSL_CERT_DIR`
  (process environment), TLS handshakes, htpasswd, ECR, GCR.

## Metrics

- One span per operation, each with `host`: `rnk_registry.auth` (the
  `/v2/` probe when a registry is first used, and each token fetch),
  `pull_manifest`, `pull_blob`, `push_blob` (`mounted`, `existed`),
  `push_manifest` ([rows](../../design/metrics.md#rnk-registry)); `blob_exists` has none.
- `bytes_in`/`bytes_out` are body bytes of every attempt; `requests`
  counts the requests each `oci-client` call makes, retries included, but
  not the token refreshes `oci-client` makes on its own. No span per
  request or chunk, never a credential in a field.

## Pitfalls

- `Client::pull()` holds every layer in memory, `push_blob(Vec<u8>)` peaked
  at 526 MB RSS for 500 MB, and chunked push (sequential PATCHes of at most
  4 MiB) took 13.0–20.4 s against 0.9 s with `use_monolithic_push` ([exp-05 Q4](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180)):
  the client uses `push_blob_stream` with `use_monolithic_push`.
- A refused mount (`202` with an upload session) is a `SpecViolationError`
  and the session is dropped: upload yourself, as kaniko does (Q4;
  `push.go:324-334`). `oci-client`'s tokens cover one repository, and a
  token registry refuses a mount without pull access to the source: on a
  token registry `mount_or_push` fetches one token for both scopes itself
  (a `GET` to the realm with two `scope`s, or the identity-token exchange
  with both) and sends the mount with it, as go-containerregistry's
  `maybeUpdateScopes` does; open, basic and static-token registries keep
  `oci-client`'s path.
- The test fake answers `HEAD` and `GET` of a blob only in repositories
  that hold it, checks the source scope of a mount under bearer
  authentication, and stores a pushed manifest with its `Content-Type`:
  without these, mounts and per-repository reads were never exercised.
- `oci-client` panics in `push_manifest_raw` when a registry omits
  `Location` and the reference has a digest, and unwraps a header built from
  a token: `push_manifest` takes tags only, and identity-token responses
  with non-header characters are refused. A hostile token from the
  ordinary token flow can still abort the process.
- `oci-client` keeps the first authorization stored per registry and reads
  manifest bodies whole: one client per registry host, and manifests over
  4 MiB are rejected only after reading. Identity tokens go into its token
  cache per scope; a token that outlives `oci-client`'s 60 s guess is put
  back before each operation.
- Without a stored authorization `oci-client` sends no credentials, and
  with one it pings `/v2/` again before each token: the client probes
  `/v2/` once per registry and stores nothing for open registries.
- `reqwest` with `rustls-no-provider` panics at build unless a crypto
  provider is installed; `Client::new` installs `ring`. Its default
  verifier fails in `scratch`, so certificates go through `tls_certs_only`,
  and one invalid root makes it refuse to build: the trust store drops
  certificates rustls rejects.
- kaniko's `--registry-certificate` (`registry=path`) joins one process-wide
  pool unless `FF_KANIKO_SCOPED_REGISTRY_CERTIFICATES` is set
  (`transport_util.go:146-170`): here it is per registry.
- kaniko tries mapped and mirror registries in order, then the original
  unless `--skip-default-registry-fallback` (`remote.go:54-98`); reads
  `DOCKER_AUTH_CONFIG` first and has built-in env, google, ecr, acr and
  gitlab helpers (`creds.go:39-55`); ranikust ships none (ADR-0020).
- `ring` compiles C: the musl targets need `feat/zigbuild-zstd`'s
  cargo-zigbuild (task 5a, Q-052).

## Phase 1

- Done (task 6): trust store and per-registry certificates; anonymous,
  basic, bearer, registry-token and identity-token auth from `config.json`;
  manifest, index and blob pulls with platform selection; one-PUT blob and
  raw manifest pushes; `blob_exists`; mounts; retries; spans.
- Deferred: mirrors, credential helpers (phase 5); cache-repo use by
  `rnk-cache` (phase 4). (Q-014, ADR-0032) Task 11 mounts (interim, on
  `feat/rnk-builder-outputs`).
  (Q-065, ADR-0032) HTTP fallback for local registries. [open: Q-048] `login`.
  Only a loopback registry was measured
  ([survey 00 §4](../../research/survey/00-summary.md#4-what-was-verified-and-what-was-not)).
