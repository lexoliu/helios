//! QEMU's own CPU hotplug AML, run through the interpreter against a
//! model of the registers it drives.
//!
//! The tables are QEMU's expected tables for its q35 machine
//! (`tests/data/acpi/x86/q35` at v9.2.0, pinned by
//! `qemu-tables.sha256` and fetched by `just acpi-tables-fetch`).
//! `DSDT.cphp` and `APIC.cphp` are the CPU hotplug configuration of
//! `bios-tables-test` (`-smp 2,cores=3,sockets=2,maxcpus=6`): six
//! possible processors with local APIC ids 0, 1, 2, 4, 5 and 6, so the
//! `_UID` and the APIC id part ways from the fourth on. The register
//! model follows `hw/acpi/cpu.c` (`cpu_hotplug_rd`, `cpu_hotplug_wr`) and
//! the ICH9 GPE0 block's status/enable halves.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use acpi::aml::AmlError;
use acpi::aml::namespace::AmlName;
use acpi::sdt::fadt::Fadt;
use acpi::{Handle, Handler, PciAddress, RawPhysicalMapping};
use helios_acpi::gpe::{Gpe, GpeMethods, GpeRegisters, GpeTrigger};
use helios_acpi::namespace::{interpreter, load_definition_block};
use helios_acpi::processor::{
    InterruptControllerEntry, LocalApicId, ProcessorError, ProcessorObjects, decode_mat,
};
use helios_acpi::{Interpreter, Notification, NotifyRecorder};
use helios_hal::cpu::ProcessorId;
use helios_hal::processor_hotplug::{HotplugAction, HotplugOutcome, HotplugRequest};
use sha2::{Digest, Sha256};

/// The host has no interrupts to mask.
struct NoInterrupts;

impl helios_hal::critical_section::LocalInterruptMask for NoInterrupts {
    fn mask() -> bool {
        false
    }

    unsafe fn restore(_was_enabled: bool) {}
}

helios_hal::critical_section::set_local_interrupt_mask_impl!(NoInterrupts);

/// A pinned QEMU table, verified against `qemu-tables.sha256`.
fn table(path: &str) -> Vec<u8> {
    let manifest = include_str!("qemu-tables.sha256");
    let pinned = manifest
        .lines()
        .find_map(|line| line.split_once("  ").filter(|(_, name)| *name == path))
        .map(|(hash, _)| hash)
        .unwrap_or_else(|| panic!("{path} is not pinned in qemu-tables.sha256"));
    let dir = std::env::var_os("HELIOS_ACPI_TABLES_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/acpi-tables"),
        PathBuf::from,
    );
    let file = dir.join(path);
    let bytes = std::fs::read(&file).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; fetch the pinned tables with `just acpi-tables-fetch`",
            file.display()
        )
    });
    let actual = hex::encode(Sha256::digest(&bytes));
    assert_eq!(
        actual,
        pinned,
        "{} does not match its pinned hash; refetch it with `just acpi-tables-fetch`",
        file.display()
    );
    bytes
}

/// The FADT, copied into a buffer the size of the newest layout so the
/// fields past this revision-3 table's end read as zero and stay unused.
fn fadt() -> Box<Fadt> {
    let bytes = table("x86/q35/FACP");
    let mut buffer = vec![0_u8; size_of::<Fadt>()];
    buffer[..bytes.len()].copy_from_slice(&bytes);
    // SAFETY: `Fadt` is a packed plain-data struct of exactly this size;
    // every bit pattern is a value of it.
    unsafe { Box::new(core::ptr::read_unaligned(buffer.as_ptr().cast::<Fadt>())) }
}

/// The possible set as the x86 backend builds it: the MADT's local APIC
/// entries in table order.
fn possible_apic_ids() -> Vec<u32> {
    madt_structures(&table("x86/q35/APIC.cphp"))
        .into_iter()
        .filter_map(|structure| LocalApicId::decode(structure).map(|id| id.0))
        .collect()
}

/// The interrupt controller structures of an MADT, after its fixed part:
/// the 36-byte header, the local interrupt controller address and the
/// flags (ACPI 6.5 §5.2.12).
fn madt_structures(madt: &[u8]) -> Vec<&[u8]> {
    let mut structures = &madt[44..];
    let mut found = Vec::new();
    while let [_, length, ..] = structures {
        let (structure, rest) = structures.split_at(usize::from(*length));
        found.push(structure);
        structures = rest;
    }
    found
}

fn slot_of(apic: LocalApicId) -> Option<ProcessorId> {
    possible_apic_ids()
        .iter()
        .position(|id| *id == apic.0)
        .map(|index| ProcessorId::new(index as u16))
}

/// QEMU's per-CPU hotplug state (`AcpiCpuStatus`).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct CpuStatus {
    present: bool,
    inserting: bool,
    removing: bool,
    ost_event: u32,
    ost_status: u32,
}

/// The two devices the hotplug AML reaches.
#[derive(Debug)]
struct Machine {
    cpus: Vec<CpuStatus>,
    selector: u32,
    command: u8,
    /// GPE0's status half, then its enable half.
    gpe0: [u8; 16],
}

/// `hw/acpi/cpu.c`'s register block, at the `PRST` region's port.
const CPU_HOTPLUG_BASE: u16 = 0x0cd8;
/// The ICH9 GPE0 block's port, which the FADT names.
const GPE0_BASE: u16 = 0x0620;
/// `ACPI_CPU_HOTPLUG_STATUS`, the GPE0 status bit QEMU's CPU hotplug
/// raises (`include/hw/acpi/acpi_dev_interface.h`): bit 2, served by
/// `_E02`.
const CPU_HOTPLUG_GPE: u8 = 1 << 2;
/// GPE0's first enable byte once every GPE with a method is enabled:
/// `_E01` and `_E02`.
const ENABLED_GPES: u8 = 1 << 1 | CPU_HOTPLUG_GPE;

impl Machine {
    fn booted_with(present: usize) -> Self {
        let mut cpus = vec![CpuStatus::default(); possible_apic_ids().len()];
        for cpu in &mut cpus[..present] {
            cpu.present = true;
        }
        Self {
            cpus,
            selector: 0,
            command: 0,
            gpe0: [0; 16],
        }
    }

    /// `acpi_cpu_plug_cb`: the CPU exists, an insert event is pending
    /// and the GPE is raised.
    fn plug(&mut self, cpu: usize) {
        self.cpus[cpu].present = true;
        self.cpus[cpu].inserting = true;
        self.gpe0[0] |= CPU_HOTPLUG_GPE;
    }

    /// `acpi_cpu_unplug_request_cb`.
    fn request_unplug(&mut self, cpu: usize) {
        self.cpus[cpu].removing = true;
        self.gpe0[0] |= CPU_HOTPLUG_GPE;
    }

    fn read(&self, offset: u16) -> u32 {
        let Some(cpu) = self.cpus.get(self.selector as usize) else {
            return 0;
        };
        match offset {
            4 => {
                u32::from(cpu.present)
                    | u32::from(cpu.inserting) << 1
                    | u32::from(cpu.removing) << 2
            }
            // `CPHP_GET_NEXT_CPU_WITH_EVENT_CMD` answers the selector.
            8 if self.command == 0 => self.selector,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u16, data: u32) {
        if offset != 0 && self.selector as usize >= self.cpus.len() {
            return;
        }
        match offset {
            0 => self.selector = data,
            4 => {
                let cpu = &mut self.cpus[self.selector as usize];
                if data & 2 != 0 {
                    cpu.inserting = false;
                } else if data & 4 != 0 {
                    cpu.removing = false;
                } else if data & 8 != 0 {
                    // Eject. QEMU refuses the first CPU.
                    assert!(cpu.present && self.selector != 0, "ejected {cpu:?}");
                    cpu.present = false;
                }
            }
            5 => {
                self.command = data as u8;
                if self.command == 0 {
                    let count = self.cpus.len() as u32;
                    let start = self.selector;
                    let mut iter = start;
                    loop {
                        let cpu = self.cpus[iter as usize];
                        if cpu.inserting || cpu.removing {
                            self.selector = iter;
                            break;
                        }
                        iter = if iter + 1 < count { iter + 1 } else { 0 };
                        if iter == start {
                            break;
                        }
                    }
                }
            }
            8 => match self.command {
                1 => self.cpus[self.selector as usize].ost_event = data,
                2 => self.cpus[self.selector as usize].ost_status = data,
                _ => {}
            },
            _ => {}
        }
    }

    fn io_read(&self, port: u16, width: u8) -> u32 {
        match port {
            GPE0_BASE..0x0630 if width == 1 => u32::from(self.gpe0[usize::from(port - GPE0_BASE)]),
            CPU_HOTPLUG_BASE..0x0ce4 => self.read(port - CPU_HOTPLUG_BASE),
            _ => panic!("the AML read {width} bytes from port {port:#x}, which no model covers"),
        }
    }

    fn io_write(&mut self, port: u16, width: u8, data: u32) {
        match port {
            // Status bytes are write-one-to-clear, enable bytes plain.
            GPE0_BASE..0x0628 if width == 1 => {
                self.gpe0[usize::from(port - GPE0_BASE)] &= !(data as u8);
            }
            0x0628..0x0630 if width == 1 => self.gpe0[usize::from(port - GPE0_BASE)] = data as u8,
            CPU_HOTPLUG_BASE..0x0ce4 => self.write(port - CPU_HOTPLUG_BASE, data),
            _ => panic!("the AML wrote {width} bytes to port {port:#x}, which no model covers"),
        }
    }
}

#[derive(Clone)]
struct ModelHandler(Arc<Mutex<Machine>>);

impl ModelHandler {
    fn machine(&self) -> std::sync::MutexGuard<'_, Machine> {
        self.0.lock().unwrap()
    }
}

impl Handler for ModelHandler {
    unsafe fn map_physical_region<T>(&self, address: usize, _size: usize) -> RawPhysicalMapping<T> {
        panic!("the hotplug path mapped physical memory at {address:#x}")
    }

    unsafe fn unmap_physical_region<T>(&self, _region: RawPhysicalMapping<T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        panic!("memory read at {address:#x}")
    }

    fn read_u16(&self, address: usize) -> u16 {
        panic!("memory read at {address:#x}")
    }

    fn read_u32(&self, address: usize) -> u32 {
        panic!("memory read at {address:#x}")
    }

    fn read_u64(&self, address: usize) -> u64 {
        panic!("memory read at {address:#x}")
    }

    fn write_u8(&self, address: usize, _value: u8) {
        panic!("memory write at {address:#x}")
    }

    fn write_u16(&self, address: usize, _value: u16) {
        panic!("memory write at {address:#x}")
    }

    fn write_u32(&self, address: usize, _value: u32) {
        panic!("memory write at {address:#x}")
    }

    fn write_u64(&self, address: usize, _value: u64) {
        panic!("memory write at {address:#x}")
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        self.machine().io_read(port, 1) as u8
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        self.machine().io_read(port, 2) as u16
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        self.machine().io_read(port, 4)
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        self.machine().io_write(port, 1, u32::from(value))
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        self.machine().io_write(port, 2, u32::from(value))
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        self.machine().io_write(port, 4, value)
    }

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8 {
        panic!("PCI read {address:?}+{offset:#x}")
    }

    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16 {
        panic!("PCI read {address:?}+{offset:#x}")
    }

    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32 {
        panic!("PCI read {address:?}+{offset:#x}")
    }

    fn write_pci_u8(&self, address: PciAddress, offset: u16, _value: u8) {
        panic!("PCI write {address:?}+{offset:#x}")
    }

    fn write_pci_u16(&self, address: PciAddress, offset: u16, _value: u16) {
        panic!("PCI write {address:?}+{offset:#x}")
    }

    fn write_pci_u32(&self, address: PciAddress, offset: u16, _value: u32) {
        panic!("PCI write {address:?}+{offset:#x}")
    }

    fn nanos_since_boot(&self) -> u64 {
        0
    }

    fn stall(&self, _microseconds: u64) {}

    fn sleep(&self, _milliseconds: u64) {}

    fn create_mutex(&self) -> Handle {
        Handle(0)
    }

    fn acquire(&self, _mutex: Handle, _timeout: u16) -> Result<(), AmlError> {
        Ok(())
    }

    fn release(&self, _mutex: Handle) {}
}

/// QEMU's q35 namespace over the model, with its GPE blocks and methods.
struct Q35 {
    model: ModelHandler,
    recorder: NotifyRecorder<ModelHandler>,
    interpreter: Interpreter<NotifyRecorder<ModelHandler>>,
    gpes: GpeRegisters<ModelHandler>,
    methods: GpeMethods,
}

impl Q35 {
    fn booted_with(present: usize) -> Self {
        let model = ModelHandler(Arc::new(Mutex::new(Machine::booted_with(present))));
        let recorder = NotifyRecorder::new(model.clone());
        let fadt = fadt();
        let dsdt = table("x86/q35/DSDT.cphp");
        // The DSDT header's revision byte fixes the integer width.
        let interpreter = interpreter(&fadt, dsdt[8], recorder.clone()).unwrap();
        load_definition_block(&interpreter, &dsdt, 0).unwrap();
        let gpes = GpeRegisters::from_fadt(&fadt, model.clone()).unwrap();
        gpes.disable_all();
        let methods = GpeMethods::discover(&interpreter, &gpes).unwrap();
        methods.enable_all(&gpes);
        Self {
            model,
            recorder,
            interpreter,
            gpes,
            methods,
        }
    }

    /// The SCI handler's latch and the task's dispatch, as the backend
    /// runs them, returning the notifications the methods raised.
    fn serve_sci(&self) -> Vec<Notification> {
        assert!(self.gpes.latch(), "the SCI fired with no GPE pending");
        let latched = self.gpes.take_latched();
        for gpe in latched {
            self.methods
                .run(gpe, &self.interpreter, &self.gpes)
                .unwrap();
        }
        self.recorder.take()
    }

    fn cpu(&self, index: usize) -> CpuStatus {
        self.model.machine().cpus[index]
    }
}

fn name(path: &str) -> AmlName {
    AmlName::from_str(path).unwrap()
}

#[test]
fn the_q35_gpe0_block_is_served_by_one_edge_method_for_cpu_hotplug() {
    let q35 = Q35::booted_with(2);
    // 16 bytes: eight status, eight enable, 64 GPEs.
    assert_eq!(q35.gpes.gpes().count(), 64);
    let methods: Vec<_> = q35
        .methods
        .methods()
        .iter()
        .map(|method| (method.gpe, method.trigger, method.path.to_string()))
        .collect();
    // q35 declares an empty `_E01` beside CPU hotplug's `_E02`.
    assert_eq!(
        methods,
        [
            (Gpe(1), GpeTrigger::Edge, "\\_GPE._E01".to_owned()),
            (Gpe(2), GpeTrigger::Edge, "\\_GPE._E02".to_owned())
        ]
    );
    // Only the GPEs with a method are enabled, and every status is clear.
    assert_eq!(q35.model.machine().gpe0, {
        let mut expected = [0; 16];
        expected[8] = ENABLED_GPES;
        expected
    });
}

/// The fourth possible processor has `_UID` 3 and local APIC id 4: the
/// request names the slot the APIC id holds, which only `_MAT` says.
#[test]
fn a_hot_added_cpu_raises_one_insert_for_the_slot_its_mat_names() {
    let q35 = Q35::booted_with(2);
    q35.model.machine().plug(3);

    let notifications = q35.serve_sci();
    assert_eq!(
        notifications,
        [Notification {
            object: name("\\_SB.CPUS.C003"),
            value: 1
        }]
    );
    let machine = q35.model.machine();
    assert!(!machine.cpus[3].inserting, "CSCN acknowledges the insert");
    assert_eq!(machine.gpe0[0], 0, "the edge's status is cleared");
    assert_eq!(machine.gpe0[8], ENABLED_GPES, "and the GPE re-enabled");
    drop(machine);

    let mut processors = ProcessorObjects::<LocalApicId>::default();
    let requests = processors
        .requests(&q35.interpreter, notifications, slot_of)
        .unwrap();
    let insert = HotplugRequest {
        processor: ProcessorId::new(3),
        action: HotplugAction::Insert,
    };
    assert_eq!(requests, [insert]);

    processors
        .report(&q35.interpreter, insert, HotplugOutcome::Completed)
        .unwrap();
    let cpu = q35.cpu(3);
    assert_eq!((cpu.ost_event, cpu.ost_status), (1, 0));
}

#[test]
fn an_unplug_request_ejects_through_ej0_and_reports_through_ost() {
    let q35 = Q35::booted_with(5);
    q35.model.machine().request_unplug(4);

    let mut processors = ProcessorObjects::<LocalApicId>::default();
    let requests = processors
        .requests(&q35.interpreter, q35.serve_sci(), slot_of)
        .unwrap();
    let eject = HotplugRequest {
        processor: ProcessorId::new(4),
        action: HotplugAction::Eject,
    };
    assert_eq!(requests, [eject]);
    assert!(!q35.cpu(4).removing, "CSCN acknowledges the request");
    assert!(q35.cpu(4).present, "nothing is released before _EJ0");

    processors
        .eject(&q35.interpreter, ProcessorId::new(4))
        .unwrap();
    assert!(!q35.cpu(4).present, "_EJ0 releases the CPU");
    processors
        .report(&q35.interpreter, eject, HotplugOutcome::Completed)
        .unwrap();
    let cpu = q35.cpu(4);
    assert_eq!((cpu.ost_event, cpu.ost_status), (3, 0));
}

#[test]
fn a_refused_eject_reports_the_processor_busy_and_keeps_it() {
    let q35 = Q35::booted_with(2);
    q35.model.machine().request_unplug(1);
    let mut processors = ProcessorObjects::<LocalApicId>::default();
    let requests = processors
        .requests(&q35.interpreter, q35.serve_sci(), slot_of)
        .unwrap();
    processors
        .report(&q35.interpreter, requests[0], HotplugOutcome::Busy)
        .unwrap();
    let cpu = q35.cpu(1);
    // Device busy (ACPI 6.5 Table 6.14).
    assert_eq!(
        (cpu.ost_event, cpu.ost_status, cpu.present),
        (3, 0x82, true)
    );
}

/// QEMU's boot processor object has no `_EJ0`: the request is answered
/// "not supported" through `_OST` and never reaches the kernel.
#[test]
fn an_eject_of_a_processor_without_ej0_is_refused_in_firmware() {
    let q35 = Q35::booted_with(2);
    q35.model.machine().request_unplug(0);
    let mut processors = ProcessorObjects::<LocalApicId>::default();
    let requests = processors
        .requests(&q35.interpreter, q35.serve_sci(), slot_of)
        .unwrap();
    assert!(requests.is_empty());
    let cpu = q35.cpu(0);
    assert_eq!((cpu.ost_event, cpu.ost_status), (3, 0x80));
}

/// A processor whose `_MAT` identity is in no slot of the possible set
/// fails the request with what was checked, instead of picking a slot.
#[test]
fn a_notified_processor_outside_the_possible_set_names_its_identity() {
    let q35 = Q35::booted_with(2);
    q35.model.machine().plug(5);
    let mut processors = ProcessorObjects::<LocalApicId>::default();
    let error = processors
        .requests(&q35.interpreter, q35.serve_sci(), |_| None)
        .unwrap_err();
    assert!(
        matches!(&error, ProcessorError::NoSlot { path, entry }
            if path.to_string() == "\\_SB_.CPUS.C005" && entry == "LocalApicId(6)"),
        "{error}"
    );
}

/// Two events in one pass are both answered, in the order CSCN raised
/// them: removals while it scans, insertions after.
#[test]
fn one_sci_carries_an_unplug_and_a_plug_in_scan_order() {
    let q35 = Q35::booted_with(3);
    q35.model.machine().plug(3);
    q35.model.machine().request_unplug(2);
    let notifications = q35.serve_sci();
    assert_eq!(
        notifications,
        [
            Notification {
                object: name("\\_SB.CPUS.C002"),
                value: 3
            },
            Notification {
                object: name("\\_SB.CPUS.C003"),
                value: 1
            },
        ]
    );
}

#[test]
fn a_mat_names_the_local_apic_or_x2apic_it_carries() {
    // C001's `_MAT` in QEMU's DSDT: Processor Local APIC, UID 1, APIC 1,
    // enabled.
    assert_eq!(
        decode_mat::<LocalApicId>(&[0x00, 0x08, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00]),
        Some(LocalApicId(1))
    );
    // QEMU lists a processor beyond APIC id 254 as a Processor Local
    // x2APIC structure; the first one of the 288-processor machine.
    let madt = table("x86/q35/APIC.xapic");
    let x2apic = madt_structures(&madt)
        .into_iter()
        .find(|structure| structure[0] == 9)
        .expect("the 288-processor MADT carries x2APIC structures");
    assert_eq!(decode_mat::<LocalApicId>(x2apic), Some(LocalApicId(0xff)));
}

#[test]
fn a_mat_of_only_non_processor_structures_names_no_processor() {
    let madt = table("x86/q35/APIC.cphp");
    // Past the six local APIC structures: the I/O APIC, the interrupt
    // source overrides and the local APIC NMI.
    let tail = &madt[44 + 6 * 8..];
    assert_eq!(tail[0], 1, "the I/O APIC structure follows the processors");
    assert_eq!(decode_mat::<LocalApicId>(tail), None);
}
