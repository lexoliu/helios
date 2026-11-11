//! The FIFO interface of the TCG PC Client Platform TPM Profile, the one
//! TCG's earlier TIS specification introduced.
//!
//! The register offsets and bits are the PTP's, as QEMU's emulation
//! spells them (`include/hw/acpi/tpm.h`, `TPM_TIS_REG_*`,
//! `TPM_TIS_STS_*`, `TPM_TIS_ACCESS_*`). The exchange follows Linux's
//! `drivers/char/tpm/tpm_tis_core.c`: claim the locality, ask for
//! `commandReady`, write the command in bursts while the TPM still
//! expects data, `tpmGo`, read the response in bursts, then return the
//! TPM to idle and release the locality.

use crate::clock::{Clock, wait_until};
use crate::transport::{
    COMMAND_DURATION, HEADER_LEN, TIMEOUT_A, TIMEOUT_B, TIMEOUT_C, TIMEOUT_D, Transport,
    TransportError, response_size,
};
use crate::window::RegisterWindow;

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

/// `TPM_INTERFACE_ID.InterfaceType`, bits 3:0: 0 for the FIFO
/// interface of a TPM 2.0, 0xF for the TIS 1.3 register layout, which
/// has no `TPM_INTERFACE_ID` and reads it as all ones (QEMU
/// `TPM_TIS_IFACE_ID_INTERFACE_FIFO`, `TPM_TIS_IFACE_ID_INTERFACE_TIS1_3`).
const INTERFACE_TYPE_MASK: u32 = 0xf;
const INTERFACE_TYPE_FIFO: u32 = 0x0;
const INTERFACE_TYPE_TIS_1_3: u32 = 0xf;

/// A TPM reached through the FIFO interface at locality 0.
pub struct Tis<W, C> {
    window: W,
    clock: C,
}

fn burst_count(status: u32) -> usize {
    ((status >> 8) & 0xffff) as usize
}

impl<W: RegisterWindow, C: Clock> Tis<W, C> {
    /// Takes locality 0's register page, after checking that it is one.
    pub fn open(mut window: W, clock: C) -> Result<Self, TransportError> {
        let interface = window.read_u32(INTERFACE_ID) & INTERFACE_TYPE_MASK;
        if interface != INTERFACE_TYPE_FIFO && interface != INTERFACE_TYPE_TIS_1_3 {
            return Err(TransportError::WrongInterface {
                expected: "the FIFO interface",
                found: interface,
            });
        }
        Ok(Self { window, clock })
    }

    async fn status(
        &mut self,
        mask: u32,
        state: &'static str,
        timeout: core::time::Duration,
    ) -> Result<u32, TransportError> {
        let mut seen = 0;
        wait_until(&mut self.window, &self.clock, state, timeout, |window| {
            seen = window.read_u32(STS);
            seen & mask == mask
        })
        .await?;
        Ok(seen)
    }

    async fn burst(&mut self, also: u32, state: &'static str) -> Result<usize, TransportError> {
        let mut burst = 0;
        wait_until(&mut self.window, &self.clock, state, TIMEOUT_D, |window| {
            let status = window.read_u32(STS);
            burst = burst_count(status);
            status & (STS_VALID | also) == STS_VALID | also && burst != 0
        })
        .await?;
        Ok(burst)
    }

    async fn send(&mut self, command: &[u8]) -> Result<(), TransportError> {
        self.window.write_u8(STS, STS_COMMAND_READY as u8);
        self.status(STS_COMMAND_READY, "TPM_STS.commandReady", TIMEOUT_B)
            .await?;

        let (last, body) = command
            .split_last()
            .expect("a TPM command frame is never empty");
        let mut sent = 0;
        while sent < body.len() {
            let burst = self.burst(0, "a write burst count").await?;
            for &byte in &body[sent..body.len().min(sent + burst)] {
                self.window.write_u8(DATA_FIFO, byte);
            }
            sent = body.len().min(sent + burst);
            let status = self
                .status(STS_VALID, "TPM_STS.stsValid", TIMEOUT_C)
                .await?;
            if status & STS_EXPECT == 0 {
                return Err(TransportError::Protocol {
                    what: "TPM_STS.Expect clear before the command's last byte",
                });
            }
        }
        self.window.write_u8(DATA_FIFO, *last);
        let status = self
            .status(STS_VALID, "TPM_STS.stsValid", TIMEOUT_C)
            .await?;
        if status & STS_EXPECT != 0 {
            return Err(TransportError::Protocol {
                what: "TPM_STS.Expect still set after the command's last byte",
            });
        }
        self.window.write_u8(STS, STS_GO);
        Ok(())
    }

    async fn read_fifo(&mut self, into: &mut [u8]) -> Result<(), TransportError> {
        let mut read = 0;
        while read < into.len() {
            let burst = self
                .burst(STS_DATA_AVAIL, "TPM_STS.dataAvail with a burst count")
                .await?;
            let end = into.len().min(read + burst);
            for byte in &mut into[read..end] {
                *byte = self.window.read_u8(DATA_FIFO);
            }
            read = end;
        }
        Ok(())
    }

    async fn receive(&mut self, response: &mut [u8]) -> Result<usize, TransportError> {
        self.status(
            STS_VALID | STS_DATA_AVAIL,
            "TPM_STS.dataAvail after tpmGo",
            COMMAND_DURATION,
        )
        .await?;

        let mut header = [0; HEADER_LEN];
        self.read_fifo(&mut header).await?;
        let len = response_size(&header);
        if !(HEADER_LEN..=response.len()).contains(&len) {
            return Err(TransportError::ResponseSize {
                len,
                capacity: response.len(),
            });
        }
        response[..HEADER_LEN].copy_from_slice(&header);
        self.read_fifo(&mut response[HEADER_LEN..len]).await?;

        let status = self
            .status(STS_VALID, "TPM_STS.stsValid", TIMEOUT_C)
            .await?;
        if status & STS_DATA_AVAIL != 0 {
            return Err(TransportError::Protocol {
                what: "TPM_STS.dataAvail still set after the response's last byte",
            });
        }
        Ok(len)
    }

    async fn exchange(
        &mut self,
        command: &[u8],
        response: &mut [u8],
    ) -> Result<usize, TransportError> {
        self.send(command).await?;
        self.receive(response).await
    }
}

impl<W: RegisterWindow, C: Clock> Transport for Tis<W, C> {
    async fn transact(
        &mut self,
        command: &[u8],
        response: &mut [u8],
    ) -> Result<usize, TransportError> {
        self.window.write_u8(ACCESS, ACCESS_REQUEST_USE);
        wait_until(
            &mut self.window,
            &self.clock,
            "TPM_ACCESS.activeLocality",
            TIMEOUT_A,
            |window| {
                let access = window.read_u8(ACCESS);
                access & (ACCESS_VALID | ACCESS_ACTIVE_LOCALITY)
                    == ACCESS_VALID | ACCESS_ACTIVE_LOCALITY
            },
        )
        .await?;

        let result = self.exchange(command, response).await;

        // Back to idle whatever happened: a commandReady write aborts a
        // half-sent command and discards a half-read response, so the
        // next exchange starts from a known state.
        self.window.write_u8(STS, STS_COMMAND_READY as u8);
        self.window.write_u8(ACCESS, ACCESS_ACTIVE_LOCALITY);
        result
    }
}
