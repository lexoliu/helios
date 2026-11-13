//! The key the kernel signs its own compiler's output with.
//!
//! No image carries it: the kernel derives it at bring-up from a 32-byte
//! seed that exists only in this boot's memory and, where the platform
//! has a measured-boot device, in that device's sealed slot. Before the
//! slot is read, the kernel extends [`PAYLOAD_REGISTER`] with the digest
//! of the boot payload it verified, so the seed is released only to the
//! kernel image and loader the pre-kernel chain measured, booting the
//! payload this kernel verified.
//!
//! | Platform | Slot | Seed |
//! | --- | --- | --- |
//! | measured boot | holds a seed sealed to this state | unsealed, the same key as the boot that sealed it |
//! | measured boot | empty, sealed to another state, or not this kernel's | drawn from the root DRBG and sealed to this state |
//! | none | — | drawn from the root DRBG, new every boot |
//!
//! A seed sealed to another state is not recoverable and is not routed
//! around: a new one replaces it, and what the old key signed no longer
//! verifies. A device that fails is a boot failure, never a quiet step
//! down to a per-boot key, which would discard what the device keeps.
//!
//! # Concurrency contract
//!
//! The seed is settled once, on the bootstrap processor, before the
//! program service exists; the device is owned by that call alone. The
//! [`CwasmSigningKey`] it yields is immutable afterwards and held by the
//! program service, so signing and verifying take no lock on any
//! processor.

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use ed25519_dalek::SigningKey;
use helios_artifact::bootfs::Fingerprint;
use helios_artifact::{ParsedArtifact, TrailerError, sign_payload_with_key, verify_signed_payload};
use helios_hal::measured_boot::{
    MeasuredBoot, MeasuredBootError, MeasurementRegister, MeasurementSet, SealedSecret,
    Sha256Digest, UnsealRefused,
};
use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::RootEntropy;
use crate::bring_up::complete_device_future_at_bring_up;

/// The register the kernel extends with the boot payload it verified.
///
/// PCR 9 is where the UAPI Group's Linux TPM PCR Registry places the
/// initrds a kernel was handed, which is the role the `helios-bootfs`
/// payload plays here; Limine extends the same register with the kernel
/// image and every module it loaded.
pub const PAYLOAD_REGISTER: MeasurementRegister = MeasurementRegister::new(9);

/// The HKDF-SHA256 `info` the compiler-signing key is expanded under.
const CWASM_SIGNING_INFO: &[u8] = b"helios.cwasm-signing.v1";

/// The Ed25519 key the in-kernel compiler's output is signed with, and
/// the only key a written `cwasm` verifies against.
///
/// The private half never leaves this type: it is wiped when the key
/// is dropped, and `Debug` names the public key's fingerprint alone.
pub struct CwasmSigningKey {
    key: SigningKey,
}

impl CwasmSigningKey {
    /// The key HKDF-SHA256 expands from `seed`.
    pub(crate) fn from_seed(seed: &SealedSecret) -> Self {
        let mut okm = Zeroizing::new([0_u8; ed25519_dalek::SECRET_KEY_LENGTH]);
        Hkdf::<Sha256>::new(None, seed.expose())
            .expand(CWASM_SIGNING_INFO, okm.as_mut_slice())
            .unwrap_or_else(|_| {
                panic!("HKDF-SHA256 expands up to 8160 bytes, and an Ed25519 secret is 32")
            });
        Self {
            key: SigningKey::from_bytes(&okm),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// The public key's short name for diagnostics.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.public_key())
    }

    /// `payload` followed by a trailer that names this key as both its
    /// signer and its issuer.
    pub fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, TrailerError> {
        sign_payload_with_key(payload, &self.key)
    }

    /// The payload of `artifact`, when its trailer was issued by this key.
    pub fn verify<'a>(&self, artifact: &'a [u8]) -> Result<ParsedArtifact<'a>, TrailerError> {
        verify_signed_payload(artifact, &[self.public_key()])
    }
}

impl fmt::Debug for CwasmSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "CwasmSigningKey({})", self.fingerprint())
    }
}

/// Where this boot's seed came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedSource {
    /// The measured-boot device released the seed an earlier boot of
    /// the same measured state sealed.
    Unsealed,
    /// The device released nothing, so a new seed was drawn and sealed
    /// to this boot's state.
    Provisioned(ProvisionCause),
    /// The platform has no measured-boot device: the seed was drawn for
    /// this boot and dies with it.
    Ephemeral,
}

impl SeedSource {
    /// The word the boot line names the source with.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unsealed => "unsealed",
            Self::Provisioned(_) => "provisioned-and-sealed",
            Self::Ephemeral => "ephemeral",
        }
    }
}

/// Why the device released no seed, so that a new one was sealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvisionCause {
    /// Nothing was sealed yet.
    Empty,
    /// The seed in the slot is bound to another measured state: another
    /// kernel, loader or payload.
    StateChanged,
    /// The slot was not defined the way the contract defines it, which
    /// only someone holding the device's owner authorization can do.
    ForeignSlot,
}

impl ProvisionCause {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::StateChanged => "state-changed",
            Self::ForeignSlot => "foreign-slot",
        }
    }
}

/// Why the seed could not be settled. Each is a device failure, and
/// each stops the boot.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SigningKeyError {
    #[error("extending {register} with the boot payload digest failed: {source}")]
    Measure {
        register: MeasurementRegister,
        source: MeasuredBootError,
    },
    #[error("reading the sealed signing seed bound to registers {bound_to:#x} failed: {source}")]
    Unseal {
        bound_to: u32,
        source: MeasuredBootError,
    },
    #[error("sealing a new signing seed to registers {bound_to:#x} failed: {source}")]
    Seal {
        bound_to: u32,
        source: MeasuredBootError,
    },
}

/// This boot's compiler-signing key and where its seed came from.
#[derive(Debug)]
pub struct SettledSigningKey {
    pub key: CwasmSigningKey,
    pub source: SeedSource,
    /// The registers the seed is sealed to; empty for an ephemeral seed.
    pub bound_to: MeasurementSet,
}

/// Settles this boot's compiler-signing key.
///
/// With a device, `payload_digest` is extended into [`PAYLOAD_REGISTER`]
/// first, and only then is the slot bound to the kernel image registers
/// and that register read; a seed it does not release is replaced by a
/// fresh one from `entropy`, sealed to the same registers. Without one,
/// the seed is drawn from `entropy` for this boot alone.
pub async fn settle_signing_key<Device: MeasuredBoot>(
    measured_boot: Option<&mut Device>,
    payload_digest: &Sha256Digest,
    entropy: &RootEntropy,
) -> Result<SettledSigningKey, SigningKeyError> {
    let Some(device) = measured_boot else {
        let seed = draw_seed(entropy);
        return Ok(SettledSigningKey {
            key: CwasmSigningKey::from_seed(&seed),
            source: SeedSource::Ephemeral,
            bound_to: MeasurementSet::empty(),
        });
    };

    device
        .extend(PAYLOAD_REGISTER, payload_digest)
        .await
        .map_err(|source| SigningKeyError::Measure {
            register: PAYLOAD_REGISTER,
            source,
        })?;
    let bound_to = device.kernel_image_registers().with(PAYLOAD_REGISTER);
    let cause = match device.load_sealed(bound_to).await {
        Ok(seed) => {
            return Ok(SettledSigningKey {
                key: CwasmSigningKey::from_seed(&seed),
                source: SeedSource::Unsealed,
                bound_to,
            });
        }
        Err(UnsealRefused::Empty) => ProvisionCause::Empty,
        Err(UnsealRefused::StateChanged) => ProvisionCause::StateChanged,
        Err(UnsealRefused::ForeignSlot) => ProvisionCause::ForeignSlot,
        Err(UnsealRefused::Device(source)) => {
            return Err(SigningKeyError::Unseal {
                bound_to: bound_to.bits(),
                source,
            });
        }
    };
    let seed = draw_seed(entropy);
    device
        .store_sealed(bound_to, &seed)
        .await
        .map_err(|source| SigningKeyError::Seal {
            bound_to: bound_to.bits(),
            source,
        })?;
    Ok(SettledSigningKey {
        key: CwasmSigningKey::from_seed(&seed),
        source: SeedSource::Provisioned(cause),
        bound_to,
    })
}

/// Settles the key during bring-up, states which path it took, and
/// stops the boot when the device failed.
///
/// It runs before the executor starts, so the device's futures are
/// driven by [`complete_device_future_at_bring_up`]: the measured-boot
/// contract makes every operation wait on nothing but the device.
pub(crate) fn settle_signing_key_at_bring_up<Device: MeasuredBoot>(
    measured_boot: Option<&mut Device>,
    payload_digest: &Sha256Digest,
    entropy: &RootEntropy,
) -> CwasmSigningKey {
    let settled = complete_device_future_at_bring_up(settle_signing_key(
        measured_boot,
        payload_digest,
        entropy,
    ))
    .unwrap_or_else(|error| panic!("the compiler-signing key could not be settled: {error}"));
    if settled.source == SeedSource::Provisioned(ProvisionCause::ForeignSlot) {
        tracing::warn!(
            "the sealed slot was not defined by this kernel; someone with the device's owner \
             authorization replaced it, and a new seed now takes its place"
        );
    }
    let cause = match settled.source {
        SeedSource::Provisioned(cause) => cause.label(),
        SeedSource::Unsealed | SeedSource::Ephemeral => "none",
    };
    tracing::info!(
        seed = settled.source.label(),
        cause,
        bound_to = format_args!("{:#x}", settled.bound_to.bits()),
        signer = %settled.key.fingerprint(),
        "compiler-signing key settled"
    );
    settled.key
}

/// A new seed from the root DRBG, written straight into the secret.
fn draw_seed(entropy: &RootEntropy) -> SealedSecret {
    SealedSecret::filled_by(|bytes| entropy.fill(bytes))
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use helios_hal::measured_boot::NoMeasuredBoot;
    use helios_hal::measured_boot_model::{MeasuredBootModel, Operation, OperationKind};

    use super::*;
    use crate::machine::MachineGeneration;
    use crate::test_support::TestCpu;

    const PCR4: MeasurementRegister = MeasurementRegister::new(4);
    const KERNEL_IMAGE: MeasurementSet = MeasurementSet::of(&[PCR4, PAYLOAD_REGISTER]);
    const BOUND: MeasurementSet = KERNEL_IMAGE.with(PAYLOAD_REGISTER);

    /// The model never suspends, so one poll completes every operation.
    fn complete<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("settling the key against the in-memory device suspended"),
        }
    }

    /// A boot's root DRBG. Each boot draws from a different processor
    /// source, as two boots of one machine do.
    fn entropy(boot: u8) -> RootEntropy {
        RootEntropy::from_platform(
            &TestCpu::with_entropy(boot),
            None,
            None,
            MachineGeneration::new(None),
        )
        .expect("a processor source seeds the root")
    }

    fn payload(byte: u8) -> Sha256Digest {
        Sha256Digest([byte; 32])
    }

    /// One boot: the pre-kernel chain measures the loader and the
    /// kernel, then the kernel settles its key for `payload_digest`.
    fn boot(
        device: &mut MeasuredBootModel,
        boot_number: u8,
        payload_digest: &Sha256Digest,
    ) -> Result<SettledSigningKey, SigningKeyError> {
        device.power_cycle();
        complete(device.extend(PCR4, &Sha256Digest([0x04; 32]))).unwrap();
        complete(device.extend(PAYLOAD_REGISTER, &Sha256Digest([0x4b; 32]))).unwrap();
        complete(settle_signing_key(
            Some(device),
            payload_digest,
            &entropy(boot_number),
        ))
    }

    fn ephemeral(boot: u8) -> SettledSigningKey {
        complete(settle_signing_key(
            None::<&mut NoMeasuredBoot>,
            &payload(0x01),
            &entropy(boot),
        ))
        .expect("a platform without a device always settles a key")
    }

    #[test]
    fn the_same_measured_state_keeps_the_key_across_boots() {
        let mut device = MeasuredBootModel::new(KERNEL_IMAGE);
        let first = boot(&mut device, 1, &payload(0x01)).unwrap();
        assert_eq!(first.source, SeedSource::Provisioned(ProvisionCause::Empty));
        assert_eq!(first.bound_to, BOUND);

        let second = boot(&mut device, 2, &payload(0x01)).unwrap();
        assert_eq!(second.source, SeedSource::Unsealed);
        assert_eq!(second.key.public_key(), first.key.public_key());
    }

    #[test]
    fn a_changed_payload_is_refused_and_its_new_key_is_kept() {
        let mut device = MeasuredBootModel::new(KERNEL_IMAGE);
        let original = boot(&mut device, 1, &payload(0x01)).unwrap();

        let rebuilt = boot(&mut device, 2, &payload(0x02)).unwrap();
        assert_eq!(
            rebuilt.source,
            SeedSource::Provisioned(ProvisionCause::StateChanged)
        );
        assert_ne!(rebuilt.key.public_key(), original.key.public_key());

        let again = boot(&mut device, 3, &payload(0x02)).unwrap();
        assert_eq!(again.source, SeedSource::Unsealed);
        assert_eq!(again.key.public_key(), rebuilt.key.public_key());

        // The original payload's seed was replaced, not kept beside the
        // new one: booting it again provisions a third key.
        let restored = boot(&mut device, 4, &payload(0x01)).unwrap();
        assert_eq!(
            restored.source,
            SeedSource::Provisioned(ProvisionCause::StateChanged)
        );
        assert_ne!(restored.key.public_key(), original.key.public_key());
    }

    #[test]
    fn a_foreign_slot_is_replaced_by_a_sealed_seed() {
        let mut device = MeasuredBootModel::new(KERNEL_IMAGE);
        device.plant_foreign_slot();
        let replaced = boot(&mut device, 1, &payload(0x01)).unwrap();
        assert_eq!(
            replaced.source,
            SeedSource::Provisioned(ProvisionCause::ForeignSlot)
        );
        let next = boot(&mut device, 2, &payload(0x01)).unwrap();
        assert_eq!(next.source, SeedSource::Unsealed);
        assert_eq!(next.key.public_key(), replaced.key.public_key());
    }

    #[test]
    fn without_a_device_every_boot_has_a_new_key() {
        let first = ephemeral(1);
        let second = ephemeral(2);
        assert_eq!(first.source, SeedSource::Ephemeral);
        assert_eq!(first.bound_to, MeasurementSet::empty());
        assert_ne!(first.key.public_key(), second.key.public_key());
    }

    #[test]
    fn the_payload_is_measured_before_the_slot_is_read() {
        let mut device = MeasuredBootModel::new(KERNEL_IMAGE);
        boot(&mut device, 1, &payload(0x01)).unwrap();
        let kernel_ops = &device.journal()[2..];
        assert_eq!(
            kernel_ops,
            [
                Operation::Extend {
                    register: PAYLOAD_REGISTER,
                    digest: payload(0x01),
                },
                Operation::LoadSealed { bound_to: BOUND },
                Operation::StoreSealed { bound_to: BOUND },
            ]
        );

        boot(&mut device, 2, &payload(0x01)).unwrap();
        assert_eq!(
            &device.journal()[2..],
            [
                Operation::Extend {
                    register: PAYLOAD_REGISTER,
                    digest: payload(0x01),
                },
                Operation::LoadSealed { bound_to: BOUND },
            ]
        );
    }

    #[test]
    fn the_seed_is_bound_to_the_payload_register_even_when_the_chain_does_not_name_it() {
        let mut device = MeasuredBootModel::new(MeasurementSet::of(&[PCR4]));
        let settled = boot(&mut device, 1, &payload(0x01)).unwrap();
        assert_eq!(settled.bound_to, BOUND);
    }

    const FAULT: MeasuredBootError = MeasuredBootError::Interface {
        what: "a status register that never settles",
    };

    #[test]
    fn a_failed_extend_stops_before_the_slot_is_touched() {
        let mut device = MeasuredBootModel::new(KERNEL_IMAGE);
        device.power_cycle();
        device.fail_next(OperationKind::Extend, FAULT);
        let error = complete(settle_signing_key(
            Some(&mut device),
            &payload(0x01),
            &entropy(1),
        ))
        .unwrap_err();
        assert_eq!(
            error,
            SigningKeyError::Measure {
                register: PAYLOAD_REGISTER,
                source: FAULT,
            }
        );
        assert_eq!(device.journal().len(), 1);
    }

    #[test]
    fn a_failed_unseal_is_an_error_and_not_a_new_seed() {
        let mut device = MeasuredBootModel::new(KERNEL_IMAGE);
        boot(&mut device, 1, &payload(0x01)).unwrap();
        device.power_cycle();
        device.fail_next(OperationKind::LoadSealed, FAULT);
        let error = complete(settle_signing_key(
            Some(&mut device),
            &payload(0x01),
            &entropy(2),
        ))
        .unwrap_err();
        assert_eq!(
            error,
            SigningKeyError::Unseal {
                bound_to: BOUND.bits(),
                source: FAULT,
            }
        );
        assert!(
            device
                .journal()
                .iter()
                .all(|operation| operation.kind() != OperationKind::StoreSealed),
            "a device that failed to unseal must not have its slot overwritten"
        );
    }

    #[test]
    fn a_failed_seal_is_an_error_and_not_an_ephemeral_key() {
        let mut device = MeasuredBootModel::new(KERNEL_IMAGE);
        device.fail_next(OperationKind::StoreSealed, FAULT);
        let error = boot(&mut device, 1, &payload(0x01)).unwrap_err();
        assert_eq!(
            error,
            SigningKeyError::Seal {
                bound_to: BOUND.bits(),
                source: FAULT,
            }
        );
    }

    #[test]
    #[should_panic(expected = "the compiler-signing key could not be settled")]
    fn bring_up_stops_the_boot_on_a_device_failure() {
        let mut device = MeasuredBootModel::new(KERNEL_IMAGE);
        device.fail_next(OperationKind::Extend, FAULT);
        settle_signing_key_at_bring_up(Some(&mut device), &payload(0x01), &entropy(1));
    }

    #[test]
    fn a_key_verifies_what_it_signed_and_nothing_another_key_signed() {
        let first = ephemeral(1).key;
        let second = ephemeral(2).key;
        let signed = first.sign(b"payload").expect("a trailer encodes");
        let parsed = first.verify(&signed).expect("its own signature verifies");
        assert_eq!(parsed.payload(), b"payload");
        assert_eq!(
            second.verify(&signed).err(),
            Some(TrailerError::UntrustedSigner)
        );
    }

    #[test]
    fn a_key_prints_its_fingerprint_and_no_secret() {
        let key = CwasmSigningKey::from_seed(&SealedSecret::new([0x11; SealedSecret::LEN]));
        assert_eq!(
            alloc::format!("{key:?}"),
            alloc::format!("CwasmSigningKey({})", key.fingerprint())
        );
    }

    #[test]
    fn the_key_is_hkdf_sha256_of_the_seed_under_the_cwasm_info() {
        let seed = SealedSecret::new([0x11; SealedSecret::LEN]);
        let mut expected = [0_u8; 32];
        Hkdf::<Sha256>::new(None, seed.expose())
            .expand(b"helios.cwasm-signing.v1", &mut expected)
            .unwrap();
        assert_eq!(
            CwasmSigningKey::from_seed(&seed).public_key(),
            SigningKey::from_bytes(&expected).verifying_key().to_bytes()
        );
    }
}
