# Wasmtime Dependency

Helios uses the sibling Wasmtime checkout through the workspace path dependency
at `../wasmtime/crates/wasmtime`.

Current required Wasmtime commit:

```text
a90057ba00125be6941d634f509fe9a4f48b9287
```

This commit includes the generic pooling allocator's bounded warm async fiber
stack reuse on non-Unix targets, and limits custom-VM anonymous memory reset to
the currently accessible linear-memory prefix. The AArch64/HVF `quickjs-loop`
profile showed one 8 MiB async stack allocation per run before stack reuse; the
custom-VM reset change then moved the profiled `quickjs-loop` median from 57 ms
to 46 ms by avoiding a full static-reservation page-table scan on Store drop.
