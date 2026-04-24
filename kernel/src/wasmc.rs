extern crate alloc;

use helios_artifact::{TrailerError, parse_signed_artifact, verify_signed_payload};
use thiserror::Error;

pub struct TrustedWasmc<'a> {
    payload: &'a [u8],
}

impl<'a> TrustedWasmc<'a> {
    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

#[derive(Clone, Copy)]
pub struct UntrustedWasm<'a> {
    bytes: &'a [u8],
}

impl<'a> UntrustedWasm<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

#[derive(Clone, Copy)]
pub struct UntrustedWasmc<'a> {
    bytes: &'a [u8],
}

impl<'a> UntrustedWasmc<'a> {
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

pub fn is_wasmc(bytes: &[u8]) -> bool {
    parse_signed_artifact(bytes).is_ok()
}

pub fn trust_bootfs_artifact(
    bytes: UntrustedWasmc<'_>,
) -> Result<TrustedWasmc<'_>, ArtifactTrustError> {
    let parsed = parse_signed_artifact(bytes.bytes())?;
    Ok(TrustedWasmc {
        payload: parsed.payload(),
    })
}

pub fn verify_signed_artifact(
    bytes: UntrustedWasmc<'_>,
) -> Result<TrustedWasmc<'_>, ArtifactTrustError> {
    let parsed = verify_signed_payload(bytes.bytes(), trusted_root_public_keys())?;
    Ok(TrustedWasmc {
        payload: parsed.payload(),
    })
}

pub fn trusted_root_public_keys() -> &'static [[u8; 32]] {
    generated::TRUSTED_ROOT_PUBLIC_KEYS
}

mod generated {
    include!(concat!(env!("OUT_DIR"), "/trusted_roots.rs"));
}
