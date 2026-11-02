//! The registry client: one `oci-client` client per registry host, with the
//! authentication, retries, size checks and spans `oci-client` lacks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use oci_client::RegistryOperation;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::errors::OciDistributionError;
use oci_client::secrets::RegistryAuth;
use oci_client::token_cache::{RegistryToken, RegistryTokenType};
use rnk_oci::{Descriptor, Digest, ImageIndex, MediaType, Platform, Reference};
use serde::Deserialize;
use serde::de::IgnoredAny;
use sha2::Digest as _;
use tokio::sync::Mutex;
use tracing::field::Empty;
use tracing::{Instrument as _, debug_span};

use crate::credentials::{Credential, Credentials};
use crate::error::{Error, Op, excerpt, retryable_status};
use crate::io::{BlobBody, BlobSink, Bounded};
use crate::platform::select_platform;
use crate::retry::{RetryPolicy, retry};
use crate::tls::{TrustStore, install_crypto_provider};
use crate::token::{self, Challenge, ExchangeError};

/// The manifest media types a pull accepts: `MediaType`'s OCI and Docker
/// manifests and indexes (checked by a test).
const ACCEPTED_MANIFESTS: [&str; 4] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// The largest manifest accepted: distribution's own limit.
const MAX_MANIFEST_BYTES: usize = 4 << 20;

/// Settings for a [`Client`], filled from kaniko's registry flags.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ClientOptions {
    /// Registries that may be reached over plain HTTP: HTTPS is tried first
    /// and HTTP used when it fails, as go-containerregistry does for kaniko's
    /// `--insecure`, `--insecure-pull` and `--insecure-registry`. A loopback
    /// registry not listed here is plain HTTP without trying HTTPS, as
    /// BuildKit (ADR-0032, Q-065).
    pub insecure: BTreeSet<String>,
    /// Registries that may receive credentials over plain HTTP: only names
    /// the operator typed (`--insecure-registry`, destinations under
    /// `--insecure`), never one only a Dockerfile names nor a loopback
    /// registry added implicitly, since a ranikust `RUN` shares the host's
    /// network (threat model T11). Any other plain-HTTP registry is
    /// accessed anonymously.
    pub http_credentials: BTreeSet<String>,
    /// Credentials only over verified TLS: never over plain HTTP (whatever
    /// [`Self::http_credentials`] lists) nor to a registry in
    /// [`Self::skip_tls_verify`]. For kaniko mode on `/`, where a `RUN` can
    /// edit `/etc/hosts` or `/etc/resolv.conf` and send later requests to a
    /// host of its choosing; only a verified certificate tells it apart
    /// (ADR-0051).
    pub verified_credentials_only: bool,
    /// Registries whose TLS certificate is not verified
    /// (`--skip-tls-verify`, `--skip-tls-verify-pull`,
    /// `--skip-tls-verify-registry`).
    pub skip_tls_verify: BTreeSet<String>,
    /// Extra CA certificate files, each trusted for one registry only
    /// (`--registry-certificate <registry>=<path>`).
    pub registry_certificates: Vec<(String, PathBuf)>,
    /// How failed requests are retried.
    pub retry: RetryPolicy,
    /// kaniko's `--image-download-retry`: how many more times
    /// [`Client::pull_image_manifest`] starts over after it failed, each
    /// attempt with [`Self::retry`] inside, waiting 1 s, then twice as long
    /// each time ([`retry_kaniko`](crate::retry_kaniko)). kaniko retries the
    /// image's manifest fetch this way, not its blobs
    /// (`kaniko@44aa7ddc0 pkg/image/remote/remote.go:79,121`). 0 by default.
    pub manifest_retries: u32,
    /// Bound on establishing a connection; Go's default dialer uses 30 s.
    pub connect_timeout: Option<Duration>,
    /// Bound on waiting for data on an open connection.
    pub read_timeout: Option<Duration>,
    /// The `User-Agent` header.
    pub user_agent: &'static str,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            insecure: BTreeSet::new(),
            http_credentials: BTreeSet::new(),
            verified_credentials_only: false,
            skip_tls_verify: BTreeSet::new(),
            registry_certificates: Vec::new(),
            retry: RetryPolicy::default(),
            manifest_retries: 0,
            connect_timeout: Some(Duration::from_secs(30)),
            read_timeout: None,
            user_agent: concat!("ranikust/", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// A manifest or index exactly as stored in the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawManifest {
    media_type: MediaType,
    digest: Digest,
    bytes: Bytes,
}

impl RawManifest {
    /// A manifest to push: `bytes` with their sha256 digest.
    pub fn new(media_type: MediaType, bytes: impl Into<Bytes>) -> Self {
        let bytes = bytes.into();
        let hash: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        Self {
            media_type,
            digest: Digest::from_sha256_bytes(&hash),
            bytes,
        }
    }

    /// The media type: an image manifest or an index, OCI or Docker.
    pub fn media_type(&self) -> &MediaType {
        &self.media_type
    }

    /// The digest of [`RawManifest::bytes`].
    pub fn digest(&self) -> &Digest {
        &self.digest
    }

    /// The exact bytes.
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// The length of the bytes.
    pub fn size(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    /// A descriptor of this manifest.
    pub fn descriptor(&self) -> Descriptor {
        Descriptor::new(self.media_type.clone(), self.digest.clone(), self.size())
    }
}

/// An image's manifest for one platform, and the index it was picked from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageManifests {
    /// The image manifest.
    pub manifest: RawManifest,
    /// The index the reference named, when it named one.
    pub index: Option<RawManifest>,
}

/// What a blob push did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// The registry already had the blob.
    Existed,
    /// The blob was mounted from another repository of the same registry.
    Mounted,
    /// The blob was uploaded.
    Uploaded,
}

/// A registry client for one build: pulls and pushes manifests and blobs,
/// verifying digests and sizes, with retries.
///
/// Connections, tokens and per-registry settings are created on first use
/// of each registry and kept for the client's life. Needs a tokio runtime
/// with the time driver.
pub struct Client {
    options: ClientOptions,
    credentials: Credentials,
    trust: TrustStore,
    /// Trust stores for registries with their own certificates.
    trust_per_registry: BTreeMap<String, TrustStore>,
    hosts: Mutex<HashMap<String, Arc<Host>>>,
}

/// Shows nothing of the client: its credentials must never reach a log.
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

/// One registry's connection state.
struct Host {
    /// The registry as `rnk_oci::Reference` names it.
    name: String,
    oci: oci_client::Client,
    strategy: Strategy,
    /// Scopes (repository, operation) already authorized; for identity
    /// tokens, with the token.
    scopes: Mutex<BTreeMap<(String, RegistryOperation), Option<ScopeToken>>>,
    /// The client for the identity-token exchange and mounts.
    http: reqwest::Client,
    /// `<scheme>://<endpoint>`, for requests sent without `oci-client`.
    base_url: String,
    /// The `Bearer` challenge of `GET /v2/`, if the registry sent one.
    challenge: Option<Challenge>,
    /// Mount tokens by (destination, source) repository.
    mount_tokens: Mutex<BTreeMap<(String, String), ScopeToken>>,
    /// Why configured credentials are held back, if they are (see
    /// [`withheld_because`]).
    withheld: Option<Withheld>,
}

/// How requests reach a registry, as far as sending credentials goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    /// Plain HTTP.
    PlainHttp,
    /// HTTPS whose certificate is not verified (`--skip-tls-verify*`).
    UnverifiedTls,
    /// HTTPS with a verified certificate.
    VerifiedTls,
}

impl Transport {
    fn new(scheme: &str, skip_verify: bool) -> Self {
        if scheme == "http" {
            Self::PlainHttp
        } else if skip_verify {
            Self::UnverifiedTls
        } else {
            Self::VerifiedTls
        }
    }
}

/// Why configured credentials are held back from a registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Withheld {
    /// Plain HTTP to a registry the operator did not name for it (T11).
    PlainHttp,
    /// Kaniko mode on `/`: only verified TLS carries them, so neither plain
    /// HTTP nor an unverified certificate does (ADR-0051).
    VerifiedTlsOnly {
        /// The registry is a loopback one not listed under `--insecure*`,
        /// reached over plain HTTP with HTTPS never tried (ADR-0032).
        https_untried: bool,
    },
}

/// The reason, and how to send them, for an error or a warning.
impl fmt::Display for Withheld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlainHttp => f.write_str(WITHHELD),
            Self::VerifiedTlsOnly { https_untried } => {
                f.write_str(WITHHELD_UNVERIFIED)?;
                if *https_untried {
                    f.write_str(HTTPS_UNTRIED)?;
                }
                Ok(())
            }
        }
    }
}

/// Why configured credentials are not sent, and how to send them.
const WITHHELD: &str = "its credentials are not sent over plain HTTP to a \
    registry the operator did not name for it; list it with \
    --insecure-registry (or, for a destination, use --insecure) to send them";

/// Why configured credentials are not sent in kaniko mode on `/`.
const WITHHELD_UNVERIFIED: &str = "in kaniko mode on / its credentials go only over \
    verified TLS: a RUN can edit /etc/hosts and /etc/resolv.conf, so plain HTTP or an \
    unverified certificate could carry them to a host of its choosing; use the registry \
    over HTTPS with a certificate ranikust trusts (--registry-certificate) and do not \
    list it under --skip-tls-verify*";

/// Added to [`WITHHELD_UNVERIFIED`] for a loopback registry not listed
/// under `--insecure*`, which ranikust reaches over plain HTTP only.
const HTTPS_UNTRIED: &str =
    " (for a loopback registry, list it with --insecure-registry so HTTPS is tried)";

/// Why configured credentials must not be sent to `registry`, reached
/// through `transport`; `None` when they may be.
///
/// Unlike BuildKit's, a ranikust `RUN` shares the host's network namespace
/// and may sniff `lo` or serve a loopback port: credentials go over plain
/// HTTP only to a registry the operator named for it (`http_credentials`,
/// threat model T11). Any other plain-HTTP registry, a loopback one or one
/// only `--insecure-pull` allows, is accessed anonymously. With
/// `verified_credentials_only` (kaniko mode on `/`), nothing but verified
/// TLS carries them (ADR-0051).
fn withheld_because(
    options: &ClientOptions,
    registry: &str,
    transport: Transport,
) -> Option<Withheld> {
    match transport {
        Transport::VerifiedTls => None,
        Transport::PlainHttp | Transport::UnverifiedTls if options.verified_credentials_only => {
            Some(Withheld::VerifiedTlsOnly {
                https_untried: transport == Transport::PlainHttp
                    && is_loopback(registry)
                    && !options.insecure.contains(registry),
            })
        }
        Transport::PlainHttp if !options.http_credentials.contains(registry) => {
            Some(Withheld::PlainHttp)
        }
        Transport::PlainHttp | Transport::UnverifiedTls => None,
    }
}

/// Whether `e` is a registry refusing access: an authentication failure,
/// or 401/403 and the distribution codes for them.
fn is_refusal(e: &Error) -> bool {
    match e {
        Error::Auth { .. } => true,
        Error::Registry { message, .. } => {
            message.contains("401")
                || message.contains("403")
                || message.contains("Unauthorized")
                || message.contains("Denied")
        }
        Error::Tls(_)
        | Error::Credentials { .. }
        | Error::Reference { .. }
        | Error::DigestMismatch { .. }
        | Error::SizeMismatch { .. }
        | Error::Manifest { .. }
        | Error::Io { .. } => false,
    }
}

/// The scopes a cross-repository mount needs: push to the destination,
/// pull from the source.
fn mount_scopes(repository: &str, from_repository: &str) -> [String; 2] {
    [
        format!("repository:{repository}:pull,push"),
        format!("repository:{from_repository}:pull"),
    ]
}

/// An access token from the identity-token exchange and when to refresh it.
type ScopeToken = (String, Instant);

/// How requests to one registry are authorized, from its `/v2/` answer and
/// the credential.
enum Strategy {
    /// No authorization: the registry is open, or nothing can answer its
    /// challenge.
    None,
    /// `oci-client` runs the token or basic flow, once per scope.
    Oci(RegistryAuth),
    /// The identity-token exchange at this challenge.
    Identity {
        refresh_token: String,
        challenge: Challenge,
    },
}

/// What the registry's `GET /v2/` asked for.
enum Ping {
    Open,
    Basic,
    Bearer(Challenge),
}

impl Client {
    /// A client with `options`, trusting `trust` (see [`TrustStore::load`])
    /// and authenticating with `credentials`.
    ///
    /// Reads every `registry_certificates` file now, so a bad path fails
    /// before any transfer.
    pub fn new(
        options: ClientOptions,
        trust: TrustStore,
        credentials: Credentials,
    ) -> Result<Self, Error> {
        install_crypto_provider();
        let mut trust_per_registry: BTreeMap<String, TrustStore> = BTreeMap::new();
        for (registry, path) in &options.registry_certificates {
            let base = trust_per_registry.get(registry).unwrap_or(&trust);
            let store = base.with_pem_file(path)?;
            trust_per_registry.insert(registry.clone(), store);
        }
        Ok(Self {
            options,
            credentials,
            trust,
            trust_per_registry,
            hosts: Mutex::new(HashMap::new()),
        })
    }

    /// Pulls a manifest or index by tag or digest, verifying its digest
    /// against the reference's digest, if any, and the registry's
    /// `Docker-Content-Digest`, if sent.
    pub async fn pull_manifest(&self, reference: &Reference) -> Result<RawManifest, Error> {
        let r = self.pull_manifest_once(reference).await;
        self.hint(reference.registry(), r).await
    }

    async fn pull_manifest_once(&self, reference: &Reference) -> Result<RawManifest, Error> {
        let oci_ref = oci_reference(reference)?;
        let host = self.host(reference.registry()).await?;
        self.authorize(
            &host,
            &oci_ref,
            reference.repository(),
            RegistryOperation::Pull,
        )
        .await?;

        let target = reference.to_string();
        let ctx = Op {
            operation: "pull manifest",
            host: &host.name,
            target: &target,
        };
        let span = debug_span!(
            "rnk_registry.pull_manifest",
            host = host.name.as_str(),
            requests = Empty,
            retries = Empty,
            bytes_in = Empty,
        );
        let mut bytes_in = 0u64;
        let (result, retries) = retry(&self.options.retry, async |_| {
            // The authorization stored by `authorize` wins over this one.
            let (body, digest) = host
                .oci
                .pull_manifest_raw(&oci_ref, &RegistryAuth::Anonymous, &ACCEPTED_MANIFESTS)
                .await
                .map_err(|e| ctx.error(e))?;
            bytes_in += u64::try_from(body.len()).unwrap_or(u64::MAX);
            raw_manifest(body, &digest).map_err(|reason| Error::Manifest {
                host: host.name.clone(),
                target: target.clone(),
                reason,
            })
        })
        .instrument(span.clone())
        .await;
        span.record("requests", u64::from(retries) + 1);
        span.record("retries", retries);
        span.record("bytes_in", bytes_in);
        result
    }

    /// Pulls the image manifest for `platform`: the manifest the reference
    /// names, or the one picked from the index it names.
    pub async fn pull_image_manifest(
        &self,
        reference: &Reference,
        platform: &Platform,
    ) -> Result<ImageManifests, Error> {
        crate::retry_kaniko(self.options.manifest_retries, async || {
            self.pull_image_manifest_once(reference, platform).await
        })
        .await
    }

    async fn pull_image_manifest_once(
        &self,
        reference: &Reference,
        platform: &Platform,
    ) -> Result<ImageManifests, Error> {
        let top = self.pull_manifest(reference).await?;
        if top.media_type().is_manifest() {
            return Ok(ImageManifests {
                manifest: top,
                index: None,
            });
        }
        let fail = |reason: String| Error::Manifest {
            host: reference.registry().to_owned(),
            target: reference.to_string(),
            reason,
        };
        let index = ImageIndex::from_json(top.bytes()).map_err(|e| fail(e.to_string()))?;
        let entry = select_platform(&index, platform).ok_or_else(|| {
            let variant = platform
                .variant
                .as_deref()
                .map(|v| format!("/{v}"))
                .unwrap_or_default();
            fail(format!(
                "no manifest for platform {}/{}{variant}",
                platform.os, platform.architecture
            ))
        })?;
        let child_ref = reference.clone().with_digest(entry.digest.clone());
        let manifest = self.pull_manifest(&child_ref).await?;
        if !manifest.media_type().is_manifest() {
            return Err(fail(format!(
                "the index entry for the platform is a {}, not an image manifest",
                manifest.media_type()
            )));
        }
        if manifest.size() != entry.size {
            return Err(fail(format!(
                "the platform's manifest is {} bytes, the index says {}",
                manifest.size(),
                entry.size
            )));
        }
        Ok(ImageManifests {
            manifest,
            index: Some(top),
        })
    }

    /// Streams the blob `blob` of `reference`'s repository into `out`,
    /// verifying its digest and size.
    ///
    /// Never writes more than `blob.size` bytes. A retry resets `out` and
    /// starts over; on error the caller discards what `out` holds.
    pub async fn pull_blob(
        &self,
        reference: &Reference,
        blob: &Descriptor,
        out: &mut impl BlobSink,
    ) -> Result<(), Error> {
        let r = self.pull_blob_once(reference, blob, out).await;
        self.hint(reference.registry(), r).await
    }

    async fn pull_blob_once(
        &self,
        reference: &Reference,
        blob: &Descriptor,
        out: &mut impl BlobSink,
    ) -> Result<(), Error> {
        let oci_ref = oci_reference(reference)?;
        let host = self.host(reference.registry()).await?;
        self.authorize(
            &host,
            &oci_ref,
            reference.repository(),
            RegistryOperation::Pull,
        )
        .await?;

        let digest = blob.digest.as_str();
        let ctx = Op {
            operation: "pull blob",
            host: &host.name,
            target: digest,
        };
        let span = debug_span!(
            "rnk_registry.pull_blob",
            host = host.name.as_str(),
            requests = Empty,
            retries = Empty,
            bytes_in = Empty,
        );
        let mut bytes_in = 0u64;
        let (result, retries) = retry(&self.options.retry, async |attempt| {
            if attempt > 0 {
                out.reset().await.map_err(|source| Error::Io {
                    what: format!("resetting the destination of {digest}"),
                    source,
                })?;
            }
            let mut bounded = Bounded::new(&mut *out, blob.size);
            let result = host.oci.pull_blob(&oci_ref, digest, &mut bounded).await;
            bytes_in += bounded.written();
            let size_error = |actual| Error::SizeMismatch {
                host: host.name.clone(),
                digest: digest.to_owned(),
                expected: blob.size,
                actual,
            };
            let (overflow, written) = (bounded.overflow(), bounded.written());
            match (result, overflow) {
                (Err(_), Some(received)) => Err(size_error(received)),
                // A sink that took bytes it cannot give back is not retried:
                // no wait, and this first error is the one reported.
                (Err(e), None) if !out.can_restart() => Err(ctx.error(e).not_retryable()),
                (Err(e), None) => Err(ctx.error(e)),
                (Ok(()), _) if written != blob.size => Err(size_error(written)),
                (Ok(()), _) => Ok(()),
            }
        })
        .instrument(span.clone())
        .await;
        span.record("requests", u64::from(retries) + 1);
        span.record("retries", retries);
        span.record("bytes_in", bytes_in);
        result
    }

    /// Opens the blob `blob` of `reference`'s repository for blocking reads
    /// while it is pulled, through a pipe of [`crate::PIPE_BYTES`]: a base
    /// layer extracted as it arrives, never kept (ADR-0055). The pull is
    /// retried only until the first byte of the body arrives; the reader
    /// fails if the blob's size or digest does not match, after the bytes
    /// that arrived, and every read after a failure fails again. `handle`'s runtime drives the
    /// pull; read it on a thread outside that runtime's workers.
    ///
    /// # Errors
    ///
    /// When the pull's thread cannot start.
    pub fn open_blob(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        reference: &Reference,
        blob: &Descriptor,
    ) -> io::Result<crate::BlobReader> {
        crate::BlobReader::open(handle, self, reference, blob)
    }

    /// Whether `reference`'s repository holds the blob `digest`.
    ///
    /// Not a span of its own: [`Client::push_blob`] checks existence inside
    /// its span.
    pub async fn blob_exists(&self, reference: &Reference, digest: &Digest) -> Result<bool, Error> {
        let r = self.blob_exists_once(reference, digest).await;
        self.hint(reference.registry(), r).await
    }

    async fn blob_exists_once(
        &self,
        reference: &Reference,
        digest: &Digest,
    ) -> Result<bool, Error> {
        let oci_ref = oci_reference(reference)?;
        let host = self.host(reference.registry()).await?;
        self.authorize(
            &host,
            &oci_ref,
            reference.repository(),
            RegistryOperation::Pull,
        )
        .await?;
        let ctx = Op {
            operation: "check blob",
            host: &host.name,
            target: digest.as_str(),
        };
        retry(&self.options.retry, async |_| {
            host.oci
                .blob_exists(&oci_ref, digest.as_str())
                .await
                .map_err(|e| ctx.error(e))
        })
        .await
        .0
    }

    /// Pushes the blob `blob` to `reference`'s repository, unless it is
    /// already there, as one streamed `PUT`.
    pub async fn push_blob(
        &self,
        reference: &Reference,
        blob: &Descriptor,
        body: &BlobBody,
    ) -> Result<PushOutcome, Error> {
        let r = self.push(reference, None, blob, body).await;
        self.hint(reference.registry(), r).await
    }

    /// Like [`Client::push_blob`], but first asks the registry to mount the
    /// blob from `from`, another repository of the same registry; when the
    /// registry refuses (`202`) or fails, uploads, as kaniko does
    /// (`kaniko@44aa7ddc0 pkg/executor/push.go:318-366`; ADR-0032).
    pub async fn mount_or_push(
        &self,
        reference: &Reference,
        from: &Reference,
        blob: &Descriptor,
        body: &BlobBody,
    ) -> Result<PushOutcome, Error> {
        let r = self.push(reference, Some(from), blob, body).await;
        self.hint(reference.registry(), r).await
    }

    async fn push(
        &self,
        reference: &Reference,
        from: Option<&Reference>,
        blob: &Descriptor,
        body: &BlobBody,
    ) -> Result<PushOutcome, Error> {
        let oci_ref = oci_reference(reference)?;
        let from = from
            .filter(|f| {
                f.registry() == reference.registry() && f.repository() != reference.repository()
            })
            .map(|f| oci_reference(f).map(|o| (o, f.repository())))
            .transpose()?;
        let host = self.host(reference.registry()).await?;
        self.authorize(
            &host,
            &oci_ref,
            reference.repository(),
            RegistryOperation::Push,
        )
        .await?;

        let digest = blob.digest.as_str();
        let ctx = Op {
            operation: "push blob",
            host: &host.name,
            target: digest,
        };
        let span = debug_span!(
            "rnk_registry.push_blob",
            host = host.name.as_str(),
            requests = Empty,
            retries = Empty,
            bytes_out = Empty,
            mounted = Empty,
            existed = Empty,
        );
        let sent = Arc::new(AtomicU64::new(0));
        let mut requests = 0u64;
        let mut retries = 0u32;
        let result = async {
            let (exists, r) = retry(&self.options.retry, async |_| {
                requests += 1;
                host.oci
                    .blob_exists(&oci_ref, digest)
                    .await
                    .map_err(|e| ctx.error(e))
            })
            .await;
            retries += r;
            if exists? {
                return Ok(PushOutcome::Existed);
            }
            if let Some((from_ref, from_repository)) = &from {
                // A refusal (`202`, which `oci-client` reports as a spec
                // violation) or any other failure falls back to an upload.
                let (mounted, sent) = self
                    .mount(
                        &host,
                        &oci_ref,
                        reference.repository(),
                        from_ref,
                        from_repository,
                        digest,
                    )
                    .await;
                requests += sent;
                if mounted {
                    return Ok(PushOutcome::Mounted);
                }
            }
            let (uploaded, r) = retry(&self.options.retry, async |_| {
                let io_error = |source| Error::Io {
                    what: body.describe(),
                    source,
                };
                let (stream, len) = body.open(Arc::clone(&sent)).await.map_err(io_error)?;
                if len != blob.size {
                    return Err(io_error(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{len} bytes to push for {digest}, the descriptor says {}",
                            blob.size
                        ),
                    )));
                }
                // A session `POST`, then one streamed `PUT`.
                requests += 2;
                host.oci
                    .push_blob_stream(&oci_ref, stream, digest, Some(len))
                    .await
                    .map_err(|e| ctx.error(e))
            })
            .await;
            retries += r;
            uploaded.map(|_location| PushOutcome::Uploaded)
        }
        .instrument(span.clone())
        .await;
        span.record("requests", requests);
        span.record("retries", retries);
        span.record("bytes_out", sent.load(Ordering::Relaxed));
        if let Ok(outcome) = &result {
            span.record("mounted", *outcome == PushOutcome::Mounted);
            span.record("existed", *outcome == PushOutcome::Existed);
        }
        result
    }

    /// Asks the registry to mount `digest` from `from_repository` into
    /// `repository`. Returns whether it did, and the requests sent.
    ///
    /// On a token registry the mount needs pull access to the source as well
    /// as push access to the destination, in one token, as
    /// go-containerregistry asks for (`kaniko@44aa7ddc0
    /// vendor/github.com/google/go-containerregistry/pkg/v1/remote/write.go:140-155`,
    /// `maybeUpdateScopes`);
    /// `oci-client` only knows one scope per token, so that token is fetched
    /// here and the mount sent with it. Without a token flow (an open
    /// registry, basic authentication, a static registry token) the push
    /// authorization is used as is.
    async fn mount(
        &self,
        host: &Host,
        oci_ref: &oci_client::Reference,
        repository: &str,
        from_ref: &oci_client::Reference,
        from_repository: &str,
        digest: &str,
    ) -> (bool, u64) {
        let token = match (&host.strategy, &host.challenge) {
            (Strategy::None, _)
            | (Strategy::Oci(RegistryAuth::Bearer(_)), _)
            | (Strategy::Oci(RegistryAuth::Anonymous | RegistryAuth::Basic(..)), None) => {
                let mounted = host.oci.mount_blob(oci_ref, from_ref, digest).await.is_ok();
                return (mounted, 1);
            }
            (Strategy::Oci(auth), Some(challenge)) => {
                self.mount_token(host, challenge, auth, repository, from_repository)
                    .await
            }
            (
                Strategy::Identity {
                    refresh_token,
                    challenge,
                },
                _,
            ) => {
                let scopes = mount_scopes(repository, from_repository);
                token::exchange(&host.http, challenge, refresh_token, &scopes.join(" "))
                    .await
                    .map(|t| t.token)
                    .ok()
            }
        };
        let Some(token) = token else {
            // No token for both scopes: the upload still works with the push
            // token, so the mount is skipped rather than failed.
            return (false, 1);
        };
        let url = format!("{}/v2/{repository}/blobs/uploads/", host.base_url);
        let mounted = host
            .http
            .post(url)
            .query(&[("mount", digest), ("from", from_repository)])
            .bearer_auth(token)
            .send()
            .await
            .is_ok_and(|r| r.status() == reqwest::StatusCode::CREATED);
        (mounted, 2)
    }

    /// A token for a mount's two scopes through the challenge's realm,
    /// cached per pair of repositories; `None` when the realm refuses.
    async fn mount_token(
        &self,
        host: &Host,
        challenge: &Challenge,
        auth: &RegistryAuth,
        repository: &str,
        from_repository: &str,
    ) -> Option<String> {
        let key = (repository.to_owned(), from_repository.to_owned());
        let mut cache = host.mount_tokens.lock().await;
        if let Some((token, refresh_after)) = cache.get(&key)
            && Instant::now() < *refresh_after
        {
            return Some(token.clone());
        }
        let basic = match auth {
            RegistryAuth::Basic(user, password) => Some((user.as_str(), password.as_str())),
            RegistryAuth::Anonymous | RegistryAuth::Bearer(_) => None,
        };
        let scopes = mount_scopes(repository, from_repository);
        let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
        let access = token::fetch(&host.http, challenge, basic, &scopes)
            .await
            .ok()?;
        cache.insert(key, (access.token.clone(), access.refresh_after));
        Some(access.token)
    }

    /// Pushes `manifest` under `reference`'s tag and returns its digest.
    ///
    /// The reference must have a tag and no digest: `oci-client` panics on a
    /// digest reference when the registry omits `Location`, and release
    /// builds abort on panic.
    pub async fn push_manifest(
        &self,
        reference: &Reference,
        manifest: &RawManifest,
    ) -> Result<Digest, Error> {
        let r = self.push_manifest_once(reference, manifest).await;
        self.hint(reference.registry(), r).await
    }

    async fn push_manifest_once(
        &self,
        reference: &Reference,
        manifest: &RawManifest,
    ) -> Result<Digest, Error> {
        if reference.digest().is_some() || reference.tag().is_none() {
            return Err(Error::Reference {
                reference: reference.to_string(),
                reason: "a manifest is pushed by tag, without a digest".to_owned(),
            });
        }
        let oci_ref = oci_reference(reference)?;
        let host = self.host(reference.registry()).await?;
        self.authorize(
            &host,
            &oci_ref,
            reference.repository(),
            RegistryOperation::Push,
        )
        .await?;
        let target = reference.to_string();
        let content_type = reqwest::header::HeaderValue::from_str(manifest.media_type().as_str())
            .map_err(|_| Error::Manifest {
            host: host.name.clone(),
            target: target.clone(),
            reason: format!(
                "invalid media type {:?}",
                excerpt(manifest.media_type().as_str())
            ),
        })?;
        let ctx = Op {
            operation: "push manifest",
            host: &host.name,
            target: &target,
        };
        let span = debug_span!(
            "rnk_registry.push_manifest",
            host = host.name.as_str(),
            requests = Empty,
            bytes_out = Empty,
        );
        let mut bytes_out = 0u64;
        let (result, retries) = retry(&self.options.retry, async |_| {
            bytes_out += manifest.size();
            host.oci
                .push_manifest_raw(&oci_ref, manifest.bytes().clone(), content_type.clone())
                .await
                .map_err(|e| ctx.error(e))
        })
        .instrument(span.clone())
        .await;
        span.record("requests", u64::from(retries) + 1);
        span.record("bytes_out", bytes_out);
        result.map(|_location| manifest.digest().clone())
    }

    /// Adds to an access refusal by a registry whose credentials were held
    /// back (see [`Client::connect`]) why, and how to send them.
    async fn hint<T>(&self, registry: &str, r: Result<T, Error>) -> Result<T, Error> {
        let Err(e) = r else { return r };
        let withheld = self
            .hosts
            .lock()
            .await
            .get(registry)
            .and_then(|h| h.withheld);
        if let Some(why) = withheld
            && is_refusal(&e)
        {
            return Err(Error::Auth {
                host: registry.to_owned(),
                reason: format!("{e}; {why}"),
            });
        }
        Err(e)
    }

    /// The connection state for `registry`, created on first use.
    async fn host(&self, registry: &str) -> Result<Arc<Host>, Error> {
        let mut hosts = self.hosts.lock().await;
        if let Some(host) = hosts.get(registry) {
            return Ok(Arc::clone(host));
        }
        let host = Arc::new(self.connect(registry).await?);
        hosts.insert(registry.to_owned(), Arc::clone(&host));
        Ok(host)
    }

    /// Builds the clients for `registry`, finds its scheme and what it asks
    /// for, and picks how to authorize.
    async fn connect(&self, registry: &str) -> Result<Host, Error> {
        let trust = self.trust_per_registry.get(registry).unwrap_or(&self.trust);
        let skip_verify = self.options.skip_tls_verify.contains(registry);
        let mut http = reqwest::Client::builder()
            .tls_certs_only(trust.to_reqwest()?)
            .danger_accept_invalid_certs(skip_verify)
            .user_agent(self.options.user_agent)
            // `localhost` is loopback, whatever a resolver says: plain HTTP
            // is chosen for it as a loopback address.
            .resolve_to_addrs(
                "localhost",
                &[
                    std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                    std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 0)),
                ],
            );
        if let Some(timeout) = self.options.connect_timeout {
            http = http.connect_timeout(timeout);
        }
        if let Some(timeout) = self.options.read_timeout {
            http = http.read_timeout(timeout);
        }
        let http = http
            .build()
            .map_err(|e| Error::Tls(format!("HTTP client for {registry}: {e}")))?;

        let endpoint = endpoint(registry);
        let span = debug_span!("rnk_registry.auth", host = registry, requests = Empty);
        let mut requests = 0u64;
        let pinged = async {
            // As BuildKit (ADR-0032, Q-065): a loopback registry not listed
            // as insecure is plain HTTP, HTTPS not even tried
            // (`util/resolver/resolver.go`); a listed one tries HTTPS, then
            // HTTP. Private-network addresses are not special.
            let insecure = self.options.insecure.contains(registry);
            if !insecure && is_loopback(registry) {
                let plain = retry(&self.options.retry, async |_| {
                    requests += 1;
                    ping(&http, "http", endpoint, registry, false).await
                })
                .await
                .0?;
                return Ok(("http", plain));
            }
            let https = retry(&self.options.retry, async |_| {
                requests += 1;
                ping(&http, "https", endpoint, registry, insecure).await
            })
            .await
            .0;
            match https {
                Err(_) if insecure => {
                    let plain = retry(&self.options.retry, async |_| {
                        requests += 1;
                        ping(&http, "http", endpoint, registry, false).await
                    })
                    .await
                    .0?;
                    Ok(("http", plain))
                }
                other => other.map(|ping| ("https", ping)),
            }
        }
        .instrument(span.clone())
        .await;
        span.record("requests", requests);
        let (scheme, ping) = pinged?;

        let config = ClientConfig {
            protocol: if scheme == "http" {
                ClientProtocol::Http
            } else {
                ClientProtocol::Https
            },
            accept_invalid_certificates: skip_verify,
            use_monolithic_push: true,
            tls_certs_only: trust.to_oci(),
            platform_resolver: None,
            connect_timeout: self.options.connect_timeout,
            read_timeout: self.options.read_timeout,
            user_agent: self.options.user_agent,
            ..ClientConfig::default()
        };
        let oci = oci_client::Client::try_from(config).map_err(|e| {
            Error::Tls(format!(
                "registry client for {registry}: {}",
                excerpt(&e.to_string())
            ))
        })?;

        let configured = self.credentials.get(registry);
        // Only configured credentials can be held back.
        let withheld = if configured.is_some() {
            withheld_because(&self.options, registry, Transport::new(scheme, skip_verify))
        } else {
            None
        };
        let credential = if withheld.is_some() { None } else { configured };
        if let Some(why) = withheld {
            if matches!(ping, Ping::Basic) {
                return Err(Error::Auth {
                    host: registry.to_owned(),
                    reason: format!("it asks for basic authentication; {why}"),
                });
            }
            tracing::warn!(registry, "{why}; accessing it anonymously");
        }
        if configured.is_none()
            && let Some(helper) = self.credentials.helper(registry)
        {
            tracing::warn!(
                registry,
                helper,
                "credential helpers are not supported yet; accessing the registry anonymously"
            );
        }
        let challenge = match &ping {
            Ping::Bearer(challenge) => Some(challenge.clone()),
            Ping::Open | Ping::Basic => None,
        };
        let strategy = strategy(registry, scheme, ping, credential)?;
        if matches!(strategy, Strategy::Identity { .. }) {
            // `oci-client` consults its token cache only for registries with
            // a stored authorization; the tokens are inserted per scope.
            oci.store_auth_if_needed(endpoint, &RegistryAuth::Anonymous)
                .await;
        }
        Ok(Host {
            name: registry.to_owned(),
            oci,
            strategy,
            scopes: Mutex::new(BTreeMap::new()),
            http,
            base_url: format!("{scheme}://{endpoint}"),
            challenge,
            mount_tokens: Mutex::new(BTreeMap::new()),
            withheld,
        })
    }

    /// Makes sure requests for `repository` and `op` carry an authorization.
    async fn authorize(
        &self,
        host: &Host,
        oci_ref: &oci_client::Reference,
        repository: &str,
        op: RegistryOperation,
    ) -> Result<(), Error> {
        if matches!(host.strategy, Strategy::None) {
            return Ok(());
        }
        // Held across the token request: parallel transfers of one
        // repository wait for one token instead of each fetching their own.
        let mut scopes = host.scopes.lock().await;
        let key = (repository.to_owned(), op);
        match scopes.get(&key) {
            Some(None) => return Ok(()),
            Some(Some((token, refresh_after))) if Instant::now() < *refresh_after => {
                insert_bearer(&host.oci, oci_ref, op, token).await;
                return Ok(());
            }
            Some(Some(_)) | None => {}
        }

        let span = debug_span!(
            "rnk_registry.auth",
            host = host.name.as_str(),
            requests = Empty
        );
        let mut requests = 0u64;
        let ctx = Op {
            operation: "authenticate",
            host: &host.name,
            target: repository,
        };
        let result = async {
            match &host.strategy {
                Strategy::None => Ok(()),
                Strategy::Oci(auth) => {
                    let (result, _) = retry(&self.options.retry, async |_| {
                        let token = host.oci.auth(oci_ref, auth, op).await;
                        // `GET /v2/`, then the realm when a token came back;
                        // a static bearer token needs no request.
                        requests += match (auth, &token) {
                            (RegistryAuth::Bearer(_), _) => 0,
                            (_, Ok(Some(_))) => 2,
                            (_, Ok(None) | Err(_)) => 1,
                        };
                        token.map_err(|e| ctx.error(e))
                    })
                    .await;
                    result?;
                    scopes.insert(key, None);
                    Ok(())
                }
                Strategy::Identity {
                    refresh_token,
                    challenge,
                } => {
                    let scope = match op {
                        RegistryOperation::Pull => format!("repository:{repository}:pull"),
                        RegistryOperation::Push => format!("repository:{repository}:pull,push"),
                    };
                    let (result, _) = retry(&self.options.retry, async |_| {
                        requests += 1;
                        token::exchange(&host.http, challenge, refresh_token, &scope)
                            .await
                            .map_err(|e| exchange_error(&ctx, e))
                    })
                    .await;
                    let access = result?;
                    insert_bearer(&host.oci, oci_ref, op, &access.token).await;
                    if op == RegistryOperation::Push {
                        // A push token also allows pulls, which a push makes
                        // to check whether a blob exists.
                        insert_bearer(&host.oci, oci_ref, RegistryOperation::Pull, &access.token)
                            .await;
                        scopes.insert(
                            (repository.to_owned(), RegistryOperation::Pull),
                            Some((access.token.clone(), access.refresh_after)),
                        );
                    }
                    scopes.insert(key, Some((access.token, access.refresh_after)));
                    Ok(())
                }
            }
        }
        .instrument(span.clone())
        .await;
        span.record("requests", requests);
        result
    }
}

/// Whether `registry` (`host[:port]`) is a loopback address, as
/// containerd's `MatchLocalhost`: `localhost`, `127.0.0.0/8` or `::1`,
/// with or without a port. Like it, names that only resolve to loopback,
/// and addresses in octal, decimal or hex form, do not count.
fn is_loopback(registry: &str) -> bool {
    if registry == "::1" || registry == "[::1]" {
        return true;
    }
    let host = match registry.rsplit_once(':') {
        // `[v6]:port`, or `v4:port` and `name:port`.
        Some((host, port)) if !host.contains(':') || host.starts_with('[') => {
            if port.is_empty() {
                return false;
            }
            host.trim_start_matches('[').trim_end_matches(']')
        }
        // A bare IPv6 address, or no port at all.
        _ => registry,
    };
    let host = if host == "localhost" {
        "127.0.0.1"
    } else {
        host
    };
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// `GET /v2/`: what the registry asks for.
///
/// With `quiet_tls`, a failed request is not retried: an insecure registry
/// that does not speak HTTPS falls back to HTTP at once.
async fn ping(
    http: &reqwest::Client,
    scheme: &str,
    endpoint: &str,
    registry: &str,
    quiet_tls: bool,
) -> Result<Ping, Error> {
    let ctx = Op {
        operation: "ping",
        host: registry,
        target: "/v2/",
    };
    let response = http
        .get(format!("{scheme}://{endpoint}/v2/"))
        .send()
        .await
        .map_err(|e| {
            let err = ctx.error(OciDistributionError::RequestError(e));
            if quiet_tls { not_retryable(err) } else { err }
        })?;
    let status = response.status().as_u16();
    match status {
        200..=299 => Ok(Ping::Open),
        401 => {
            let challenge = response
                .headers()
                .get_all(reqwest::header::WWW_AUTHENTICATE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find_map(token::parse_challenge);
            Ok(challenge.map_or(Ping::Basic, Ping::Bearer))
        }
        _ => Err(Error::Registry {
            operation: "ping",
            host: registry.to_owned(),
            target: "/v2/".to_owned(),
            message: format!("HTTP {status}"),
            retryable: retryable_status(status),
        }),
    }
}

/// How to authorize, from the registry's challenge and the credential.
fn strategy(
    registry: &str,
    scheme: &str,
    ping: Ping,
    credential: Option<&Credential>,
) -> Result<Strategy, Error> {
    let challenge = match ping {
        Ping::Open => return Ok(Strategy::None),
        Ping::Basic => {
            return Ok(match credential {
                Some(Credential::Basic { username, password }) => {
                    Strategy::Oci(RegistryAuth::Basic(username.clone(), password.clone()))
                }
                Some(Credential::RegistryToken(token)) => {
                    Strategy::Oci(RegistryAuth::Bearer(token.clone()))
                }
                // Basic authentication has no use for an OAuth2 refresh token.
                Some(Credential::IdentityToken(_)) | None => Strategy::None,
            });
        }
        Ping::Bearer(challenge) => challenge,
    };
    // A credential never travels in clear text to a realm when the registry
    // itself speaks HTTPS.
    if credential.is_some() && scheme == "https" && !challenge.realm.starts_with("https://") {
        return Err(Error::Auth {
            host: registry.to_owned(),
            reason: format!(
                "the token service {:?} is not HTTPS; refusing to send credentials",
                excerpt(&challenge.realm)
            ),
        });
    }
    Ok(match credential {
        None => Strategy::Oci(RegistryAuth::Anonymous),
        Some(Credential::Basic { username, password }) => {
            Strategy::Oci(RegistryAuth::Basic(username.clone(), password.clone()))
        }
        Some(Credential::RegistryToken(token)) => {
            Strategy::Oci(RegistryAuth::Bearer(token.clone()))
        }
        Some(Credential::IdentityToken(token)) => Strategy::Identity {
            refresh_token: token.clone(),
            challenge,
        },
    })
}

/// Puts an access token into `oci-client`'s cache for one scope.
async fn insert_bearer(
    oci: &oci_client::Client,
    oci_ref: &oci_client::Reference,
    op: RegistryOperation,
    token: &str,
) {
    let token = RegistryTokenType::Bearer(RegistryToken::Token {
        token: token.to_owned(),
    });
    oci.tokens.insert(oci_ref, op, token).await;
}

fn exchange_error(ctx: &Op<'_>, err: ExchangeError) -> Error {
    match err {
        ExchangeError::Request(e) => ctx.error(OciDistributionError::RequestError(e)),
        ExchangeError::Status(code) if retryable_status(code) => Error::Registry {
            operation: "token exchange",
            host: ctx.host.to_owned(),
            target: ctx.target.to_owned(),
            message: format!("HTTP {code}"),
            retryable: true,
        },
        ExchangeError::Status(code) => Error::Auth {
            host: ctx.host.to_owned(),
            reason: format!("the token service refused the identity token (HTTP {code})"),
        },
        ExchangeError::Invalid(reason) => Error::Auth {
            host: ctx.host.to_owned(),
            reason: reason.to_owned(),
        },
    }
}

fn not_retryable(err: Error) -> Error {
    match err {
        Error::Registry {
            operation,
            host,
            target,
            message,
            ..
        } => Error::Registry {
            operation,
            host,
            target,
            message,
            retryable: false,
        },
        other => other,
    }
}

/// The host requests for `registry` go to: `oci-client` sends Docker Hub
/// requests to `index.docker.io`.
fn endpoint(registry: &str) -> &str {
    if registry == Reference::DOCKER_HUB {
        "index.docker.io"
    } else {
        registry
    }
}

/// `reference` as an `oci-client` reference, built from the parts
/// `rnk_oci` parsed, never by parsing its text again: `oci-spec` splits the
/// registry off by other rules (an upper-case, dot-less first component is
/// a registry for `rnk_oci`, as for distribution, but Docker Hub for
/// `oci-spec`), so a re-parse could send a request, and the credential
/// chosen for `reference.registry()`, to another host.
///
/// Fails if `oci-client` would still address another host or repository
/// ([`same_target`]); callers convert before they connect, so nothing is
/// sent then.
fn oci_reference(reference: &Reference) -> Result<oci_client::Reference, Error> {
    let registry = reference.registry().to_owned();
    let repository = reference.repository().to_owned();
    let tag = reference.tag().map(str::to_owned);
    let digest = reference.digest().map(|d| d.as_str().to_owned());
    let oci = match (tag, digest) {
        (Some(tag), Some(digest)) => {
            oci_client::Reference::with_tag_and_digest(registry, repository, tag, digest)
        }
        (None, Some(digest)) => oci_client::Reference::with_digest(registry, repository, digest),
        (tag, None) => oci_client::Reference::with_tag(
            registry,
            repository,
            tag.unwrap_or_else(|| Reference::DEFAULT_TAG.to_owned()),
        ),
    };
    same_target(&oci, reference)?;
    Ok(oci)
}

/// Refuses `oci` unless `oci-client` would address it to the host and
/// repository [`Client::connect`] reached and chose the credential for:
/// `reference`'s endpoint and repository, exactly, since `oci-client` keys
/// its credentials and tokens on them case-sensitively.
fn same_target(oci: &oci_client::Reference, reference: &Reference) -> Result<(), Error> {
    let expected = endpoint(reference.registry());
    if oci.resolve_registry() != expected || oci.repository() != reference.repository() {
        return Err(Error::Reference {
            reference: reference.to_string(),
            reason: format!(
                "the registry client would address {}/{}, not {expected}/{}",
                oci.resolve_registry(),
                oci.repository(),
                reference.repository()
            ),
        });
    }
    Ok(())
}

/// The fields that tell a manifest's kind.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Shape {
    schema_version: Option<u32>,
    media_type: Option<String>,
    manifests: Option<IgnoredAny>,
    config: Option<IgnoredAny>,
}

/// A pulled body and the digest `oci-client` verified, as a [`RawManifest`].
fn raw_manifest(body: Bytes, digest: &str) -> Result<RawManifest, String> {
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(format!("larger than {MAX_MANIFEST_BYTES} bytes"));
    }
    let digest = Digest::parse(digest).map_err(|e| e.to_string())?;
    let shape: Shape = serde_json::from_slice(&body).map_err(|e| format!("not JSON: {e}"))?;
    if shape.schema_version != Some(2) {
        return Err(format!(
            "unsupported schema version {}",
            shape
                .schema_version
                .map_or("(none)".to_owned(), |v| v.to_string())
        ));
    }
    // An OCI manifest or index may omit `mediaType`; its fields tell.
    let media_type = match (shape.media_type, shape.manifests, shape.config) {
        (Some(media_type), _, _) => MediaType::new(media_type),
        (None, Some(_), _) => MediaType::OCI_INDEX,
        (None, None, Some(_)) => MediaType::OCI_MANIFEST,
        (None, None, None) => return Err("neither a manifest nor an index".to_owned()),
    };
    if !media_type.is_manifest() && !media_type.is_index() {
        return Err(format!(
            "unsupported media type {:?}",
            excerpt(media_type.as_str())
        ));
    }
    Ok(RawManifest {
        media_type,
        digest,
        bytes: body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_withheld_from_plain_http_and_unverified_tls_as_configured() {
        let named = "named.example";
        let other = "other.example";
        let mut options = ClientOptions::default();
        options.http_credentials.insert(named.to_owned());
        // Over verified TLS they always go; over plain HTTP only to a
        // registry named for it (T11).
        assert_eq!(
            withheld_because(&options, other, Transport::VerifiedTls),
            None
        );
        assert_eq!(
            withheld_because(&options, named, Transport::PlainHttp),
            None
        );
        assert_eq!(
            withheld_because(&options, other, Transport::PlainHttp),
            Some(Withheld::PlainHttp)
        );
        // An unverified certificate alone does not hold them back.
        assert_eq!(
            withheld_because(&options, other, Transport::UnverifiedTls),
            None
        );
        // Kaniko mode on `/`: only verified TLS, whatever the lists say
        // (ADR-0051).
        options.verified_credentials_only = true;
        assert_eq!(
            withheld_because(&options, other, Transport::VerifiedTls),
            None
        );
        for (registry, transport) in [
            (named, Transport::PlainHttp),
            (other, Transport::PlainHttp),
            (other, Transport::UnverifiedTls),
            (named, Transport::UnverifiedTls),
        ] {
            assert_eq!(
                withheld_because(&options, registry, transport),
                Some(Withheld::VerifiedTlsOnly {
                    https_untried: false
                }),
                "{registry} {transport:?}"
            );
        }
        assert_eq!(Withheld::PlainHttp.to_string(), WITHHELD);
        let advice = Withheld::VerifiedTlsOnly {
            https_untried: false,
        }
        .to_string();
        assert_eq!(advice, WITHHELD_UNVERIFIED);
        // The advice must work for both halves: plain HTTP and a registry
        // listed under `--skip-tls-verify*`.
        assert!(advice.contains("over HTTPS"), "{advice}");
        assert!(advice.contains("--skip-tls-verify"), "{advice}");
    }

    /// A loopback registry not listed under `--insecure*` is plain HTTP
    /// only (ADR-0032): the advice says how to get HTTPS tried. A listed one
    /// already tries HTTPS first, and a remote one is not loopback.
    #[test]
    fn the_verified_tls_advice_says_how_to_get_https_tried_on_loopback() {
        let mut options = ClientOptions {
            verified_credentials_only: true,
            ..ClientOptions::default()
        };
        let unlisted = withheld_because(&options, "localhost:5000", Transport::PlainHttp);
        assert_eq!(
            unlisted,
            Some(Withheld::VerifiedTlsOnly {
                https_untried: true
            })
        );
        let advice = unlisted.map(|w| w.to_string()).unwrap_or_default();
        assert!(
            advice.ends_with("list it with --insecure-registry so HTTPS is tried)"),
            "{advice}"
        );
        options.insecure.insert("localhost:5000".to_owned());
        for (registry, transport) in [
            ("localhost:5000", Transport::PlainHttp),
            ("registry.example", Transport::PlainHttp),
            ("127.0.0.1:5000", Transport::UnverifiedTls),
        ] {
            assert_eq!(
                withheld_because(&options, registry, transport),
                Some(Withheld::VerifiedTlsOnly {
                    https_untried: false
                }),
                "{registry} {transport:?}"
            );
        }
    }

    #[test]
    fn transports_are_named_from_the_scheme_and_the_verification() {
        assert_eq!(Transport::new("http", false), Transport::PlainHttp);
        assert_eq!(Transport::new("http", true), Transport::PlainHttp);
        assert_eq!(Transport::new("https", true), Transport::UnverifiedTls);
        assert_eq!(Transport::new("https", false), Transport::VerifiedTls);
    }

    #[test]
    fn loopback_registries_are_matched_as_containerd_does() {
        for host in [
            "localhost",
            "localhost:5000",
            "127.0.0.1",
            "127.0.0.1:5000",
            "127.1.2.3:80",
            "::1",
            "[::1]",
            "[::1]:5000",
        ] {
            assert!(is_loopback(host), "{host}");
        }
        for host in [
            "registry.example",
            "localhost.example:5000",
            "10.0.0.1:5000",
            "192.168.1.2",
            "registry.local",
            "128.0.0.1",
            "[::2]:5000",
            "localhost:",
        ] {
            assert!(!is_loopback(host), "{host}");
        }
    }

    #[test]
    fn accepted_media_types_are_the_manifest_and_index_types() {
        for media_type in ACCEPTED_MANIFESTS {
            let media_type = MediaType::new(media_type);
            assert!(
                media_type.is_manifest() || media_type.is_index(),
                "{media_type}"
            );
        }
        let distinct: BTreeSet<&str> = ACCEPTED_MANIFESTS.into_iter().collect();
        assert_eq!(distinct.len(), 4);
    }

    #[test]
    fn manifests_are_recognised_by_media_type_or_shape() {
        let raw = |json: &str| {
            raw_manifest(
                Bytes::from(json.to_owned()),
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            )
        };
        let docker = raw(r#"{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json"}"#).unwrap();
        assert_eq!(docker.media_type(), &MediaType::DOCKER_MANIFEST);
        assert_eq!(
            raw(r#"{"schemaVersion":2,"manifests":[]}"#)
                .unwrap()
                .media_type(),
            &MediaType::OCI_INDEX
        );
        assert_eq!(
            raw(r#"{"schemaVersion":2,"config":{}}"#)
                .unwrap()
                .media_type(),
            &MediaType::OCI_MANIFEST
        );
        assert!(raw(r#"{"schemaVersion":1,"fsLayers":[]}"#).is_err());
        assert!(raw(r#"{"schemaVersion":2,"mediaType":"text/plain"}"#).is_err());
        assert!(raw(r#"{"schemaVersion":2}"#).is_err());
        assert!(raw("not json").is_err());
    }

    #[test]
    fn credentials_never_go_to_a_plain_http_realm() {
        let challenge = Challenge {
            realm: "http://auth.example/token".to_owned(),
            service: None,
        };
        let basic = Credential::Basic {
            username: "u".to_owned(),
            password: "p".to_owned(),
        };
        let refused = strategy(
            "r.example",
            "https",
            Ping::Bearer(challenge.clone()),
            Some(&basic),
        );
        assert!(matches!(refused, Err(Error::Auth { .. })));
        let anonymous = strategy("r.example", "https", Ping::Bearer(challenge.clone()), None);
        assert!(matches!(
            anonymous,
            Ok(Strategy::Oci(RegistryAuth::Anonymous))
        ));
        let insecure = strategy("r.example", "http", Ping::Bearer(challenge), Some(&basic));
        assert!(matches!(
            insecure,
            Ok(Strategy::Oci(RegistryAuth::Basic(..)))
        ));
    }

    #[test]
    fn raw_manifests_hash_their_bytes() {
        let m = RawManifest::new(MediaType::OCI_MANIFEST, &b"{}"[..]);
        assert_eq!(
            m.digest().as_str(),
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        assert_eq!(m.descriptor().size, 2);
    }

    #[test]
    fn oci_client_addresses_the_registry_credentials_were_chosen_for() {
        // The host a credential is picked for (`Client::connect`) is the one
        // `oci-client` sends requests and that credential to. `agrees`: the
        // string form `oci-spec` parses gives the same reference.
        const DIGEST: &str =
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let digest_only = format!("debian@{DIGEST}");
        let tag_and_digest = format!("registry.example.com/app:1@{DIGEST}");
        for (text, host, repository, agrees) in [
            ("LOCALDOMAIN/debian:1", "LOCALDOMAIN", "debian", false),
            ("LOCALHOST/app:1", "LOCALHOST", "app", false),
            ("Registry/team/app:1", "Registry", "team/app", false),
            ("[fc00::1]:5000/app:1", "[fc00::1]:5000", "app", false),
            ("[::1]:5000/app:1", "[::1]:5000", "app", false),
            ("localhost/app:1", "localhost", "app", true),
            ("localhost:5000/app:1", "localhost:5000", "app", true),
            (
                "registry.example.com/team/app:1",
                "registry.example.com",
                "team/app",
                true,
            ),
            ("debian:1", "index.docker.io", "library/debian", true),
            (
                "index.docker.io/team/app:1",
                "index.docker.io",
                "team/app",
                true,
            ),
            (&digest_only, "index.docker.io", "library/debian", true),
            (&tag_and_digest, "registry.example.com", "app", true),
        ] {
            let reference = Reference::parse(text).unwrap().with_default_tag();
            // Blob requests pass the caller's reference, which may have no
            // tag or digest; `oci-client` uses only its registry and
            // repository there.
            let untagged = Reference::parse(&reference.name()).unwrap();
            for (what, r) in [("whole", &reference), ("untagged", &untagged)] {
                let oci = oci_reference(r).unwrap();
                assert_eq!(oci.resolve_registry(), host, "{text} ({what})");
                assert_eq!(oci.repository(), repository, "{text} ({what})");
                assert_eq!(
                    oci.digest(),
                    r.digest().map(Digest::as_str),
                    "{text} ({what})"
                );
                if r.digest().is_none() {
                    assert_eq!(
                        oci.tag(),
                        Some(r.tag().unwrap_or(Reference::DEFAULT_TAG)),
                        "{text} ({what})"
                    );
                }
                let parsed = r.to_string().parse::<oci_client::Reference>();
                if agrees {
                    // Ordinary references are unchanged.
                    assert_eq!(oci, parsed.unwrap(), "{text} ({what})");
                } else if text.starts_with('[') {
                    // `oci-spec` has no `[v6]:port` form: these were refused.
                    assert!(parsed.is_err(), "{text} ({what})");
                } else {
                    // The string form goes to Docker Hub.
                    assert!(same_target(&parsed.unwrap(), r).is_err(), "{text} ({what})");
                }
            }
        }
    }

    #[test]
    fn a_reference_oci_client_would_address_elsewhere_is_refused() {
        // `oci-spec` reads an upper-case, dot-less registry as a Docker Hub
        // repository: the request would reach `index.docker.io`.
        let reference = Reference::parse("LOCALDOMAIN/debian:1").unwrap();
        let reparsed: oci_client::Reference = "LOCALDOMAIN/debian:1".parse().unwrap();
        assert_eq!(reparsed.resolve_registry(), "index.docker.io");
        let err = same_target(&reparsed, &reference).unwrap_err();
        assert!(
            matches!(&err, Error::Reference { reason, .. }
                if reason.contains("index.docker.io") && reason.contains("not LOCALDOMAIN/debian")),
            "{err}"
        );
        // The host differs only in case: `oci-client` keys its credentials
        // and tokens on the exact name, so that is refused too.
        let lower = oci_client::Reference::with_tag(
            "localdomain".to_owned(),
            "debian".to_owned(),
            "1".to_owned(),
        );
        assert!(same_target(&lower, &reference).is_err());
        assert!(same_target(&oci_reference(&reference).unwrap(), &reference).is_ok());
    }
}
