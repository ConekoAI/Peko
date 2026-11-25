//! TLS for links (ADR-125 D3).
//!
//! Each engine makes a self-signed certificate when it starts and neither
//! side checks the other's against any authority: the certificate proves
//! nothing. The session gives the link forward secrecy and hides its
//! metadata from the network; who is on the other end is proved by the
//! `link_hello` each side signs over this session's exported keying
//! material ([`export_binding`]). TLS 1.3 only.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use super::hello::EXPORTER_LABEL;
use crate::tunnel::tls::crypto_provider;

/// The name the dialer gives as SNI; the certificate carries it too.
pub const LINK_SERVER_NAME: &str = "peko-link";

/// This engine's certificate and key for links.
pub struct LinkIdentity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl std::fmt::Debug for LinkIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkIdentity").finish_non_exhaustive()
    }
}

impl LinkIdentity {
    /// A new self-signed certificate.
    ///
    /// # Errors
    ///
    /// The key or certificate could not be made.
    pub fn generate() -> Result<Self, String> {
        let certified = rcgen::generate_simple_self_signed(vec![LINK_SERVER_NAME.to_string()])
            .map_err(|e| format!("could not make a link certificate: {e}"))?;
        Ok(Self {
            cert: certified.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        })
    }

    /// The listener's TLS config: TLS 1.3, no client certificate.
    ///
    /// # Errors
    ///
    /// rustls refused the key.
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, String> {
        let config = rustls::ServerConfig::builder_with_provider(crypto_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_single_cert(
                vec![self.cert.clone()],
                PrivateKeyDer::Pkcs8(self.key.clone_key()),
            )
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(config))
    }
}

/// The dialer's TLS config: TLS 1.3, any certificate (the hello is the
/// proof), the handshake's own signature still checked.
///
/// # Errors
///
/// rustls refused the protocol versions (it does not).
pub fn client_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = crypto_provider();
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate { provider }))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// SNI for the dialer.
#[must_use]
pub fn server_name() -> ServerName<'static> {
    ServerName::try_from(LINK_SERVER_NAME).expect("a valid DNS name")
}

/// The 32 bytes this TLS session exports under [`EXPORTER_LABEL`]
/// (RFC 5705 / RFC 8446 §7.5), which each side's hello signs.
///
/// # Errors
///
/// The handshake is not complete.
pub fn export_binding<D>(conn: &rustls::ConnectionCommon<D>) -> Result<[u8; 32], String> {
    conn.export_keying_material([0u8; 32], EXPORTER_LABEL, None)
        .map_err(|e| format!("could not export this session's keying material: {e}"))
}

/// Takes any certificate; checks only that the handshake was signed by
/// the key in it.
#[derive(Debug)]
struct AnyCertificate {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Both ends of one session export the same binding; another session
    /// exports another.
    #[tokio::test]
    async fn both_ends_of_a_session_export_the_same_binding() {
        let identity = LinkIdentity::generate().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(identity.server_config().unwrap());
        let connector = tokio_rustls::TlsConnector::from(client_config().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let mut out = Vec::new();
            for _ in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut tls = acceptor.accept(tcp).await.unwrap();
                let binding = export_binding(tls.get_ref().1).unwrap();
                let mut byte = [0u8; 1];
                tls.read_exact(&mut byte).await.unwrap();
                out.push(binding);
            }
            out
        });

        let mut client_bindings = Vec::new();
        for _ in 0..2 {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let mut tls = connector.connect(server_name(), tcp).await.unwrap();
            assert_eq!(
                tls.get_ref().1.protocol_version(),
                Some(rustls::ProtocolVersion::TLSv1_3)
            );
            client_bindings.push(export_binding(tls.get_ref().1).unwrap());
            tls.write_all(b"x").await.unwrap();
            tls.flush().await.unwrap();
        }
        let server_bindings = server.await.unwrap();
        assert_eq!(client_bindings, server_bindings);
        assert_ne!(client_bindings[0], client_bindings[1]);
    }
}
