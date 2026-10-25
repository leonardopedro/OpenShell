// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Client side of the Chutes end-to-end encryption protocol.
//!
//! The protocol is the one the Chutes TEE (Trusted Execution Environment)
//! models speak natively: every HTTP hop — including this gateway — only ever
//! carries opaque bytes, and only the chute instance that owns the ML-KEM
//! secret key can read the prompt. The wire format (reimplemented here from
//! the protocol description shared by the `claurst` and `zentinel`
//! references) is:
//!
//! * **Request.** The JSON payload gains an `e2e_response_pk` field (the
//!   caller's ephemeral ML-KEM-768 public key, base64), is gzip-compressed,
//!   then encrypted with ChaCha20-Poly1305 under a key derived via
//!   `HKDF-SHA256(ikm = shared_secret, salt = kem_ct[0..16],
//!   info = "e2e-req-v1")`. The final blob is
//!   `[KEM_CT (1088) | NONCE (12) | CT+TAG (n)]`, where `KEM_CT` is the
//!   encapsulation produced against the instance's public key.
//! * **Response (non-streaming).** The instance encapsulates a *fresh*
//!   ML-KEM ciphertext to `e2e_response_pk`; the blob has the same layout
//!   with `info = "e2e-resp-v1"`, decrypting to gzip-compressed JSON.
//! * **Response (streaming).** SSE events of the form
//!   `{"e2e_init": "<base64 ml-kem ct>"}` establish the stream key
//!   (`info = "e2e-stream-v1"`, decapsulated with the same ephemeral secret),
//!   `{"e2e": "<base64 nonce||ct||tag>"}` carries independently-encrypted
//!   chunks of raw SSE text, and `{"e2e_error": ...}` surfaces server-side
//!   failures.

use base64::Engine as _;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::Aead};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use hkdf::Hkdf;
use pqcrypto_kyber::kyber768::{
    self, Ciphertext as KyberCiphertext, PublicKey as KyberPublicKey, SecretKey as KyberSecretKey,
};
use pqcrypto_traits::kem::{
    Ciphertext as CiphertextTrait, PublicKey as PublicKeyTrait, SharedSecret as SharedSecretTrait,
};
use sha2::Sha256;

/// ML-KEM-768 public key size in bytes.
pub(crate) const ML_KEM_768_PUBKEY_SIZE: usize = 1184;
/// ML-KEM-768 ciphertext size in bytes.
pub(crate) const ML_KEM_768_CIPHERTEXT_SIZE: usize = 1088;

const ML_KEM_768_SHARED_SECRET_SIZE: usize = 32;
const CHACHA_NONCE_SIZE: usize = 12;
const CHACHA_TAG_SIZE: usize = 16;

/// HKDF context strings — protocol versioning.
const HKDF_INFO_REQ: &[u8] = b"e2e-req-v1";
const HKDF_INFO_RESP: &[u8] = b"e2e-resp-v1";
const HKDF_INFO_STREAM: &[u8] = b"e2e-stream-v1";
const HKDF_SALT_LEN: usize = 16;

/// JSON field carrying the caller's ephemeral response public key.
const RESPONSE_PK_FIELD: &str = "e2e_response_pk";

/// Errors produced by the E2EE protocol implementation.
#[derive(Debug, thiserror::Error)]
pub enum E2eeError {
    /// Payload encryption failed (JSON, gzip, or AEAD).
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),
    /// Blob decryption failed (structure, AEAD, or decapsulation).
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),
    /// Gzip compression failed.
    #[error("compression failed: {0}")]
    CompressionFailed(String),
    /// Gzip decompression failed.
    #[error("decompression failed: {0}")]
    DecompressionFailed(String),
    /// Key encoding/decoding failure (public keys, ciphertexts, HKDF).
    #[error("crypto error: {0}")]
    CryptoError(String),
}

/// Client state for one request/response cycle against one chute instance.
///
/// Created from the instance's public key returned by discovery; holds the
/// ephemeral keypair used to authenticate the response back to this caller.
pub struct E2eeSession {
    /// Ephemeral ML-KEM secret key — decapsulates the response and stream
    /// init ciphertexts produced to [`Self::my_response_pk`].
    my_response_sk: KyberSecretKey,
    /// Ephemeral ML-KEM public key — sent to the instance inside the
    /// encrypted payload as `e2e_response_pk`.
    my_response_pk: KyberPublicKey,
    /// ML-KEM ciphertext the instance decapsulates to read the request.
    request_kem_ct: Vec<u8>,
    /// Shared secret from the request encapsulation.
    shared_secret: [u8; ML_KEM_768_SHARED_SECRET_SIZE],
}

impl std::fmt::Debug for E2eeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("E2eeSession")
            .field("request_kem_ct_len", &self.request_kem_ct.len())
            .finish_non_exhaustive()
    }
}

impl E2eeSession {
    /// Start a session by encapsulating to `instance_pubkey` (from discovery)
    /// and generating the ephemeral response keypair.
    pub fn new(instance_pubkey: &KyberPublicKey) -> Self {
        let (pk, sk) = kyber768::keypair();
        let (shared_secret, request_kem_ct) = kyber768::encapsulate(instance_pubkey);
        let mut secret = [0u8; ML_KEM_768_SHARED_SECRET_SIZE];
        secret.copy_from_slice(shared_secret.as_bytes());
        Self {
            my_response_sk: sk,
            my_response_pk: pk,
            request_kem_ct: request_kem_ct.as_bytes().to_vec(),
            shared_secret: secret,
        }
    }

    /// `HKDF-SHA256(ikm = shared_secret, salt, info) -> 32 bytes`.
    fn derive_key(shared_secret: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
        let hkdf = Hkdf::<Sha256>::new(Some(salt), shared_secret);
        let mut key = [0u8; 32];
        // HKDF expand with a 32-byte output and SHA-256 cannot overflow.
        hkdf.expand(info, &mut key)
            .expect("HKDF-SHA256 expand into 32 bytes");
        key
    }

    /// Build the encrypted request blob for `json_payload`.
    ///
    /// The payload must be a JSON object; `e2e_response_pk` is injected
    /// automatically.
    pub fn encrypt_request(&self, json_payload: &str) -> Result<Vec<u8>, E2eeError> {
        let mut payload: serde_json::Value = serde_json::from_str(json_payload)
            .map_err(|e| E2eeError::EncryptionFailed(format!("JSON parse: {e}")))?;
        if !payload.is_object() {
            return Err(E2eeError::EncryptionFailed(
                "payload is not a JSON object".to_string(),
            ));
        }
        let response_pk_b64 = base64::engine::general_purpose::STANDARD
            .encode(PublicKeyTrait::as_bytes(&self.my_response_pk));
        payload[RESPONSE_PK_FIELD] = serde_json::Value::String(response_pk_b64);
        let augmented = payload.to_string();

        let compressed = compress_gzip(augmented.as_bytes())?;

        let salt = &self.request_kem_ct[..HKDF_SALT_LEN];
        let key = Self::derive_key(&self.shared_secret, salt, HKDF_INFO_REQ);

        let nonce = random_nonce()?;
        let ciphertext = aead_encrypt(&key, &nonce, &compressed)
            .map_err(|e| E2eeError::EncryptionFailed(format!("AEAD: {e}")))?;

        Ok(build_blob(&self.request_kem_ct, &nonce, &ciphertext))
    }

    /// Decrypt a non-streaming response blob into its plaintext JSON bytes.
    pub fn decrypt_response(&self, response_blob: &[u8]) -> Result<Vec<u8>, E2eeError> {
        let min_len = ML_KEM_768_CIPHERTEXT_SIZE + CHACHA_NONCE_SIZE + CHACHA_TAG_SIZE;
        if response_blob.len() < min_len {
            return Err(E2eeError::DecryptionFailed(format!(
                "response too short: expected at least {min_len} bytes, got {}",
                response_blob.len()
            )));
        }
        let (mlkem_ct, nonce, ciphertext) = split_blob(response_blob)?;
        let response_ss = decapsulate(mlkem_ct, &self.my_response_sk)?;
        let key = Self::derive_key(&response_ss, &mlkem_ct[..HKDF_SALT_LEN], HKDF_INFO_RESP);
        let compressed = aead_decrypt(&key, nonce, ciphertext)?;
        decompress_gzip(&compressed)
    }

    /// Derive the stream key from an `e2e_init` event payload (base64
    /// ML-KEM ciphertext produced to `my_response_pk`).
    pub fn decrypt_stream_init(&self, mlkem_ct_b64: &str) -> Result<Vec<u8>, E2eeError> {
        let mlkem_ct = base64::engine::general_purpose::STANDARD
            .decode(mlkem_ct_b64)
            .map_err(|e| E2eeError::CryptoError(format!("invalid base64 for e2e_init: {e}")))?;
        if mlkem_ct.len() != ML_KEM_768_CIPHERTEXT_SIZE {
            return Err(E2eeError::CryptoError(format!(
                "invalid e2e_init ciphertext length: {}",
                mlkem_ct.len()
            )));
        }
        let stream_ss = decapsulate(&mlkem_ct, &self.my_response_sk)?;
        let key = Self::derive_key(&stream_ss, &mlkem_ct[..HKDF_SALT_LEN], HKDF_INFO_STREAM);
        Ok(key.to_vec())
    }

    /// Decrypt one `e2e` stream chunk (base64 `nonce||ct||tag`) with the
    /// stream key from [`Self::decrypt_stream_init`].
    pub fn decrypt_stream_chunk(
        &self,
        enc_chunk_b64: &str,
        stream_key: &[u8],
    ) -> Result<Vec<u8>, E2eeError> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(enc_chunk_b64)
            .map_err(|e| E2eeError::DecryptionFailed(format!("base64 chunk: {e}")))?;
        if raw.len() < CHACHA_NONCE_SIZE + CHACHA_TAG_SIZE {
            return Err(E2eeError::DecryptionFailed(
                "stream chunk too short".to_string(),
            ));
        }
        if stream_key.len() != 32 {
            return Err(E2eeError::DecryptionFailed(format!(
                "stream key must be 32 bytes, got {}",
                stream_key.len()
            )));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(stream_key);
        aead_decrypt(&key, &raw[..CHACHA_NONCE_SIZE], &raw[CHACHA_NONCE_SIZE..])
    }
}

/// Parse a base64-encoded ML-KEM-768 instance public key (discovery payload).
pub(crate) fn parse_instance_pubkey(pk_b64: &str) -> Result<KyberPublicKey, E2eeError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(pk_b64.trim())
        .map_err(|e| E2eeError::CryptoError(format!("invalid public key base64: {e}")))?;
    if bytes.len() != ML_KEM_768_PUBKEY_SIZE {
        return Err(E2eeError::CryptoError(format!(
            "invalid pubkey size: expected {ML_KEM_768_PUBKEY_SIZE}, got {}",
            bytes.len()
        )));
    }
    KyberPublicKey::from_bytes(&bytes)
        .map_err(|e| E2eeError::CryptoError(format!("invalid ML-KEM pubkey bytes: {e:?}")))
}

/// Gzip-compress `data`.
pub(crate) fn compress_gzip(data: &[u8]) -> Result<Vec<u8>, E2eeError> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, data)
        .map_err(|e| E2eeError::CompressionFailed(format!("gzip write: {e}")))?;
    encoder
        .finish()
        .map_err(|e| E2eeError::CompressionFailed(format!("gzip finish: {e}")))
}

/// Gzip-decompress `data`.
pub(crate) fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, E2eeError> {
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut out)
        .map_err(|e| E2eeError::DecompressionFailed(format!("gzip: {e}")))?;
    Ok(out)
}

fn random_nonce() -> Result<[u8; CHACHA_NONCE_SIZE], E2eeError> {
    let mut nonce = [0u8; CHACHA_NONCE_SIZE];
    getrandom::fill(&mut nonce)
        .map_err(|e| E2eeError::EncryptionFailed(format!("getrandom: {e}")))?;
    Ok(nonce)
}

fn build_blob(kem_ct: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(kem_ct.len() + nonce.len() + ciphertext.len());
    blob.extend_from_slice(kem_ct);
    blob.extend_from_slice(nonce);
    blob.extend_from_slice(ciphertext);
    blob
}

/// The three fixed-layout slices of a `[KEM_CT | NONCE | CIPHERTEXT]` blob.
type BlobParts<'a> = (&'a [u8], &'a [u8], &'a [u8]);

fn split_blob(blob: &[u8]) -> Result<BlobParts<'_>, E2eeError> {
    let min_len = ML_KEM_768_CIPHERTEXT_SIZE + CHACHA_NONCE_SIZE;
    if blob.len() < min_len {
        return Err(E2eeError::DecryptionFailed(format!(
            "blob too short: expected at least {min_len} bytes, got {}",
            blob.len()
        )));
    }
    let kem_end = ML_KEM_768_CIPHERTEXT_SIZE;
    let nonce_end = kem_end + CHACHA_NONCE_SIZE;
    Ok((
        &blob[..kem_end],
        &blob[kem_end..nonce_end],
        &blob[nonce_end..],
    ))
}

fn aead_encrypt(key: &[u8; 32], nonce: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|e| e.to_string())?;
    cipher
        .encrypt(nonce.into(), plaintext)
        .map_err(|e| e.to_string())
}

fn aead_decrypt(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, E2eeError> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| E2eeError::DecryptionFailed(format!("invalid key: {e}")))?;
    cipher
        .decrypt(nonce.into(), ciphertext)
        .map_err(|e| E2eeError::DecryptionFailed(format!("AEAD auth: {e}")))
}

fn decapsulate(mlkem_ct: &[u8], sk: &KyberSecretKey) -> Result<Vec<u8>, E2eeError> {
    let ct = KyberCiphertext::from_bytes(mlkem_ct)
        .map_err(|e| E2eeError::CryptoError(format!("invalid ML-KEM ciphertext: {e:?}")))?;
    Ok(kyber768::decapsulate(&ct, sk).as_bytes().to_vec())
}

#[cfg(test)]
pub(crate) mod server_sim {
    //! Server side of the protocol, used by tests to simulate a chute
    //! instance: decapsulates request blobs and encrypts responses/stream
    //! chunks back to the caller's `e2e_response_pk`.

    use super::*;
    use std::sync::Mutex;

    pub struct ChuteInstance {
        pk: KyberPublicKey,
        sk: KyberSecretKey,
        nonces: Mutex<u64>,
    }

    impl ChuteInstance {
        pub(crate) fn new() -> Self {
            let (pk, sk) = kyber768::keypair();
            Self {
                pk,
                sk,
                nonces: Mutex::new(0),
            }
        }

        /// Base64 public key as it appears in discovery responses.
        pub(crate) fn pubkey_b64(&self) -> String {
            base64::engine::general_purpose::STANDARD.encode(PublicKeyTrait::as_bytes(&self.pk))
        }

        /// Single-use nonces, like the real discovery API.
        pub(crate) fn next_nonce(&self) -> String {
            let mut counter = self.nonces.lock().expect("nonce counter poisoned");
            *counter += 1;
            let value = *counter;
            format!("test-nonce-{value:016x}")
        }

        /// Decapsulate and decrypt a request blob; returns the augmented JSON
        /// payload (including `e2e_response_pk`).
        pub(crate) fn decrypt_request(&self, blob: &[u8]) -> Result<String, E2eeError> {
            let (mlkem_ct, nonce, ciphertext) = split_blob(blob)?;
            let ss = decapsulate(mlkem_ct, &self.sk)?;
            let key = E2eeSession::derive_key(&ss, &mlkem_ct[..HKDF_SALT_LEN], HKDF_INFO_REQ);
            let compressed = aead_decrypt(&key, nonce, ciphertext)?;
            decompress_gzip(&compressed)
                .map(|bytes| String::from_utf8(bytes).expect("decrypted payload is JSON text"))
        }

        fn response_keypair_b64(request_json: &str) -> (KyberPublicKey, Vec<u8>, Vec<u8>) {
            let payload: serde_json::Value =
                serde_json::from_str(request_json).expect("decrypted request is JSON");
            let response_pk_b64 = payload[RESPONSE_PK_FIELD]
                .as_str()
                .expect("request carries e2e_response_pk");
            let response_pk = parse_instance_pubkey(response_pk_b64).expect("valid response pk");
            let (ss, ct) = kyber768::encapsulate(&response_pk);
            (response_pk, ss.as_bytes().to_vec(), ct.as_bytes().to_vec())
        }

        /// Encrypt a non-streaming response blob to the caller's
        /// `e2e_response_pk`.
        pub(crate) fn encrypt_response(
            request_json: &str,
            response_json: &str,
        ) -> Result<Vec<u8>, E2eeError> {
            let (_, ss, ct) = Self::response_keypair_b64(request_json);
            let key = E2eeSession::derive_key(&ss, &ct[..HKDF_SALT_LEN], HKDF_INFO_RESP);
            let compressed = compress_gzip(response_json.as_bytes())?;
            let nonce = random_nonce()?;
            let ciphertext = aead_encrypt(&key, &nonce, &compressed)
                .map_err(|e| E2eeError::EncryptionFailed(format!("AEAD: {e}")))?;
            Ok(build_blob(&ct, &nonce, &ciphertext))
        }

        /// Produce the `e2e_init` payload and matching stream key.
        pub(crate) fn stream_init(request_json: &str) -> (String, Vec<u8>) {
            let (_, ss, ct) = Self::response_keypair_b64(request_json);
            let key = E2eeSession::derive_key(&ss, &ct[..HKDF_SALT_LEN], HKDF_INFO_STREAM);
            (
                base64::engine::general_purpose::STANDARD.encode(&ct),
                key.to_vec(),
            )
        }

        /// Encrypt one SSE chunk with the stream key.
        pub(crate) fn stream_chunk(
            stream_key: &[u8],
            plaintext: &str,
        ) -> Result<String, E2eeError> {
            let mut key = [0u8; 32];
            key.copy_from_slice(stream_key);
            let nonce = random_nonce()?;
            let ciphertext = aead_encrypt(&key, &nonce, plaintext.as_bytes())
                .map_err(|e| E2eeError::EncryptionFailed(format!("AEAD: {e}")))?;
            let mut raw = Vec::with_capacity(CHACHA_NONCE_SIZE + ciphertext.len());
            raw.extend_from_slice(&nonce);
            raw.extend_from_slice(&ciphertext);
            Ok(base64::engine::general_purpose::STANDARD.encode(raw))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::server_sim::ChuteInstance;
    use super::*;

    #[test]
    fn request_response_roundtrip() {
        let instance = ChuteInstance::new();
        let instance_pk = parse_instance_pubkey(&instance.pubkey_b64()).expect("parse pk");
        let session = E2eeSession::new(&instance_pk);

        let payload = r#"{"model":"zai-org/GLM-5.1-TEE","messages":[],"stream":true}"#;
        let blob = session.encrypt_request(payload).expect("encrypt");
        assert!(
            blob.len() > ML_KEM_768_CIPHERTEXT_SIZE + CHACHA_NONCE_SIZE + CHACHA_TAG_SIZE,
            "blob must carry ciphertext beyond the fixed header"
        );

        let decrypted = instance.decrypt_request(&blob).expect("server decrypt");
        let augmented: serde_json::Value = serde_json::from_str(&decrypted).expect("json");
        assert_eq!(augmented["model"], "zai-org/GLM-5.1-TEE");
        assert!(augmented[RESPONSE_PK_FIELD].is_string());

        let response_json = r#"{"id":"x","choices":[{"delta":{"content":"hi"}}]}"#;
        let response_blob =
            ChuteInstance::encrypt_response(&decrypted, response_json).expect("server encrypt");
        let plaintext = session
            .decrypt_response(&response_blob)
            .expect("client decrypt");
        assert_eq!(
            std::str::from_utf8(&plaintext).expect("utf8"),
            response_json
        );
    }

    #[test]
    fn stream_roundtrip() {
        let instance = ChuteInstance::new();
        let instance_pk = parse_instance_pubkey(&instance.pubkey_b64()).expect("parse pk");
        let session = E2eeSession::new(&instance_pk);

        let payload = r#"{"model":"m","stream":true}"#;
        let blob = session.encrypt_request(payload).expect("encrypt");
        let decrypted = instance.decrypt_request(&blob).expect("server decrypt");

        let (init_b64, stream_key) = ChuteInstance::stream_init(&decrypted);
        let client_key = session.decrypt_stream_init(&init_b64).expect("client init");
        assert_eq!(client_key, stream_key);

        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
        let chunk_b64 = ChuteInstance::stream_chunk(&client_key, sse).expect("chunk");
        let plaintext = session
            .decrypt_stream_chunk(&chunk_b64, &client_key)
            .expect("client chunk");
        assert_eq!(std::str::from_utf8(&plaintext).expect("utf8"), sse);
    }

    #[test]
    fn rejects_truncated_response_blob() {
        let instance = ChuteInstance::new();
        let instance_pk = parse_instance_pubkey(&instance.pubkey_b64()).expect("parse pk");
        let session = E2eeSession::new(&instance_pk);
        let err = session.decrypt_response(&[0u8; 32]).expect_err("too short");
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn rejects_wrong_key_size_in_stream_chunk() {
        let instance = ChuteInstance::new();
        let instance_pk = parse_instance_pubkey(&instance.pubkey_b64()).expect("parse pk");
        let session = E2eeSession::new(&instance_pk);
        let fake_chunk = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let err = session
            .decrypt_stream_chunk(&fake_chunk, &[0u8; 16])
            .expect_err("bad key length");
        assert!(err.to_string().contains("32 bytes"));
    }

    #[test]
    fn rejects_invalid_instance_pubkey() {
        assert!(parse_instance_pubkey("not-base64!!").is_err());
        assert!(
            parse_instance_pubkey(&base64::engine::general_purpose::STANDARD.encode([0u8; 8]))
                .is_err()
        );
    }
}
