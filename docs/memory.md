# Guest memory and the two allocation domains

Helios keeps kernel memory and user memory in separate ownership
domains: the kernel heap funds the kernel's own structures and a kernel
out-of-memory is fatal, while the user pool funds wasm linear memories
and a user out-of-memory kills one instance and reclaims it. This page
states how the memory a guest boots with is divided between them, and
what that division means for how many instances a guest can hold.

The policy itself lives in one place, `kernel/src/memory/policy.rs`, and
every backend reaches it through the same call. `riscv/`, `x86/`,
`aarch64/` and `hosted/` each hand `helios_kernel::prime_bootstrap_allocator`
the usable regions of their boot memory map and nothing else; no backend
decides how much memory either domain gets.

## The policy

**All usable memory is user pool.** The kernel heap is seeded at boot
with a boot share and takes the rest of what it needs out of the user
pool at run time:

| Quantity | Value | What it is |
| --- | --- | --- |
| usable bytes | whatever the boot memory map describes | the machine |
| kernel reserve | `max(32 MiB, usable / 16)` | free kernel heap a user grow may never dip into |
| kernel boot share | `min(kernel reserve + 16 MiB, usable / 2)` | what the kernel heap starts with, taken off the front of the map once |
| user pool | `usable - kernel boot share` | seeded with everything else |
| kernel growth chunk | 64 MiB | what the kernel heap takes out of the pool when it needs more |
| task arena, per processor | `max(1 MiB, usable / 256)` | one processor's executor task arena, taken from the kernel heap when the executor is built |

The task arena is the executor's, not the heap's: it holds the futures
of live tasks, and a live instance costs its processor's arena about
8.5 KiB — an 8 KiB launch task and a 256-byte phase heartbeat — against
the ~12.5 MiB it costs the machine. A 256th of the machine per processor
is therefore about six times the arena the machine's memory can fund
instances for, so what a processor can hold is the guest's memory and
not a constant (#159). The executor rounds the share down to a whole
number of its largest block class (256 KiB) and keeps one of those
blocks as the kernel reserve, so the kernel can place a task of any size
it spawns however full the instance share is. Bytes inside an arena are
fungible: blocks are buddy-split and buddy-merged over 64-byte granules,
so a freed 8 KiB block serves 256-byte spawns and free buddies coalesce
(#142).

The kernel reserve is derived from the boot memory map once and never
moves afterwards. It cannot be a share of the kernel heap, because the
kernel heap's own size is demand-driven now: a floor expressed against a
total that moves is a floor that moves with it.

The transfer is one-way. When both domains want the last frame the
kernel takes it, because a kernel OOM ends the guest and a user OOM ends
one instance. Memory an instance frees goes back to the pool; memory the
kernel heap has borrowed stays with the kernel heap.

The guest sees one number for both domains. `helios:system/stats`
reports the machine's usable bytes and the machine's free bytes — the
kernel heap's free space plus the pool's — because with the kernel heap
funding itself out of the pool, the kernel heap's own size is not a
footprint anyone can reason about. `procbench`'s
`memory_per_instance_bytes` therefore measures what an instance costs the
machine.

The boot log states the policy as the kernel applied it:

```
Memory policy usable_bytes=… kernel_heap_bytes=… kernel_reserve_bytes=… kernel_growth_chunk_bytes=…
```

## Why the split is not a fraction

It used to be. The kernel heap kept a quarter of every boot region and
the user pool took the three quarters left, so both shares grew with the
guest — and the density workload still could not place 100 instances on
a guest with a gigabyte and a half free.

Run 33943692491, job `bench-x86-64-linux`, on a 2 GiB guest:

```
User memory pool total_bytes=1429364736 available_bytes=1429364736
instance-startup-1: memory_per_instance_bytes = 8464992
... exceeds its memory budget: available=132608808 of 548597760 reserved=137149440
```

The refusal is the kernel heap, not the pool: 523.2 MiB of it, 130.8 MiB
held back as its reserve, 392 MiB to spend, and about 8.1 MiB spent per
live instance. That is 46 instances, and the run was refused at the 46th
while the user pool — which funds only the ~4.4 MiB of linear memory each
instance holds — was essentially untouched.

A fixed ratio cannot be right, because the ratio a workload needs is the
workload's and not the machine's. What a static partition guarantees is
that one domain runs out with the other's share stranded, which is what
1.4 GiB of free pool at the moment of the refusal means. Demand decides
the split now, and the only numbers left are a floor and a granularity.

## What a guest can hold

An instance of `/bin/hello` costs the machine about 12.5 MiB: 8.1 MiB of
kernel-side structures and 4.4 MiB of linear memory, both from the same
machine. So the instance ceiling is roughly

```
instances ≈ (usable bytes − kernel boot share − kernel baseline) / 12.5 MiB
```

and it is a property of the guest's memory rather than of a budget. The
hosted test `memory_policy_tests` places instances against a real
`UserMemoryPool` until it refuses one, and records 140 on the 2 GiB
guest's memory map and 422 on three times it.

Memory is no longer what stops the density workload, and neither is the
executor. On run
[33969418797](https://github.com/lexoliu/helios/actions/runs/33969418797)
`instance-startup-100` reached instance 104 with no `ProgramOutOfMemory`
and no OOM-killer line anywhere in the boot, and was refused by the
executor's fixed 768 KiB instance task share instead (#159) — a constant
the guest's memory did not move. The arena is a share of the machine
now, so on the same 2 GiB guest (1,977,962,496 usable) it is 7.25 MiB
per processor once it is rounded to whole 256 KiB blocks, of which 7 MiB
is the instance share: 896 launch tasks, against the ~140 instances the
guest's memory can fund. `instance-startup-100` is back in
the `bench-x86-64-linux` gating set.

`instance-startup-500` stays out of it, and for the machine rather than
the arena: 500 instances want about 6.1 GiB of machine before the
kernel's own baseline, and the lane's guest has 2 GiB. It needs a bigger
guest to be measured, not a bigger arena.

## The kernel heap's per-processor front

The kernel heap is one buddy allocator behind one `IrqSafeMutex`
(`kernel/src/lib.rs`), and that lock is the machine's hottest word: every
processor's executor, network service and component host allocate on it
at once, and the buddy allocator behind it walks a free list on every
free looking for the block's buddy, at every class it merges up through.

`kernel/src/memory/magazine.rs` puts a per-processor front in front of
it. Each processor keeps a magazine of recently freed blocks per small
size class and serves its own allocations out of it; the shared heap
sees a batch of sixteen instead of sixteen separate allocations. The
size classes are buddy orders from 8 bytes to 512 bytes, which is where
the measured distribution ends: over `hostcall-loop`, `sched-tasks`,
`spawn-wait`, `instance-startup-100` and `pipe-pingpong`, 99.85% of the
7.6 million kernel allocations are 512 bytes or smaller and 72% of them
land in the single 65–128 byte class.

Two properties make it safe and cheap:

- **Blocks are fungible.** A class-`k` block is `1 << k` bytes at
  `1 << k` alignment, which is exactly what the buddy heap serves for
  that class, and the front asks the heap for that layout rather than
  the caller's. So a block one processor allocated is a block any
  processor may later hand out of its own magazine, there is no owner to
  return a block to, and the heap's own byte accounting stays symmetric
  between the allocation and the free.
- **The metadata is owner-only.** The list heads are plain pointers with
  no lock and no atomic on them, reached only from the owning processor
  with local interrupts masked — the same mask the heap lock already
  takes, and for the same reason: the processor's own interrupt handler
  allocates. What another processor reads (the depths, the magazine's
  own hit and drain counts, and the allocation counters) is atomic. The
  depths and the magazine counts are stepped inside the masked region
  the magazine operation already holds, so they take a plain load, add
  and store; the allocation counters are stepped on paths that hold no
  such region and take a relaxed `fetch_add` instead of opening one.
  Either way the line belongs to one processor, so no kernel allocation
  performs a *contended* atomic any more — which is the property that
  mattered, and the reason the counters are per processor at all.

  Masking for the allocation counters was measured and reverted. A mask
  is two calls through the backend's linkage (`_helios_local_interrupt_
  mask` and its restore are `#[no_mangle]` symbols a backend defines, so
  they never inline into the allocator) plus the architecture's
  interrupt-state write, and a counter is stepped on *every* kernel
  allocation, magazine hit or not. Paying that to avoid an uncontended
  L1-exclusive atomic cost `instance-startup-100` 2.6% (#169), which the
  shared heap the front no longer reaches did not pay back: on that
  workload the heap lock was never contended enough.

The array is sized at bring-up, out of the same
`prime_bootstrap_allocator` call that fills the heap, by the processor
count the backend reports. Before that the heap serves every allocation
directly; afterwards a processor naming a slot the array does not hold
panics rather than borrowing another processor's magazine. A processor
that has not installed its per-processor runtime yet names no slot at
all — `helios_hal::cpu::current_processor_slot` answers `None`, which
every backend implements out of the same processor-local register its
`Cpu::current_processor` reads — and takes the direct path.

`HeapStats::magazine_cached_bytes` reports what the magazines are
holding. It is inside `allocated_bytes` as well, because from the buddy
heap's point of view those blocks are out on loan; the field says how
much of that is a cache rather than a live kernel object. A refill never
takes the heap below its reserve, so the cache cannot eat the memory the
kernel keeps for itself.

## Related

- `docs/benchmarks.md` — the density workloads and what they report.
- AGENTS.md §3 — the domain split and the rules against masking a memory
  bug with a bigger budget.
