//! Machine-wide settings: the operator's values for limits the kernel
//! enforces.
//!
//! A setting is the value new processes and new requests take. Changing one
//! never reaches into a process that is already running; each consumer reads
//! the setting where it creates the thing the setting bounds, and that thing
//! keeps what it read.
//!
//! # Concurrency contract
//!
//! - **Storage.** One `AtomicU64` per setting. All of them live in one
//!   [`Settings`] value that the runtime state owns inside a
//!   `crossbeam_utils::CachePadded` block, so the words do not share a cache
//!   line with the state's neighbours, which are written often. The words
//!   themselves are written rarely, by whichever processor serves a write,
//!   and share that one block.
//! - **Reads are lock-free and wait-free.** A read is one relaxed load, on
//!   any processor and from any task. No interrupt handler reads a setting,
//!   though nothing forbids it.
//! - **Writes are lock-free.** A write is one relaxed swap on the task that
//!   serves it, and answers the exact value it replaced. Concurrent writers
//!   are ordered by the word's modification order: the last swap wins and each
//!   writer is told precisely what it displaced.
//! - **A reader racing a writer** sees the old value or the new one, never a
//!   torn mix, because every target has native 64-bit atomics. No ordering
//!   with other memory is promised, since a setting publishes no other data
//!   with it. A write that happens-before the creation of the thing it bounds
//!   (the writer later starts it, or a message, channel or task wake orders
//!   the two) is seen by that creation's read, or a later write is, by
//!   write-read coherence. Relaxed ordering is enough for that.
//! - **No multi-setting consistency.** Each setting is independent and there
//!   is no transaction. Nothing reads two settings that must agree.
//! - **Nothing blocks and nothing is async.** Every operation completes in a
//!   bounded number of instructions.
//!
//! # Declaring a setting
//!
//! A setting is a type implementing [`Setting`], declared beside the value it
//! bounds, and listed once in the `settings!` invocation at the end of this
//! module. A reader names the type, so renaming or removing a setting fails
//! the build at every reader:
//!
//! ```ignore
//! let limit = runtime_state.settings().get::<DescriptorLimitSetting>();
//! ```
//!
//! The list generates the [`Settings`] storage, one [`Holds`] impl per
//! setting, [`SETTING_COUNT`], the listing and the by-name write the guest
//! interface needs, and a compile-time check that no two settings share a
//! name. The guest interface maps through [`Settings::entries`],
//! [`Settings::read_by_name`] and [`Settings::write_by_name`] and never names
//! a setting, so a setting's guest-visible name is its [`Setting::NAME`] by
//! construction. A unit added to [`SettingUnit`] fails the build at that
//! mapping until it renders the unit.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::WriteSettingsCap;

/// What a setting's value measures.
///
/// The guest interface renders each unit as its own case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingUnit {
    /// A number of things: descriptor numbers, table entries.
    Count,
    /// A length in bytes.
    Bytes,
}

/// A setting's value at the guest boundary: its unit and its amount.
///
/// Kernel readers never see this; they read the setting's own type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingValue {
    Count(u64),
    Bytes(u64),
}

impl SettingValue {
    /// `amount` measured in `unit`.
    pub const fn new(unit: SettingUnit, amount: u64) -> Self {
        match unit {
            SettingUnit::Count => Self::Count(amount),
            SettingUnit::Bytes => Self::Bytes(amount),
        }
    }

    pub const fn unit(self) -> SettingUnit {
        match self {
            Self::Count(_) => SettingUnit::Count,
            Self::Bytes(_) => SettingUnit::Bytes,
        }
    }

    pub const fn amount(self) -> u64 {
        match self {
            Self::Count(amount) | Self::Bytes(amount) => amount,
        }
    }
}

/// One operator setting. The implementing type is the setting's name in
/// Rust: a reader names it, so renaming or removing it fails the build at
/// every reader.
pub trait Setting: 'static {
    /// The name the guest interface lists it under: `subsystem.name`.
    const NAME: &'static str;
    /// What it bounds, in one sentence.
    const DESCRIPTION: &'static str;
    const UNIT: SettingUnit;
    /// The type its consumers enforce.
    type Value: Copy;
    /// What it holds until an operator sets it.
    const DEFAULT: Self::Value;
    /// The amount, in [`Self::UNIT`], that stands for `value`.
    fn to_raw(value: Self::Value) -> u64;
    /// The value an amount in [`Self::UNIT`] stands for. Operators are
    /// trusted, so every amount is a value.
    fn from_raw(raw: u64) -> Self::Value;
}

/// The word one setting lives in.
pub struct SettingCell<S: Setting> {
    raw: AtomicU64,
    _setting: PhantomData<fn() -> S>,
}

impl<S: Setting> Default for SettingCell<S> {
    fn default() -> Self {
        Self {
            raw: AtomicU64::new(S::to_raw(S::DEFAULT)),
            _setting: PhantomData,
        }
    }
}

impl<S: Setting> SettingCell<S> {
    /// What the setting is now.
    pub fn get(&self) -> S::Value {
        S::from_raw(self.raw.load(Ordering::Relaxed))
    }

    /// Makes `value` what the setting is from now on and answers what it
    /// was. `_proof` is the caller's right to write settings.
    pub fn set(&self, _proof: &WriteSettingsCap, value: S::Value) -> S::Value {
        S::from_raw(self.raw.swap(S::to_raw(value), Ordering::Relaxed))
    }

    /// The setting as one row of the listing.
    pub fn entry(&self) -> SettingEntry {
        SettingEntry {
            name: S::NAME,
            description: S::DESCRIPTION,
            value: SettingValue::new(S::UNIT, self.raw.load(Ordering::Relaxed)),
        }
    }

    /// As [`Self::set`], for a value that arrived with a unit. Refused, and
    /// changing nothing, when `value` is in another unit than the setting's.
    pub fn write_value(
        &self,
        _proof: &WriteSettingsCap,
        value: SettingValue,
    ) -> Result<SettingValue, SettingWriteError> {
        if value.unit() != S::UNIT {
            return Err(SettingWriteError::UnitMismatch { holds: S::UNIT });
        }
        let previous = self.raw.swap(value.amount(), Ordering::Relaxed);
        Ok(SettingValue::new(S::UNIT, previous))
    }
}

/// `Settings` holds `S`. Implemented only by the `settings!` list, so a
/// setting missing from the list has no reader.
pub trait Holds<S: Setting> {
    fn cell(&self) -> &SettingCell<S>;
}

/// Why a write by name changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum SettingWriteError {
    #[error("no setting has that name")]
    Unknown,
    #[error("the setting holds a {holds:?}")]
    UnitMismatch { holds: SettingUnit },
}

/// One row of the listing the guest interface returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub value: SettingValue,
}

/// Whether no two of `names` are equal. `str` equality is not `const`, so the
/// bytes are compared by hand; the list checks its own names with this at
/// compile time.
const fn all_distinct(names: &[&str]) -> bool {
    let mut first = 0;
    while first < names.len() {
        let mut second = first + 1;
        while second < names.len() {
            if same_bytes(names[first].as_bytes(), names[second].as_bytes()) {
                return false;
            }
            second += 1;
        }
        first += 1;
    }
    true
}

const fn same_bytes(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Declares the machine's settings: one `field: SettingType` line each.
///
/// Expands, in the module it is invoked in, to the `Settings` storage, its
/// `Default`, a `Holds` impl per setting, `SETTING_COUNT`, `entries`,
/// `read_by_name` and `write_by_name`, and a compile-time check that the
/// names are distinct. An empty list is valid.
macro_rules! settings {
    ($($field:ident: $setting:ty),* $(,)?) => {
        const NAMES: &[&str] = &[$(<$setting as $crate::settings::Setting>::NAME),*];

        /// How many settings the machine has.
        pub const SETTING_COUNT: usize = NAMES.len();

        const _: () = assert!(
            $crate::settings::all_distinct(NAMES),
            "two settings share a name"
        );

        /// Every setting's word. Written by whichever processor serves a
        /// write; see the module's concurrency contract.
        pub struct Settings {
            $($field: $crate::settings::SettingCell<$setting>,)*
        }

        impl Default for Settings {
            fn default() -> Self {
                Self {
                    $($field: $crate::settings::SettingCell::default(),)*
                }
            }
        }

        $(
            impl $crate::settings::Holds<$setting> for Settings {
                fn cell(&self) -> &$crate::settings::SettingCell<$setting> {
                    &self.$field
                }
            }
        )*

        impl Settings {
            /// What `S` is now, as its consumers enforce it.
            pub fn get<S: $crate::settings::Setting>(&self) -> S::Value
            where
                Self: $crate::settings::Holds<S>,
            {
                $crate::settings::Holds::<S>::cell(self).get()
            }

            /// Makes `value` what `S` is from now on and answers what it
            /// was. `proof` is the caller's right to write settings.
            pub fn set<S: $crate::settings::Setting>(
                &self,
                proof: &$crate::WriteSettingsCap,
                value: S::Value,
            ) -> S::Value
            where
                Self: $crate::settings::Holds<S>,
            {
                $crate::settings::Holds::<S>::cell(self).set(proof, value)
            }

            /// Every setting, in declaration order.
            pub fn entries(&self) -> [$crate::settings::SettingEntry; SETTING_COUNT] {
                [$(self.$field.entry()),*]
            }

            /// The named setting, for a guest that names it by text.
            pub fn read_by_name(&self, name: &str) -> Option<$crate::settings::SettingEntry> {
                self.entries().into_iter().find(|entry| entry.name == name)
            }

            /// Writes the named setting if `value` has its unit, and answers
            /// the value it replaced. `proof` is the caller's right to write
            /// settings.
            pub fn write_by_name(
                &self,
                proof: &$crate::WriteSettingsCap,
                name: &str,
                value: $crate::settings::SettingValue,
            ) -> Result<$crate::settings::SettingValue, $crate::settings::SettingWriteError> {
                match (name, proof, value) {
                    $(
                        (<$setting as $crate::settings::Setting>::NAME, proof, value) => {
                            self.$field.write_value(proof, value)
                        }
                    )*
                    _ => Err($crate::settings::SettingWriteError::Unknown),
                }
            }
        }
    };
}

// The machine's settings. A setting is listed here together with the change
// that makes a consumer enforce it, so that none is declared and then ignored.
settings! {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProcessAuthority, SettingsAuthorityRights};

    /// The proof a holder of the right derives from its authority.
    fn proof() -> WriteSettingsCap {
        let mut authority = ProcessAuthority::empty();
        authority.grant_settings_rights(SettingsAuthorityRights::WRITE);
        authority
            .derive_write_settings_cap()
            .expect("an authority granted the right derives its proof")
    }

    struct Slots;

    impl Setting for Slots {
        const NAME: &'static str = "test.slots";
        const DESCRIPTION: &'static str = "Slots a test fixture offers.";
        const UNIT: SettingUnit = SettingUnit::Count;
        type Value = u32;
        const DEFAULT: u32 = 7;

        fn to_raw(value: u32) -> u64 {
            u64::from(value)
        }

        fn from_raw(raw: u64) -> u32 {
            u32::try_from(raw).expect("the fixture is only written values that fit")
        }
    }

    struct Budget;

    impl Setting for Budget {
        const NAME: &'static str = "test.budget";
        const DESCRIPTION: &'static str = "Bytes a test fixture may spend.";
        const UNIT: SettingUnit = SettingUnit::Bytes;
        type Value = u64;
        const DEFAULT: u64 = 4096;

        fn to_raw(value: u64) -> u64 {
            value
        }

        fn from_raw(raw: u64) -> u64 {
            raw
        }
    }

    /// A list with two settings, one per unit, declared with the same macro
    /// as the machine's.
    mod fixture {
        use super::{Budget, Slots};

        settings! {
            slots: Slots,
            budget: Budget,
        }
    }

    #[test]
    fn an_unknown_name_is_refused_and_lists_nothing() {
        let settings = Settings::default();
        let proof = proof();

        assert_eq!(SETTING_COUNT, 0, "the machine declares no setting yet");
        assert_eq!(settings.entries(), []);
        assert_eq!(settings.read_by_name("process.descriptor-limit"), None);
        assert_eq!(
            settings.write_by_name(&proof, "process.descriptor-limit", SettingValue::Count(1)),
            Err(SettingWriteError::Unknown)
        );
    }

    #[test]
    fn entries_follow_declaration_order_and_the_declared_count() {
        let settings = fixture::Settings::default();

        let entries = settings.entries();
        assert_eq!(entries.len(), fixture::SETTING_COUNT);
        assert_eq!(fixture::SETTING_COUNT, 2);
        assert_eq!(
            entries,
            [
                SettingEntry {
                    name: "test.slots",
                    description: "Slots a test fixture offers.",
                    value: SettingValue::Count(7),
                },
                SettingEntry {
                    name: "test.budget",
                    description: "Bytes a test fixture may spend.",
                    value: SettingValue::Bytes(4096),
                },
            ]
        );
    }

    #[test]
    fn a_typed_write_answers_the_value_it_replaced() {
        let settings = fixture::Settings::default();
        let proof = proof();

        assert_eq!(settings.get::<Slots>(), 7);
        assert_eq!(settings.set::<Slots>(&proof, 64), 7);
        assert_eq!(settings.get::<Slots>(), 64);
        assert_eq!(settings.set::<Slots>(&proof, 1), 64);
        assert_eq!(settings.get::<Budget>(), 4096, "a write moves one setting");
    }

    #[test]
    fn a_write_by_name_in_the_settings_unit_lands_and_reads_back() {
        let settings = fixture::Settings::default();
        let proof = proof();

        assert_eq!(
            settings.write_by_name(&proof, "test.budget", SettingValue::Bytes(1 << 20)),
            Ok(SettingValue::Bytes(4096))
        );
        assert_eq!(settings.get::<Budget>(), 1 << 20);
        assert_eq!(
            settings
                .read_by_name("test.budget")
                .map(|entry| entry.value),
            Some(SettingValue::Bytes(1 << 20))
        );
    }

    #[test]
    fn a_write_in_the_other_unit_is_refused_and_changes_nothing() {
        let settings = fixture::Settings::default();
        let proof = proof();

        assert_eq!(
            settings.write_by_name(&proof, "test.slots", SettingValue::Bytes(9)),
            Err(SettingWriteError::UnitMismatch {
                holds: SettingUnit::Count
            })
        );
        assert_eq!(settings.get::<Slots>(), 7);
    }

    #[test]
    fn a_name_matches_whole_or_not_at_all() {
        let settings = fixture::Settings::default();
        let proof = proof();

        for name in ["test", "test.slot", "test.slots ", "TEST.SLOTS", ""] {
            assert_eq!(settings.read_by_name(name), None, "{name:?}");
            assert_eq!(
                settings.write_by_name(&proof, name, SettingValue::Count(1)),
                Err(SettingWriteError::Unknown),
                "{name:?}"
            );
        }
        assert_eq!(settings.get::<Slots>(), 7);
    }

    #[test]
    fn the_compile_time_name_check_sees_a_repeat() {
        assert!(all_distinct(&[]));
        assert!(all_distinct(&["a.b", "a.c", "b.b"]));
        assert!(!all_distinct(&["a.b", "a.c", "a.b"]));
        assert!(all_distinct(&["a.b", "a.bc"]), "a prefix is not a repeat");
    }
}
