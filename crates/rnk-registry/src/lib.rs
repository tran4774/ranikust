//! Registry client for the OCI distribution API.
//!
//! Authentication (docker `config.json`, credential helpers); TLS with rustls
//! and the `ring` provider, trusting the host's CA bundle together with the
//! embedded webpki roots and any `--registry-certificate`; mirrors; streamed
//! blob pulls into the store and pushes; cross-repository mounts with a
//! fallback to a regular upload; retries. A thin layer over `oci-client`.
//!
//! Phase 1 has the trust store, `config.json` credentials without helpers,
//! manifest, index and blob pulls, blob and manifest pushes, mounts and
//! retries; mirrors and credential helpers come in Phase 5.
//!
//! # Boundaries
//!
//! - Moves blobs and manifests; never builds, compresses or inspects layer
//!   contents (`rnk-layer`).
//! - Holds no caching policy (`rnk-cache`) and does not define the store
//!   layout (`rnk-store`): it fills a temporary store entry and verifies its
//!   digest.
//! - Keeps no process-wide state but one: it installs `ring` as rustls's
//!   default crypto provider, which `reqwest` requires. Certificates and
//!   credentials live in a [`Client`].
//! - Never puts a credential in an error, a `Debug` output, a log event or a
//!   span field.

mod client;
mod credentials;
mod error;
mod io;
mod platform;
mod retry;
mod tls;
mod token;

pub use client::{Client, ClientOptions, ImageManifests, PushOutcome, RawManifest};
pub use credentials::{Credential, Credentials};
pub use error::Error;
pub use io::{BlobBody, BlobFile, BlobReader, BlobSink, PIPE_BYTES, RemoteBlob};
pub use platform::select_platform;
pub use retry::{RetryPolicy, retry_kaniko};
pub use tls::TrustStore;
