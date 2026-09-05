//! What the boot memory policy means for instance density.
//!
//! `helios_kernel::BootMemoryPlan` is unit-tested in the kernel for the
//! arithmetic. This asks the question the arithmetic exists to answer:
//! how many live instances a machine of a given size can hold before
//! the pool refuses one, and whether that number grows with the
//! machine.
//!
//! Hosted-side for the same reason as `pmm_tests` and
//! `oom_killer_tests`: the kernel crate's lib-test compilation has
//! pre-existing breakage in unrelated WASI binding code, and
//! `UserMemoryPool` behaves identically wherever its memory comes from.

#![cfg(test)]

use std::alloc::{Layout, alloc};

use helios_kernel::{BootMemoryPlan, UserMemoryPool};

/// What one live instance costs each domain, measured on the density
/// workload rather than guessed.
///
/// Run 33943692491, job `bench-x86-64-linux`, on a 2 GiB guest:
/// `instance-startup-1` reported `memory_per_instance_bytes = 8464992`
/// against the kernel heap, and the OOM killer's victim line reported
/// `victim_memory_bytes = 4653056` of wasm linear memory for a
/// `/bin/procbench` child. Both come out of the same machine now — the
/// kernel heap funds itself from the user pool — so an instance costs
/// the pool both.
const KERNEL_BYTES_PER_INSTANCE: usize = 8_464_992;
const USER_BYTES_PER_INSTANCE: usize = 4_653_056;

/// The instance count the density workload claims (#28, #130).
const DENSITY_WORKLOAD_INSTANCES: usize = 100;

/// Page-aligned host memory standing in for the machine's usable
/// frames, leaked for the life of the test process.
fn leak_machine_memory(bytes: usize) -> (usize, usize) {
    let layout = Layout::from_size_align(bytes, 4096).expect("a page-aligned machine");
    let start = unsafe { alloc(layout) };
    assert!(!start.is_null(), "the host would not lend {bytes} bytes");
    let start = start as usize;
    (start, start + bytes)
}

/// Spawns instances against a machine of `usable_bytes` until the pool
/// refuses one, and reports how many were placed.
///
/// Each instance takes both of its costs from the pool, because the
/// boot memory policy leaves the machine's memory there and lets the
/// kernel heap draw on it: a spawn stops when the *machine* is full,
/// not when either domain's boot share is.
fn instances_until_refusal(usable_bytes: usize) -> usize {
    let plan = BootMemoryPlan::for_usable_bytes(usable_bytes);
    let pool = UserMemoryPool::empty();
    pool.configure_processors(1);
    let (start, _end) = leak_machine_memory(usable_bytes);
    pool.add_region(start + plan.kernel_boot_bytes, start + usable_bytes);

    let mut placed = 0;
    while take(&pool, USER_BYTES_PER_INSTANCE) && take(&pool, KERNEL_BYTES_PER_INSTANCE) {
        placed += 1;
        // A runaway would allocate the host's memory, not the pool's.
        assert!(placed < 100_000, "the pool never refused an instance");
    }
    placed
}

/// Takes `bytes` out of the pool and keeps them, the way a live
/// instance does.
///
/// Split into descending powers of two rather than asked for in one
/// piece, because that is the shape both domains really take it in:
/// the kernel heap draws whole buddy blocks and hands out allocations
/// inside them, and a wasm linear memory is committed a frame at a
/// time. Asking the buddy for one 8,464,992-byte block would round it
/// to 16 MiB and measure the rounding instead of the footprint.
fn take(pool: &UserMemoryPool, bytes: usize) -> bool {
    const PAGE: usize = 4096;
    let mut remaining = bytes.next_multiple_of(PAGE);
    while remaining >= PAGE {
        let block = prev_power_of_two(remaining);
        let layout = Layout::from_size_align(block, PAGE).expect("a page-aligned block");
        if pool.allocate_zeroed(layout).is_err() {
            return false;
        }
        remaining -= block;
    }
    true
}

fn prev_power_of_two(value: usize) -> usize {
    1 << (usize::BITS - 1 - value.leading_zeros())
}

/// The size a QEMU `-m 2G` guest's memory map actually came to on
/// x86-64 once firmware had taken its share (run 33943692491).
const TWO_GIB_USABLE: usize = 1_977_962_496;

#[test]
fn a_two_gigabyte_guest_holds_the_density_workload() {
    let placed = instances_until_refusal(TWO_GIB_USABLE);
    assert!(
        placed >= DENSITY_WORKLOAD_INSTANCES,
        "a 2 GiB guest placed {placed} instances, short of the {DENSITY_WORKLOAD_INSTANCES} \
         the density workload holds"
    );
}

#[test]
fn instance_density_grows_with_the_machine() {
    let small = instances_until_refusal(TWO_GIB_USABLE);
    let large = instances_until_refusal(3 * TWO_GIB_USABLE);
    assert!(
        large > small,
        "three times the machine placed {large} instances against {small}"
    );
    // Not exactly three times: the kernel keeps one bootstrap slice
    // off the top however big the machine is, and the pool's last
    // blocks are too fragmented to place one more instance. What
    // matters is that the machine's memory reaches the guest — a pool
    // that were a fixed budget would return the same count twice.
    assert!(
        large >= small * 5 / 2,
        "density did not track the machine: {small} then {large}"
    );
}
