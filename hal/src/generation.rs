//! The VM generation ID: a platform fact that says a machine's state was
//! duplicated or rolled back underneath the guest.
//!
//! A virtual-machine monitor that snapshots or clones a guest writes a
//! fresh 128-bit identifier into a window of guest memory before the
//! copy runs (Microsoft's VM Generation ID, which QEMU and Firecracker
//! implement as `vmgenid`). A guest that remembers the last identifier
//! it saw can tell, by comparing, that it has been restored.
//!
//! This module names only the window and the value. Finding the window
//! (ACPI, a device tree node, a device) is the backend's job, and what
//! the kernel does about a change is the kernel's.

use core::num::NonZeroU64;
use core::ptr::NonNull;

use thiserror::Error;

/// The 16 bytes a VMM writes into the window.
///
/// The value has no ordering and no sentinel: an all-zero identifier is
/// as valid as any other, and two readings differ exactly when the VMM
/// wrote between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmGenerationId([u8; 16]);

impl VmGenerationId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// The two little-endian halves, in the order they sit in the window.
    pub const fn to_words(self) -> [u64; 2] {
        let [
            a0,
            a1,
            a2,
            a3,
            a4,
            a5,
            a6,
            a7,
            b0,
            b1,
            b2,
            b3,
            b4,
            b5,
            b6,
            b7,
        ] = self.0;
        [
            u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
            u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]),
        ]
    }

    pub const fn from_words(words: [u64; 2]) -> Self {
        let low = words[0].to_le_bytes();
        let high = words[1].to_le_bytes();
        Self([
            low[0], low[1], low[2], low[3], low[4], low[5], low[6], low[7], high[0], high[1],
            high[2], high[3], high[4], high[5], high[6], high[7],
        ])
    }
}

/// The guest-physical address of the window, as the platform's `ADDR`
/// object spells it.
///
/// The VM Generation ID specification has firmware describe the window
/// as an `ADDR` package of two dword integers, the low and the high half
/// of the address. This decodes the package alone: whether firmware has
/// written the address at all is the platform's own signal (QEMU's
/// device reports it through `_STA`), checked before `ADDR` is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationIdAddress(NonZeroU64);

/// Why an `ADDR` package does not name a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum GenerationIdAddressError {
    #[error("ADDR holds {found} elements; the VM Generation ID specification defines two")]
    Arity { found: usize },
    #[error("ADDR element {index} is {value:#x}, which is not a dword")]
    NotADword { index: usize, value: u64 },
    #[error("ADDR is zero: firmware has not written the window address")]
    Zero,
    #[error("ADDR {address:#x} is not aligned to 8 bytes, which the window reads need")]
    Unaligned { address: u64 },
}

impl GenerationIdAddress {
    /// Decodes the integers of an `ADDR` package, low half first.
    pub fn from_acpi_addr(elements: &[u64]) -> Result<Self, GenerationIdAddressError> {
        let &[low, high] = elements else {
            return Err(GenerationIdAddressError::Arity {
                found: elements.len(),
            });
        };
        for (index, value) in [low, high].into_iter().enumerate() {
            if value > u64::from(u32::MAX) {
                return Err(GenerationIdAddressError::NotADword { index, value });
            }
        }
        let address = low | (high << 32);
        let address = NonZeroU64::new(address).ok_or(GenerationIdAddressError::Zero)?;
        if !address.get().is_multiple_of(8) {
            return Err(GenerationIdAddressError::Unaligned {
                address: address.get(),
            });
        }
        Ok(Self(address))
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// The guest-physical window in which a VMM publishes the
/// [`VmGenerationId`], mapped into the kernel's address space.
///
/// The guest only ever reads it. A reading is two volatile 8-byte loads,
/// so a VMM write that lands between them yields a value that mixes the
/// old and new halves: that is a third value, different from both, and
/// is therefore still seen as a change. It can cause an extra
/// observation of change, never a missed one.
#[derive(Clone, Copy, Debug)]
pub struct GenerationIdWindow {
    ptr: NonNull<u64>,
}

// SAFETY: the window is a read-only mapping the guest never writes, and
// every read is a volatile load; sharing the pointer between processors
// adds no hazard a single reader does not have.
unsafe impl Send for GenerationIdWindow {}
// SAFETY: see above.
unsafe impl Sync for GenerationIdWindow {}

impl GenerationIdWindow {
    /// Wraps the mapping of a generation ID window.
    ///
    /// # Safety
    ///
    /// `ptr` must address 16 bytes aligned to 8 that stay mapped and
    /// readable for as long as the returned window, or any copy of it,
    /// is used. The kernel's lifetime satisfies this for a mapping the
    /// backend never unmaps.
    pub const unsafe fn new(ptr: NonNull<u64>) -> Self {
        Self { ptr }
    }

    /// Reads the identifier currently in the window.
    pub fn read(&self) -> VmGenerationId {
        // SAFETY: `new` promised 16 readable, 8-aligned bytes for the
        // life of the window.
        let words = unsafe {
            [
                self.ptr.as_ptr().read_volatile(),
                self.ptr.as_ptr().add(1).read_volatile(),
            ]
        };
        VmGenerationId::from_words(words)
    }
}

#[cfg(test)]
mod tests {
    use super::{GenerationIdAddress, GenerationIdAddressError, VmGenerationId};

    #[test]
    fn addr_joins_its_two_dwords_low_first() {
        let address = GenerationIdAddress::from_acpi_addr(&[0x1234_5678, 0x2]).unwrap();
        assert_eq!(address.get(), 0x2_1234_5678);
    }

    #[test]
    fn a_zero_addr_is_refused() {
        assert_eq!(
            GenerationIdAddress::from_acpi_addr(&[0, 0]),
            Err(GenerationIdAddressError::Zero)
        );
    }

    #[test]
    fn addr_of_the_wrong_arity_is_refused() {
        for elements in [&[][..], &[0x1000][..], &[0x1000, 0, 0][..]] {
            assert_eq!(
                GenerationIdAddress::from_acpi_addr(elements),
                Err(GenerationIdAddressError::Arity {
                    found: elements.len()
                })
            );
        }
    }

    #[test]
    fn addr_halves_wider_than_a_dword_are_refused() {
        assert_eq!(
            GenerationIdAddress::from_acpi_addr(&[0x1_0000_0000, 0]),
            Err(GenerationIdAddressError::NotADword {
                index: 0,
                value: 0x1_0000_0000
            })
        );
    }

    #[test]
    fn an_unaligned_addr_is_refused() {
        assert_eq!(
            GenerationIdAddress::from_acpi_addr(&[0x1004, 0]),
            Err(GenerationIdAddressError::Unaligned { address: 0x1004 })
        );
    }

    #[test]
    fn words_round_trip_through_bytes() {
        let id = VmGenerationId::from_bytes(*b"0123456789abcdef");
        assert_eq!(VmGenerationId::from_words(id.to_words()), id);
        assert_eq!(id.to_words()[0], u64::from_le_bytes(*b"01234567"));
        assert_eq!(id.to_words()[1], u64::from_le_bytes(*b"89abcdef"));
    }
}
