# Wasmtime Dependency

Helios uses the sibling Wasmtime checkout through the workspace path dependency
at `../wasmtime/crates/wasmtime`.

Current required Wasmtime commit:

```text
7b464276ee62af6d3e27d8c7dd122aebf97b0c2d
```

This commit makes the generic pooling allocator's async fiber stack pool keep
bounded warm stacks on non-Unix targets. The AArch64/HVF `quickjs-loop` profile
showed one 8 MiB async stack allocation per run before this change; with this
Wasmtime commit, the same workload keeps one warm stack and reuses it across
subsequent runs.
