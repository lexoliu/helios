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

`instance-startup-500` is above the 2 GiB ceiling and stays out of the
`bench-x86-64-linux` lane's gating set: 500 instances want about 6.1 GiB
of machine before the kernel's own baseline, so it needs a guest of 8 GiB
to be measured rather than a different pool. `instance-startup-100` fits
and is measured.

## Related

- `docs/benchmarks.md` — the density workloads and what they report.
- AGENTS.md §3 — the domain split and the rules against masking a memory
  bug with a bigger budget.
