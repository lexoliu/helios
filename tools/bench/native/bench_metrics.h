/*
 * Measurement helpers shared by the Linux-native benchmark counterparts.
 *
 * The output contract mirrors tools/wasi-apps/bench-metrics: the primary
 * result is `<workload>:<value>` and every secondary measurement is a
 * `bench.<name>=<number>` line, so the Linux runner and the inspector parse
 * the two sides into the same metric names.
 */
#ifndef HELIOS_BENCH_METRICS_H
#define HELIOS_BENCH_METRICS_H

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

struct latency_samples {
    uint64_t *nanos;
    size_t len;
    size_t capacity;
};

static inline void die(const char *what) {
    if (errno != 0) {
        fprintf(stderr, "%s: %s\n", what, strerror(errno));
    } else {
        fprintf(stderr, "%s\n", what);
    }
    exit(1);
}

static inline uint64_t monotonic_nanos(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        die("clock_gettime");
    }
    return (uint64_t)now.tv_sec * 1000000000ull + (uint64_t)now.tv_nsec;
}

static inline uint64_t parse_count(const char *raw, const char *usage) {
    char *end = NULL;
    errno = 0;
    unsigned long long value = strtoull(raw, &end, 10);
    if (errno != 0 || end == raw || *end != '\0' || value == 0) {
        fprintf(stderr, "%s\n", usage);
        exit(2);
    }
    return (uint64_t)value;
}

static inline void samples_init(struct latency_samples *samples, size_t capacity) {
    samples->nanos = calloc(capacity, sizeof(uint64_t));
    if (samples->nanos == NULL) {
        die("calloc");
    }
    samples->len = 0;
    samples->capacity = capacity;
}

static inline void samples_record(struct latency_samples *samples, uint64_t nanos) {
    if (samples->len == samples->capacity) {
        errno = 0;
        die("latency sample buffer overflowed");
    }
    samples->nanos[samples->len++] = nanos;
}

static inline int compare_u64(const void *left, const void *right) {
    uint64_t a = *(const uint64_t *)left;
    uint64_t b = *(const uint64_t *)right;
    return (a > b) - (a < b);
}

/* Nearest-rank percentile over the sorted samples. */
static inline uint64_t samples_percentile(const struct latency_samples *samples, unsigned percent) {
    uint64_t rank = (samples->len * (uint64_t)percent + 99) / 100;
    if (rank == 0) {
        rank = 1;
    }
    return samples->nanos[rank - 1];
}

static inline void report_metric_micros(const char *prefix, const char *suffix, uint64_t nanos) {
    printf("bench.%s_%s=%llu.%03llu\n", prefix, suffix,
           (unsigned long long)(nanos / 1000), (unsigned long long)(nanos % 1000));
}

/* Reports p50, p99, max and mean in microseconds, like LatencySamples::report. */
static inline void samples_report(struct latency_samples *samples, const char *prefix) {
    if (samples->len == 0) {
        errno = 0;
        die("no latency samples were collected");
    }
    qsort(samples->nanos, samples->len, sizeof(uint64_t), compare_u64);
    report_metric_micros(prefix, "p50_us", samples_percentile(samples, 50));
    report_metric_micros(prefix, "p99_us", samples_percentile(samples, 99));
    report_metric_micros(prefix, "max_us", samples_percentile(samples, 100));
    unsigned __int128 total = 0;
    for (size_t index = 0; index < samples->len; index++) {
        total += samples->nanos[index];
    }
    report_metric_micros(prefix, "mean_us", (uint64_t)(total / samples->len));
}

static inline void report_mib_per_second(const char *name, uint64_t bytes, uint64_t nanos) {
    if (nanos == 0) {
        errno = 0;
        die("throughput needs a non-zero elapsed time");
    }
    printf("bench.%s=%.3f\n", name, (double)bytes / (1024.0 * 1024.0) / ((double)nanos / 1e9));
}

#endif
