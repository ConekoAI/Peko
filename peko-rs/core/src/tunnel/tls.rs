//! Shared TLS primitives for the PekoHub tunnel.
//!
//! This module exposes certificate/SPKI pinning, root-store construction,
//! and rustls client/server config builders. It lived under
//! `tunnel::direct` until sprint 3 Phase 12b retired the direct
//! transport; `tunnel::client` (the PekoHub tunnel) is the remaining
//! consumer. The server-side builders are retained for the tunnel's
//! own test surface.

use std::path::Path;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::Digest;

/// The crypto provider every tunnel TLS config is built with.
///
/// Named explicitly rather than taken from the process default: both
/// `ring` and `aws-lc-rs` are compiled into the dependency tree, so
/// rustls cannot choose one on its own and `ClientConfig::builder()`
/// would panic unless something had installed a default first.
#[must_use]
pub fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Make `ring` the process-wide rustls provider, once.
///
/// Called at engine and CLI start so that any library building a rustls
/// config without naming a provider gets the same one the tunnel uses.
/// Safe to call repeatedly; a provider installed earlier is left alone.
pub fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Read every certificate from a PEM buffer.
fn parse_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::CertParse(e.to_string()))
}

/// Errors that can occur while building or verifying TLS configuration.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("Failed to read TLS file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("Failed to parse certificate chain: {0}")]
    CertParse(String),
    #[error("Certificate file contains no valid certificates: {0}")]
    EmptyCert(String),
    #[error("Failed to parse private key: {0}")]
    KeyParse(String),
    #[error("No supported private key found in {0}")]
    NoKeyFound(String),
    #[error("Invalid client cert/key pair: {0}")]
    InvalidClientAuth(String),
    #[error("Failed to build TLS verifier: {0}")]
    VerifierBuild(String),
    #[error("Failed to add CA certificate: {0}")]
    AddCa(String),
    #[error("Invalid pinned_cert_sha256: {0}")]
    InvalidPin(String),
    #[error("Certificate pin mismatch")]
    PinMismatch,
}

/// Build a root certificate store from an optional custom CA path.
///
/// If `ca_path` is `Some`, the file is loaded and its certificates are
/// added to an otherwise empty store. If `None`, the WebPKI root store is
/// used.
pub fn build_root_cert_store(ca_path: Option<&Path>) -> Result<rustls::RootCertStore, TlsError> {
    let mut roots = rustls::RootCertStore::empty();

    if let Some(ca_path) = ca_path {
        let ca_pem = std::fs::read(ca_path).map_err(|e| TlsError::Read {
            path: ca_path.display().to_string(),
            source: e,
        })?;
        let certs = parse_certs(&ca_pem)?;
        if certs.is_empty() {
            return Err(TlsError::EmptyCert(ca_path.display().to_string()));
        }
        for cert in certs {
            roots
                .add(cert)
                .map_err(|e| TlsError::AddCa(e.to_string()))?;
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    Ok(roots)
}

/// Build a rustls client config from raw TLS options.
///
/// `pinned_cert_sha256` is an optional base64-encoded SHA-256 fingerprint of
/// the expected end-entity certificate.
pub fn build_client_config(
    ca_path: Option<&Path>,
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    pinned_cert_sha256: Option<&str>,
) -> Result<Arc<rustls::ClientConfig>, TlsError> {
    let roots = build_root_cert_store(ca_path)?;
    let roots = Arc::new(roots);

    let provider = crypto_provider();

    let default_verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        roots.clone(),
        provider.clone(),
    )
    .build()
    .map_err(|e| TlsError::VerifierBuild(e.to_string()))?;

    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| TlsError::VerifierBuild(e.to_string()))?
        .with_root_certificates(roots);

    // ADR-057 transport audit: a half-configured mTLS (one of
    // cert/key set, the other missing) used to silently fall back to
    // no client auth — an operator believing mTLS was on while it was
    // off. Hard-error instead.
    let mut config = match (cert_path, key_path) {
        (Some(cert_path), Some(key_path)) => {
            let cert_chain = load_cert_chain(cert_path)?;
            let key = load_private_key(key_path)?;
            builder
                .with_client_auth_cert(cert_chain, key)
                .map_err(|e| TlsError::InvalidClientAuth(e.to_string()))?
        }
        (None, None) => builder.with_no_client_auth(),
        (Some(_), None) => {
            return Err(TlsError::InvalidClientAuth(
                "tls.cert_path set without tls.key_path — mTLS is half-configured; \
                 set both or neither"
                    .to_string(),
            ))
        }
        (None, Some(_)) => {
            return Err(TlsError::InvalidClientAuth(
                "tls.key_path set without tls.cert_path — mTLS is half-configured; \
                 set both or neither"
                    .to_string(),
            ))
        }
    };

    if let Some(pinned_sha256) = pinned_cert_sha256 {
        let expected = BASE64
            .decode(pinned_sha256)
            .map_err(|e| TlsError::InvalidPin(e.to_string()))?;
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(PinningServerCertVerifier {
                inner: default_verifier,
                expected,
            }));
    }

    Ok(Arc::new(config))
}

/// Load a PEM-encoded certificate chain from disk.
pub fn load_cert_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let pem = std::fs::read(path).map_err(|e| TlsError::Read {
        path: path.display().to_string(),
        source: e,
    })?;
    let certs = parse_certs(&pem)?;
    if certs.is_empty() {
        return Err(TlsError::EmptyCert(path.display().to_string()));
    }
    Ok(certs)
}

/// Load a PEM-encoded private key from disk.
///
/// Supports PKCS#8, RSA (PKCS#1) and SEC1 EC private keys.
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let pem = std::fs::read(path).map_err(|e| TlsError::Read {
        path: path.display().to_string(),
        source: e,
    })?;

    match PrivateKeyDer::from_pem_slice(&pem) {
        Ok(key) => Ok(key),
        Err(rustls::pki_types::pem::Error::NoItemsFound) => {
            Err(TlsError::NoKeyFound(path.display().to_string()))
        }
        Err(e) => Err(TlsError::KeyParse(e.to_string())),
    }
}

/// Verifier that delegates to the default WebPKI verifier and then checks
/// the end-entity certificate fingerprint against a configured pin.
#[derive(Debug)]
pub struct PinningServerCertVerifier {
    inner: Arc<dyn rustls::client::danger::ServerCertVerifier>,
    expected: Vec<u8>,
}

impl PinningServerCertVerifier {
    /// Create a new pinning verifier wrapping the default WebPKI verifier.
    ///
    /// `expected` is the raw SHA-256 digest of the expected end-entity
    /// certificate.
    #[must_use]
    pub fn new(
        inner: Arc<dyn rustls::client::danger::ServerCertVerifier>,
        expected: Vec<u8>,
    ) -> Self {
        Self { inner, expected }
    }
}

impl rustls::client::danger::ServerCertVerifier for PinningServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;

        let actual = sha2::Sha256::digest(end_entity.as_ref());
        if actual.as_slice() != self.expected {
            return Err(rustls::Error::General(
                "server certificate does not match configured pin".to_string(),
            ));
        }

        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_build_client_config_with_webpki_defaults() {
        let config = build_client_config(None, None, None, None).unwrap();
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn test_build_client_config_rejects_missing_ca() {
        let result = build_client_config(Some(Path::new("/nonexistent/ca.crt")), None, None, None);
        assert!(matches!(result, Err(TlsError::Read { .. })));
    }

    #[test]
    fn test_build_client_config_with_invalid_pin() {
        let result = build_client_config(None, None, None, Some("not-base64!!!"));
        assert!(matches!(result, Err(TlsError::InvalidPin(_))));
    }

    #[test]
    fn test_load_cert_chain_rejects_empty_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("empty.crt");
        std::fs::write(&path, b"").unwrap();
        let result = load_cert_chain(&path);
        assert!(matches!(result, Err(TlsError::EmptyCert(_))));
    }

    #[test]
    fn test_load_private_key_rejects_empty_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("empty.key");
        std::fs::write(&path, b"").unwrap();
        let result = load_private_key(&path);
        assert!(matches!(result, Err(TlsError::NoKeyFound(_))));
    }

    /// A self-signed certificate for `localhost`, written to disk as PEM.
    struct TestCert {
        _dir: TempDir,
        cert_path: std::path::PathBuf,
        key_path: std::path::PathBuf,
        cert_der: Vec<u8>,
    }

    fn test_cert() -> TestCert {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = TempDir::new().unwrap();
        let cert_path = dir.path().join("server.crt");
        let key_path = dir.path().join("server.key");
        std::fs::write(&cert_path, generated.cert.pem()).unwrap();
        std::fs::write(&key_path, generated.signing_key.serialize_pem()).unwrap();
        TestCert {
            _dir: dir,
            cert_path,
            key_path,
            cert_der: generated.cert.der().to_vec(),
        }
    }

    /// Run one TLS handshake between a local server presenting `cert`
    /// and a client built by `build_client_config` with `pin`.
    async fn handshake(cert: &TestCert, pin: Option<&str>) -> std::io::Result<()> {
        handshake_trusting(cert, &cert.cert_path, pin).await
    }

    /// As `handshake`, with the client trusting the CA file at `ca_path`.
    async fn handshake_trusting(
        cert: &TestCert,
        ca_path: &Path,
        pin: Option<&str>,
    ) -> std::io::Result<()> {
        let server_config = rustls::ServerConfig::builder_with_provider(crypto_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                load_cert_chain(&cert.cert_path).unwrap(),
                load_private_key(&cert.key_path).unwrap(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            // The client may abort the handshake; that is the point of
            // the mismatch test, so the server side's result is ignored.
            if let Ok(mut tls) = acceptor.accept(tcp).await {
                use tokio::io::AsyncWriteExt;
                let _ = tls.shutdown().await;
            }
        });

        let client_config = build_client_config(Some(ca_path), None, None, pin).unwrap();
        let connector = tokio_rustls::TlsConnector::from(client_config);
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let result = connector.connect(name, tcp).await.map(|_| ());
        let _ = server.await;
        result
    }

    #[test]
    fn test_load_private_key_reads_generated_key() {
        let cert = test_cert();
        assert!(load_private_key(&cert.key_path).is_ok());
        assert_eq!(load_cert_chain(&cert.cert_path).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_handshake_succeeds_with_custom_ca() {
        let cert = test_cert();
        handshake(&cert, None).await.unwrap();
    }

    #[tokio::test]
    async fn test_handshake_succeeds_with_matching_pin() {
        let cert = test_cert();
        let pin = BASE64.encode(sha2::Sha256::digest(&cert.cert_der));
        handshake(&cert, Some(&pin)).await.unwrap();
    }

    #[tokio::test]
    async fn test_handshake_fails_with_wrong_pin() {
        let cert = test_cert();
        let pin = BASE64.encode([0u8; 32]);
        let err = handshake(&cert, Some(&pin)).await.unwrap_err();
        assert!(
            err.to_string().contains("does not match configured pin"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_handshake_fails_for_untrusted_certificate() {
        // A matching pin does not replace chain validation: a server whose
        // certificate is not under the configured CA is still refused.
        let trusted = test_cert();
        let presented = test_cert();
        let pin = BASE64.encode(sha2::Sha256::digest(&presented.cert_der));
        let err = handshake_trusting(&presented, &trusted.cert_path, Some(&pin))
            .await
            .unwrap_err();
        assert!(
            !err.to_string().contains("does not match configured pin"),
            "chain validation should fail before the pin check: {err}"
        );
    }
}
