//! The ACPI TPM2 table: which interface the firmware's TPM has, and
//! where its locality 0 registers are.
//!
//! The layout is the TCG ACPI Specification's "TPM2 ACPI Table"
//! (Family 1.2 and 2.0, Version 1.3, Table 7), the one QEMU writes in
//! `build_tpm2` (`hw/acpi/aml-build.c`): the standard 36-byte header,
//! the platform class, the address of the CRB control area at offset 40
//! and the start method at offset 48. Everything after the start method
//! (its parameters, the optional log area) is not read here.

use thiserror::Error;

use crate::transport::{ACPI_START_METHOD_CRB, ACPI_START_METHOD_TIS};

const SIGNATURE: &[u8; 4] = b"TPM2";
/// The standard ACPI system description table header.
const HEADER_LEN: usize = 36;
const LENGTH_OFFSET: usize = 4;
const CONTROL_AREA_OFFSET: usize = 40;
const START_METHOD_OFFSET: usize = 48;
/// The bytes up to and including the start method: the least a table
/// this parser reads may have.
const MIN_LEN: usize = START_METHOD_OFFSET + 4;

/// The offset of `TPM_CRB_CTRL_REQ_0`, the first register of the CRB
/// control area, from the locality 0 base (TCG PC Client Platform TPM
/// Profile, CRB register map; QEMU `A_CRB_CTRL_REQ`).
const CRB_CONTROL_AREA_OFFSET: u64 = 0x40;

/// Where a PC Client platform's FIFO interface puts locality 0: the
/// TCG PC Client Platform TPM Profile fixes the FIFO's locality 0 at
/// `FED4_0000h` on every PC platform ("TPM Locality Address Mapping"),
/// which is why the TPM2 table carries no address for it. QEMU's ISA
/// `tpm-tis` sits there (`TPM_TIS_ADDR_BASE`).
pub const PC_CLIENT_FIFO_LOCALITY_0: u64 = 0xfed4_0000;

/// Why the TPM2 table could not be read.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum AcpiTpm2Error {
    #[error("the table's signature is {found:?}, not \"TPM2\"")]
    Signature { found: [u8; 4] },
    #[error(
        "the table declares {declared} bytes; at least {MIN_LEN} are needed and {mapped} are mapped"
    )]
    Length { declared: usize, mapped: usize },
    #[error("the table's bytes do not sum to zero")]
    Checksum,
    #[error(
        "the CRB control area at {address:#x} lies below the {CRB_CONTROL_AREA_OFFSET:#x}-byte \
         offset it has from locality 0"
    )]
    ControlAreaAddress { address: u64 },
}

/// The interface an ACPI TPM2 table names, and where locality 0 is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcpiTpm2Locality {
    /// The firmware's start method is the CRB, whose control area the
    /// table names: locality 0 is the page it sits in.
    Crb { base: u64 },
    /// The firmware's start method is the FIFO interface, for which the
    /// table carries no address: the platform's own description gives
    /// it (the PC Client fixed address, or an ACPI device's `_CRS`).
    Fifo,
    /// A start method this crate does not drive.
    Unsupported { start_method: u32 },
}

/// A parsed ACPI TPM2 table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpiTpm2 {
    start_method: u32,
    control_area: u64,
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("a four-byte range is four bytes"),
    )
}

impl AcpiTpm2 {
    /// Reads the table whose bytes, header included, start `bytes`.
    ///
    /// `bytes` may extend past the table; the header's length bounds
    /// what is read and checksummed.
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiTpm2Error> {
        if bytes.len() < HEADER_LEN {
            return Err(AcpiTpm2Error::Length {
                declared: 0,
                mapped: bytes.len(),
            });
        }
        let signature: [u8; 4] = bytes[..4].try_into().expect("four bytes");
        if &signature != SIGNATURE {
            return Err(AcpiTpm2Error::Signature { found: signature });
        }
        let declared = u32_at(bytes, LENGTH_OFFSET) as usize;
        if declared < MIN_LEN || declared > bytes.len() {
            return Err(AcpiTpm2Error::Length {
                declared,
                mapped: bytes.len(),
            });
        }
        let table = &bytes[..declared];
        if table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
            return Err(AcpiTpm2Error::Checksum);
        }
        Ok(Self {
            start_method: u32_at(table, START_METHOD_OFFSET),
            control_area: u64::from(u32_at(table, CONTROL_AREA_OFFSET))
                | (u64::from(u32_at(table, CONTROL_AREA_OFFSET + 4)) << 32),
        })
    }

    /// The start method, as the table spells it.
    pub const fn start_method(&self) -> u32 {
        self.start_method
    }

    /// Which interface the table names and, for the CRB, where its
    /// locality 0 registers are.
    pub fn locality(&self) -> Result<AcpiTpm2Locality, AcpiTpm2Error> {
        match self.start_method {
            ACPI_START_METHOD_CRB => self
                .control_area
                .checked_sub(CRB_CONTROL_AREA_OFFSET)
                .map(|base| AcpiTpm2Locality::Crb { base })
                .ok_or(AcpiTpm2Error::ControlAreaAddress {
                    address: self.control_area,
                }),
            ACPI_START_METHOD_TIS => Ok(AcpiTpm2Locality::Fifo),
            start_method => Ok(AcpiTpm2Locality::Unsupported { start_method }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TPM2 table QEMU 11.1 builds for `tpm-crb` on `q35`, as
    /// `build_tpm2` lays it out: revision 4, client platform class, the
    /// control area at `TPM_CRB_ADDR_CTRL` (0xfed40040), start method 7,
    /// twelve zero parameter bytes, a 64 KiB log area whose address the
    /// firmware's linker fills in.
    fn qemu_table(control_area: u64, start_method: u32) -> [u8; 76] {
        let mut table = [0_u8; 76];
        table[..4].copy_from_slice(b"TPM2");
        table[4..8].copy_from_slice(&76_u32.to_le_bytes());
        table[8] = 4;
        table[10..16].copy_from_slice(b"BOCHS ");
        table[16..24].copy_from_slice(b"BXPC    ");
        table[36..38].copy_from_slice(&0_u16.to_le_bytes());
        table[40..48].copy_from_slice(&control_area.to_le_bytes());
        table[48..52].copy_from_slice(&start_method.to_le_bytes());
        table[64..68].copy_from_slice(&0x1_0000_u32.to_le_bytes());
        let sum = table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        table[9] = 0_u8.wrapping_sub(sum);
        table
    }

    #[test]
    fn a_crb_table_names_the_page_its_control_area_is_in() {
        let table = AcpiTpm2::parse(&qemu_table(0xfed4_0040, 7)).unwrap();
        assert_eq!(table.start_method(), 7);
        assert_eq!(
            table.locality().unwrap(),
            AcpiTpm2Locality::Crb { base: 0xfed4_0000 }
        );
    }

    #[test]
    fn a_fifo_table_leaves_the_address_to_the_platform() {
        let table = AcpiTpm2::parse(&qemu_table(0, 6)).unwrap();
        assert_eq!(table.locality().unwrap(), AcpiTpm2Locality::Fifo);
    }

    #[test]
    fn a_start_method_this_crate_cannot_drive_is_named() {
        // 11: the CRB started through an Arm SMC.
        let table = AcpiTpm2::parse(&qemu_table(0x1000_0040, 11)).unwrap();
        assert_eq!(
            table.locality().unwrap(),
            AcpiTpm2Locality::Unsupported { start_method: 11 }
        );
    }

    #[test]
    fn a_damaged_table_is_refused() {
        let mut table = qemu_table(0xfed4_0040, 7);
        table[48] ^= 1;
        assert_eq!(AcpiTpm2::parse(&table), Err(AcpiTpm2Error::Checksum));

        let mut table = qemu_table(0xfed4_0040, 7);
        table[..4].copy_from_slice(b"TCPA");
        assert_eq!(
            AcpiTpm2::parse(&table),
            Err(AcpiTpm2Error::Signature { found: *b"TCPA" })
        );

        let table = qemu_table(0xfed4_0040, 7);
        assert_eq!(
            AcpiTpm2::parse(&table[..60]),
            Err(AcpiTpm2Error::Length {
                declared: 76,
                mapped: 60
            })
        );

        let table = AcpiTpm2::parse(&qemu_table(0x20, 7)).unwrap();
        assert_eq!(
            table.locality(),
            Err(AcpiTpm2Error::ControlAreaAddress { address: 0x20 })
        );
    }
}
