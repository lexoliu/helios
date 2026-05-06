#include <arm_neon.h>
#include <stdint.h>
#include <stdio.h>

int main(void) {
    const int32x4_t left = {10, 20, 30, 40};
    const int32x4_t right = {7, 0, 0, 0};
    const int32x4_t sum = vaddq_s32(left, right);
    printf("simd-lanes:%d\n", vgetq_lane_s32(sum, 0));
    return 0;
}
