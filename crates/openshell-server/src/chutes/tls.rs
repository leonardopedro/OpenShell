// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Optional TLS pinning for the Chutes upstream client.
//!
//! Discovery returns the ML-KEM public key the prompt is encrypted to, so the
//! only thing standing between the prompt and a network intermediary is the
//! TLS chain that carried that key. The default Web PKI set accepts any public
//! CA; `CHUTES_TLS_SPKI_PINS` narrows it to a fixed set of leaf public keys.
//!
//! Pins are the base64 SHA-256 digest of the leaf certificate's
//! `SubjectPublicKeyInfo` (the usual "SPKI pin", not the certificate digest),
//! comma separated. The wrapped verifier still performs full chain and
//! hostname validation — pinning only *additionally* restricts the leaf key,
//! so a pin can never make an otherwise-invalid chain acceptable.

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};

/// SPKI pin: SHA-256 of the leaf certificate's `SubjectPublicKeyInfo` DER.
pub type SpkiPin = [u8; 32];

/// Decode one base64 pin entry into a digest, rejecting wrong lengths.
///
/// # Errors
/// Returns a human-readable reason when the entry is not base64 or not 32
/// bytes once decoded.
pub fn decode_pin(entry: &str) -> Result<SpkiPin, String> {
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        entry.trim(),
    )
    .map_err(|error| format!("pin is not valid base64: {error}"))?;
    decoded.as_slice().try_into().map_err(|_| {
        format!(
            "pin must decode to a 32-byte SHA-256 digest, got {} bytes",
            decoded.len()
        )
    })
}

/// SHA-256 of the leaf certificate's `SubjectPublicKeyInfo`.
///
/// Returns `None` when the DER blob does not parse as an X.509 certificate, in
/// which case the caller must fail the handshake rather than fall back to
/// validating without a pin.
fn spki_sha256(cert_der: &[u8]) -> Option<SpkiPin> {
    let (_, certificate) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    let spki = certificate.tbs_certificate.subject_pki.raw;
    Some(Sha256::digest(spki).into())
}

/// Verifier that applies normal chain/hostname validation, then requires the
/// leaf key to match one of `pins`.
#[derive(Debug)]
struct SpkiPinVerifier {
    inner: Arc<WebPkiServerVerifier>,
    pins: Vec<SpkiPin>,
}

impl ServerCertVerifier for SpkiPinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        let presented = spki_sha256(end_entity.as_ref()).ok_or_else(|| {
            Error::General("chutes tls pin: leaf certificate could not be parsed".to_string())
        })?;
        if self.pins.iter().any(|pin| pin == &presented) {
            Ok(verified)
        } else {
            Err(Error::General(format!(
                "chutes tls pin mismatch for {}: presented SPKI {}. Update CHUTES_TLS_SPKI_PINS if the upstream rotated its key.",
                server_name.to_str(),
                hex::encode(presented)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Build a rustls client configuration that pins the leaf SPKI.
///
/// # Errors
/// Returns a reason when the crypto provider is unavailable, the native root
/// store cannot be loaded, or the verifier cannot be constructed.
pub fn pinned_client_config(pins: &[SpkiPin]) -> Result<ClientConfig, String> {
    let native = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(native.certs);
    pinned_client_config_with_roots(pins, roots)
}

fn pinned_client_config_with_roots(
    pins: &[SpkiPin],
    roots: RootCertStore,
) -> Result<ClientConfig, String> {
    let provider = openshell_crypto::tls::ensure_default_provider().clone();
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|error| format!("failed to build webpki verifier: {error}"))?;
    let verifier = Arc::new(SpkiPinVerifier {
        inner,
        pins: pins.to_vec(),
    });
    Ok(ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed() -> (CertificateDer<'static>, RootCertStore) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("self-signed certificate");
        let der = certified.cert.der().clone();
        let mut roots = RootCertStore::empty();
        roots.add(der.clone()).expect("trust the test root");
        (der, roots)
    }

    #[test]
    fn pin_parsing_rejects_bad_input() {
        assert!(decode_pin("not base64!").is_err());
        assert!(decode_pin("AAAA").is_err());
        let pin = decode_pin("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").expect("32 zeros");
        assert_eq!(pin, [0u8; 32]);
    }

    #[test]
    fn matching_pin_is_accepted_and_other_pins_are_rejected() {
        let (cert, _roots) = self_signed();
        let pin = spki_sha256(cert.as_ref()).expect("spki");
        let provider = openshell_crypto::tls::ensure_default_provider().clone();

        let verifier_for = |pins: Vec<SpkiPin>| {
            let mut roots = RootCertStore::empty();
            roots.add(cert.clone()).expect("trust the test root");
            let inner =
                WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                    .build()
                    .expect("verifier");
            SpkiPinVerifier { inner, pins }
        };
        let server_name = ServerName::try_from("localhost").expect("server name");

        verifier_for(vec![pin])
            .verify_server_cert(&cert, &[], &server_name, &[], UnixTime::now())
            .expect("matching pin accepted");

        let error = verifier_for(vec![[0xAA; 32]])
            .verify_server_cert(&cert, &[], &server_name, &[], UnixTime::now())
            .expect_err("mismatched pin rejected");
        assert!(format!("{error}").contains("pin mismatch"));
    }
}
