//! The checks a backend runs before it hands the kernel a TPM, and the
//! event log they read.
//!
//! The PCR-bank query is replayed from the `swtpm` recording. The
//! owner-authorization query is not in that recording: its response is
//! marshalled here by `tpm2-protocol` from the property values TPM 2.0
//! Part 2 gives a freshly started TPM, and its command is spelled out
//! from Part 3's `TPM2_GetCapability` layout, so the bytes the crate
//! sends are checked against the specification rather than against
//! themselves.

use helios_hal::boot::MeasurementLogFormat;
use helios_hal::measured_boot::{MeasuredBoot, MeasurementRegister};
use helios_tpm::{
    Discovery, EV_IPL, Event, EventLog, EventLogError, InterfaceDescription, InterfaceError,
    KERNEL_IMAGE_REGISTERS, LIMINE_KERNEL_IMAGE, LoaderLog, QualifyError, Unqualified, discover,
    qualify,
};
use tpm2_protocol::TpmWriter;
use tpm2_protocol::basic::TpmUint32;
use tpm2_protocol::constant::TPM_MAX_COMMAND_SIZE;
use tpm2_protocol::data::{
    TpmCap, TpmPt, TpmRc, TpmRcBase, TpmiYesNo, TpmlTaggedTpmProperty, TpmsCapabilityData,
    TpmsTaggedProperty, TpmuCapabilities,
};
use tpm2_protocol::frame::{TpmGetCapabilityResponse, tpm_marshal_response};

use crate::support::crb::CrbModel;
use crate::support::tis::TisModel;
use crate::support::{Exchange, Replay, TestClock, complete, load_recording};

/// `TPM2_GetCapability(TPM_CAP_TPM_PROPERTIES, TPM_PT_PERMANENT, 2)`:
/// tag `TPM_ST_NO_SESSIONS`, 22 bytes, `TPM_CC_GetCapability`, then
/// the capability, the first property and the count (TPM 2.0 Part 3,
/// "TPM2_GetCapability").
const OWNER_QUERY: [u8; 22] = [
    0x80, 0x01, 0x00, 0x00, 0x00, 0x16, 0x00, 0x00, 0x01, 0x7a, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00,
    0x02, 0x00, 0x00, 0x00, 0x00, 0x02,
];

/// `TPMA_PERMANENT` of a TPM whose endorsement seed it generated itself
/// and whose hierarchies carry no authorization value: only
/// `tpmGeneratedEPS` (bit 10).
const PERMANENT_FRESH: u32 = 1 << 10;
/// `TPMA_STARTUP_CLEAR` after `TPM2_Startup(TPM_SU_CLEAR)`: `phEnable`,
/// `shEnable`, `ehEnable` and `phEnableNV` (bits 0 to 3).
const STARTUP_CLEAR_ENABLED: u32 = 0b1111;

fn properties_response(permanent: u32, startup_clear: u32) -> Vec<u8> {
    let mut properties = TpmlTaggedTpmProperty::new();
    for (property, value) in [
        (TpmPt::Permanent, permanent),
        (TpmPt::StartupClear, startup_clear),
    ] {
        properties
            .try_push(TpmsTaggedProperty {
                property,
                value: TpmUint32::new(value),
            })
            .unwrap();
    }
    let response = TpmGetCapabilityResponse {
        handles: [],
        more_data: TpmiYesNo(false),
        capability_data: TpmsCapabilityData {
            capability: TpmCap::TpmProperties,
            data: TpmuCapabilities::TpmProperties(properties),
        },
    };
    let mut buffer = [0; TPM_MAX_COMMAND_SIZE];
    let mut writer = TpmWriter::new(&mut buffer);
    tpm_marshal_response(&response, TpmRc::Fmt0(TpmRcBase::Success), &[], &mut writer).unwrap();
    let len = writer.len();
    buffer[..len].to_vec()
}

/// The exchanges of one qualification: the PCR-bank query swtpm
/// answered when the recording was made, then the owner query.
fn exchanges(permanent: u32, startup_clear: u32) -> Vec<Exchange> {
    let banks = load_recording().phase("seal").remove(0);
    vec![
        banks,
        Exchange {
            command: OWNER_QUERY.to_vec(),
            response: properties_response(permanent, startup_clear),
        },
    ]
}

/// The banks swtpm 0.10.2 keeps, as its recorded `TPM_CAP_PCRS` answer
/// lists them: SHA-1, SHA-256, SHA-384 and SHA-512.
const BANKS: [(u16, u16); 4] = [(0x0004, 20), (0x000b, 32), (0x000c, 48), (0x000d, 64)];

/// `EV_NO_ACTION`, the event type of the crypto-agile log's header.
const EV_NO_ACTION: u32 = 0x0000_0003;
/// `EV_EFI_BOOT_SERVICES_APPLICATION`, how firmware records each boot
/// application it starts in PCR 4.
const EV_EFI_BOOT_SERVICES_APPLICATION: u32 = 0x8000_0003;
/// `EV_SEPARATOR`, which firmware extends into PCRs 0 to 7 before it
/// hands over.
const EV_SEPARATOR: u32 = 0x0000_0004;

/// A crypto-agile log: the Spec ID Event03 header naming `banks`, then
/// one record per event, each carrying a digest for every bank.
fn crypto_agile_log(banks: &[(u16, u16)], events: &[(u32, u32, &[u8])]) -> Vec<u8> {
    let mut spec_id = Vec::new();
    spec_id.extend_from_slice(b"Spec ID Event03\0");
    spec_id.extend_from_slice(&0_u32.to_le_bytes()); // platform class: client
    spec_id.extend_from_slice(&[0, 2, 0, 2]); // version 2.0, errata 0, UINTN of 8 bytes
    spec_id.extend_from_slice(&(banks.len() as u32).to_le_bytes());
    for (algorithm, size) in banks {
        spec_id.extend_from_slice(&algorithm.to_le_bytes());
        spec_id.extend_from_slice(&size.to_le_bytes());
    }
    spec_id.push(0); // no vendor information

    let mut log = Vec::new();
    log.extend_from_slice(&0_u32.to_le_bytes());
    log.extend_from_slice(&EV_NO_ACTION.to_le_bytes());
    log.extend_from_slice(&[0; 20]);
    log.extend_from_slice(&(spec_id.len() as u32).to_le_bytes());
    log.extend_from_slice(&spec_id);

    for (index, (register, event_type, data)) in events.iter().enumerate() {
        log.extend_from_slice(&register.to_le_bytes());
        log.extend_from_slice(&event_type.to_le_bytes());
        log.extend_from_slice(&(banks.len() as u32).to_le_bytes());
        for (algorithm, size) in banks {
            log.extend_from_slice(&algorithm.to_le_bytes());
            log.extend(std::iter::repeat_n(index as u8, usize::from(*size)));
        }
        log.extend_from_slice(&(data.len() as u32).to_le_bytes());
        log.extend_from_slice(data);
    }
    log
}

/// What firmware and Limine 12.9.6 log for this tree's `limine.conf`
/// with `measured_boot: yes`: the firmware's own boot application and
/// separator events, then Limine's in the order `USAGE.md` gives them.
fn measured_boot_events() -> Vec<(u32, u32, &'static [u8])> {
    vec![
        (4, EV_EFI_BOOT_SERVICES_APPLICATION, b"\0\0\0\0\0\0\0\0"),
        (4, EV_SEPARATOR, b"\0\0\0\0"),
        (9, EV_IPL, b"limine_cfg\0"),
        (8, EV_IPL, b"path: boot():/boot/helios\0"),
        (9, EV_IPL, b"path: boot():/boot/helios\0"),
        (8, EV_IPL, b"module_path: boot():/boot/helios-bootfs\0"),
        (9, EV_IPL, b"module_path: boot():/boot/helios-bootfs\0"),
    ]
}

fn log(bytes: &[u8]) -> Option<LoaderLog<'_>> {
    Some(LoaderLog {
        format: MeasurementLogFormat::Tcg2,
        bytes,
    })
}

#[test]
fn a_tpm_the_chain_measured_the_kernel_into_is_handed_over() {
    let replay = Replay::new(exchanges(PERMANENT_FRESH, STARTUP_CLEAR_ENABLED));
    let bytes = crypto_agile_log(&BANKS, &measured_boot_events());
    let tpm = complete(qualify(replay.clone(), log(&bytes))).expect("every check holds");
    assert_eq!(tpm.kernel_image_registers(), KERNEL_IMAGE_REGISTERS);
    replay.finish();
}

#[test]
fn an_owner_authorization_value_refuses_the_tpm() {
    let replay = Replay::new(exchanges(PERMANENT_FRESH | 1, STARTUP_CLEAR_ENABLED));
    let bytes = crypto_agile_log(&BANKS, &measured_boot_events());
    assert_eq!(
        complete(qualify(replay.clone(), log(&bytes))).err(),
        Some(QualifyError::Unqualified(
            Unqualified::OwnerAuthorizationSet
        ))
    );
    replay.finish();
}

#[test]
fn a_disabled_owner_hierarchy_refuses_the_tpm() {
    let replay = Replay::new(exchanges(PERMANENT_FRESH, STARTUP_CLEAR_ENABLED & !0b10));
    let bytes = crypto_agile_log(&BANKS, &measured_boot_events());
    assert_eq!(
        complete(qualify(replay.clone(), log(&bytes))).err(),
        Some(QualifyError::Unqualified(
            Unqualified::OwnerHierarchyDisabled
        ))
    );
    replay.finish();
}

#[test]
fn a_boot_without_an_event_log_refuses_the_tpm() {
    let replay = Replay::new(exchanges(PERMANENT_FRESH, STARTUP_CLEAR_ENABLED));
    assert_eq!(
        complete(qualify(replay.clone(), None)).err(),
        Some(QualifyError::Unqualified(Unqualified::NoEventLog))
    );
    replay.finish();
}

/// Without `measured_boot: yes` Limine still relays the firmware's log,
/// which then holds no event of Limine's own.
#[test]
fn a_loader_that_did_not_measure_the_kernel_refuses_the_tpm() {
    let replay = Replay::new(exchanges(PERMANENT_FRESH, STARTUP_CLEAR_ENABLED));
    let bytes = crypto_agile_log(&BANKS, &measured_boot_events()[..2]);
    assert_eq!(
        complete(qualify(replay.clone(), log(&bytes))).err(),
        Some(QualifyError::Unqualified(
            Unqualified::KernelImageNotMeasured
        ))
    );
    replay.finish();
}

#[test]
fn the_kernel_image_event_is_told_apart_from_its_path_in_pcr_8() {
    let only_path = crypto_agile_log(&BANKS, &[(8, EV_IPL, b"path: boot():/boot/helios\0")]);
    let log = EventLog::parse(MeasurementLogFormat::Tcg2, &only_path).unwrap();
    assert!(!log.records(&LIMINE_KERNEL_IMAGE).unwrap());

    let module_only = crypto_agile_log(
        &BANKS,
        &[(9, EV_IPL, b"module_path: boot():/boot/helios-bootfs\0")],
    );
    let log = EventLog::parse(MeasurementLogFormat::Tcg2, &module_only).unwrap();
    assert!(!log.records(&LIMINE_KERNEL_IMAGE).unwrap());
}

#[test]
fn a_crypto_agile_log_yields_every_record_after_its_header() {
    let bytes = crypto_agile_log(&BANKS, &measured_boot_events());
    let log = EventLog::parse(MeasurementLogFormat::Tcg2, &bytes).unwrap();
    let events: Vec<Event<'_>> = log.events().collect::<Result<_, _>>().unwrap();
    assert_eq!(events.len(), 7);
    assert_eq!(
        events[4],
        Event {
            register: 9,
            event_type: EV_IPL,
            data: b"path: boot():/boot/helios\0",
        }
    );
}

#[test]
fn a_tcg_1_2_log_is_read_as_sha1_records() {
    let mut bytes = Vec::new();
    for (register, event_type, data) in [
        (4_u32, EV_EFI_BOOT_SERVICES_APPLICATION, &b"\0\0\0\0"[..]),
        (9, EV_IPL, b"path: boot():/boot/helios\0"),
    ] {
        bytes.extend_from_slice(&register.to_le_bytes());
        bytes.extend_from_slice(&event_type.to_le_bytes());
        bytes.extend_from_slice(&[0xa5; 20]);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
    }
    let log = EventLog::parse(MeasurementLogFormat::Tcg12, &bytes).unwrap();
    assert!(log.records(&LIMINE_KERNEL_IMAGE).unwrap());
}

#[test]
fn a_damaged_log_is_refused_with_where_it_broke() {
    let bytes = crypto_agile_log(&BANKS, &measured_boot_events());
    let cut = &bytes[..bytes.len() - 3];
    let log = EventLog::parse(MeasurementLogFormat::Tcg2, cut).unwrap();
    // The kernel image's record comes before the damage.
    assert_eq!(log.records(&LIMINE_KERNEL_IMAGE), Ok(true));
    let last: Vec<_> = log.events().collect();
    assert!(matches!(
        last.last(),
        Some(Err(EventLogError::Truncated { len, .. })) if *len == cut.len()
    ));

    // A record naming a bank the header does not.
    let mut foreign = crypto_agile_log(&BANKS[..1], &[(9, EV_IPL, b"path: x\0")]);
    let first_digest = foreign.len() - (4 + 8) - 20 - 2;
    foreign[first_digest..first_digest + 2].copy_from_slice(&0x0012_u16.to_le_bytes());
    let log = EventLog::parse(MeasurementLogFormat::Tcg2, &foreign).unwrap();
    assert!(matches!(
        log.records(&LIMINE_KERNEL_IMAGE),
        Err(EventLogError::UnknownAlgorithm {
            algorithm: 0x0012,
            ..
        })
    ));

    let mut no_header = crypto_agile_log(&BANKS, &[]);
    no_header[32] = b'X';
    assert_eq!(
        EventLog::parse(MeasurementLogFormat::Tcg2, &no_header).err(),
        Some(EventLogError::NoSpecIdHeader)
    );

    assert_eq!(
        EventLog::parse(MeasurementLogFormat::Other(3), &bytes).err(),
        Some(EventLogError::UnknownFormat(3))
    );
}

/// Where QEMU puts locality 0 on `q35` and where the PC Client profile
/// fixes it; any page serves the models.
const LOCALITY_0: u64 = 0xfed4_0000;

/// Each interface firmware can name is opened from the register page,
/// checked over that interface, and named in the boot line.
#[test]
fn discovery_opens_the_described_interface_and_hands_the_tpm_over() {
    let bytes = crypto_agile_log(&BANKS, &measured_boot_events());

    let replay = Replay::new(exchanges(PERMANENT_FRESH, STARTUP_CLEAR_ENABLED));
    let (tpm, discovery) = complete(discover(
        InterfaceDescription::DeviceTreeFifo,
        TisModel::new(replay.clone(), 2, 16),
        LOCALITY_0,
        TestClock::default(),
        log(&bytes),
    ))
    .expect("the FIFO interface answers");
    assert!(tpm.is_some());
    assert_eq!(
        discovery,
        Discovery::HandedOver {
            interface: "FIFO",
            base: LOCALITY_0
        }
    );
    replay.finish();

    let replay = Replay::new(exchanges(PERMANENT_FRESH, STARTUP_CLEAR_ENABLED));
    let (tpm, discovery) = complete(discover(
        InterfaceDescription::AcpiStartMethod(7),
        CrbModel::new(replay.clone(), 2, LOCALITY_0),
        LOCALITY_0,
        TestClock::default(),
        log(&bytes),
    ))
    .expect("the CRB answers");
    assert!(tpm.is_some());
    assert_eq!(
        discovery,
        Discovery::HandedOver {
            interface: "CRB",
            base: LOCALITY_0
        }
    );
    replay.finish();
}

/// A check that does not hold is a boot line, and no device.
#[test]
fn a_tpm_that_fails_a_check_is_refused_with_its_reason() {
    let replay = Replay::new(exchanges(PERMANENT_FRESH | 1, STARTUP_CLEAR_ENABLED));
    let bytes = crypto_agile_log(&BANKS, &measured_boot_events());
    let (tpm, discovery) = complete(discover(
        InterfaceDescription::AcpiStartMethod(6),
        TisModel::new(replay.clone(), 0, 16),
        LOCALITY_0,
        TestClock::default(),
        log(&bytes),
    ))
    .expect("the FIFO interface answers");
    assert!(tpm.is_none());
    assert_eq!(
        discovery,
        Discovery::Refused {
            base: Some(LOCALITY_0),
            reason: Unqualified::OwnerAuthorizationSet
        }
    );
    assert!(discovery.is_refusal());
    replay.finish();

    let (tpm, discovery) = complete(discover(
        InterfaceDescription::AcpiStartMethod(2),
        TisModel::new(Replay::new(Vec::new()), 0, 16),
        LOCALITY_0,
        TestClock::default(),
        log(&bytes),
    ))
    .expect("an undriven start method touches no register");
    assert!(tpm.is_none());
    assert_eq!(
        discovery,
        Discovery::Refused {
            base: Some(LOCALITY_0),
            reason: Unqualified::Interface(InterfaceError::StartMethod(2))
        }
    );
}

/// A register page that answers for another interface than the one the
/// firmware named is a TPM that is there and broken, which stops the
/// boot rather than being quietly done without.
#[test]
fn a_register_page_of_another_interface_stops_the_boot() {
    let result = complete(discover(
        InterfaceDescription::DeviceTreeFifo,
        TisModel::new(Replay::new(Vec::new()), 0, 16).with_interface_id(1),
        LOCALITY_0,
        TestClock::default(),
        None,
    ));
    let Err(error) = result else {
        panic!("a CRB register page was taken for the FIFO interface");
    };
    assert!(
        error.to_string().contains("another interface type"),
        "{error}"
    );
}

#[test]
fn the_kernel_image_is_measured_into_a_register_it_is_sealed_to() {
    assert!(KERNEL_IMAGE_REGISTERS.contains(LIMINE_KERNEL_IMAGE.register));
    assert!(KERNEL_IMAGE_REGISTERS.contains(MeasurementRegister::new(4)));
}
