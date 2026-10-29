//! Registry client for the OCI distribution API.
//!
//! Authentication (docker `config.json`, credential helpers); TLS with rustls
//! and the `ring` provider, trusting the host's CA bundle together with the
//! embedded webpki roots and any `--registry-certificate`; mirrors; streamed
//! blob pulls into the store and pushes; cross-repository mounts with a
//! fallback to a regular upload; retries. A thin layer over `oci-client`.
//!
//! # Boundaries
//!
//! - Moves blobs and manifests; never builds, compresses or inspects layer
//!   contents (`rnk-layer`).
//! - Holds no caching policy (`rnk-cache`) and does not define the store
//!   layout (`rnk-store`): it fills a temporary store entry and verifies its
//!   digest.
