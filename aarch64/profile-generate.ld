/* Placement of the LLVM instrumentation an aarch64 kernel built with
 * `-C profile-generate` carries (docs/pgo.md).
 *
 * This fragment is added with a second `-T` only by the `profile-generate`
 * build, so a plain kernel's image, symbol table and segments are exactly
 * what `aarch64/linker.ld` alone produces.
 *
 * The sections have to be placed rather than left as orphans: `linker.ld`
 * names its own segments, and an orphan writable section can land outside
 * the PT_LOAD the loader zero-fills, which would leave the counters and the
 * per-function records reading as whatever was in memory at boot. The
 * `__start_`/`__stop_` pairs are defined here rather than left to LLD so
 * that `__llvm_prf_bits` and `__llvm_prf_vnds`, which the kernel's build
 * emits no input for, still have bounds instead of undefined symbols.
 *
 * KEEP, because the sections are only reachable through these symbols and
 * `--gc-sections` runs on this link.
 */

SECTIONS
{
    .llvm_prf : ALIGN(8)
    {
        __start___llvm_prf_data = .;
        KEEP(*(__llvm_prf_data))
        __stop___llvm_prf_data = .;

        . = ALIGN(8);
        __start___llvm_prf_names = .;
        KEEP(*(__llvm_prf_names))
        __stop___llvm_prf_names = .;

        . = ALIGN(8);
        __start___llvm_prf_vnds = .;
        KEEP(*(__llvm_prf_vnds))
        __stop___llvm_prf_vnds = .;

        . = ALIGN(8);
        __start___llvm_prf_bits = .;
        KEEP(*(__llvm_prf_bits))
        __stop___llvm_prf_bits = .;

        . = ALIGN(8);
        __start___llvm_prf_cnts = .;
        KEEP(*(__llvm_prf_cnts))
        __stop___llvm_prf_cnts = .;
    } :data
} INSERT AFTER .data;
