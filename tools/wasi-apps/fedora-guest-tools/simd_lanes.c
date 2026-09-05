// Native counterpart of the `wasm-simd-lanes` workload: one 4-lane 32-bit
// vector add, printed the same way the wasm program prints it.
//
// The compiler's own vector extension is what keeps this one program: it
// lowers to NEON on the aarch64 guest and to SSE on the x86-64 one, so
// the two lanes compare the same source built for their own machine
// rather than two hand-written intrinsic paths, and the file needs no
// architecture header — <arm_neon.h> does not exist on x86-64, which is
// what stopped the x86-64 Fedora guest from provisioning at all.
#include <stdint.h>
#include <stdio.h>

typedef int32_t int32x4 __attribute__((vector_size(16)));

int main(void) {
    const int32x4 left = {10, 20, 30, 40};
    const int32x4 right = {7, 0, 0, 0};
    const int32x4 sum = left + right;
    printf("simd-lanes:%d\n", sum[0]);
    return 0;
}
