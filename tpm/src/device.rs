//! [`MeasuredBoot`] on a TPM 2.0.
//!
//! Registers are the SHA-256 bank's PCRs. The sealed slot is one NV
//! index whose only read and write authorization is a policy session
//! that has run `TPM2_PolicyPCR` over the registers it is bound to: the
//! TPM itself refuses the read once any of them holds another value.

use helios_hal::measured_boot::{
    MeasuredBoot, MeasuredBootError, MeasurementRegister, MeasurementSet, SealedSecret,
    Sha256Digest, UnsealRefused,
};
use thiserror::Error;
use tpm2_protocol::TpmWriter;
use tpm2_protocol::basic::{TpmBuffer, TpmHandle, TpmUint16, TpmUint32};
use tpm2_protocol::constant::TPM_MAX_COMMAND_SIZE;
use tpm2_protocol::data::{
    Tpm2bNvPublic, TpmAlgId, TpmCap, TpmRc, TpmRcBase, TpmRh, TpmSe, TpmSt, TpmaNv, TpmaSession,
    TpmlDigestValues, TpmlPcrSelection, TpmsAuthCommand, TpmsNvPublic, TpmsPcrSelect,
    TpmsPcrSelection, TpmtHa, TpmtSymDef, TpmuCapabilities, TpmuHa, TpmuSymKeyBits, TpmuSymMode,
};
use tpm2_protocol::frame::{
    TpmFlushContextCommand, TpmFlushContextResponse, TpmFrame, TpmGetCapabilityCommand,
    TpmGetCapabilityResponse, TpmGetRandomCommand, TpmGetRandomResponse, TpmNvDefineSpaceCommand,
    TpmNvDefineSpaceResponse, TpmNvReadCommand, TpmNvReadPublicCommand, TpmNvReadPublicResponse,
    TpmNvReadResponse, TpmNvUndefineSpaceCommand, TpmNvUndefineSpaceResponse, TpmNvWriteCommand,
    TpmNvWriteResponse, TpmPcrExtendCommand, TpmPcrExtendResponse, TpmPolicyGetDigestCommand,
    TpmPolicyGetDigestResponse, TpmPolicyPcrCommand, TpmPolicyPcrResponse, TpmResponse,
    TpmStartAuthSessionCommand, TpmStartAuthSessionResponse, TpmUnmarshalBody, tpm_marshal_command,
};
use zeroize::Zeroize;

use crate::transport::{Transport, TransportError};

/// The NV index of the sealed slot.
///
/// It lies in the range the TCG "Registry of Reserved TPM 2.0 Handles
/// and Localities" (Table 2, v1.1) leaves to the TPM owner,
/// 0x01800000-0x01BFFFFF, inside the 0x01890000-0x01BFFFFF part that its
/// list of known owner usages (v1.9, Table 4) marks unclaimed. The low
/// half spells "HL".
pub const SLOT_INDEX: u32 = 0x0189_484c;

/// The slot's attributes: read and written only through its policy,
/// never with owner or index authorization, and outside dictionary-
/// attack lockout, since a failed policy is not a guessed password.
const SLOT_ATTRIBUTES: TpmaNv = TpmaNv::from_bits_truncate(
    TpmaNv::POLICYREAD.bits() | TpmaNv::POLICYWRITE.bits() | TpmaNv::NO_DA.bits(),
);

/// A SHA-256 digest's size: the size of every policy digest here, of
/// every value extended and of the secret.
const SHA256_LEN: usize = 32;

/// Why [`Tpm::open`] refused a TPM.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum OpenError {
    #[error(transparent)]
    Device(#[from] MeasuredBootError),
    #[error("the TPM has no SHA-256 PCR bank")]
    NoSha256Bank,
    #[error("the TPM's SHA-256 bank does not keep the kernel image registers {missing:?}")]
    RegistersMissing { missing: MeasurementSet },
}

/// What the TPM holds at [`SLOT_INDEX`].
enum Slot {
    Absent,
    Defined(TpmsNvPublic),
}

/// A TPM 2.0 as a [`MeasuredBoot`] device.
pub struct Tpm<T> {
    transport: T,
    kernel_image_registers: MeasurementSet,
    /// The registers the SHA-256 bank keeps.
    registers: MeasurementSet,
    /// How many bytes the bank's PCR bitmap has.
    select_size: usize,
    command: [u8; TPM_MAX_COMMAND_SIZE],
    response: [u8; TPM_MAX_COMMAND_SIZE],
}

fn handle(value: u32) -> TpmHandle {
    TpmHandle::new(value)
}

fn empty<const N: usize>() -> TpmBuffer<N> {
    TpmBuffer::new()
}

/// The password session that carries the empty authorization of PCRs
/// and of the owner hierarchy (TPM 2.0 Part 1, "Password
/// Authorizations").
fn password() -> TpmsAuthCommand {
    TpmsAuthCommand {
        session_handle: handle(TpmRh::Pw.value()),
        nonce: empty(),
        session_attributes: TpmaSession::CONTINUE_SESSION,
        hmac: empty(),
    }
}

/// A policy session as a command's authorization. The session is kept
/// open so it is flushed on every path, success or not.
fn policy(session: TpmHandle) -> TpmsAuthCommand {
    TpmsAuthCommand {
        session_handle: session,
        nonce: empty(),
        session_attributes: TpmaSession::CONTINUE_SESSION,
        hmac: empty(),
    }
}

fn refused(operation: &'static str, code: TpmRc) -> MeasuredBootError {
    MeasuredBootError::Refused {
        operation,
        code: code.value(),
    }
}

fn malformed(operation: &'static str) -> MeasuredBootError {
    MeasuredBootError::MalformedResponse { operation }
}

fn transport_failed(operation: &'static str, error: TransportError) -> MeasuredBootError {
    match error {
        TransportError::Timeout { state, waited_ms } => {
            MeasuredBootError::Timeout { state, waited_ms }
        }
        TransportError::Fatal => MeasuredBootError::DeviceFault { operation },
        TransportError::ResponseSize { .. } => malformed(operation),
        TransportError::Protocol { what } => MeasuredBootError::Interface { what },
        TransportError::CommandTooLarge { .. } => MeasuredBootError::Interface {
            what: "a command larger than the interface's buffer",
        },
        TransportError::BufferOutsideWindow { .. } => MeasuredBootError::Interface {
            what: "a CRB buffer outside the register window",
        },
        TransportError::WrongInterface { .. } => MeasuredBootError::Interface {
            what: "a register page of another interface type",
        },
    }
}

fn succeeded<R>(
    operation: &'static str,
    outcome: Result<R, TpmRc>,
) -> Result<R, MeasuredBootError> {
    outcome.map_err(|code| refused(operation, code))
}

/// Whether `public` is the slot this contract defines, whatever state
/// it is bound to.
fn is_ours(public: &TpmsNvPublic) -> bool {
    public.nv_index.value() == SLOT_INDEX
        && public.name_alg == TpmAlgId::Sha256
        && public.attributes.bits() & !TpmaNv::WRITTEN.bits() == SLOT_ATTRIBUTES.bits()
        && usize::from(public.data_size.value()) == SealedSecret::LEN
        && public.auth_policy.len() == SHA256_LEN
}

impl<T: Transport> Tpm<T> {
    /// Takes the TPM behind `transport`, after checking that its SHA-256
    /// bank keeps every one of `kernel_image_registers`, the registers
    /// the chain that loaded the kernel measured it into.
    pub async fn open(
        transport: T,
        kernel_image_registers: MeasurementSet,
    ) -> Result<Self, OpenError> {
        let mut tpm = Self {
            transport,
            kernel_image_registers,
            registers: MeasurementSet::empty(),
            select_size: 0,
            command: [0; TPM_MAX_COMMAND_SIZE],
            response: [0; TPM_MAX_COMMAND_SIZE],
        };

        const OPERATION: &str = "TPM2_GetCapability(TPM_CAP_PCRS)";
        // For TPM_CAP_PCRS the property is reserved and every bank is
        // reported in one list (TPM 2.0 Part 3, TPM2_GetCapability).
        let command = TpmGetCapabilityCommand {
            handles: [],
            cap: TpmCap::Pcrs,
            property: TpmUint32::new(0),
            property_count: TpmUint32::new(1),
        };
        let response: TpmGetCapabilityResponse =
            succeeded(OPERATION, tpm.execute(OPERATION, &command, &[]).await?)?;
        let TpmuCapabilities::Pcrs(banks) = response.capability_data.data else {
            return Err(malformed(OPERATION).into());
        };
        let Some(bank) = banks.iter().find(|bank| bank.hash == TpmAlgId::Sha256) else {
            if response.more_data.0 {
                return Err(malformed(OPERATION).into());
            }
            return Err(OpenError::NoSha256Bank);
        };
        let mut bitmap = [0; 4];
        bitmap[..bank.pcr_select.len()].copy_from_slice(&bank.pcr_select);
        tpm.select_size = bank.pcr_select.len();
        tpm.registers = MeasurementSet::from_bits(u32::from_le_bytes(bitmap));

        let missing =
            MeasurementSet::from_bits(kernel_image_registers.bits() & !tpm.registers.bits());
        if !missing.is_empty() {
            return Err(OpenError::RegistersMissing { missing });
        }
        Ok(tpm)
    }

    /// Sends one command and reads its response, returning the TPM's
    /// own refusal as the inner error.
    async fn execute<C, R>(
        &mut self,
        operation: &'static str,
        command: &C,
        sessions: &[TpmsAuthCommand],
    ) -> Result<Result<R, TpmRc>, MeasuredBootError>
    where
        C: TpmFrame + Sync,
        R: TpmUnmarshalBody + Send,
    {
        let tag = if sessions.is_empty() {
            TpmSt::NoSessions
        } else {
            TpmSt::Sessions
        };
        let mut writer = TpmWriter::new(&mut self.command);
        tpm_marshal_command(command, tag, sessions, &mut writer).unwrap_or_else(|error| {
            panic!("{operation} does not marshal into a TPM_MAX_COMMAND_SIZE frame: {error}")
        });
        let len = writer.len();

        let received = self
            .transport
            .transact(&self.command[..len], &mut self.response)
            .await
            .map_err(|error| transport_failed(operation, error))?;
        let response =
            TpmResponse::cast(&self.response[..received]).map_err(|_| malformed(operation))?;
        let code = response.rc().map_err(|_| malformed(operation))?;
        if code != TpmRc::Fmt0(TpmRcBase::Success) {
            return Ok(Err(code));
        }
        response
            .unmarshal::<R>()
            .map(Ok)
            .map_err(|_| malformed(operation))
    }

    fn check(&self, registers: MeasurementSet) -> Result<(), MeasuredBootError> {
        match registers
            .iter()
            .find(|register| !self.registers.contains(*register))
        {
            Some(register) => Err(MeasuredBootError::NoSuchRegister { register }),
            None => Ok(()),
        }
    }

    fn selection(&self, registers: MeasurementSet) -> TpmlPcrSelection {
        let bitmap = registers.bits().to_le_bytes();
        let mut list = TpmlPcrSelection::new();
        list.try_push(TpmsPcrSelection {
            hash: TpmAlgId::Sha256,
            pcr_select: TpmsPcrSelect::try_from(&bitmap[..self.select_size])
                .expect("the bank's bitmap size was read from the TPM and fits TPMS_PCR_SELECT"),
        })
        .expect("one selection fits a TPML_PCR_SELECTION");
        list
    }

    /// Starts a session of `kind` and runs `TPM2_PolicyPCR` over
    /// `bound_to`'s current values in it.
    async fn policy_session(
        &mut self,
        kind: TpmSe,
        bound_to: MeasurementSet,
    ) -> Result<TpmHandle, MeasuredBootError> {
        const RANDOM: &str = "TPM2_GetRandom";
        let random: TpmGetRandomResponse = succeeded(
            RANDOM,
            self.execute(
                RANDOM,
                &TpmGetRandomCommand {
                    handles: [],
                    bytes_requested: TpmUint16::new(SHA256_LEN as u16),
                },
                &[],
            )
            .await?,
        )?;

        const START: &str = "TPM2_StartAuthSession";
        // An unbound, unsalted session without parameter encryption:
        // nothing it authorizes carries an HMAC, so it needs no key.
        let started: TpmStartAuthSessionResponse = succeeded(
            START,
            self.execute(
                START,
                &TpmStartAuthSessionCommand {
                    handles: [handle(TpmRh::Null.value()), handle(TpmRh::Null.value())],
                    nonce_caller: random.random_bytes,
                    encrypted_salt: empty(),
                    session_type: kind,
                    symmetric: TpmtSymDef {
                        algorithm: TpmAlgId::Null,
                        key_bits: TpmuSymKeyBits::Null,
                        mode: TpmuSymMode::Null,
                    },
                    auth_hash: TpmAlgId::Sha256,
                },
                &[],
            )
            .await?,
        )?;
        let [session] = started.handles;

        const POLICY: &str = "TPM2_PolicyPCR";
        // An empty pcrDigest asks the TPM to use the registers' current
        // values (TPM 2.0 Part 3, TPM2_PolicyPCR).
        let bound = self
            .execute::<_, TpmPolicyPcrResponse>(
                POLICY,
                &TpmPolicyPcrCommand {
                    handles: [session],
                    pcr_digest: empty(),
                    pcrs: self.selection(bound_to),
                },
                &[],
            )
            .await
            .and_then(|outcome| succeeded(POLICY, outcome));
        if let Err(error) = bound {
            self.flush(session).await?;
            return Err(error);
        }
        Ok(session)
    }

    async fn flush(&mut self, session: TpmHandle) -> Result<(), MeasuredBootError> {
        const OPERATION: &str = "TPM2_FlushContext";
        let _: TpmFlushContextResponse = succeeded(
            OPERATION,
            self.execute(
                OPERATION,
                &TpmFlushContextCommand {
                    handles: [],
                    flush_handle: session,
                },
                &[],
            )
            .await?,
        )?;
        Ok(())
    }

    /// The policy digest `TPM2_PolicyPCR` over `bound_to`'s current
    /// values produces, computed by the TPM in a trial session.
    async fn policy_digest(
        &mut self,
        bound_to: MeasurementSet,
    ) -> Result<[u8; SHA256_LEN], MeasuredBootError> {
        const OPERATION: &str = "TPM2_PolicyGetDigest";
        let session = self.policy_session(TpmSe::Trial, bound_to).await?;
        let digest = self
            .execute::<_, TpmPolicyGetDigestResponse>(
                OPERATION,
                &TpmPolicyGetDigestCommand { handles: [session] },
                &[],
            )
            .await
            .and_then(|outcome| succeeded(OPERATION, outcome));
        self.flush(session).await?;
        digest?
            .policy_digest
            .as_ref()
            .try_into()
            .map_err(|_| malformed(OPERATION))
    }

    async fn read_public(&mut self) -> Result<Slot, MeasuredBootError> {
        const OPERATION: &str = "TPM2_NV_ReadPublic";
        let outcome = self
            .execute::<_, TpmNvReadPublicResponse>(
                OPERATION,
                &TpmNvReadPublicCommand {
                    handles: [handle(SLOT_INDEX)],
                },
                &[],
            )
            .await?;
        match outcome {
            Ok(response) => Ok(Slot::Defined(response.nv_public.inner)),
            // TPM_RC_HANDLE on the index: nothing is defined there.
            Err(code) if code.base() == TpmRcBase::Handle => Ok(Slot::Absent),
            Err(code) => Err(refused(OPERATION, code)),
        }
    }

    async fn undefine(&mut self) -> Result<(), MeasuredBootError> {
        const OPERATION: &str = "TPM2_NV_UndefineSpace";
        let _: TpmNvUndefineSpaceResponse = succeeded(
            OPERATION,
            self.execute(
                OPERATION,
                &TpmNvUndefineSpaceCommand {
                    handles: [handle(TpmRh::Owner.value()), handle(SLOT_INDEX)],
                },
                &[password()],
            )
            .await?,
        )?;
        Ok(())
    }

    async fn define(&mut self, policy_digest: &[u8; SHA256_LEN]) -> Result<(), MeasuredBootError> {
        const OPERATION: &str = "TPM2_NV_DefineSpace";
        let auth_policy =
            TpmBuffer::try_from(&policy_digest[..]).expect("a SHA-256 digest fits TPM2B_DIGEST");
        let _: TpmNvDefineSpaceResponse = succeeded(
            OPERATION,
            self.execute(
                OPERATION,
                &TpmNvDefineSpaceCommand {
                    handles: [handle(TpmRh::Owner.value())],
                    auth: empty(),
                    public_info: Tpm2bNvPublic {
                        inner: TpmsNvPublic {
                            nv_index: handle(SLOT_INDEX),
                            name_alg: TpmAlgId::Sha256,
                            attributes: SLOT_ATTRIBUTES,
                            auth_policy,
                            data_size: TpmUint16::new(SealedSecret::LEN as u16),
                        },
                    },
                },
                &[password()],
            )
            .await?,
        )?;
        Ok(())
    }

    async fn write(
        &mut self,
        bound_to: MeasurementSet,
        secret: &SealedSecret,
    ) -> Result<(), MeasuredBootError> {
        const OPERATION: &str = "TPM2_NV_Write";
        let session = self.policy_session(TpmSe::Policy, bound_to).await?;
        let written = self
            .execute::<_, TpmNvWriteResponse>(
                OPERATION,
                &TpmNvWriteCommand {
                    handles: [handle(SLOT_INDEX), handle(SLOT_INDEX)],
                    data: TpmBuffer::try_from(&secret.expose()[..])
                        .expect("a sealed secret fits TPM2B_MAX_NV_BUFFER"),
                    offset: TpmUint16::new(0),
                },
                &[policy(session)],
            )
            .await
            .and_then(|outcome| succeeded(OPERATION, outcome));
        // The command frame held the secret; the buffer outlives it.
        self.command.zeroize();
        self.flush(session).await?;
        written.map(|_| ())
    }

    async fn read(&mut self, bound_to: MeasurementSet) -> Result<SealedSecret, UnsealRefused> {
        const OPERATION: &str = "TPM2_NV_Read";
        let session = self.policy_session(TpmSe::Policy, bound_to).await?;
        let read = self
            .execute::<_, TpmNvReadResponse>(
                OPERATION,
                &TpmNvReadCommand {
                    handles: [handle(SLOT_INDEX), handle(SLOT_INDEX)],
                    size: TpmUint16::new(SealedSecret::LEN as u16),
                    offset: TpmUint16::new(0),
                },
                &[policy(session)],
            )
            .await;
        // The response frame held the secret; the buffer outlives it.
        self.response.zeroize();
        self.flush(session).await?;
        match read? {
            Ok(response) => {
                let bytes: [u8; SealedSecret::LEN] = response
                    .data
                    .as_ref()
                    .try_into()
                    .map_err(|_| malformed(OPERATION))?;
                Ok(SealedSecret::new(bytes))
            }
            // The session's PolicyPCR digest is not the slot's
            // authPolicy: a register it is bound to has changed.
            Err(code) if code.base() == TpmRcBase::PolicyFail => Err(UnsealRefused::StateChanged),
            Err(code) => Err(refused(OPERATION, code).into()),
        }
    }
}

impl<T: Transport + 'static> MeasuredBoot for Tpm<T> {
    fn kernel_image_registers(&self) -> MeasurementSet {
        self.kernel_image_registers
    }

    async fn extend(
        &mut self,
        register: MeasurementRegister,
        digest: &Sha256Digest,
    ) -> Result<(), MeasuredBootError> {
        const OPERATION: &str = "TPM2_PCR_Extend";
        self.check(MeasurementSet::empty().with(register))?;
        let mut digests = TpmlDigestValues::new();
        digests
            .try_push(TpmtHa {
                hash_alg: TpmAlgId::Sha256,
                digest: TpmuHa::Digest(
                    TpmBuffer::try_from(&digest.0[..]).expect("a SHA-256 digest fits TPMU_HA"),
                ),
            })
            .expect("one digest fits a TPML_DIGEST_VALUES");
        let command = TpmPcrExtendCommand {
            handles: [handle(u32::from(register.index()))],
            digests,
        };
        let _: TpmPcrExtendResponse = succeeded(
            OPERATION,
            self.execute(OPERATION, &command, &[password()]).await?,
        )?;
        Ok(())
    }

    async fn load_sealed(
        &mut self,
        bound_to: MeasurementSet,
    ) -> Result<SealedSecret, UnsealRefused> {
        self.check(bound_to)?;
        let public = match self.read_public().await? {
            Slot::Absent => return Err(UnsealRefused::Empty),
            Slot::Defined(public) => public,
        };
        // Checked before any read: anyone with the empty owner
        // authorization can define a look-alike index at this handle
        // that a forger can write.
        if !is_ours(&public) {
            return Err(UnsealRefused::ForeignSlot);
        }
        if !public.attributes.contains(TpmaNv::WRITTEN) {
            return Err(UnsealRefused::Empty);
        }
        self.read(bound_to).await
    }

    async fn store_sealed(
        &mut self,
        bound_to: MeasurementSet,
        secret: &SealedSecret,
    ) -> Result<(), MeasuredBootError> {
        self.check(bound_to)?;
        let policy_digest = self.policy_digest(bound_to).await?;
        let defined = match self.read_public().await? {
            Slot::Absent => false,
            Slot::Defined(public)
                if is_ours(&public) && public.auth_policy.as_ref() == policy_digest =>
            {
                true
            }
            Slot::Defined(_) => {
                self.undefine().await?;
                false
            }
        };
        if !defined {
            self.define(&policy_digest).await?;
        }
        self.write(bound_to, secret).await
    }
}
