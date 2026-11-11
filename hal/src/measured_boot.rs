//! Measured boot: extending measurement registers and keeping one secret
//! that only an unchanged boot chain can read back.
//!
//! A measurement register can only be extended, never written: each
//! extend folds a digest into the register's previous value, so the
//! value a register holds names everything that was extended into it, in
//! order. Code that runs before the kernel records the kernel image and
//! its loader into registers this way, and the device releases a sealed
//! secret only while the registers it was bound to hold the values they
//! held when it was stored. A kernel that measured itself would prove
//! nothing, which is why [`MeasuredBoot::kernel_image_registers`] names
//! registers the pre-kernel chain filled.
//!
//! # Concurrency contract
//!
//! A measurement device runs one command at a time. Its handle is owned
//! exclusively by one task and every operation takes `&mut self`: it is
//! never shared between processors and holds no lock. No interrupt
//! handler reaches it; an implementation polls the device's status and
//! yields to the executor on every not-ready read, so a slow device
//! never pins a processor and needs no interrupt-masking lock.
//!
//! Every value type here is a plain value and may cross processors
//! freely; [`SealedSecret`] is the one that is neither `Copy` nor
//! `Clone`, and it erases its bytes when dropped.

use core::fmt;
use core::future::Future;

use thiserror::Error;
use zeroize::Zeroize;

/// One measurement register, by its index on the device.
///
/// The index is below 32, the width of a [`MeasurementSet`]: a set must
/// be able to name every register a register can be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MeasurementRegister(u8);

impl MeasurementRegister {
    /// The register at `index`.
    ///
    /// # Panics
    ///
    /// Panics when `index` is 32 or more, which no [`MeasurementSet`]
    /// can name. Called in a constant, the panic is a build error.
    pub const fn new(index: u8) -> Self {
        assert!(
            index < MeasurementSet::CAPACITY,
            "a measurement register index must be below 32"
        );
        Self(index)
    }

    /// The register's index on the device.
    pub const fn index(self) -> u8 {
        self.0
    }
}

impl fmt::Display for MeasurementRegister {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "register {}", self.0)
    }
}

/// A set of measurement registers: bit `n` names register `n`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MeasurementSet(u32);

impl MeasurementSet {
    /// How many registers a set can name.
    pub const CAPACITY: u8 = u32::BITS as u8;

    /// The set that names no register.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The set of exactly `registers`.
    pub const fn of(registers: &[MeasurementRegister]) -> Self {
        let mut set = Self::empty();
        let mut index = 0;
        while index < registers.len() {
            set = set.with(registers[index]);
            index += 1;
        }
        set
    }

    /// This set with `register` added.
    pub const fn with(self, register: MeasurementRegister) -> Self {
        Self(self.0 | (1 << register.0))
    }

    /// Every register in either set.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether `register` is in the set.
    pub const fn contains(self, register: MeasurementRegister) -> bool {
        self.0 & (1 << register.0) != 0
    }

    /// Whether every register of `other` is in this set.
    pub const fn contains_all(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The set as a bitmap: bit `n` names register `n`.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// The set a bitmap names: bit `n` names register `n`.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The registers in the set, lowest index first.
    pub fn iter(self) -> impl Iterator<Item = MeasurementRegister> {
        (0..Self::CAPACITY)
            .map(MeasurementRegister)
            .filter(move |register| self.contains(*register))
    }
}

/// A SHA-256 digest: the value extended into a register.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest(pub [u8; 32]);

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Sha256Digest(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// The secret a measurement device keeps for one boot chain.
///
/// It is neither `Copy` nor `Clone`, its `Debug` output names no byte,
/// and its bytes are erased when it is dropped, so the only copies that
/// exist are the ones a caller makes from [`SealedSecret::expose`].
pub struct SealedSecret([u8; SealedSecret::LEN]);

impl SealedSecret {
    /// The size of a sealed secret in bytes.
    pub const LEN: usize = 32;

    pub const fn new(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    /// The secret's bytes.
    pub const fn expose(&self) -> &[u8; Self::LEN] {
        &self.0
    }
}

impl Drop for SealedSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SealedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SealedSecret(..)")
    }
}

/// Why a measurement device could not do what it was asked.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum MeasuredBootError {
    /// The device does not have the register, or does not keep a SHA-256
    /// value for it.
    #[error("the measurement device has no SHA-256 value for {register}")]
    NoSuchRegister { register: MeasurementRegister },
    /// The device did not reach a state it must reach to make progress.
    #[error("the measurement device did not reach {state} within {waited_ms} ms")]
    Timeout { state: &'static str, waited_ms: u64 },
    /// The interface to the device broke its own protocol.
    #[error("the measurement device's interface reported {what}")]
    Interface { what: &'static str },
    /// The device reported a fatal error of its own.
    #[error("the measurement device reported a fatal error during {operation}")]
    DeviceFault { operation: &'static str },
    /// The device answered a command with an error code.
    #[error("the measurement device refused {operation} with response code {code:#010x}")]
    Refused { operation: &'static str, code: u32 },
    /// The device's answer to a command does not parse as one.
    #[error("the measurement device's answer to {operation} is malformed")]
    MalformedResponse { operation: &'static str },
}

/// Why [`MeasuredBoot::load_sealed`] returned no secret.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum UnsealRefused {
    /// No secret has been stored.
    #[error("no secret is sealed")]
    Empty,
    /// A secret is stored, but the registers it was bound to no longer
    /// hold the values they held when it was stored. It cannot be
    /// recovered.
    #[error("the sealed secret is bound to a different measured state")]
    StateChanged,
    /// The device's slot exists but was not defined the way this
    /// contract defines it, so whoever wrote it is not known.
    #[error("the sealed slot was not defined by this contract")]
    ForeignSlot,
    /// The device failed.
    #[error(transparent)]
    Device(#[from] MeasuredBootError),
}

/// A device that extends measurement registers and seals one secret to
/// their values.
///
/// See the [module documentation](self) for the concurrency contract:
/// one owner, one command at a time, no lock, no interrupt.
pub trait MeasuredBoot: Send + 'static {
    /// The registers where pre-kernel code recorded the kernel image and
    /// its loader.
    fn kernel_image_registers(&self) -> MeasurementSet;

    /// Folds `digest` into `register`'s SHA-256 value.
    fn extend<'a>(
        &'a mut self,
        register: MeasurementRegister,
        digest: &'a Sha256Digest,
    ) -> impl Future<Output = Result<(), MeasuredBootError>> + Send + 'a;

    /// The secret stored by [`MeasuredBoot::store_sealed`], released only
    /// while every register in `bound_to` holds the value it held then.
    fn load_sealed(
        &mut self,
        bound_to: MeasurementSet,
    ) -> impl Future<Output = Result<SealedSecret, UnsealRefused>> + Send + '_;

    /// Replaces the stored secret with `secret`, bound to the current
    /// values of the registers in `bound_to`.
    fn store_sealed<'a>(
        &'a mut self,
        bound_to: MeasurementSet,
        secret: &'a SealedSecret,
    ) -> impl Future<Output = Result<(), MeasuredBootError>> + Send + 'a;
}

/// The measurement device of a platform that has none.
///
/// It has no value, so `Option<NoMeasuredBoot>` is always `None`.
#[derive(Debug)]
pub enum NoMeasuredBoot {}

impl MeasuredBoot for NoMeasuredBoot {
    fn kernel_image_registers(&self) -> MeasurementSet {
        match *self {}
    }

    async fn extend(
        &mut self,
        _register: MeasurementRegister,
        _digest: &Sha256Digest,
    ) -> Result<(), MeasuredBootError> {
        match *self {}
    }

    async fn load_sealed(
        &mut self,
        _bound_to: MeasurementSet,
    ) -> Result<SealedSecret, UnsealRefused> {
        match *self {}
    }

    async fn store_sealed(
        &mut self,
        _bound_to: MeasurementSet,
        _secret: &SealedSecret,
    ) -> Result<(), MeasuredBootError> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R4: MeasurementRegister = MeasurementRegister::new(4);
    const R9: MeasurementRegister = MeasurementRegister::new(9);

    #[test]
    fn a_set_names_exactly_the_registers_it_was_built_from() {
        let set = MeasurementSet::of(&[R4, R9]);
        assert_eq!(set.bits(), (1 << 4) | (1 << 9));
        assert!(set.contains(R4) && set.contains(R9));
        assert!(!set.contains(MeasurementRegister::new(0)));
        assert_eq!(set.iter().collect::<alloc::vec::Vec<_>>(), [R4, R9]);
        assert!(set.contains_all(MeasurementSet::empty().with(R9)));
        assert!(!MeasurementSet::empty().with(R9).contains_all(set));
    }

    #[test]
    fn the_highest_register_a_set_can_name_round_trips() {
        let top = MeasurementRegister::new(31);
        assert_eq!(MeasurementSet::empty().with(top).bits(), 1 << 31);
    }

    #[test]
    #[should_panic(expected = "a measurement register index must be below 32")]
    fn a_register_a_set_cannot_name_is_refused() {
        let _ = MeasurementRegister::new(32);
    }

    #[test]
    fn a_sealed_secret_never_prints_its_bytes() {
        let secret = SealedSecret::new([0x5a; SealedSecret::LEN]);
        assert_eq!(alloc::format!("{secret:?}"), "SealedSecret(..)");
    }
}
