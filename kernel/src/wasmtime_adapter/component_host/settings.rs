//! What a caller may do through `helios:system/settings`, decided once for
//! both worlds.
//!
//! Both worlds link the interface, and every program is linked against it
//! (linkage is cached per binary while authority belongs to the instance),
//! so the refusal happens at the call. Each host function reaches the
//! kernel through one function here, which reads the caller's
//! [`crate::ProcessAuthority`] and answers in kernel types.
//!
//! Two kinds of call:
//! - `write` changes the machine and is refused with
//!   [`WriteRefusal::Denied`], before the name is looked up, unless the
//!   caller holds [`WriteSettingsCap`].
//! - `list` and `read` are answered for every caller. A setting is machine
//!   policy rather than any instance's data, and a program can already
//!   learn a limit from its effect.
//!
//! Concurrency: everything here runs on the task that owns the store and
//! reads the store's own authority, which no other processor changes. The
//! settings themselves are single atomic words; see [`crate::Settings`].
//!
//! The listing and a single row cross the boundary as the views below. Their
//! text is the kernel's own `&'static str`, lowered by reference, so a call
//! allocates nothing per row on the kernel heap, and the one list `list`
//! hands back is bounded by the kernel's own list, never by the guest. The
//! views name the interface's records by their component model shape, as
//! `filters` does, so they are written once for both worlds.

use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use wasmtime::component::{ComponentType, Lift, Lower};

use crate::{
    ComponentHostNetwork, SETTING_COUNT, SettingEntry, SettingUnit, SettingValue,
    SettingWriteError, WriteSettingsCap,
};

use super::StoreData;

/// Why `write` changed nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WriteRefusal {
    /// The caller does not hold the right to write settings.
    Denied,
    /// The caller may write, and the kernel refused this write.
    Setting(SettingWriteError),
}

/// The proof that the caller may write settings.
fn write_settings<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
) -> Result<WriteSettingsCap, WriteRefusal>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    data.process_authority()
        .derive_write_settings_cap()
        .map_err(|_| WriteRefusal::Denied)
}

/// `settings.list`.
pub(super) fn list<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
) -> [SettingEntry; SETTING_COUNT]
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    data.runtime_state.settings().entries()
}

/// `settings.read`, after the name is decoded.
pub(super) fn read<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    name: &str,
) -> Option<SettingEntry>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    data.runtime_state.settings().read_by_name(name)
}

/// `settings.write`, after the name is decoded.
pub(super) fn write<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
    name: &str,
    value: SettingValue,
) -> Result<SettingValue, WriteRefusal>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let proof = write_settings(data)?;
    data.runtime_state
        .settings()
        .write_by_name(&proof, name, value)
        .map_err(WriteRefusal::Setting)
}

/// `helios:system/settings.unit`.
///
/// `Copy` because the derived `Lower` reads the discriminant through a
/// reference.
#[derive(Clone, Copy, ComponentType, Lower)]
#[component(enum)]
#[repr(u8)]
pub(super) enum UnitView {
    #[component(name = "count")]
    Count,
    #[component(name = "bytes")]
    Bytes,
}

/// `helios:system/settings.value`.
#[derive(ComponentType, Lift, Lower)]
#[component(variant)]
pub(super) enum ValueView {
    #[component(name = "count")]
    Count(u64),
    #[component(name = "bytes")]
    Bytes(u64),
}

/// `helios:system/settings.setting`.
#[derive(ComponentType, Lower)]
#[component(record)]
pub(super) struct SettingView {
    name: &'static str,
    description: &'static str,
    value: ValueView,
}

/// `helios:system/settings.write-error`.
#[derive(ComponentType, Lower)]
#[component(variant)]
pub(super) enum WriteErrorView {
    #[component(name = "denied")]
    Denied,
    #[component(name = "unknown")]
    Unknown,
    #[component(name = "unit-mismatch")]
    UnitMismatch(UnitView),
}

impl From<SettingUnit> for UnitView {
    fn from(unit: SettingUnit) -> Self {
        match unit {
            SettingUnit::Count => Self::Count,
            SettingUnit::Bytes => Self::Bytes,
        }
    }
}

impl From<SettingValue> for ValueView {
    fn from(value: SettingValue) -> Self {
        match value {
            SettingValue::Count(amount) => Self::Count(amount),
            SettingValue::Bytes(amount) => Self::Bytes(amount),
        }
    }
}

impl From<ValueView> for SettingValue {
    fn from(value: ValueView) -> Self {
        match value {
            ValueView::Count(amount) => Self::Count(amount),
            ValueView::Bytes(amount) => Self::Bytes(amount),
        }
    }
}

impl From<SettingEntry> for SettingView {
    fn from(entry: SettingEntry) -> Self {
        Self {
            name: entry.name,
            description: entry.description,
            value: entry.value.into(),
        }
    }
}

impl From<WriteRefusal> for WriteErrorView {
    fn from(refusal: WriteRefusal) -> Self {
        match refusal {
            WriteRefusal::Denied => Self::Denied,
            WriteRefusal::Setting(SettingWriteError::Unknown) => Self::Unknown,
            WriteRefusal::Setting(SettingWriteError::UnitMismatch { holds }) => {
                Self::UnitMismatch(holds.into())
            }
        }
    }
}

/// The listing as the one list the call hands back.
pub(super) fn listing<CpuImpl, Net, HostFs>(
    data: &StoreData<CpuImpl, Net, HostFs>,
) -> Vec<SettingView>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    list(data).into_iter().map(SettingView::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasmtime_adapter::component_host::service::test_store::{
        TestStoreData, store_data_authorized,
    };
    use crate::{
        DescriptorLimit, DescriptorLimitSetting, ProcessAuthority, Setting, SettingsAuthorityRights,
    };

    /// A store whose instance holds `rights`.
    fn store(rights: SettingsAuthorityRights) -> TestStoreData {
        let mut authority = ProcessAuthority::empty();
        if !rights.is_empty() {
            authority.grant_settings_rights(rights);
        }
        store_data_authorized(authority, |_| {})
    }

    fn ordinary() -> TestStoreData {
        store(SettingsAuthorityRights::empty())
    }

    fn holder() -> TestStoreData {
        store(SettingsAuthorityRights::WRITE)
    }

    #[test]
    fn a_write_without_the_right_is_refused_before_the_name_is_looked_up() {
        let program = ordinary();

        // No setting has this name, so were the authority check not first
        // the answer would be `Setting(Unknown)`.
        assert_eq!(
            write(&program, "no.such-setting", SettingValue::Count(1)),
            Err(WriteRefusal::Denied)
        );
        assert_eq!(
            write(&program, "no.such-setting", SettingValue::Bytes(1)),
            Err(WriteRefusal::Denied)
        );

        // A holder is let through to the lookup, which is what
        // distinguishes the two answers.
        assert_eq!(
            write(&holder(), "no.such-setting", SettingValue::Count(1)),
            Err(WriteRefusal::Setting(SettingWriteError::Unknown))
        );
    }

    #[test]
    fn a_holder_writes_and_is_told_the_value_it_replaced() {
        let program = holder();
        let state = program.runtime_state.settings();

        assert_eq!(
            write(
                &program,
                "process.descriptor-limit",
                SettingValue::Count(64)
            ),
            Ok(SettingValue::Count(1024))
        );
        assert_eq!(
            state.get::<DescriptorLimitSetting>(),
            DescriptorLimit::new(64),
            "the write reached the value new processes read"
        );
        assert_eq!(
            read(&program, "process.descriptor-limit").map(|entry| entry.value),
            Some(SettingValue::Count(64))
        );
        assert_eq!(
            write(&program, "process.descriptor-limit", SettingValue::Count(8)),
            Ok(SettingValue::Count(64)),
            "each write answers the value it displaced"
        );
    }

    #[test]
    fn a_write_of_another_unit_is_refused_and_changes_nothing() {
        let program = holder();

        assert_eq!(
            write(
                &program,
                "process.descriptor-limit",
                SettingValue::Bytes(64)
            ),
            Err(WriteRefusal::Setting(SettingWriteError::UnitMismatch {
                holds: SettingUnit::Count
            }))
        );
        assert_eq!(
            program
                .runtime_state
                .settings()
                .get::<DescriptorLimitSetting>(),
            DescriptorLimit::DEFAULT
        );
    }

    #[test]
    fn list_shows_the_descriptor_limit_with_its_unit_and_value() {
        let program = ordinary();

        assert_eq!(
            list(&program),
            [SettingEntry {
                name: "process.descriptor-limit",
                description: DescriptorLimitSetting::DESCRIPTION,
                value: SettingValue::Count(1024),
            }]
        );
        let views = listing(&program);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "process.descriptor-limit");
        assert_eq!(views[0].description, DescriptorLimitSetting::DESCRIPTION);
        assert!(matches!(views[0].value, ValueView::Count(1024)));
    }

    #[test]
    fn a_read_needs_no_right() {
        let program = ordinary();
        let state = program.runtime_state.settings();

        // The listing is the kernel's whole list, and each row reads back
        // by its name, for a caller that holds no right at all.
        assert_eq!(list(&program), state.entries());
        for entry in list(&program) {
            assert_eq!(read(&program, entry.name), Some(entry));
        }
        assert_eq!(listing(&program).len(), SETTING_COUNT);
        assert_eq!(read(&program, "no.such-setting"), None);
    }

    #[test]
    fn a_refusal_is_rendered_with_the_unit_the_setting_holds() {
        assert!(matches!(
            WriteErrorView::from(WriteRefusal::Denied),
            WriteErrorView::Denied
        ));
        assert!(matches!(
            WriteErrorView::from(WriteRefusal::Setting(SettingWriteError::Unknown)),
            WriteErrorView::Unknown
        ));
        assert!(matches!(
            WriteErrorView::from(WriteRefusal::Setting(SettingWriteError::UnitMismatch {
                holds: SettingUnit::Bytes
            })),
            WriteErrorView::UnitMismatch(UnitView::Bytes)
        ));
    }

    #[test]
    fn a_value_keeps_its_unit_and_amount_across_the_boundary() {
        for value in [
            SettingValue::Count(0),
            SettingValue::Count(u64::MAX),
            SettingValue::Bytes(4096),
        ] {
            assert_eq!(SettingValue::from(ValueView::from(value)), value);
        }
    }
}
