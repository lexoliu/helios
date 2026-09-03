/* Linux counterpart of tools/wasi-apps/hostcall-loop: the same number of
 * clock_gettime(CLOCK_MONOTONIC) calls, the cheapest syscall-shaped
 * operation a native process has (a vDSO call on every mainstream
 * kernel, which is the point: it is the floor Helios is compared to). */
#include <stdio.h>

#include "bench_metrics.h"

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: hostcall-loop <calls>\n");
        return 2;
    }
    uint64_t calls = parse_count(argv[1], "usage: hostcall-loop <calls>");
    uint64_t started = monotonic_nanos();
    uint64_t last = started;
    for (uint64_t index = 0; index < calls; index++) {
        uint64_t now = monotonic_nanos();
        if (now < last) {
            errno = 0;
            die("the monotonic clock went backwards");
        }
        last = now;
    }
    printf("hostcall-loop:%llu\n", (unsigned long long)calls);
    printf("bench.ns_per_call=%.2f\n", (double)(last - started) / (double)calls);
    return 0;
}
