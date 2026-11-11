//! An in-memory [`MeasuredBoot`] device for tests of the code that
//! consumes the contract.
//!
//! It keeps the two properties a consumer depends on. A register is only
//! ever extended, and its value is determined by the ordered sequence of
//! digests extended into it since the last power cycle. The sealed slot
//! survives a power cycle and releases its secret only while every
//! register it is bound to holds the value it held when the secret was
//! stored.
//!
//! The model keeps each register as that ordered sequence rather than as
//! a running SHA-256 fold. The contract never reveals a register's value,
//! only whether two states are equal, and two sequences are equal exactly
//! when their folds are (up to a SHA-256 collision), so the model's
//! answers are the device's answers without a hash in this crate.
//!
//! Like [`crate::vmm_test_harness`], the model lives in `hal` so every
//! consumer's tests share it instead of each writing its own.

use alloc::vec::Vec;

use crate::measured_boot::{
    MeasuredBoot, MeasuredBootError, MeasurementRegister, MeasurementSet, SealedSecret,
    Sha256Digest, UnsealRefused,
};

/// What one register has had extended into it since the last power
/// cycle, oldest first.
type RegisterHistory = Vec<Sha256Digest>;

enum Slot {
    Absent,
    /// A slot someone else defined: its attributes are not this
    /// contract's, so nothing it holds can be trusted.
    Foreign,
    Sealed {
        bound_to: MeasurementSet,
        /// The histories of the registers in `bound_to` when the secret
        /// was stored, lowest register first.
        state: Vec<RegisterHistory>,
        secret: SealedSecret,
    },
}

/// A measurement device whose registers and slot live in memory.
pub struct MeasuredBootModel {
    kernel_image_registers: MeasurementSet,
    registers: [RegisterHistory; MeasurementSet::CAPACITY as usize],
    slot: Slot,
}

impl MeasuredBootModel {
    /// A device fresh from the factory: every register at its reset
    /// value and no slot defined. `kernel_image_registers` is what
    /// [`MeasuredBoot::kernel_image_registers`] reports.
    pub fn new(kernel_image_registers: MeasurementSet) -> Self {
        Self {
            kernel_image_registers,
            registers: core::array::from_fn(|_| Vec::new()),
            slot: Slot::Absent,
        }
    }

    /// Returns every register to its reset value, as a power cycle does.
    /// The slot is non-volatile and keeps what it holds.
    pub fn power_cycle(&mut self) {
        for register in &mut self.registers {
            register.clear();
        }
    }

    /// Replaces the slot with one this contract did not define, the way
    /// anyone holding the device's owner authorization could.
    pub fn plant_foreign_slot(&mut self) {
        self.slot = Slot::Foreign;
    }

    fn snapshot(&self, bound_to: MeasurementSet) -> Vec<RegisterHistory> {
        bound_to
            .iter()
            .map(|register| self.registers[usize::from(register.index())].clone())
            .collect()
    }
}

impl MeasuredBoot for MeasuredBootModel {
    fn kernel_image_registers(&self) -> MeasurementSet {
        self.kernel_image_registers
    }

    async fn extend(
        &mut self,
        register: MeasurementRegister,
        digest: &Sha256Digest,
    ) -> Result<(), MeasuredBootError> {
        self.registers[usize::from(register.index())].push(*digest);
        Ok(())
    }

    async fn load_sealed(
        &mut self,
        bound_to: MeasurementSet,
    ) -> Result<SealedSecret, UnsealRefused> {
        let current = self.snapshot(bound_to);
        match &self.slot {
            Slot::Absent => Err(UnsealRefused::Empty),
            Slot::Foreign => Err(UnsealRefused::ForeignSlot),
            Slot::Sealed {
                bound_to: sealed_to,
                state,
                secret,
            } => {
                if *sealed_to == bound_to && *state == current {
                    Ok(SealedSecret::new(*secret.expose()))
                } else {
                    Err(UnsealRefused::StateChanged)
                }
            }
        }
    }

    async fn store_sealed(
        &mut self,
        bound_to: MeasurementSet,
        secret: &SealedSecret,
    ) -> Result<(), MeasuredBootError> {
        self.slot = Slot::Sealed {
            bound_to,
            state: self.snapshot(bound_to),
            secret: SealedSecret::new(*secret.expose()),
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use super::*;

    const PCR4: MeasurementRegister = MeasurementRegister::new(4);
    const PCR9: MeasurementRegister = MeasurementRegister::new(9);
    const BOUND: MeasurementSet = MeasurementSet::of(&[PCR4, PCR9]);

    /// The model never suspends, so one poll completes every operation.
    fn complete<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the in-memory measurement device suspended"),
        }
    }

    fn digest(byte: u8) -> Sha256Digest {
        Sha256Digest([byte; 32])
    }

    /// Firmware measuring the loader and the loader measuring the kernel.
    fn boot(model: &mut MeasuredBootModel, kernel: u8) {
        complete(model.extend(PCR4, &digest(0x04))).unwrap();
        complete(model.extend(PCR9, &digest(kernel))).unwrap();
    }

    fn secret(byte: u8) -> SealedSecret {
        SealedSecret::new([byte; SealedSecret::LEN])
    }

    #[test]
    fn a_fresh_device_has_nothing_sealed() {
        let mut model = MeasuredBootModel::new(BOUND);
        boot(&mut model, 0x09);
        assert_eq!(
            complete(model.load_sealed(BOUND)).unwrap_err(),
            UnsealRefused::Empty
        );
    }

    #[test]
    fn the_same_boot_chain_reads_back_the_stored_secret() {
        let mut model = MeasuredBootModel::new(BOUND);
        boot(&mut model, 0x09);
        complete(model.store_sealed(BOUND, &secret(0xa5))).unwrap();

        model.power_cycle();
        boot(&mut model, 0x09);
        let loaded = complete(model.load_sealed(BOUND)).unwrap();
        assert_eq!(loaded.expose(), secret(0xa5).expose());
    }

    #[test]
    fn a_changed_register_refuses_the_secret() {
        let mut model = MeasuredBootModel::new(BOUND);
        boot(&mut model, 0x09);
        complete(model.store_sealed(BOUND, &secret(0xa5))).unwrap();

        model.power_cycle();
        boot(&mut model, 0x10);
        assert_eq!(
            complete(model.load_sealed(BOUND)).unwrap_err(),
            UnsealRefused::StateChanged
        );
    }

    #[test]
    fn an_extend_after_storing_refuses_the_secret() {
        let mut model = MeasuredBootModel::new(BOUND);
        boot(&mut model, 0x09);
        complete(model.store_sealed(BOUND, &secret(0xa5))).unwrap();
        complete(model.extend(PCR9, &digest(0x99))).unwrap();
        assert_eq!(
            complete(model.load_sealed(BOUND)).unwrap_err(),
            UnsealRefused::StateChanged
        );
    }

    #[test]
    fn a_register_outside_the_binding_does_not_matter() {
        let mut model = MeasuredBootModel::new(BOUND);
        boot(&mut model, 0x09);
        complete(model.store_sealed(BOUND, &secret(0xa5))).unwrap();
        complete(model.extend(MeasurementRegister::new(16), &digest(0x16))).unwrap();
        assert!(complete(model.load_sealed(BOUND)).is_ok());
    }

    #[test]
    fn a_different_binding_refuses_the_secret() {
        let mut model = MeasuredBootModel::new(BOUND);
        boot(&mut model, 0x09);
        complete(model.store_sealed(BOUND, &secret(0xa5))).unwrap();
        assert_eq!(
            complete(model.load_sealed(MeasurementSet::empty().with(PCR9))).unwrap_err(),
            UnsealRefused::StateChanged
        );
    }

    #[test]
    fn a_foreign_slot_is_reported_and_replaced_by_a_store() {
        let mut model = MeasuredBootModel::new(BOUND);
        boot(&mut model, 0x09);
        model.plant_foreign_slot();
        assert_eq!(
            complete(model.load_sealed(BOUND)).unwrap_err(),
            UnsealRefused::ForeignSlot
        );

        complete(model.store_sealed(BOUND, &secret(0x3c))).unwrap();
        let loaded = complete(model.load_sealed(BOUND)).unwrap();
        assert_eq!(loaded.expose(), secret(0x3c).expose());
    }

    #[test]
    fn storing_in_a_new_state_makes_the_old_secret_unrecoverable() {
        let mut model = MeasuredBootModel::new(BOUND);
        boot(&mut model, 0x09);
        complete(model.store_sealed(BOUND, &secret(0xa5))).unwrap();

        model.power_cycle();
        boot(&mut model, 0x10);
        complete(model.store_sealed(BOUND, &secret(0x5a))).unwrap();

        model.power_cycle();
        boot(&mut model, 0x09);
        assert_eq!(
            complete(model.load_sealed(BOUND)).unwrap_err(),
            UnsealRefused::StateChanged
        );
    }

    #[test]
    fn the_model_reports_the_registers_it_was_built_with() {
        assert_eq!(
            MeasuredBootModel::new(BOUND).kernel_image_registers(),
            BOUND
        );
    }
}
