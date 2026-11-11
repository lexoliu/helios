extern crate alloc;

use alloc::vec::Vec;
use helios_artifact::{TrailerError, parse_signed_artifact};
use thiserror::Error;

use crate::CwasmSigningKey;

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

/// Accepts a written artifact only when `signer`, this boot's
/// compiler-signing key, issued its trailer.
///
/// The build root that verified the boot payload signs nothing the
/// kernel loads from anywhere else, so it is not consulted here.
/// Signs `payload` with the build root, the key a kernel image carried
/// before the kernel generated its own, for tests that prove such an
/// artifact is refused.
#[cfg(test)]
pub(crate) fn sign_artifact_with_build_root_for_tests(payload: &[u8]) -> Vec<u8> {
    helios_artifact::sign_payload_with_secret_key_bytes(
        payload,
        &generated::TRUSTED_ROOT_SIGNING_KEY,
    )
    .expect("signing a test payload must succeed")
}

pub fn verify_signed_artifact<'a>(
    bytes: UntrustedCwasm<'a>,
    signer: &CwasmSigningKey,
) -> Result<TrustedCwasm<'a>, ArtifactTrustError> {
    let parsed = signer.verify(bytes.bytes())?;
    Ok(TrustedCwasm {
        payload: parsed.payload(),
    })
}

/// Signs the compiler plugin's output with this boot's key.
pub fn sign_artifact_payload(
    payload: &[u8],
    signer: &CwasmSigningKey,
) -> Result<Vec<u8>, ArtifactTrustError> {
    signer.sign(payload).map_err(ArtifactTrustError::Trailer)
}

/// The public keys a boot payload's signature footer must name.
pub fn trusted_root_public_keys() -> &'static [[u8; 32]] {
    generated::TRUSTED_ROOT_PUBLIC_KEYS
}

mod generated {
    include!(concat!(env!("OUT_DIR"), "/trusted_roots.rs"));
    // The build root's secret signs test payloads only; no kernel image
    // carries it.
    #[cfg(test)]
    include!(concat!(env!("OUT_DIR"), "/trusted_signing_key.rs"));
}
