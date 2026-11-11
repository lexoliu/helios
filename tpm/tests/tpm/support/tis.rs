//! A register model of the PTP FIFO interface at locality 0.
//!
//! It follows the interface's state machine (Idle, Ready, Reception,
//! Execution, Completion) and panics on any access the PTP does not
//! allow in the current state, so a driver that reaches the FIFO out of
//! order fails the test at the access that broke the protocol. Every
//! transition that a real TPM takes time over is delayed by a number of
//! status reads, so the driver's polling is exercised.

use helios_tpm::RegisterWindow;

use super::Core;

const ACCESS: usize = 0x00;
const STS: usize = 0x18;
const DATA_FIFO: usize = 0x24;
const INTERFACE_ID: usize = 0x30;

const ACCESS_VALID: u8 = 1 << 7;
const ACCESS_ACTIVE_LOCALITY: u8 = 1 << 5;
const ACCESS_REQUEST_USE: u8 = 1 << 1;
const STS_VALID: u32 = 1 << 7;
const STS_COMMAND_READY: u32 = 1 << 6;
const STS_GO: u8 = 1 << 5;
const STS_DATA_AVAIL: u32 = 1 << 4;
const STS_EXPECT: u32 = 1 << 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// `commandReady` was written; the TPM is getting ready.
    Readying(u32),
    Ready,
    Reception,
    Executing(u32),
    Completion,
}

pub struct TisModel<K> {
    core: K,
    /// How many reads every slow transition takes.
    delay: u32,
    /// How many bytes one burst allows.
    burst: usize,
    interface_id: u32,
    grant: Option<u32>,
    active: bool,
    state: State,
    /// Bytes the driver may move before it must read the status again.
    burst_left: usize,
    command: Vec<u8>,
    response: Vec<u8>,
    read_at: usize,
    /// Reads that found the device not ready.
    pub not_ready_reads: usize,
    /// Commands carried to the core.
    pub commands: usize,
}

impl<K: Core> TisModel<K> {
    pub fn new(core: K, delay: u32, burst: usize) -> Self {
        Self {
            core,
            delay,
            burst,
            interface_id: 0,
            grant: None,
            active: false,
            state: State::Idle,
            burst_left: 0,
            command: Vec::new(),
            response: Vec::new(),
            read_at: 0,
            not_ready_reads: 0,
            commands: 0,
        }
    }

    /// A register page that identifies itself as another interface.
    pub fn with_interface_id(self, interface_id: u32) -> Self {
        Self {
            interface_id,
            ..self
        }
    }

    /// A model whose locality is never granted.
    pub fn never_granting(self) -> Self {
        Self {
            delay: u32::MAX,
            ..self
        }
    }

    pub fn is_idle_and_released(&self) -> bool {
        self.state == State::Idle && !self.active && self.grant.is_none()
    }

    fn require_locality(&self, what: &str) {
        assert!(self.active, "{what} without locality 0 held");
    }

    fn expecting(&self) -> bool {
        if self.command.len() < 6 {
            return true;
        }
        let size = u32::from_be_bytes(self.command[2..6].try_into().unwrap()) as usize;
        self.command.len() < size
    }

    fn status(&mut self) -> u32 {
        match self.state {
            State::Idle => STS_VALID,
            State::Readying(0) => {
                self.state = State::Ready;
                self.status()
            }
            State::Readying(left) => {
                self.state = State::Readying(left - 1);
                self.not_ready_reads += 1;
                STS_VALID
            }
            State::Ready => {
                self.burst_left = self.burst;
                STS_VALID | STS_COMMAND_READY | ((self.burst as u32) << 8)
            }
            State::Reception => {
                self.burst_left = self.burst;
                let expect = if self.expecting() { STS_EXPECT } else { 0 };
                STS_VALID | expect | ((self.burst as u32) << 8)
            }
            State::Executing(0) => {
                self.response = self.core.execute(&self.command);
                self.commands += 1;
                self.read_at = 0;
                self.state = State::Completion;
                self.status()
            }
            State::Executing(left) => {
                self.state = State::Executing(left - 1);
                self.not_ready_reads += 1;
                STS_VALID
            }
            State::Completion => {
                let remaining = self.response.len() - self.read_at;
                self.burst_left = remaining.min(self.burst);
                let avail = if remaining > 0 { STS_DATA_AVAIL } else { 0 };
                STS_VALID | avail | ((self.burst_left as u32) << 8)
            }
        }
    }
}

impl<K: Core> RegisterWindow for TisModel<K> {
    fn span(&self) -> usize {
        0x1000
    }

    fn read_u8(&mut self, offset: usize) -> u8 {
        match offset {
            ACCESS => {
                match self.grant {
                    Some(0) => {
                        self.grant = None;
                        self.active = true;
                    }
                    Some(left) => {
                        self.grant = Some(left.saturating_sub(1));
                        self.not_ready_reads += 1;
                    }
                    None => {}
                }
                ACCESS_VALID
                    | if self.active {
                        ACCESS_ACTIVE_LOCALITY
                    } else {
                        0
                    }
            }
            DATA_FIFO => {
                self.require_locality("a FIFO read");
                assert_eq!(
                    self.state,
                    State::Completion,
                    "a FIFO read outside the Completion state"
                );
                assert!(self.burst_left > 0, "a FIFO read past the burst count");
                assert!(
                    self.read_at < self.response.len(),
                    "a FIFO read past the end of the response"
                );
                self.burst_left -= 1;
                self.read_at += 1;
                self.response[self.read_at - 1]
            }
            other => panic!("an 8-bit read of TIS register {other:#x}"),
        }
    }

    fn read_u32(&mut self, offset: usize) -> u32 {
        match offset {
            STS => {
                self.require_locality("a TPM_STS read");
                self.status()
            }
            INTERFACE_ID => self.interface_id,
            other => panic!("a 32-bit read of TIS register {other:#x}"),
        }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        match (offset, value) {
            (ACCESS, ACCESS_REQUEST_USE) => {
                assert!(!self.active, "locality 0 requested while already held");
                self.grant = Some(self.delay);
            }
            (ACCESS, ACCESS_ACTIVE_LOCALITY) => {
                self.require_locality("a locality release");
                assert!(
                    matches!(self.state, State::Idle | State::Ready),
                    "locality released in the {:?} state",
                    self.state
                );
                self.active = false;
                self.state = State::Idle;
            }
            (STS, value) if u32::from(value) == STS_COMMAND_READY => {
                self.require_locality("a commandReady write");
                self.state = match self.state {
                    State::Idle => State::Readying(self.delay),
                    State::Ready => State::Ready,
                    // The PTP: commandReady in Completion discards the
                    // response and returns to Idle; in Reception or
                    // Execution it aborts the command.
                    State::Completion | State::Reception | State::Executing(_) => State::Idle,
                    State::Readying(left) => State::Readying(left),
                };
                self.command.clear();
            }
            (STS, STS_GO) => {
                self.require_locality("a tpmGo write");
                assert!(
                    self.state == State::Reception && !self.expecting(),
                    "tpmGo before the whole command was written ({:?}, {} bytes)",
                    self.state,
                    self.command.len()
                );
                self.state = State::Executing(self.delay);
            }
            (DATA_FIFO, byte) => {
                self.require_locality("a FIFO write");
                assert!(
                    matches!(self.state, State::Ready | State::Reception),
                    "a FIFO write in the {:?} state",
                    self.state
                );
                assert!(self.burst_left > 0, "a FIFO write past the burst count");
                assert!(
                    self.expecting(),
                    "a FIFO write past the size the command header names"
                );
                self.burst_left -= 1;
                self.command.push(byte);
                self.state = State::Reception;
            }
            (other, value) => panic!("an 8-bit write of {value:#x} to TIS register {other:#x}"),
        }
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        panic!("a 32-bit write of {value:#x} to TIS register {offset:#x}")
    }
}
