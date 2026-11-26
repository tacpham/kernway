//! Client TLS on the Kernway runtime: `rustls` driven over the async socket
//! ([`AsyncTlsStream`]) plus the [`ClientConfig`]s to drive it with.
//!
//! Shared by the HTTP client (`https://`) and the MySQL driver (which upgrades an
//! already-open plaintext connection to TLS mid-handshake — hence
//! [`AsyncTlsStream::connect`] wraps an existing socket rather than dialing one).
//!
//! # Certificate checking
//!
//! [`client_config`] picks the trust roots (Mozilla's, or a CA you supply as PEM)
//! and how much of the server certificate to check ([`CertCheck`]):
//!
//! | [`CertCheck`] | chain to a trusted CA | name matches host | use |
//! |---|---|---|---|
//! | `Full` | yes | yes | the default; public servers, `VERIFY_IDENTITY` |
//! | `CaOnly` | yes | no | a private CA whose certs don't name the host (`VERIFY_CA`) |
//! | `None` | no | no | encryption without authentication (`PREFERRED` / `REQUIRED`) |
//!
//! Even `None` verifies the handshake signatures, so the session is bound to the
//! key the server presented; it only skips deciding whether that key is *the right
//! server's*. That protects against passive sniffing, not an active man-in-the-middle.

mod stream;

use std::io;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

pub use rustls;
pub use stream::AsyncTlsStream;

/// How much of the server certificate to check. See the [crate docs](crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertCheck {
    /// Chain to a trusted root **and** the name matches the host. The default.
    Full,
    /// Chain to a trusted root, but accept any name — for a private CA whose
    /// server certificates do not carry the hostname used to connect.
    CaOnly,
    /// No certificate check: encrypt, but do not authenticate the server.
    None,
}

/// The ring crypto provider, named explicitly so no process-wide default
/// provider is ever needed.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The shared default config: Mozilla roots, full verification, no client
/// certificate. Cheap to clone (an `Arc`).
pub fn default_config() -> Arc<ClientConfig> {
    client_config(None, CertCheck::Full).expect("the default roots always load")
}

/// Build a client config trusting `ca_pem` (one or more PEM certificates) — or
/// Mozilla's roots when `None` — and checking the server certificate per `check`.
///
/// Fails if `ca_pem` holds no parseable certificate.
pub fn client_config(ca_pem: Option<&[u8]>, check: CertCheck) -> io::Result<Arc<ClientConfig>> {
    client_config_with_identity(ca_pem, check, None)
}

/// A client certificate chain and its private key, both PEM — presented to
/// servers that require mutual TLS.
#[derive(Debug, Clone, Copy)]
pub struct ClientIdentity<'a> {
    /// The certificate chain, leaf first.
    pub cert_pem: &'a [u8],
    /// The private key (PKCS#8, PKCS#1 or SEC1).
    pub key_pem: &'a [u8],
}

/// [`client_config`], also presenting `identity` when the server asks for a
/// client certificate (mutual TLS).
pub fn client_config_with_identity(
    ca_pem: Option<&[u8]>,
    check: CertCheck,
    identity: Option<ClientIdentity<'_>>,
) -> io::Result<Arc<ClientConfig>> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?;

    let wants_cert = match check {
        CertCheck::None => builder.dangerous().with_custom_certificate_verifier(Arc::new(NoCertCheck(provider()))),
        CertCheck::Full => builder.with_root_certificates(Arc::new(root_store(ca_pem)?)),
        CertCheck::CaOnly => {
            let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(root_store(ca_pem)?), provider())
                .build()
                .map_err(io::Error::other)?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(IgnoreName(inner)))
        }
    };
    let config = match identity {
        None => wants_cert.with_no_client_auth(),
        Some(id) => {
            let bad = |what: &str, e: &dyn std::fmt::Display| io::Error::new(io::ErrorKind::InvalidData, format!("bad {what}: {e}"));
            let certs = CertificateDer::pem_slice_iter(id.cert_pem)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| bad("client certificate PEM", &e))?;
            if certs.is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "no certificate in the client certificate PEM"));
            }
            let key = PrivateKeyDer::from_pem_slice(id.key_pem).map_err(|e| bad("client key PEM", &e))?;
            // An unparseable key body, or a key that is not the certificate's.
            wants_cert.with_client_auth_cert(certs, key).map_err(|e| bad("client certificate / key pair", &e))?
        }
    };
    Ok(Arc::new(config))
}

/// Fill `buf` with cryptographically secure random bytes (the TLS provider's
/// system RNG).
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    provider().secure_random.fill(buf).map_err(|_| io::Error::other("system RNG failed"))
}

/// Mozilla's roots, or the certificates in `ca_pem`.
fn root_store(ca_pem: Option<&[u8]>) -> io::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    match ca_pem {
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        Some(pem) => {
            for cert in CertificateDer::pem_slice_iter(pem) {
                let cert = cert.map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad CA PEM: {e}"))
                })?;
                roots.add(cert).map_err(io::Error::other)?;
            }
            if roots.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "CA PEM contains no certificate",
                ));
            }
        }
    }
    Ok(roots)
}

/// [`CertCheck::CaOnly`]: the full WebPKI check, except a name mismatch passes.
#[derive(Debug)]
struct IgnoreName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for IgnoreName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// [`CertCheck::None`]: accept any certificate, but still verify the handshake
/// signatures with the provider's algorithms.
#[derive(Debug)]
struct NoCertCheck(Arc<CryptoProvider>);

impl ServerCertVerifier for NoCertCheck {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_check_level_builds_with_default_roots() {
        for check in [CertCheck::Full, CertCheck::CaOnly, CertCheck::None] {
            client_config(None, check).unwrap();
        }
    }

    #[test]
    fn a_pem_without_certificates_is_rejected() {
        let err = client_config(Some(b"not a pem"), CertCheck::Full).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
