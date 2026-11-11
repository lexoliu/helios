extern crate alloc;

use alloc::vec::Vec;
use helios_artifact::{
    TrailerError, parse_signed_artifact, sign_payload_with_secret_key_bytes, verify_signed_payload,
};
use thiserror::Error;

pub struct TrustedCwasm<'a> {
    payload: &'a [u8],
}

impl<'a> TrustedCwasm<'a> {
    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

#[derive(Clone, Copy)]
pub struct UntrustedCwasm<'a> {
    bytes: &'a [u8],
}

impl<'a> UntrustedCwasm<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

#[derive(Debug, Error)]
pub enum ArtifactTrustError {
    #[error("artifact trailer is invalid: {0}")]
    Trailer(#[from] TrailerError),
}

pub fn is_cwasm(bytes: &[u8]) -> bool {
    parse_signed_artifact(bytes).is_ok()
}

/// Accepts a boot-payload artifact as the bare Wasmtime artifact it is.
///
/// The payload's signature footer covered every byte of the image
/// before [`crate::BootPayload::parse`] read it, so a file inside it
/// carries no signature of its own; [`crate::BootfsBytes`] is what
/// proves the bytes are part of it. Anything else goes through
/// [`verify_signed_artifact`].
pub fn trust_bootfs_artifact(bytes: &crate::BootfsBytes) -> TrustedCwasm<'_> {
    TrustedCwasm {
        payload: bytes.as_slice(),
    }
}

/// Signs a boot-payload image body with the build root, the way
/// `helios-cli kernel-prebuild` does, for tests that need a payload the
/// kernel's embedded root accepts.
#[cfg(test)]
pub(crate) fn sign_bootfs_image_for_tests(body: Vec<u8>) -> Vec<u8> {
    helios_artifact::bootfs::sign_image_with_secret_key_bytes(
        body,
        &generated::TRUSTED_ROOT_SIGNING_KEY,
    )
}

pub fn verify_signed_artifact(
    bytes: UntrustedCwasm<'_>,
) -> Result<TrustedCwasm<'_>, ArtifactTrustError> {
    let parsed = verify_signed_payload(bytes.bytes(), trusted_root_public_keys())?;
    Ok(TrustedCwasm {
        payload: parsed.payload(),
    })
}

pub fn trusted_root_public_keys() -> &'static [[u8; 32]] {
    generated::TRUSTED_ROOT_PUBLIC_KEYS
}

pub fn sign_trusted_artifact_payload(payload: &[u8]) -> Result<Vec<u8>, ArtifactTrustError> {
    sign_payload_with_secret_key_bytes(payload, &generated::TRUSTED_ROOT_SIGNING_KEY)
        .map_err(ArtifactTrustError::Trailer)
}

mod generated {
    include!(concat!(env!("OUT_DIR"), "/trusted_roots.rs"));
    include!(concat!(env!("OUT_DIR"), "/trusted_signing_key.rs"));
}
