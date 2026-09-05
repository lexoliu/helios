//! How the kernel divides the memory the bootloader hands it.
//!
//! Every backend reaches this through the same call — `riscv/`,
//! `x86/`, `aarch64/` and `hosted/` all hand
//! [`crate::prime_bootstrap_allocator`] the usable regions of their
//! boot memory map and nothing else. No backend decides how much
//! memory either domain gets, and one that did would be the defect
//! this module exists to remove.
//!
//! # The policy
//!
//! **All usable memory is user pool.** The kernel heap starts with a
//! boot share — its reserve plus [`KERNEL_HEAP_BOOTSTRAP_BYTES`] of
//! working room — and takes the rest of what it needs out of the user
//! pool at run time, [`KERNEL_HEAP_GROWTH_CHUNK_BYTES`] at a time, for
//! as long as the pool can serve it. Nothing else is partitioned.
//!
//! The transfer is one-way by design, and the reason is the asymmetry
//! AGENTS §3 already states: a kernel out-of-memory is fatal, a user
//! out-of-memory kills one instance and the OOM killer reclaims it. So
//! when both domains want the last frame, the kernel takes it.
//!
//! # Why not a fraction of the machine
//!
//! The split used to be a blind per-region fraction: the kernel heap
//! kept a quarter of every boot region and the user pool took the
//! three quarters left. Both shares grew with the guest, so on the face
//! of it both scaled — and the density workload still could not place
//! 100 instances on a guest with a gigabyte and a half free.
//!
//! Run 33943692491, job `bench-x86-64-linux`, on a 2 GiB guest:
//!
//! ```text
//! User memory pool total_bytes=1429364736 available_bytes=1429364736
//! memory_per_instance_bytes = 8464992
//! ... exceeds its memory budget: available=132608808 of 548597760 reserved=137149440
//! ```
//!
//! The refusal is the **kernel heap**: 523.2 MiB of it, 130.8 MiB held
//! back as its reserve, so 392 MiB to spend — and a live instance costs
//! it about 8.1 MiB. That is 46 instances, and the run was refused at
//! the 46th while the user pool, which funds only the ~4.4 MiB of wasm
//! linear memory each instance holds, was still essentially untouched.
//!
//! A fixed ratio cannot be right, because the ratio a workload needs is
//! the workload's, not the machine's. What a static partition
//! guarantees is that one domain runs out with the other's share
//! stranded — which is exactly what happened, with 1.4 GiB free at the
//! moment of the refusal. Demand decides the split here instead, and
//! the only numbers left are a floor (what the kernel must never be
//! without) and a granularity (how much it takes at a time).

use core::ops::Range;

/// Working room the kernel heap holds at boot on top of its reserve.
///
/// All it has to do is carry the kernel from its first allocation to
/// the point where the user pool exists and growth is possible, which
/// is a handful of allocations later inside
/// [`crate::prime_bootstrap_allocator`]. Everything after that comes
/// from the pool on demand, so this is deliberately small: it is memory
/// taken off the top of every machine, however small the machine.
pub const KERNEL_HEAP_BOOTSTRAP_BYTES: usize = 16 * 1024 * 1024;

/// The most of a machine the kernel heap's boot share may be.
///
/// The share is a floor plus a slice, neither of which is a proportion,
/// so on a small enough machine it would crowd the user pool out
/// entirely. The kernel takes what it needs out of the pool afterwards
/// in any case, so starting under the floor on a small machine costs
/// nothing: [`crate::KernelHeapHeadroom`] measures the kernel's room
/// against the pool as well as against the kernel heap.
pub const KERNEL_HEAP_MAX_BOOT_FRACTION: usize = 2;

/// The share of the machine the kernel heap holds back for itself.
///
/// A kernel OOM is fatal and a user OOM is not, so the reserve is what
/// keeps user-mode demand from being able to end the kernel. It is a
/// share of the machine rather than of the kernel heap because the
/// kernel heap's own size is now demand-driven: a reserve defined
/// against a total that moves is a floor that moves with it.
///
/// One sixteenth reproduces the number the kernel defended before this
/// module existed, where the reserve was a quarter of a kernel heap
/// that was itself a quarter of the machine.
pub const KERNEL_HEAP_RESERVE_FRACTION: usize = 16;

/// The floor under [`KERNEL_HEAP_RESERVE_FRACTION`], for machines small
/// enough that a sixteenth is not a working set.
pub const KERNEL_HEAP_MIN_RESERVE_BYTES: usize = 32 * 1024 * 1024;

/// What the kernel heap takes out of the user pool in one grow.
///
/// Large enough that a hundred live instances cost a dozen transfers
/// rather than hundreds, and that each one comes out of the pool's
/// buddy as a single aligned block rather than scattering it.
pub const KERNEL_HEAP_GROWTH_CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// The smallest leftover worth handing to the user pool.
///
/// Also the alignment the user share starts on: the kernel's boot share
/// is a floor it keeps, so the rounding comes out of the pool's side.
pub const USER_POOL_MIN_REGION_BYTES: usize = 2 * 1024 * 1024;

/// The boot memory map's totals, and what the policy makes of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootMemoryPlan {
    /// Every usable byte the boot memory map described.
    pub usable_bytes: usize,
    /// Free kernel heap a user grow may not dip into, for the life of
    /// the kernel. Derived from `usable_bytes` once, so it does not
    /// move when memory moves between the two domains.
    pub kernel_reserve_bytes: usize,
    /// What the kernel heap is seeded with before the user pool takes
    /// the rest. The reserve plus [`KERNEL_HEAP_BOOTSTRAP_BYTES`], so
    /// the kernel starts above its own floor rather than on it.
    pub kernel_boot_bytes: usize,
}

impl BootMemoryPlan {
    /// The plan for a machine whose boot memory map came to
    /// `usable_bytes`.
    pub const fn for_usable_bytes(usable_bytes: usize) -> Self {
        let kernel_reserve_bytes = kernel_reserve_for(usable_bytes);
        Self {
            usable_bytes,
            kernel_reserve_bytes,
            kernel_boot_bytes: saturating_min(
                kernel_reserve_bytes.saturating_add(KERNEL_HEAP_BOOTSTRAP_BYTES),
                usable_bytes / KERNEL_HEAP_MAX_BOOT_FRACTION,
            ),
        }
    }

    /// What the user pool is seeded with: everything the kernel heap
    /// did not take at boot.
    ///
    /// It is not a ceiling. The pool gives frames back to the kernel
    /// heap as the kernel needs them, so this is the pool's starting
    /// size and the machine's size is its bound.
    pub const fn user_pool_bytes(self) -> usize {
        self.usable_bytes.saturating_sub(self.kernel_boot_bytes)
    }

    /// A splitter that walks the same memory map a second time and
    /// hands each region to the two domains in the order this plan
    /// states.
    pub const fn splitter(self) -> BootRegionSplitter {
        BootRegionSplitter {
            kernel_owed_bytes: self.kernel_boot_bytes,
        }
    }
}

/// The kernel heap's reserve on a machine of `usable_bytes`.
///
/// Public and pure so a test can state the relationship between guest
/// memory and the two domains without booting anything.
pub const fn kernel_reserve_for(usable_bytes: usize) -> usize {
    let share = usable_bytes / KERNEL_HEAP_RESERVE_FRACTION;
    let floored = if share < KERNEL_HEAP_MIN_RESERVE_BYTES {
        KERNEL_HEAP_MIN_RESERVE_BYTES
    } else {
        share
    };
    saturating_min(floored, usable_bytes)
}

const fn saturating_min(left: usize, right: usize) -> usize {
    if left < right { left } else { right }
}

/// `value` rounded up to a multiple of `align`, a power of two.
///
/// Saturates rather than panicking on overflow, because the caller
/// compares the result against a region end: a region that reaches the
/// top of the address space should lose its tail to the rounding, not
/// take the kernel down.
const fn align_up_saturating(value: usize, align: usize) -> usize {
    let mask = align - 1;
    match value.checked_add(mask) {
        Some(raised) => raised & !mask,
        None => usize::MAX,
    }
}

/// One boot memory region as the policy divides it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionShares {
    /// The part the kernel heap is seeded with, if any.
    pub kernel: Option<Range<usize>>,
    /// The part the user pool is seeded with, if any.
    pub user: Option<Range<usize>>,
}

/// Hands successive boot memory regions to the kernel heap until its
/// boot share is met, then to the user pool.
///
/// The kernel's share is taken from the front of the map rather than
/// from a slice of each region, because it is a fixed quantity and not
/// a proportion: a machine with twenty usable segments should not get
/// twenty kernel heaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootRegionSplitter {
    kernel_owed_bytes: usize,
}

impl BootRegionSplitter {
    /// Divides `start..end`, consuming what the kernel heap is still
    /// owed.
    pub fn split(&mut self, start: usize, end: usize) -> RegionShares {
        let Some(len) = end.checked_sub(start).filter(|len| *len != 0) else {
            return RegionShares {
                kernel: None,
                user: None,
            };
        };

        let kernel_len = self.kernel_owed_bytes.min(len);
        self.kernel_owed_bytes -= kernel_len;
        let kernel = (kernel_len != 0).then(|| start..start + kernel_len);

        let user_start = align_up_saturating(start + kernel_len, USER_POOL_MIN_REGION_BYTES);
        let user = (user_start < end && end - user_start >= USER_POOL_MIN_REGION_BYTES)
            .then_some(user_start..end);

        RegionShares { kernel, user }
    }

    /// Bytes the kernel heap's boot share is still short of.
    ///
    /// Non-zero after the whole map has been walked means the machine
    /// is smaller than the kernel's floor, which is a machine the
    /// kernel cannot run on.
    pub const fn kernel_owed_bytes(self) -> usize {
        self.kernel_owed_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a QEMU `-m 2G` guest's memory map actually came to on
    /// x86-64 once firmware had taken its share (run 33943692491), and
    /// the same machine with three times the memory.
    const TWO_GIB_USABLE: usize = 1_977_962_496;
    const SIX_GIB_USABLE: usize = 3 * TWO_GIB_USABLE;

    #[test]
    fn the_user_pool_scales_with_the_machine() {
        let small = BootMemoryPlan::for_usable_bytes(TWO_GIB_USABLE);
        let large = BootMemoryPlan::for_usable_bytes(SIX_GIB_USABLE);

        assert!(
            large.user_pool_bytes() > small.user_pool_bytes(),
            "a bigger machine produced a smaller user pool: {} then {}",
            small.user_pool_bytes(),
            large.user_pool_bytes()
        );
        // Three times the machine is three times the pool, to within
        // the bootstrap slice: that slice is a floor the kernel keeps
        // off the top once, not a share, so it is the only part of the
        // machine that does not reach the guest — and its weight falls
        // as the guest grows.
        let tripled = small.user_pool_bytes() * 3;
        assert!(
            (tripled..=tripled + 2 * KERNEL_HEAP_BOOTSTRAP_BYTES)
                .contains(&large.user_pool_bytes()),
            "the pool did not track the machine: {} against {tripled}",
            large.user_pool_bytes()
        );
        // And the pool is the machine, not a fraction of it.
        assert!(
            small.user_pool_bytes() * 100 / small.usable_bytes >= 90,
            "the user pool took only {} of {} bytes",
            small.user_pool_bytes(),
            small.usable_bytes
        );
    }

    #[test]
    fn the_kernel_reserve_never_falls_below_its_floor() {
        for usable in [
            0,
            4096,
            KERNEL_HEAP_MIN_RESERVE_BYTES,
            64 * 1024 * 1024,
            TWO_GIB_USABLE,
            SIX_GIB_USABLE,
            64 * 1024 * 1024 * 1024,
        ] {
            let plan = BootMemoryPlan::for_usable_bytes(usable);
            assert!(
                plan.kernel_reserve_bytes >= KERNEL_HEAP_MIN_RESERVE_BYTES.min(usable),
                "a {usable}-byte machine reserved {} bytes for the kernel",
                plan.kernel_reserve_bytes
            );
            assert!(
                plan.kernel_reserve_bytes <= usable,
                "the reserve outgrew the machine it was taken from"
            );
            assert!(
                plan.kernel_boot_bytes <= usable / KERNEL_HEAP_MAX_BOOT_FRACTION,
                "the kernel heap's boot share crowded out the user pool"
            );
        }
    }

    #[test]
    fn the_reserve_grows_with_the_machine_above_the_floor() {
        let small = BootMemoryPlan::for_usable_bytes(TWO_GIB_USABLE);
        let large = BootMemoryPlan::for_usable_bytes(SIX_GIB_USABLE);
        assert_eq!(large.kernel_reserve_bytes, 3 * small.kernel_reserve_bytes);
    }

    #[test]
    fn the_kernel_share_is_taken_once_across_the_whole_map() {
        let plan = BootMemoryPlan::for_usable_bytes(TWO_GIB_USABLE);
        let mut splitter = plan.splitter();

        // A Limine-shaped map: a scrap of low memory, then the bulk.
        let low = splitter.split(0x1000, 0x1000 + 0x9_0000);
        let bulk = splitter.split(0x10_0000, 0x10_0000 + TWO_GIB_USABLE - 0x9_0000);

        let kernel_bytes = [&low, &bulk]
            .into_iter()
            .filter_map(|shares| shares.kernel.clone())
            .map(|range| range.end - range.start)
            .sum::<usize>();
        assert_eq!(
            kernel_bytes,
            plan.kernel_boot_bytes.min(TWO_GIB_USABLE),
            "the kernel heap took a share of every region instead of one share of the map"
        );
        assert_eq!(splitter.kernel_owed_bytes(), 0);

        let user = bulk.user.expect("the bulk region has a user share");
        assert!(
            user.start >= 0x10_0000 + plan.kernel_boot_bytes - 0x9_0000,
            "the user share started inside the kernel's"
        );
        assert!(user.start.is_multiple_of(USER_POOL_MIN_REGION_BYTES));
    }

    #[test]
    fn a_region_too_small_to_matter_goes_nowhere() {
        let mut splitter = BootRegionSplitter {
            kernel_owed_bytes: 0,
        };
        for len in [0_usize, 4096, USER_POOL_MIN_REGION_BYTES - 1] {
            let shares = splitter.split(0x4000_0000, 0x4000_0000 + len);
            assert_eq!(shares.kernel, None);
            assert_eq!(shares.user, None, "a {len}-byte region reached the pool");
        }
    }

    #[test]
    fn no_share_ever_reaches_past_the_region_it_came_from() {
        for offset in [0_usize, 4096, 2 * 1024 * 1024, 37 * 1024 * 1024] {
            for len in [
                8 * 1024 * 1024,
                100 * 1024 * 1024,
                1024 * 1024 * 1024,
                3 * 1024 * 1024 * 1024,
            ] {
                let start = 0x4000_0000 + offset;
                let end = start + len;
                let mut splitter = BootMemoryPlan::for_usable_bytes(len).splitter();
                let shares = splitter.split(start, end);
                if let Some(kernel) = &shares.kernel {
                    assert!(kernel.start == start && kernel.end <= end);
                }
                if let Some(user) = &shares.user {
                    assert!(user.start >= start && user.end == end);
                    assert!(
                        shares
                            .kernel
                            .as_ref()
                            .is_none_or(|kernel| kernel.end <= user.start),
                        "the two shares overlapped"
                    );
                }
            }
        }
    }
}
