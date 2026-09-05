/* Placement of the LLVM instrumentation a riscv64 kernel built with
 * `-C profile-generate` carries (docs/pgo.md).
 *
 * This fragment is added with a third `-T` only by the `profile-generate`
 * build, so a plain kernel's image is exactly what `memory.x` and `link.x`
 * alone produce.
 *
 * `INSERT AFTER .data` is what makes the sections part of the image the
 * boot code copies: `link.x` closes `__edata` after every section inserted
 * there, and `_start` copies `__sdata .. __edata` from `__sidata`. Left as
 * orphans they would land outside that window, and the per-function records
 * — which carry link-time relative pointers, not zeroes — would never
 * reach RAM.
 *
 * The `__start_`/`__stop_` pairs are defined here rather than left to LLD
 * so that `__llvm_prf_bits` and `__llvm_prf_vnds`, which the kernel's build
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
    } > REGION_DATA AT > REGION_RODATA
} INSERT AFTER .data;
