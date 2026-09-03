/* Linux counterpart of programs/sched-tasks: N threads each calling
 * sched_yield() K times, timing every yield. Helios yields a cooperative
 * task to the kernel executor; Linux yields a thread to CFS. Both report
 * the distribution of one yield under a load of N runnable peers. */
#include <pthread.h>
#include <sched.h>
#include <stdio.h>

#include "bench_metrics.h"

static const char USAGE[] = "usage: sched-tasks <tasks> <yields>";

struct worker {
    pthread_t thread;
    uint64_t yields;
    struct latency_samples samples;
};

static void *run_worker(void *argument) {
    struct worker *worker = argument;
    for (uint64_t index = 0; index < worker->yields; index++) {
        uint64_t yielded = monotonic_nanos();
        if (sched_yield() != 0) {
            die("sched_yield");
        }
        samples_record(&worker->samples, monotonic_nanos() - yielded);
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "%s\n", USAGE);
        return 2;
    }
    uint64_t tasks = parse_count(argv[1], USAGE);
    uint64_t yields = parse_count(argv[2], USAGE);

    struct worker *workers = calloc((size_t)tasks, sizeof *workers);
    if (workers == NULL) {
        die("calloc");
    }
    for (uint64_t index = 0; index < tasks; index++) {
        workers[index].yields = yields;
        samples_init(&workers[index].samples, (size_t)yields);
    }

    uint64_t started = monotonic_nanos();
    for (uint64_t index = 0; index < tasks; index++) {
        int rc = pthread_create(&workers[index].thread, NULL, run_worker, &workers[index]);
        if (rc != 0) {
            errno = rc;
            die("pthread_create");
        }
    }
    struct latency_samples all;
    samples_init(&all, (size_t)(tasks * yields));
    for (uint64_t index = 0; index < tasks; index++) {
        int rc = pthread_join(workers[index].thread, NULL);
        if (rc != 0) {
            errno = rc;
            die("pthread_join");
        }
        for (size_t sample = 0; sample < workers[index].samples.len; sample++) {
            samples_record(&all, workers[index].samples.nanos[sample]);
        }
    }
    uint64_t elapsed = monotonic_nanos() - started;

    printf("sched-tasks:%llu\n", (unsigned long long)(tasks * yields));
    samples_report(&all, "switch");
    printf("bench.switches_per_s=%.0f\n", (double)(tasks * yields) / ((double)elapsed / 1e9));
    return 0;
}
