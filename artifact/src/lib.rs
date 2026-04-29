#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;
use thiserror::Error;

pub const TRAILER_MAGIC: &[u8; 8] = b"HLCWSG01";
pub const TRAILER_VERSION: u8 = 1;
pub const CWASM_NO_VMEM_MEMORY_GUARD_SIZE: u64 = 0;
pub const CWASM_NO_VMEM_MEMORY_RESERVATION: u64 = 0;
pub const CWASM_NO_VMEM_MEMORY_RESERVATION_FOR_GROWTH: u64 = 1 << 20;

pub fn cwasm_target_uses_lazy_commit_virtual_memory(target: &str) -> bool {
    matches!(target, "aarch64-unknown-none")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SignatureAlgorithm {
    Ed25519 = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerCertificate {
    pub public_key: [u8; 32],
    pub issuer_public_key: [u8; 32],
    #[serde(with = "BigArray")]
    pub issuer_signature: [u8; 64],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureTrailer {
    pub version: u8,
    pub algorithm: SignatureAlgorithm,
    pub signer: SignerCertificate,
    #[serde(with = "BigArray")]
    pub payload_signature: [u8; 64],
}

impl SignatureTrailer {
    pub const fn new(
        algorithm: SignatureAlgorithm,
        signer: SignerCertificate,
        payload_signature: [u8; 64],
    ) -> Self {
        Self {
            version: TRAILER_VERSION,
            algorithm,
            signer,
            payload_signature,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TrailerError {
    #[error("artifact does not contain a Helios signature trailer")]
    MissingTrailer,
    #[error("artifact trailer footer is truncated")]
    TruncatedFooter,
    #[error("artifact trailer payload is truncated")]
    TruncatedTrailer,
    #[error("artifact trailer length overflowed usize")]
    LengthOverflow,
    #[error("artifact trailer version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("artifact trailer failed to decode: {0}")]
    Decode(postcard::Error),
    #[error("artifact trailer failed to encode: {0}")]
    Encode(postcard::Error),
    #[error("artifact signer public key is invalid")]
    InvalidSignerKey,
    #[error("artifact issuer public key is invalid")]
    InvalidIssuerKey,
    #[error("artifact signer certificate signature is invalid")]
    InvalidCertificateSignature,
    #[error("artifact signer is not rooted in a trusted certificate")]
    UntrustedSigner,
    #[error("artifact payload signature is invalid")]
    InvalidPayloadSignature,
}

pub struct ParsedArtifact<'a> {
    payload: &'a [u8],
    trailer: SignatureTrailer,
}

impl<'a> ParsedArtifact<'a> {
    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }

    pub const fn trailer(&self) -> &SignatureTrailer {
        &self.trailer
    }
}

pub fn append_signature_trailer(
    payload: &[u8],
    trailer: &SignatureTrailer,
) -> Result<Vec<u8>, TrailerError> {
    let encoded = postcard::to_allocvec(trailer).map_err(TrailerError::Encode)?;
    let encoded_len = u32::try_from(encoded.len()).map_err(|_| TrailerError::LengthOverflow)?;
    let mut bytes = Vec::with_capacity(payload.len() + encoded.len() + 12);
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&encoded);
    bytes.extend_from_slice(&encoded_len.to_le_bytes());
    bytes.extend_from_slice(TRAILER_MAGIC);
    Ok(bytes)
}

pub fn parse_signed_artifact(bytes: &[u8]) -> Result<ParsedArtifact<'_>, TrailerError> {
    const FOOTER_LEN: usize = 12;
    if bytes.len() < FOOTER_LEN {
        return Err(TrailerError::TruncatedFooter);
    }

    let footer = &bytes[bytes.len() - FOOTER_LEN..];
    if &footer[4..] != TRAILER_MAGIC {
        return Err(TrailerError::MissingTrailer);
    }

    let trailer_len_u32 = u32::from_le_bytes(
        footer[..4]
            .try_into()
            .unwrap_or_else(|_| panic!("footer trailer length slice must be 4 bytes")),
    );
    let trailer_len = usize::try_from(trailer_len_u32).map_err(|_| TrailerError::LengthOverflow)?;
    let trailer_end = bytes.len() - FOOTER_LEN;
    if trailer_len > trailer_end {
        return Err(TrailerError::TruncatedTrailer);
    }
    let trailer_start = trailer_end - trailer_len;
    let payload = &bytes[..trailer_start];
    let trailer_bytes = &bytes[trailer_start..trailer_end];
    let trailer: SignatureTrailer =
        postcard::from_bytes(trailer_bytes).map_err(TrailerError::Decode)?;
    if trailer.version != TRAILER_VERSION {
        return Err(TrailerError::UnsupportedVersion(trailer.version));
    }

    Ok(ParsedArtifact { payload, trailer })
}

pub fn strip_signature_trailer(bytes: &[u8]) -> Result<&[u8], TrailerError> {
    parse_signed_artifact(bytes).map(|artifact| artifact.payload())
}

pub fn sign_payload_with_key(
    payload: &[u8],
    signing_key: &SigningKey,
) -> Result<Vec<u8>, TrailerError> {
    let public_key = VerifyingKey::from(signing_key).to_bytes();
    append_signature_trailer(
        payload,
        &SignatureTrailer::new(
            SignatureAlgorithm::Ed25519,
            SignerCertificate {
                public_key,
                issuer_public_key: public_key,
                issuer_signature: signing_key
                    .sign(&certificate_message(&public_key))
                    .to_bytes(),
            },
            signing_key.sign(payload).to_bytes(),
        ),
    )
}

pub fn sign_payload_with_secret_key_bytes(
    payload: &[u8],
    secret_key: &[u8; 32],
) -> Result<Vec<u8>, TrailerError> {
    sign_payload_with_key(payload, &SigningKey::from_bytes(secret_key))
}

pub fn verify_signed_payload<'a>(
    bytes: &'a [u8],
    trusted_root_public_keys: &[[u8; 32]],
) -> Result<ParsedArtifact<'a>, TrailerError> {
    let parsed = parse_signed_artifact(bytes)?;
    let signer = parsed.trailer().signer;
    let signer_key =
        VerifyingKey::from_bytes(&signer.public_key).map_err(|_| TrailerError::InvalidSignerKey)?;
    let issuer_key = VerifyingKey::from_bytes(&signer.issuer_public_key)
        .map_err(|_| TrailerError::InvalidIssuerKey)?;
    let issuer_signature = Signature::from_bytes(&signer.issuer_signature);
    issuer_key
        .verify(&certificate_message(&signer.public_key), &issuer_signature)
        .map_err(|_| TrailerError::InvalidCertificateSignature)?;
    if !trusted_root_public_keys
        .iter()
        .any(|trusted| trusted == &signer.issuer_public_key)
    {
        return Err(TrailerError::UntrustedSigner);
    }
    let payload_signature = Signature::from_bytes(&parsed.trailer().payload_signature);
    signer_key
        .verify(parsed.payload(), &payload_signature)
        .map_err(|_| TrailerError::InvalidPayloadSignature)?;
    Ok(parsed)
}

pub fn certificate_message(public_key: &[u8; 32]) -> [u8; 48] {
    let mut bytes = [0_u8; 48];
    bytes[..16].copy_from_slice(b"helios-cert-v01:");
    bytes[16..].copy_from_slice(public_key);
    bytes
}

impl fmt::Display for SignatureAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignatureAlgorithm::Ed25519 => f.write_str("ed25519"),
        }
    }
}
