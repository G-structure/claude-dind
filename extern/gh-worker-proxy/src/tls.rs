//! TLS identity generation and certificate fingerprint verification.
//!
//! # Design
//!
//! We use a self-signed certificate with SHA-256 fingerprint pinning instead of a
//! traditional CA-based PKI. This eliminates the need for a certificate authority,
//! Let's Encrypt, or a system trust store — the server generates a fresh keypair on
//! each run, prints its fingerprint, and the agent verifies that exact fingerprint.
//!
//! # Why `ring` and not `aws-lc-rs`
//!
//! `rustls` defaults to the `aws-lc-rs` crypto backend, which requires a C compiler
//! and CMake to build. GitHub Actions runners have these, but it adds build time and
//! complexity. The `ring` backend compiles from Rust + assembly with no C toolchain,
//! so we explicitly select it via `rustls = { features = ["ring"] }` and use
//! `builder_with_provider(ring::default_provider())` everywhere.
//!
//! # Fingerprint verification
//!
//! The custom [`FingerprintVerifier`] implements `rustls::client::danger::ServerCertVerifier`.
//! This is in the `danger` module because it bypasses normal certificate validation —
//! we don't check the certificate's validity period, issuer, or subject name. We only
//! check that SHA-256(cert_der) matches the expected fingerprint. This is safe because:
//!
//! 1. The fingerprint is communicated out-of-band (printed to terminal, passed as CLI arg)
//! 2. The fingerprint uniquely identifies the server's public key
//! 3. TLS signature verification still happens (via `verify_tls12/13_signature`),
//!    proving the server holds the private key matching the pinned certificate

use std::sync::Arc;

use anyhow::Result;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring as ring_provider;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio_rustls::TlsConnector;

/// A freshly generated TLS server identity.
pub struct GeneratedIdentity {
    /// SHA-256 hex fingerprint of the DER-encoded certificate.
    /// Printed to the terminal for the user to pass to the agent.
    pub fingerprint: String,

    /// Ready-to-use TLS server config with the generated cert and key.
    pub tls_config: Arc<ServerConfig>,
}

/// Generate a self-signed TLS certificate for "localhost" and return the
/// server config + fingerprint.
///
/// Uses `rcgen::generate_simple_self_signed` which creates an ECDSA P-256
/// keypair and a certificate with the given SANs. The certificate is
/// ephemeral — a new one is generated every time the server starts.
pub fn generate_identity() -> Result<GeneratedIdentity> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()])?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let fp = fingerprint(cert_der.as_ref());

    let key_der = rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der())
        .map_err(|e| anyhow::anyhow!("bad key DER: {e}"))?;

    let config = ServerConfig::builder_with_provider(Arc::new(ring_provider::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;

    Ok(GeneratedIdentity {
        fingerprint: fp,
        tls_config: Arc::new(config),
    })
}

/// Build a TLS connector that verifies the server's certificate by SHA-256
/// fingerprint instead of using a CA trust store.
///
/// The connector uses `rustls::dangerous()` to install a custom verifier.
/// TLS handshake signatures are still verified using the ring crypto provider,
/// ensuring the server actually holds the private key for the pinned certificate.
pub fn make_connector(expected_fingerprint: String) -> TlsConnector {
    let provider = ring_provider::default_provider();
    let supported_algs = provider.signature_verification_algorithms;
    let verifier = FingerprintVerifier {
        expected: expected_fingerprint,
        supported_algs,
    };
    let config = ClientConfig::builder_with_provider(Arc::new(ring_provider::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("default versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// Compute SHA-256 of raw DER bytes, returned as lowercase hex.
pub fn fingerprint(der: &[u8]) -> String {
    let hash = Sha256::digest(der);
    hex::encode(hash)
}

/// Custom TLS certificate verifier that checks the server's certificate
/// fingerprint instead of validating against a CA trust store.
///
/// This lives in `rustls::client::danger` for good reason — it skips all
/// normal X.509 validation. We compensate by:
/// - Pinning to the exact certificate (SHA-256 of DER)
/// - Still verifying TLS handshake signatures (proving private key possession)
#[derive(Debug)]
struct FingerprintVerifier {
    expected: String,
    supported_algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = fingerprint(end_entity.as_ref());
        if got == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "fingerprint mismatch: expected {}, got {got}",
                self.expected
            )))
        }
    }

    /// Delegate TLS 1.2 signature verification to ring's algorithms.
    /// This ensures the server actually holds the private key for the certificate,
    /// even though we don't validate the certificate chain.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}
