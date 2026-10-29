# rnk-registry

> **Status:** skeleton · **Phase:** 1 ·
> **Depends on:** `rnk-oci`, `rnk-store` · **Used by:** `rnk-builder`, `rnk-cache` (`ranikust` for `login`/`push` [open: Q-028]) · **Path:** `crates/rnk-registry/`

**TL;DR:** A thin wrapper over `oci-client` 0.18 adding what it lacks:
`config.json` auth, retries, mirrors, mount fallback, fast streamed push,
rustls with `ring` and an explicit trust store. Registry responses are
hostile: verify every digest while streaming; never log a credential.

## Owns

- Authentication (docker `config.json`, credential helpers); TLS: rustls
  with `ring`, trusting the host's CAs plus the embedded webpki roots, plus
  `--registry-certificate` (ADR-0024).
- Mirrors, retries, streamed blob pull into an `rnk-store` temporary entry
  and push; cross-repository mounts with a fallback to a regular upload.

## Must not

- Build, compress or inspect layer contents (`rnk-layer`), or hold caching
  policy (`rnk-cache`).
- Keep process-wide state (a global certificate pool, credentials in
  statics), or define the store layout or publish entries (`rnk-store`).

## API (planned)

```rust
pub fn trust_store(extra_pem: &[PathBuf]) -> Result<Vec<Certificate>, Error>; // [decided] host CAs + webpki-root-certs + extra, ADR-0024
pub struct ClientOptions { pub insecure: Vec<String>, pub registry_ca: Vec<(String, PathBuf)>, pub mirrors: Vec<Mirror>, pub retries: u32 } // [design]
pub struct Client { /* oci_client::Client per registry + Credentials */ }   // [design] wrapper, ~400–600 lines
impl Client {
    pub async fn pull_manifest(&self, r: &Reference) -> Result<RawManifest, Error>;
    pub async fn pull_blob(&self, r: &Reference, d: &Descriptor, out: impl AsyncWrite + Unpin) -> Result<(), Error>; // digest-checked
    pub async fn blob_exists(&self, r: &Reference, d: &Digest) -> Result<bool, Error>;
    pub async fn push_blob(&self, r: &Reference, d: &Descriptor, body: BlobBody) -> Result<(), Error>; // one streamed PUT
    pub async fn mount_or_push(&self, r: &Reference, from: &Reference, d: &Descriptor, body: BlobBody) -> Result<(), Error>;
    pub async fn push_manifest(&self, r: &Reference, m: &RawManifest) -> Result<Digest, Error>; // raw bytes
}
pub struct Credentials; // [design] docker_credential 1.4 lookup; Debug and Display redact
```

## Invariants

- No pulled byte is used before its digest is verified; a mismatch discards
  the temporary file ([design/cache-and-store.md#failure-handling](../../design/cache-and-store.md#failure-handling)). Memory stays flat
  whatever the blob size: stream both ways.
- Credentials never reach `Debug`, `Display`, logs, errors, the image or
  cache keys ([threat model T11](../../security/threat-model.md#5-threats-and-mitigations)).

## Refs

- **Design:** [design/cache-and-store.md#ownership](../../design/cache-and-store.md#ownership) (blob transfer here), [#atomic-publication](../../design/cache-and-store.md#atomic-publication), [#locks](../../design/cache-and-store.md#locks)
- **Decisions:** [ADR-0024](../../adr/0024-tls-rustls-ring.md) (`ring`, combined trust store), [ADR-0020](../../adr/0020-static-musl-via-cargo-zigbuild.md) (DER certs to `oci-client`; helpers external), [ADR-0021](../../adr/0021-rnk-store-crate.md) (blobs go through `rnk-store`), [ADR-0011](../../adr/0011-accept-common-kaniko-flags.md) (`--destination`, `--registry-certificate`, `--cache-repo`), [ADR-0006](../../adr/0006-persistent-store.md) (per-digest locks)
- **Evidence:** [exp-05 Q4](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180) (`oci-client` behaviour, timings),
  [Q1](../../research/experiments/05-rust-ecosystem.md#q1-static-binary) (CAs in `scratch`), [Q7](../../research/experiments/05-rust-ecosystem.md#q7-docker_credential-and-oci-spec) (credential lookup table).
- **kaniko:** `kaniko@44aa7ddc0 pkg/image/remote/remote.go:40-129` (maps,
  mirrors, retries), `pkg/creds/creds.go:35-70` (keychains),
  `pkg/util/gcr_util.go:30-46` (`DOCKER_CONFIG`), `transport_util.go:143-196`,
  `pkg/executor/push.go:95-140` (permission check), `:318-366` (mount, then
  upload, retries); `cmd/executor/cmd/root.go:391-410` (flags)
- **External:** [oci-client v0.18.0 client.rs](https://github.com/oras-project/rust-oci-client/blob/v0.18.0/src/client.rs), [OCI distribution-spec v1.1.1](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md), [docker_credential](https://crates.io/crates/docker_credential), [webpki-root-certs](https://crates.io/crates/webpki-root-certs)

## Tests

- T0, loopback mock registry: token flow, `202` on mount then upload, retry
  on 5xx and resets, digest mismatch rejected, one-PUT push, raw manifests.
- T0: exp-05's Table 14 credential cases (fake `config.json` and helpers in a
  temp dir); trust store with no CAs, `SSL_CERT_FILE`, `SSL_CERT_DIR`. Outside
  T0: `registry:2` in a container; htpasswd, ECR, GCR (never tested).

## Metrics

- One span per operation, each with `host`: `rnk_registry.auth`,
  `pull_manifest`, `pull_blob`, `push_blob` (`mounted`, `existed`),
  `push_manifest` ([rows](../../design/metrics.md#rnk-registry)).
- `bytes_in`/`bytes_out` are body bytes, `requests` includes retries; they
  fill `network.hosts`. No span per request or chunk, no `host` on nested
  spans, never a credential in a field.

## Pitfalls

- `Client::pull()` holds every layer in memory, `push_blob(Vec<u8>)` peaked
  at 526 MB RSS for 500 MB, and chunked push (sequential PATCHes of at most
  4 MiB) took 13.0–20.4 s against 0.9 s with `use_monolithic_push` ([exp-05 Q4](../../research/experiments/05-rust-ecosystem.md#q4-oci-client-0180)).
- A refused mount (`202` with an upload session) is a `SpecViolationError`
  and the session is dropped: upload yourself, as kaniko does (Q4;
  `push.go:324-334`). `oci-client` also lacks retries, `config.json` and
  mirror fallback.
- `docker_credential` is synchronous, runs helpers without a timeout and only
  reads: use `spawn_blocking` with a timeout; `login` writes `config.json`
  itself ([exp-05 Q7](../../research/experiments/05-rust-ecosystem.md#q7-docker_credential-and-oci-spec)).
- reqwest 0.13's default verifier fails in `scratch`; `oci-client` takes only
  certificate lists, so pass DER roots via `tls_certs_only` (untested;
  [Q1](../../research/experiments/05-rust-ecosystem.md#q1-static-binary), ADR-0020). Its default `rustls-tls` feature
  selects `aws-lc-rs`: use `rustls-tls-no-provider` (ADR-0024).
- kaniko's `--registry-certificate` (`registry=path`) joins one process-wide
  pool unless `FF_KANIKO_SCOPED_REGISTRY_CERTIFICATES` is set
  (`transport_util.go:146-170`): scope it per registry.
- kaniko tries mapped and mirror registries in order, then the original
  unless `--skip-default-registry-fallback` (`remote.go:54-98`); reads
  `DOCKER_AUTH_CONFIG` first and has built-in env, google, ecr, acr and
  gitlab helpers (`creds.go:39-55`); ranikust ships none (ADR-0020).

## Phase 1

- Trust store and `--registry-certificate`; anonymous and `config.json` auth;
  manifest, index and blob pulls; one-PUT blob and raw manifest pushes;
  `blob_exists`; retries ([survey 04 §2](../../research/survey/04-risks-and-roadmap.md#2-mvp-and-10-scope)).
- Deferred: mirrors, credential helpers (phase 5); cache-repo use by
  `rnk-cache` (phase 4). [open: Q-014] Cross-repository mounts now or
  later (0.04 s each in exp-05). [open: Q-048] `login`. Only a loopback
  registry was measured
  ([survey 00 §4](../../research/survey/00-summary.md#4-what-was-verified-and-what-was-not)).
