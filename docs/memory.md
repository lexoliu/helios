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

## What allocates each domain

The kernel heap is a TLSF allocator, `rlsf`, behind the one lock an
interrupt handler is allowed to take (`kernel/src/memory/irq_safe.rs`).
TLSF answers both an allocation and a free in constant time: a request
is a bitmap search over two levels of segregated free lists, and a free
is a bitmap update and a few pointer writes, whatever the heap is
holding. The kernel builds it with 32 first-level classes and 32
second-level subdivisions of each, so the lists span one granule (32
bytes) to 128 GiB — `rlsf` divides a pool region larger than the top
class into several pools, and the kernel heap's boot share can be half
of the machine — and a block a search settles on is at most 3.1% larger
than the request it rounded up to.

It was a buddy allocator until #246. That allocator found a freed
block's buddy by walking the block's size class from the head of the
free list, and walked the list whole whenever the buddy was absent,
which is the ordinary case in a mass free. Tearing down a hundred
instances is about forty thousand frees, and `instance-startup-100` paid
68 ms of a 123 ms run in teardown alone. The walk is also why a
per-processor allocation cache could not sit in front of that heap
(#169): every block a cache held was a block whose buddy arrived, found
nothing to merge with, and stayed on the list for every later search to
walk past.

TLSF keeps no running totals, so the kernel keeps the two the stats
report — every byte the heap owns, and what live allocations hold of it
— in the same structure as the heap and therefore under the same lock as
the operation that moves them. What an allocation is charged is the
block `rlsf` searches for: the payload, its used-block header, the
padding an over-aligned payload needs, rounded up to a granule. It is
computed from the `Layout` alone, which is what makes the charge and the
refund the same number.

The user pool (`kernel/src/memory/user.rs`) and the kernel frame
allocator (`kernel/src/memory/pmm.rs`) are still buddy heaps, each with
a lock-free per-processor frame slab in front that keeps single-frame
churn off the lock. Their multi-frame returns go to the same `dealloc`,
so they carry the same walk on the same workloads; #248 and #249 track
reading that off the bench lane on top of the new kernel heap.

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

## Related

- `docs/benchmarks.md` — the density workloads and what they report.
- AGENTS.md §3 — the domain split and the rules against masking a memory
  bug with a bigger budget.
