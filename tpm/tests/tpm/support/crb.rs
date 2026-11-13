//! A register model of the PTP Command Response Buffer at locality 0.
//!
//! The layout is QEMU's `tpm-crb`: the control area at 0x40 and one
//! buffer, shared by command and response, from 0x80 to the end of the
//! 4 KiB page. The model panics on any access the PTP does not allow in
//! the current state: the control area read before `cmdReady`, the
//! buffer touched while the TPM is idle or executing, a start without a
//! ready TPM, a locality released before `goIdle`.

use helios_tpm::RegisterWindow;

use super::Core;

const LOC_STATE: usize = 0x00;
const LOC_CTRL: usize = 0x08;
const INTF_ID: usize = 0x30;
const CTRL_REQ: usize = 0x40;
const CTRL_STS: usize = 0x44;
const CTRL_CANCEL: usize = 0x48;
const CTRL_START: usize = 0x4c;
const CTRL_CMD_SIZE: usize = 0x58;
const CTRL_CMD_LADDR: usize = 0x5c;
const CTRL_CMD_HADDR: usize = 0x60;
const CTRL_RSP_SIZE: usize = 0x64;
const CTRL_RSP_ADDR_LOW: usize = 0x68;
const CTRL_RSP_ADDR_HIGH: usize = 0x6c;
const DATA_BUFFER: usize = 0x80;
const PAGE: usize = 0x1000;

const LOC_STATE_VALID: u32 = 1 << 7;
const LOC_STATE_ASSIGNED: u32 = 1 << 1;
const REQUEST_ACCESS: u32 = 1 << 0;
const RELINQUISH: u32 = 1 << 1;
const CMD_READY: u32 = 1 << 0;
const GO_IDLE: u32 = 1 << 1;
const STS_IDLE: u32 = 1 << 1;
const START: u32 = 1 << 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Readying(u32),
    Ready,
    Executing(u32),
    Done,
    Idling(u32),
}

pub struct CrbModel<K> {
    core: K,
    delay: u32,
    physical_base: u64,
    /// Where the control area says the buffer is, as a physical address.
    buffer_address: u64,
    interface_id: u32,
    grant: Option<u32>,
    assigned: bool,
    state: State,
    cancel: u32,
    buffer: Vec<u8>,
    pub not_ready_reads: usize,
    pub commands: usize,
}

impl<K: Core> CrbModel<K> {
    pub fn new(core: K, delay: u32, physical_base: u64) -> Self {
        Self {
            core,
            delay,
            physical_base,
            buffer_address: physical_base + DATA_BUFFER as u64,
            interface_id: 1,
            grant: None,
            assigned: false,
            state: State::Idle,
            cancel: 0,
            buffer: vec![0; PAGE - DATA_BUFFER],
            not_ready_reads: 0,
            commands: 0,
        }
    }

    pub fn with_interface_id(self, interface_id: u32) -> Self {
        Self {
            interface_id,
            ..self
        }
    }

    /// A control area that names a buffer at `buffer_address`.
    pub fn with_buffer_address(self, buffer_address: u64) -> Self {
        Self {
            buffer_address,
            ..self
        }
    }

    /// A model whose locality is never assigned.
    pub fn never_granting(self) -> Self {
        Self {
            delay: u32::MAX,
            ..self
        }
    }

    pub fn is_idle_and_released(&self) -> bool {
        self.state == State::Idle && !self.assigned && self.grant.is_none()
    }

    fn require_locality(&self, what: &str) {
        assert!(self.assigned, "{what} without locality 0 assigned");
    }

    fn buffer_index(&self, offset: usize) -> usize {
        let start = (self.buffer_address - self.physical_base) as usize;
        assert!(
            offset >= start && offset < start + self.buffer.len(),
            "an access at {offset:#x} outside the data buffer"
        );
        offset - start
    }

    fn control_area(&self, what: &str) {
        self.require_locality(what);
        assert_eq!(
            self.state,
            State::Ready,
            "{what} before the TPM was made ready"
        );
    }
}

impl<K: Core> RegisterWindow for CrbModel<K> {
    fn span(&self) -> usize {
        PAGE
    }

    fn read_u8(&mut self, offset: usize) -> u8 {
        self.require_locality("a buffer read");
        assert_eq!(
            self.state,
            State::Done,
            "a buffer read before the command completed"
        );
        let index = self.buffer_index(offset);
        self.buffer[index]
    }

    fn read_u32(&mut self, offset: usize) -> u32 {
        match offset {
            LOC_STATE => {
                match self.grant {
                    Some(0) => {
                        self.grant = None;
                        self.assigned = true;
                    }
                    Some(left) => {
                        self.grant = Some(left - 1);
                        self.not_ready_reads += 1;
                    }
                    None => {}
                }
                LOC_STATE_VALID | if self.assigned { LOC_STATE_ASSIGNED } else { 0 }
            }
            INTF_ID => self.interface_id,
            CTRL_REQ => {
                self.require_locality("a TPM_CRB_CTRL_REQ read");
                match self.state {
                    State::Readying(0) => {
                        self.state = State::Ready;
                        0
                    }
                    State::Readying(left) => {
                        self.state = State::Readying(left - 1);
                        self.not_ready_reads += 1;
                        CMD_READY
                    }
                    State::Idling(0) => {
                        self.state = State::Idle;
                        0
                    }
                    State::Idling(left) => {
                        self.state = State::Idling(left - 1);
                        self.not_ready_reads += 1;
                        GO_IDLE
                    }
                    _ => 0,
                }
            }
            CTRL_STS => {
                self.require_locality("a TPM_CRB_CTRL_STS read");
                if self.state == State::Idle {
                    STS_IDLE
                } else {
                    0
                }
            }
            CTRL_START => {
                self.require_locality("a TPM_CRB_CTRL_START read");
                match self.state {
                    State::Executing(0) => {
                        let size =
                            u32::from_be_bytes(self.buffer[2..6].try_into().unwrap()) as usize;
                        let response = self.core.execute(&self.buffer[..size]);
                        self.commands += 1;
                        self.buffer[..response.len()].copy_from_slice(&response);
                        self.state = State::Done;
                        0
                    }
                    State::Executing(left) => {
                        self.state = State::Executing(left - 1);
                        self.not_ready_reads += 1;
                        START
                    }
                    _ => 0,
                }
            }
            CTRL_CMD_SIZE | CTRL_RSP_SIZE => {
                self.control_area("a buffer size read");
                self.buffer.len() as u32
            }
            CTRL_CMD_LADDR | CTRL_RSP_ADDR_LOW => {
                self.control_area("a buffer address read");
                self.buffer_address as u32
            }
            CTRL_CMD_HADDR | CTRL_RSP_ADDR_HIGH => {
                self.control_area("a buffer address read");
                (self.buffer_address >> 32) as u32
            }
            other => panic!("a 32-bit read of CRB register {other:#x}"),
        }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.require_locality("a buffer write");
        assert_eq!(
            self.state,
            State::Ready,
            "a buffer write while the TPM is not ready"
        );
        let index = self.buffer_index(offset);
        self.buffer[index] = value;
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        match (offset, value) {
            (LOC_CTRL, REQUEST_ACCESS) => {
                assert!(
                    !self.assigned,
                    "locality 0 requested while already assigned"
                );
                self.grant = Some(self.delay);
            }
            (LOC_CTRL, RELINQUISH) => {
                self.require_locality("a locality release");
                assert_eq!(
                    self.state,
                    State::Idle,
                    "locality released before the TPM went idle"
                );
                self.assigned = false;
            }
            (CTRL_REQ, CMD_READY) => {
                self.require_locality("a cmdReady request");
                assert!(
                    matches!(self.state, State::Idle | State::Ready | State::Done),
                    "cmdReady requested in the {:?} state",
                    self.state
                );
                self.state = State::Readying(self.delay);
            }
            (CTRL_REQ, GO_IDLE) => {
                self.require_locality("a goIdle request");
                assert!(
                    !matches!(self.state, State::Executing(_)),
                    "goIdle requested while a command runs"
                );
                self.state = State::Idling(self.delay);
            }
            (CTRL_CANCEL, 0) => self.require_locality("a cancel clear"),
            (CTRL_START, START) => {
                self.require_locality("a start");
                assert_eq!(
                    self.state,
                    State::Ready,
                    "a start while the TPM is not ready"
                );
                assert_eq!(self.cancel, 0, "a start with cancel set");
                self.state = State::Executing(self.delay);
            }
            (other, value) => panic!("a 32-bit write of {value:#x} to CRB register {other:#x}"),
        }
    }
}
