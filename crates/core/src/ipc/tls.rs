//! The line's TLS (rustls, with ring): edualcd's, with the certificate it made itself, and a
//! window's, which trusts exactly the certificate it was paired with (its SHA-256, see
//! `pairing`), whatever name or signer it shows.
//!
//! Each is a configuration, owned and handed over as it is used: rustls takes it and its
//! provider behind a reference count of its own (`into`, where rustls' signature names it), and
//! nothing here keeps a share. The one place the rule against shared state lets off
//! (`lint/rules/no-shared-state.yml`) is the window's verifier: rustls takes a verifier of one's
//! own only as an `Arc<dyn ServerCertVerifier>`, which no `into` reaches.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};

use super::pairing::Fingerprint;

/// ring's, the one rustls is built with here.
fn provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// A window's TLS, trusting only the certificate whose SHA-256 is `pinned`.
#[expect(
    clippy::disallowed_types,
    reason = "rustls takes a verifier of one's own only as an Arc<dyn ServerCertVerifier>"
)]
pub fn client(pinned: Fingerprint) -> Result<rustls::ClientConfig, rustls::Error> {
    let provider = provider();
    let verifier = Pinned {
        fingerprint: pinned,
        algorithms: provider.signature_verification_algorithms,
    };
    Ok(rustls::ClientConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(verifier))
        .with_no_client_auth())
}

/// edualcd's TLS, with its certificate (DER) and that certificate's key (PKCS #8 DER).
pub fn server(certificate: Vec<u8>, key: Vec<u8>) -> Result<rustls::ServerConfig, rustls::Error> {
    rustls::ServerConfig::builder_with_provider(provider().into())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
}

/// Trusts the one certificate paired with: its name and signer aren't looked at (it is
/// edualcd's own, signed by itself), but the handshake is still checked to be signed with its
/// key.
#[derive(Debug)]
struct Pinned {
    fingerprint: Fingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Fingerprint::of(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, certificate, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
