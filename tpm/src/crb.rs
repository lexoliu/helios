//! The Command Response Buffer interface of the TCG PC Client Platform
//! TPM Profile.
//!
//! The register offsets and bits are the PTP's, as QEMU's emulation
//! spells them (`include/hw/acpi/tpm.h`, `REG32(CRB_*)`). The exchange
//! follows Linux's `drivers/char/tpm/tpm_crb.c`: claim the locality,
//! request `cmdReady`, copy the command into the command buffer, start
//! it, wait for the start bit to clear, copy the response out, then
//! `goIdle` and release the locality.
//!
//! The command and response buffers are found through the control
//! area, as physical addresses. The driver reaches them only inside the
//! register window it was given, which is where QEMU and the PTP's
//! default layout put them (`TPM_CRB_DATA_BUFFER_0` at offset 0x80).

use core::time::Duration;

use crate::clock::{Clock, wait_until};
use crate::transport::{
    COMMAND_DURATION, HEADER_LEN, TIMEOUT_C, Transport, TransportError, response_size,
};
use crate::window::RegisterWindow;

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

const LOC_STATE_VALID: u32 = 1 << 7;
const LOC_STATE_ASSIGNED: u32 = 1 << 1;
const LOC_CTRL_REQUEST_ACCESS: u32 = 1 << 0;
const LOC_CTRL_RELINQUISH: u32 = 1 << 1;
const CTRL_REQ_CMD_READY: u32 = 1 << 0;
const CTRL_REQ_GO_IDLE: u32 = 1 << 1;
const CTRL_STS_ERROR: u32 = 1 << 0;
const CTRL_START_INVOKE: u32 = 1 << 0;

/// `TPM_CRB_INTF_ID.InterfaceType`, bits 3:0: 1 for an active CRB
/// (QEMU `CRB_INTF_TYPE_CRB_ACTIVE`).
const INTERFACE_TYPE_MASK: u32 = 0xf;
const INTERFACE_TYPE_CRB: u32 = 0x1;

/// Where in the window a buffer lives.
#[derive(Clone, Copy)]
struct Buffer {
    offset: usize,
    size: usize,
}

/// A TPM reached through the Command Response Buffer at locality 0.
pub struct Crb<W, C> {
    window: W,
    clock: C,
    command: Buffer,
    response: Buffer,
}

impl<W: RegisterWindow, C: Clock> Crb<W, C> {
    /// Takes locality 0's register page, mapped from `physical_base`,
    /// and finds the command and response buffers in it.
    ///
    /// The control area is read with the locality held and the TPM
    /// ready, the way Linux's `crb_map_io` reads it: a TPM may not
    /// publish valid buffer addresses before.
    pub async fn open(window: W, physical_base: u64, clock: C) -> Result<Self, TransportError> {
        let mut crb = Self {
            window,
            clock,
            command: Buffer { offset: 0, size: 0 },
            response: Buffer { offset: 0, size: 0 },
        };
        let interface = crb.window.read_u32(INTF_ID) & INTERFACE_TYPE_MASK;
        if interface != INTERFACE_TYPE_CRB {
            return Err(TransportError::WrongInterface {
                expected: "the Command Response Buffer",
                found: interface,
            });
        }

        crb.request_locality().await?;
        let found = match crb.ready().await {
            Ok(()) => crb.find_buffers(physical_base),
            Err(error) => Err(error),
        };
        let released = crb.release().await;
        let (command, response) = found?;
        released?;
        crb.command = command;
        crb.response = response;
        Ok(crb)
    }

    fn find_buffers(&mut self, physical_base: u64) -> Result<(Buffer, Buffer), TransportError> {
        let command_address = u64::from(self.window.read_u32(CTRL_CMD_LADDR))
            | (u64::from(self.window.read_u32(CTRL_CMD_HADDR)) << 32);
        let command_size = self.window.read_u32(CTRL_CMD_SIZE);
        let response_address = u64::from(self.window.read_u32(CTRL_RSP_ADDR_LOW))
            | (u64::from(self.window.read_u32(CTRL_RSP_ADDR_HIGH)) << 32);
        let response_size = self.window.read_u32(CTRL_RSP_SIZE);
        let window = self.window.span();
        Ok((
            locate(
                "command",
                command_address,
                command_size,
                physical_base,
                window,
            )?,
            locate(
                "response",
                response_address,
                response_size,
                physical_base,
                window,
            )?,
        ))
    }

    async fn wait_register(
        &mut self,
        register: usize,
        mask: u32,
        value: u32,
        state: &'static str,
        timeout: Duration,
    ) -> Result<(), TransportError> {
        wait_until(&mut self.window, &self.clock, state, timeout, |window| {
            window.read_u32(register) & mask == value
        })
        .await
    }

    async fn request_locality(&mut self) -> Result<(), TransportError> {
        self.window.write_u32(LOC_CTRL, LOC_CTRL_REQUEST_ACCESS);
        let held = LOC_STATE_ASSIGNED | LOC_STATE_VALID;
        self.wait_register(
            LOC_STATE,
            held,
            held,
            "TPM_LOC_STATE.locAssigned",
            TIMEOUT_C,
        )
        .await
    }

    async fn ready(&mut self) -> Result<(), TransportError> {
        self.window.write_u32(CTRL_REQ, CTRL_REQ_CMD_READY);
        self.wait_register(
            CTRL_REQ,
            CTRL_REQ_CMD_READY,
            0,
            "TPM_CRB_CTRL_REQ.cmdReady cleared",
            TIMEOUT_C,
        )
        .await
    }

    /// Returns the TPM to idle and gives the locality back.
    async fn release(&mut self) -> Result<(), TransportError> {
        self.window.write_u32(CTRL_REQ, CTRL_REQ_GO_IDLE);
        let idle = self
            .wait_register(
                CTRL_REQ,
                CTRL_REQ_GO_IDLE,
                0,
                "TPM_CRB_CTRL_REQ.goIdle cleared",
                TIMEOUT_C,
            )
            .await;
        self.window.write_u32(LOC_CTRL, LOC_CTRL_RELINQUISH);
        let released = self
            .wait_register(
                LOC_STATE,
                LOC_STATE_ASSIGNED | LOC_STATE_VALID,
                LOC_STATE_VALID,
                "TPM_LOC_STATE.locAssigned cleared",
                TIMEOUT_C,
            )
            .await;
        idle.and(released)
    }

    async fn exchange(
        &mut self,
        command: &[u8],
        response: &mut [u8],
    ) -> Result<usize, TransportError> {
        self.ready().await?;
        // A cancel left set would cancel the command about to start.
        self.window.write_u32(CTRL_CANCEL, 0);
        for (index, &byte) in command.iter().enumerate() {
            self.window.write_u8(self.command.offset + index, byte);
        }
        // The buffer and the start register are the same device
        // mapping, whose accesses reach it in program order.
        self.window.write_u32(CTRL_START, CTRL_START_INVOKE);
        self.wait_register(
            CTRL_START,
            CTRL_START_INVOKE,
            0,
            "TPM_CRB_CTRL_START cleared",
            COMMAND_DURATION,
        )
        .await?;
        if self.window.read_u32(CTRL_STS) & CTRL_STS_ERROR != 0 {
            return Err(TransportError::Fatal);
        }

        let mut header = [0; HEADER_LEN];
        for (index, byte) in header.iter_mut().enumerate() {
            *byte = self.window.read_u8(self.response.offset + index);
        }
        let len = response_size(&header);
        let capacity = response.len().min(self.response.size);
        if !(HEADER_LEN..=capacity).contains(&len) {
            return Err(TransportError::ResponseSize { len, capacity });
        }
        response[..HEADER_LEN].copy_from_slice(&header);
        for (index, byte) in response[..len].iter_mut().enumerate().skip(HEADER_LEN) {
            *byte = self.window.read_u8(self.response.offset + index);
        }
        Ok(len)
    }
}

/// The window offset of a buffer the control area names by physical
/// address.
fn locate(
    buffer: &'static str,
    address: u64,
    size: u32,
    physical_base: u64,
    window: usize,
) -> Result<Buffer, TransportError> {
    let outside = TransportError::BufferOutsideWindow {
        buffer,
        address,
        size,
    };
    let offset = address
        .checked_sub(physical_base)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or(outside)?;
    let size = size as usize;
    if size < HEADER_LEN || offset.checked_add(size).is_none_or(|end| end > window) {
        return Err(outside);
    }
    Ok(Buffer { offset, size })
}

impl<W: RegisterWindow, C: Clock> Transport for Crb<W, C> {
    async fn transact(
        &mut self,
        command: &[u8],
        response: &mut [u8],
    ) -> Result<usize, TransportError> {
        if command.len() > self.command.size {
            return Err(TransportError::CommandTooLarge {
                len: command.len(),
                capacity: self.command.size,
            });
        }
        self.request_locality().await?;
        let result = self.exchange(command, response).await;
        let released = self.release().await;
        let len = result?;
        released?;
        Ok(len)
    }
}
