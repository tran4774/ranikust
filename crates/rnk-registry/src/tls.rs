//! The trust store: the host's CA bundle together with the embedded webpki
//! roots (ADR-0024), plus per-registry `--registry-certificate` files.
//!
//! `oci-client` and `reqwest` take certificate lists, not a rustls verifier,
//! so the store is a list of DER certificates. Certificates rustls cannot
//! parse are dropped here: `reqwest` refuses to build a client when one of
//! its roots is invalid, and one bad file in `/etc/ssl/certs` must not stop
//! every pull.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject as _;

use crate::error::Error;

/// Certificates trusted for registry connections.
///
/// Cheap to clone: the certificates are shared.
#[derive(Clone, Debug)]
pub struct TrustStore {
    certs: Arc<Vec<CertificateDer<'static>>>,
    host: usize,
    embedded: usize,
}

impl TrustStore {
    /// The host's CAs (`SSL_CERT_FILE`, `SSL_CERT_DIR`, else the system
    /// bundle, read as `rustls-native-certs` reads them) together with the
    /// embedded webpki roots.
    ///
    /// A missing, empty or partly unreadable host bundle is not an error:
    /// the embedded roots still apply, as in a `scratch` image.
    pub fn load() -> Self {
        // `errors` lists unreadable host files; ADR-0024 makes a missing or
        // broken host bundle a non-event, so they are dropped.
        let native = rustls_native_certs::load_native_certs();
        Self::from_parts(native.certs)
    }

    /// The store built from `host` CAs and the embedded roots.
    pub(crate) fn from_parts(host: Vec<CertificateDer<'static>>) -> Self {
        let host = valid(host);
        let embedded: Vec<CertificateDer<'static>> = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|c| c.clone().into_owned())
            .collect();
        let host_len = host.len();
        let embedded_len = embedded.len();
        let mut certs = host;
        certs.extend(embedded);
        Self {
            certs: Arc::new(certs),
            host: host_len,
            embedded: embedded_len,
        }
    }

    /// This store plus the certificates in the PEM file at `path`
    /// (`--registry-certificate`).
    ///
    /// Unlike the host bundle, a file the user named must be readable and
    /// hold at least one valid certificate.
    pub fn with_pem_file(&self, path: &Path) -> Result<Self, Error> {
        let fail = |reason: String| Error::Tls(format!("{}: {reason}", path.display()));
        let pem = fs::read(path).map_err(|e| fail(e.to_string()))?;
        let parsed: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<_, _>>()
            .map_err(|e| fail(format!("not a PEM certificate list: {e}")))?;
        let parsed_len = parsed.len();
        let added = valid(parsed);
        if added.is_empty() {
            return Err(fail(format!("no usable certificate ({parsed_len} found)")));
        }
        let mut certs = Vec::clone(&self.certs);
        certs.extend(added);
        Ok(Self {
            certs: Arc::new(certs),
            host: self.host,
            embedded: self.embedded,
        })
    }

    /// How many certificates came from the host bundle.
    pub fn host_len(&self) -> usize {
        self.host
    }

    /// How many certificates are embedded webpki roots.
    pub fn embedded_len(&self) -> usize {
        self.embedded
    }

    /// Every trusted certificate.
    pub fn len(&self) -> usize {
        self.certs.len()
    }

    /// Whether the store is empty; never true for [`TrustStore::load`].
    pub fn is_empty(&self) -> bool {
        self.certs.is_empty()
    }

    /// The store as `oci-client` takes it.
    pub(crate) fn to_oci(&self) -> Vec<oci_client::client::Certificate> {
        self.certs
            .iter()
            .map(|c| oci_client::client::Certificate {
                encoding: oci_client::client::CertificateEncoding::Der,
                data: c.to_vec(),
            })
            .collect()
    }

    /// The store as `reqwest` takes it.
    pub(crate) fn to_reqwest(&self) -> Result<Vec<reqwest::Certificate>, Error> {
        self.certs
            .iter()
            .map(|c| {
                reqwest::Certificate::from_der(c)
                    .map_err(|e| Error::Tls(format!("certificate rejected by reqwest: {e}")))
            })
            .collect()
    }
}

/// The certificates a rustls root store accepts, in order.
fn valid(certs: Vec<CertificateDer<'static>>) -> Vec<CertificateDer<'static>> {
    certs
        .into_iter()
        .filter(|c| rustls::RootCertStore::empty().add(c.clone()).is_ok())
        .collect()
}

/// Installs `ring` as the process's default rustls crypto provider, which
/// `reqwest` reads when it builds a client (ADR-0024).
///
/// The one piece of process-wide state this crate sets; it holds no
/// configuration, and installing it twice is harmless.
pub(crate) fn install_crypto_provider() {
    // `Err` means a provider is already installed: `ring` by an earlier call,
    // since no other provider is compiled in (ADR-0024).
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn embedded_roots_are_always_there() {
        let store = TrustStore::from_parts(Vec::new());
        assert_eq!(store.host_len(), 0);
        assert!(store.embedded_len() > 100, "{}", store.embedded_len());
        assert_eq!(store.len(), store.embedded_len());
        assert_eq!(store.to_oci().len(), store.len());
    }

    #[test]
    fn invalid_host_certificates_are_dropped() {
        let good = webpki_root_certs::TLS_SERVER_ROOT_CERTS[0]
            .clone()
            .into_owned();
        let bad = CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x00]);
        let store = TrustStore::from_parts(vec![bad, good]);
        assert_eq!(store.host_len(), 1);
    }

    #[test]
    fn pem_files_add_certificates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.pem");
        let der = webpki_root_certs::TLS_SERVER_ROOT_CERTS[1].clone();
        let pem = pem_of(&der);
        fs::File::create(&path)
            .unwrap()
            .write_all(pem.as_bytes())
            .unwrap();

        let base = TrustStore::from_parts(Vec::new());
        let store = base.with_pem_file(&path).unwrap();
        assert_eq!(store.len(), base.len() + 1);
        assert_eq!(base.len(), base.embedded_len(), "the base is unchanged");
    }

    #[test]
    fn named_files_must_hold_a_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.pem");
        fs::write(&empty, "no certificate here\n").unwrap();
        let base = TrustStore::from_parts(Vec::new());
        let err = base.with_pem_file(&empty).unwrap_err();
        assert!(err.to_string().contains("no usable certificate"), "{err}");
        let err = base
            .with_pem_file(&dir.path().join("missing.pem"))
            .unwrap_err();
        assert!(matches!(err, Error::Tls(_)), "{err}");
    }

    /// PEM text for one DER certificate.
    pub(crate) fn pem_of(der: &[u8]) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(der);
        let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
        for line in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).unwrap());
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
        pem
    }
}
