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

use core::ptr::NonNull;

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
    use super::VmGenerationId;

    #[test]
    fn words_round_trip_through_bytes() {
        let id = VmGenerationId::from_bytes(*b"0123456789abcdef");
        assert_eq!(VmGenerationId::from_words(id.to_words()), id);
        assert_eq!(id.to_words()[0], u64::from_le_bytes(*b"01234567"));
        assert_eq!(id.to_words()[1], u64::from_le_bytes(*b"89abcdef"));
    }
}
