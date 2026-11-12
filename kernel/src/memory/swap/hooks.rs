//! The hook table, generated once for every backend.
//!
//! The kernel is not generic over the address space: there is exactly one
//! per machine, chosen at link time, and the consumers that need it (the
//! swap service, [`SwapHandle::ensure_present`](super::SwapHandle), the
//! backend's page-fault entry) hold no value of its type. The table is
//! therefore a `&'static` of plain function items, which puts no vtable on
//! the fault path, and a backend names its address space once instead of
//! writing the forwarding glue.

use helios_hal::vmm::{AddressSpace, AddressSpaceError, PageFlags, SwapToken, VirtAddr};

use super::{CommittedPageVisitor, OrphanedTokenVisitor, SwapVmHooks};

/// An address space a backend installed at boot and the kernel can reach
/// without holding a value of it.
pub trait InstalledAddressSpace: AddressSpace + 'static {
    /// The machine's one address space.
    ///
    /// Called from trap context for [`AddressSpace::swapped_token`], so it
    /// must be a load of an already-initialised static and nothing more.
    fn installed() -> &'static Self;
}

/// The hook table for `S`, whose forwarders reach it through
/// [`InstalledAddressSpace::installed`].
///
/// `now_ticks` is the platform's monotonic tick counter, the one
/// `Cpu::now` reads: it is not the address space's, but the table is how
/// the fault entry, which holds no `Cpu`, reaches it.
pub const fn swap_vm_hooks<S: InstalledAddressSpace>(now_ticks: fn() -> u64) -> SwapVmHooks {
    SwapVmHooks {
        swap_out_page: swap_out_page::<S>,
        swap_in_page: swap_in_page::<S>,
        swapped_token: swapped_token::<S>,
        scan_committed_pages: scan_committed_pages::<S>,
        owned_resident_bytes: owned_resident_bytes::<S>,
        drain_orphaned_swap_tokens: drain_orphaned_swap_tokens::<S>,
        now_ticks,
    }
}

fn swap_out_page<S: InstalledAddressSpace>(
    addr: VirtAddr,
    token: SwapToken,
    out: &mut [u8],
) -> Result<PageFlags, AddressSpaceError> {
    S::installed().swap_out_page(addr, token, out)
}

fn swap_in_page<S: InstalledAddressSpace>(
    addr: VirtAddr,
    bytes: &[u8],
) -> Result<SwapToken, AddressSpaceError> {
    S::installed().swap_in_page(addr, bytes)
}

fn swapped_token<S: InstalledAddressSpace>(addr: VirtAddr) -> Option<SwapToken> {
    S::installed().swapped_token(addr)
}

/// The visitor takes the caller's state as an opaque pointer and a plain
/// function, because a function pointer cannot be generic over a closure;
/// the closure is built here, around the pointer, where the address space
/// can take it.
fn scan_committed_pages<S: InstalledAddressSpace>(
    owner: u64,
    context: *mut (),
    visit: CommittedPageVisitor,
) -> usize {
    S::installed().scan_committed_pages(owner, |addr, flags, age| visit(context, addr, flags, age))
}

fn owned_resident_bytes<S: InstalledAddressSpace>(owner: u64) -> u64 {
    S::installed().owned_resident_bytes(owner)
}

fn drain_orphaned_swap_tokens<S: InstalledAddressSpace>(
    context: *mut (),
    visit: OrphanedTokenVisitor,
) -> usize {
    S::installed().drain_orphaned_swap_tokens(|token| visit(context, token))
}
