//! The two interfaces against register models that enforce their state
//! machines.

use helios_tpm::{Crb, Interface, InterfaceError, Tis, Transport, TransportError};

use crate::support::crb::CrbModel;
use crate::support::tis::TisModel;
use crate::support::{Loopback, TestClock, complete, run};

/// Where QEMU maps the CRB on `q35` (`include/hw/acpi/tpm.h`,
/// `TPM_CRB_ADDR_BASE`).
const CRB_BASE: u64 = 0xfed4_0000;

/// A command frame as the crate's marshaller writes one: TPM2_GetRandom
/// for 32 bytes (TPM 2.0 Part 3: tag, size, command code, bytesRequested).
fn get_random_frame() -> Vec<u8> {
    use tpm2_protocol::TpmWriter;
    use tpm2_protocol::basic::TpmUint16;
    use tpm2_protocol::data::TpmSt;
    use tpm2_protocol::frame::{TpmGetRandomCommand, tpm_marshal_command};

    let mut buffer = [0; 64];
    let mut writer = TpmWriter::new(&mut buffer);
    tpm_marshal_command(
        &TpmGetRandomCommand {
            handles: [],
            bytes_requested: TpmUint16::new(32),
        },
        TpmSt::NoSessions,
        &[],
        &mut writer,
    )
    .unwrap();
    writer.as_bytes().to_vec()
}

/// A command long enough to take several bursts of the model's size.
fn long_frame() -> Vec<u8> {
    let mut frame = get_random_frame();
    frame.resize(70, 0);
    let len = frame.len() as u32;
    frame[2..6].copy_from_slice(&len.to_be_bytes());
    frame
}

#[test]
fn tis_carries_a_command_and_its_response_in_bursts() {
    let command = long_frame();
    let mut tis = Tis::open(TisModel::new(Loopback, 3, 8), TestClock::default()).unwrap();
    let mut response = [0; 128];

    let (len, suspensions) = run(tis.transact(&command, &mut response));
    assert_eq!(&response[..len.unwrap()], command.as_slice());
    assert!(suspensions > 0, "the exchange never yielded");
}

#[test]
fn tis_yields_once_per_not_ready_read_and_returns_to_idle() {
    let command = get_random_frame();
    let mut model = TisModel::new(Loopback, 4, 4);
    let mut response = [0; 64];
    let suspensions = {
        let mut tis = Tis::open(&mut model, TestClock::default()).unwrap();
        let (len, suspensions) = run(tis.transact(&command, &mut response));
        assert_eq!(len.unwrap(), command.len());
        suspensions
    };
    assert_eq!(model.commands, 1);
    assert_eq!(suspensions, model.not_ready_reads);
    assert!(model.is_idle_and_released());
}

#[test]
fn tis_runs_one_exchange_after_another() {
    let command = get_random_frame();
    let mut tis = Tis::open(TisModel::new(Loopback, 1, 5), TestClock::default()).unwrap();
    let mut response = [0; 64];
    for _ in 0..3 {
        let len = complete(tis.transact(&command, &mut response)).unwrap();
        assert_eq!(&response[..len], command.as_slice());
    }
}

#[test]
fn tis_refuses_a_response_larger_than_the_buffer() {
    let command = long_frame();
    let mut tis = Tis::open(TisModel::new(Loopback, 0, 16), TestClock::default()).unwrap();
    let mut response = [0; 32];
    assert_eq!(
        complete(tis.transact(&command, &mut response)),
        Err(TransportError::ResponseSize {
            len: command.len(),
            capacity: 32
        })
    );
}

#[test]
fn tis_times_out_naming_the_locality_it_waited_for() {
    let mut tis = Tis::open(
        TisModel::new(Loopback, 0, 8).never_granting(),
        TestClock::default(),
    )
    .unwrap();
    let mut response = [0; 64];
    assert_eq!(
        complete(tis.transact(&get_random_frame(), &mut response)),
        Err(TransportError::Timeout {
            state: "TPM_ACCESS.activeLocality",
            waited_ms: 750
        })
    );
}

#[test]
fn tis_refuses_a_crb_register_page() {
    assert_eq!(
        Tis::open(
            TisModel::new(Loopback, 0, 8).with_interface_id(1),
            TestClock::default()
        )
        .err(),
        Some(TransportError::WrongInterface {
            expected: "the FIFO interface",
            found: 1
        })
    );
}

#[test]
fn crb_carries_a_command_and_its_response() {
    let command = long_frame();
    let mut model = CrbModel::new(Loopback, 3, CRB_BASE);
    let mut response = [0; 128];
    let suspensions = {
        let mut crb = complete(Crb::open(&mut model, CRB_BASE, TestClock::default())).unwrap();
        let (len, suspensions) = run(crb.transact(&command, &mut response));
        assert_eq!(&response[..len.unwrap()], command.as_slice());
        suspensions
    };
    assert_eq!(model.commands, 1);
    assert!(model.is_idle_and_released());
    assert!(suspensions > 0);
}

#[test]
fn crb_yields_once_per_not_ready_read() {
    let command = get_random_frame();
    let mut model = CrbModel::new(Loopback, 5, CRB_BASE);
    let mut response = [0; 64];
    let suspensions = {
        let (crb, opening) = run(Crb::open(&mut model, CRB_BASE, TestClock::default()));
        let mut crb = crb.unwrap();
        let (len, exchanging) = run(crb.transact(&command, &mut response));
        assert_eq!(len.unwrap(), command.len());
        opening + exchanging
    };
    assert_eq!(suspensions, model.not_ready_reads);
}

#[test]
fn crb_refuses_a_buffer_outside_its_window() {
    let model = CrbModel::new(Loopback, 0, CRB_BASE).with_buffer_address(CRB_BASE + 0x2000);
    assert_eq!(
        complete(Crb::open(model, CRB_BASE, TestClock::default())).err(),
        Some(TransportError::BufferOutsideWindow {
            buffer: "command",
            address: CRB_BASE + 0x2000,
            size: 0xf80
        })
    );
}

#[test]
fn crb_refuses_a_command_larger_than_its_buffer() {
    let mut crb = complete(Crb::open(
        CrbModel::new(Loopback, 0, CRB_BASE),
        CRB_BASE,
        TestClock::default(),
    ))
    .unwrap();
    let command = vec![0; 0xf81];
    let mut response = [0; 64];
    assert_eq!(
        complete(crb.transact(&command, &mut response)),
        Err(TransportError::CommandTooLarge {
            len: 0xf81,
            capacity: 0xf80
        })
    );
}

#[test]
fn crb_times_out_naming_the_locality_it_waited_for() {
    assert_eq!(
        complete(Crb::open(
            CrbModel::new(Loopback, 0, CRB_BASE).never_granting(),
            CRB_BASE,
            TestClock::default()
        ))
        .err(),
        Some(TransportError::Timeout {
            state: "TPM_LOC_STATE.locAssigned",
            waited_ms: 200
        })
    );
}

#[test]
fn crb_refuses_a_fifo_register_page() {
    assert_eq!(
        complete(Crb::open(
            CrbModel::new(Loopback, 0, CRB_BASE).with_interface_id(0),
            CRB_BASE,
            TestClock::default()
        ))
        .err(),
        Some(TransportError::WrongInterface {
            expected: "the Command Response Buffer",
            found: 0
        })
    );
}

#[test]
fn acpi_start_method_seven_selects_the_crb() {
    let interface = complete(Interface::from_acpi_start_method(
        7,
        CrbModel::new(Loopback, 0, CRB_BASE),
        CRB_BASE,
        TestClock::default(),
    ));
    assert!(matches!(interface, Ok(Interface::Crb(_))));
}

#[test]
fn acpi_start_method_six_selects_the_fifo_interface() {
    let mut interface = complete(Interface::from_acpi_start_method(
        6,
        TisModel::new(Loopback, 0, 8),
        CRB_BASE,
        TestClock::default(),
    ))
    .unwrap();
    assert!(matches!(interface, Interface::Tis(_)));
    let command = get_random_frame();
    let mut response = [0; 64];
    assert_eq!(
        complete(interface.transact(&command, &mut response)),
        Ok(command.len())
    );
}

#[test]
fn an_unknown_start_method_is_named() {
    let interface = complete(Interface::from_acpi_start_method(
        2,
        TisModel::new(Loopback, 0, 8),
        CRB_BASE,
        TestClock::default(),
    ));
    assert_eq!(interface.err(), Some(InterfaceError::StartMethod(2)));
}

#[test]
fn the_device_tree_compatible_selects_the_fifo_interface() {
    let interface = Interface::<TisModel<Loopback>, TestClock>::from_device_tree(
        ["tcg,tpm-tis-mmio"].into_iter(),
        TisModel::new(Loopback, 0, 8),
        TestClock::default(),
    );
    assert!(matches!(interface, Ok(Interface::Tis(_))));

    let other = Interface::<TisModel<Loopback>, TestClock>::from_device_tree(
        ["tcg,tpm_tis-spi"].into_iter(),
        TisModel::new(Loopback, 0, 8),
        TestClock::default(),
    );
    assert_eq!(other.err(), Some(InterfaceError::Compatible));
}
