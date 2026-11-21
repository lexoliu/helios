/* SMP threads, futexes, robust lists, scheduling and thread-group exit
 * (#1032). Driven by check-pthreads.sh on Linux (in build-fixtures.sh) and
 * on Helios (in smoke-x86-64 and smoke-aarch64 with --smp 4). */
#include "check.h"

#include <linux/futex.h>
#include <linux/sched.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <time.h>

#define WORKER_COUNT 4

static _Thread_local char tls_greeting[64] = "中文 日本語 한국어";
static _Thread_local uint64_t tls_zero_counter;

static atomic_int key_destructor_runs;

static void tls_key_destructor(void *value) {
    int delta = (int)(intptr_t)value;
    atomic_fetch_add_explicit(&key_destructor_runs, delta, memory_order_acq_rel);
}

static void check_identity_and_scheduling(int expect_four_cpus) {
    long pid = SYS(SYS_getpid);
    long tid = SYS(SYS_gettid);
    CHECK(pid > 0);
    CHECK(tid == pid);
    CHECK(getpid() == pid);

    cpu_set_t mask;
    CPU_ZERO(&mask);
    long raw_bytes = SYS(SYS_sched_getaffinity, 0, sizeof(mask), &mask);
    CHECK(raw_bytes >= 8 && (raw_bytes % 8) == 0);
    CHECK(sched_getaffinity(0, sizeof(mask), &mask) == 0);
    int cpus = CPU_COUNT(&mask);
    CHECK(cpus >= 1);
    if (expect_four_cpus) {
        CHECK(cpus == 4);
    }
    CHECK(SYS(SYS_sched_getaffinity, tid, sizeof(mask), &mask) == raw_bytes);
    CHECK(SYS(SYS_sched_getaffinity, -1, sizeof(mask), &mask) == -ESRCH);
    CHECK(SYS(SYS_sched_getaffinity, 0x3fffffff, sizeof(mask), &mask) == -ESRCH);
    CHECK(SYS(SYS_sched_getaffinity, 0, 4, &mask) == -EINVAL);
    CHECK(SYS(SYS_sched_getaffinity, -1, 4, &mask) == -EINVAL);
    CHECK(SYS(SYS_sched_getaffinity, 0, sizeof(mask), UNMAPPED) == -EFAULT);

    CHECK(sched_yield() == 0);
    CHECK(SYS(SYS_sched_yield) == 0);

    struct timespec before, after, req, rem;
    CHECK(clock_gettime(CLOCK_MONOTONIC, &before) == 0);
    req.tv_sec = 0;
    req.tv_nsec = 2 * 1000 * 1000;
    rem.tv_sec = 99;
    rem.tv_nsec = 99;
    CHECK(SYS(SYS_nanosleep, &req, &rem) == 0);
    CHECK(clock_gettime(CLOCK_MONOTONIC, &after) == 0);
    int64_t elapsed = (int64_t)(after.tv_sec - before.tv_sec) * 1000000000LL
        + (after.tv_nsec - before.tv_nsec);
    CHECK(elapsed >= 1500000LL);

    struct timespec bad = { .tv_sec = 0, .tv_nsec = 1000000000L };
    CHECK(SYS(SYS_nanosleep, &bad, NULL) == -EINVAL);
    bad.tv_sec = -1;
    bad.tv_nsec = 0;
    CHECK(SYS(SYS_nanosleep, &bad, NULL) == -EINVAL);
    CHECK(SYS(SYS_nanosleep, UNMAPPED, NULL) == -EFAULT);

    req.tv_sec = 0;
    req.tv_nsec = 1000 * 1000;
    CHECK(SYS(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, &req, NULL) == 0);
    CHECK(SYS(SYS_clock_nanosleep, CLOCK_REALTIME, 0, &req, NULL) == 0);

    CHECK(clock_gettime(CLOCK_MONOTONIC, &before) == 0);
    struct timespec target = before;
    target.tv_nsec += 1000 * 1000;
    if (target.tv_nsec >= 1000000000L) {
        target.tv_sec += 1;
        target.tv_nsec -= 1000000000L;
    }
    CHECK(SYS(SYS_clock_nanosleep, CLOCK_MONOTONIC, TIMER_ABSTIME, &target, &rem) == 0);
    CHECK(SYS(SYS_clock_nanosleep, CLOCK_THREAD_CPUTIME_ID, 0, &req, NULL) == -EINVAL);
    CHECK(SYS(SYS_clock_nanosleep, CLOCK_MONOTONIC_RAW, 0, &req, NULL) == -EOPNOTSUPP);
    CHECK(SYS(SYS_clock_nanosleep, 999, 0, &req, NULL) == -EINVAL);
}

struct parallel_arg {
    long expected_pid;
    int index;
    pthread_key_t key;
    atomic_int *arrived;
    atomic_int *release;
    long recorded_tid;
};

static void *parallel_worker(void *raw) {
    struct parallel_arg *arg = raw;
    CHECK(SYS(SYS_getpid) == arg->expected_pid);
    long tid = SYS(SYS_gettid);
    CHECK(tid > 0 && tid != arg->expected_pid);
    arg->recorded_tid = tid;

    /* Verify initial .tdata (with CJK) and .tbss in every spawned thread, then
     * mutate thread-locally so any shared-TLS aliasing breaks immediately. */
    CHECK(strcmp(tls_greeting, "中文 日本語 한국어") == 0);
    CHECK(tls_zero_counter == 0);
    snprintf(tls_greeting, sizeof(tls_greeting), "worker-%d: 中文/日本語/한국어", arg->index);
    tls_zero_counter = (uint64_t)(0x1000 + arg->index);
    CHECK(pthread_setspecific(arg->key, (void *)(intptr_t)(arg->index + 1)) == 0);

    /* Sense-reversing rendezvous across all 4 workers: on --smp 4 all four run
     * concurrently across processors; bounded spin + sched_yield also lets a
     * 1- or 2-vCPU host pass build-fixtures.sh. */
    if (atomic_fetch_add_explicit(arg->arrived, 1, memory_order_acq_rel) + 1 == WORKER_COUNT) {
        atomic_store_explicit(arg->release, 1, memory_order_release);
    } else {
        unsigned spins = 0;
        while (atomic_load_explicit(arg->release, memory_order_acquire) == 0) {
            if (++spins > 20000) {
                spins = 0;
                sched_yield();
            }
        }
    }

    /* Burn a small amount of user slice work so process CPU time advances. */
    volatile uint64_t acc = 0;
    for (uint64_t i = 0; i < 50000; i++) {
        acc += i ^ (uint64_t)tid;
    }
    (void)acc;

    char expected_tls[64];
    snprintf(expected_tls, sizeof(expected_tls), "worker-%d: 中文/日本語/한국어", arg->index);
    CHECK(strcmp(tls_greeting, expected_tls) == 0);
    CHECK(tls_zero_counter == (uint64_t)(0x1000 + arg->index));

    return (void *)(intptr_t)(100 + arg->index);
}

static void check_create_join_and_tls(void) {
    pthread_key_t key;
    atomic_store(&key_destructor_runs, 0);
    CHECK(pthread_key_create(&key, tls_key_destructor) == 0);

    long main_pid = SYS(SYS_getpid);
    atomic_int arrived = 0;
    atomic_int release = 0;
    pthread_t threads[WORKER_COUNT];
    struct parallel_arg args[WORKER_COUNT];

    for (int i = 0; i < WORKER_COUNT; i++) {
        args[i] = (struct parallel_arg){
            .expected_pid = main_pid,
            .index = i,
            .key = key,
            .arrived = &arrived,
            .release = &release,
            .recorded_tid = 0,
        };
        CHECK(pthread_create(&threads[i], NULL, parallel_worker, &args[i]) == 0);
    }

    for (int i = 0; i < WORKER_COUNT; i++) {
        void *retval = NULL;
        CHECK(pthread_join(threads[i], &retval) == 0);
        CHECK((intptr_t)retval == 100 + i);
    }

    for (int i = 0; i < WORKER_COUNT; i++) {
        for (int j = i + 1; j < WORKER_COUNT; j++) {
            CHECK(args[i].recorded_tid != args[j].recorded_tid);
        }
    }

    /* Main's own TLS remained untouched, and all 4 key destructors ran
     * (1 + 2 + 3 + 4 = 10). */
    CHECK(strcmp(tls_greeting, "中文 日本語 한국어") == 0);
    CHECK(tls_zero_counter == 0);
    CHECK(atomic_load(&key_destructor_runs) == 10);
    CHECK(pthread_key_delete(key) == 0);

    struct timespec main_cpu, proc_cpu;
    CHECK(clock_gettime(CLOCK_THREAD_CPUTIME_ID, &main_cpu) == 0);
    CHECK(clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &proc_cpu) == 0);
    int64_t proc_ns = (int64_t)proc_cpu.tv_sec * 1000000000LL + proc_cpu.tv_nsec;
    int64_t main_ns = (int64_t)main_cpu.tv_sec * 1000000000LL + main_cpu.tv_nsec;
    CHECK(proc_ns >= main_ns);
}

struct detach_state {
    _Atomic uint32_t completed;
};

static void *detached_worker(void *raw) {
    struct detach_state *st = raw;
    atomic_fetch_add_explicit(&st->completed, 1, memory_order_acq_rel);
    SYS(SYS_futex, &st->completed, FUTEX_WAKE_PRIVATE, INT32_MAX, 0, 0, 0);
    return NULL;
}

static void check_detached_threads(void) {
    struct detach_state st = { .completed = 0 };
    pthread_attr_t attr;
    CHECK(pthread_attr_init(&attr) == 0);
    CHECK(pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED) == 0);

    for (int i = 0; i < 4; i++) {
        pthread_t th;
        CHECK(pthread_create(&th, &attr, detached_worker, &st) == 0);
    }
    CHECK(pthread_attr_destroy(&attr) == 0);

    for (int i = 0; i < 4; i++) {
        pthread_t th;
        CHECK(pthread_create(&th, NULL, detached_worker, &st) == 0);
        CHECK(pthread_detach(th) == 0);
    }

    while (1) {
        uint32_t cur = atomic_load_explicit(&st.completed, memory_order_acquire);
        if (cur == 8) {
            break;
        }
        long rc = SYS(SYS_futex, &st.completed, FUTEX_WAIT_PRIVATE, cur, NULL, 0, 0);
        CHECK(rc == 0 || rc == -EAGAIN);
    }
}

struct mutex_cond_state {
    pthread_mutex_t mtx;
    pthread_cond_t ready_cv;
    pthread_cond_t go_cv;
    int ready;
    int go;
    long counter;
};

static void *mutex_cond_worker(void *raw) {
    struct mutex_cond_state *st = raw;
    CHECK(pthread_mutex_lock(&st->mtx) == 0);
    st->ready += 1;
    if (st->ready == WORKER_COUNT) {
        CHECK(pthread_cond_signal(&st->ready_cv) == 0);
    }
    while (!st->go) {
        CHECK(pthread_cond_wait(&st->go_cv, &st->mtx) == 0);
    }
    CHECK(pthread_mutex_unlock(&st->mtx) == 0);

    for (int i = 0; i < 2000; i++) {
        CHECK(pthread_mutex_lock(&st->mtx) == 0);
        st->counter += 1;
        CHECK(pthread_mutex_unlock(&st->mtx) == 0);
    }
    return NULL;
}

static void check_mutex_and_condvar(void) {
    struct mutex_cond_state st = {
        .mtx = PTHREAD_MUTEX_INITIALIZER,
        .ready_cv = PTHREAD_COND_INITIALIZER,
        .go_cv = PTHREAD_COND_INITIALIZER,
        .ready = 0,
        .go = 0,
        .counter = 0,
    };

    pthread_t threads[WORKER_COUNT];
    for (int i = 0; i < WORKER_COUNT; i++) {
        CHECK(pthread_create(&threads[i], NULL, mutex_cond_worker, &st) == 0);
    }

    CHECK(pthread_mutex_lock(&st.mtx) == 0);
    while (st.ready < WORKER_COUNT) {
        CHECK(pthread_cond_wait(&st.ready_cv, &st.mtx) == 0);
    }
    st.go = 1;
    CHECK(pthread_cond_broadcast(&st.go_cv) == 0);
    CHECK(pthread_mutex_unlock(&st.mtx) == 0);

    for (int i = 0; i < WORKER_COUNT; i++) {
        CHECK(pthread_join(threads[i], NULL) == 0);
    }
    CHECK(st.counter == WORKER_COUNT * 2000);

    /* Recursive and errorcheck mutex types plus timed condvar wait. */
    pthread_mutexattr_t mattr;
    pthread_mutex_t rec_mtx, err_mtx;
    CHECK(pthread_mutexattr_init(&mattr) == 0);
    CHECK(pthread_mutexattr_settype(&mattr, PTHREAD_MUTEX_RECURSIVE) == 0);
    CHECK(pthread_mutex_init(&rec_mtx, &mattr) == 0);
    CHECK(pthread_mutex_lock(&rec_mtx) == 0);
    CHECK(pthread_mutex_lock(&rec_mtx) == 0);
    CHECK(pthread_mutex_unlock(&rec_mtx) == 0);
    CHECK(pthread_mutex_unlock(&rec_mtx) == 0);
    CHECK(pthread_mutex_destroy(&rec_mtx) == 0);

    CHECK(pthread_mutexattr_settype(&mattr, PTHREAD_MUTEX_ERRORCHECK) == 0);
    CHECK(pthread_mutex_init(&err_mtx, &mattr) == 0);
    CHECK(pthread_mutex_lock(&err_mtx) == 0);
    CHECK(pthread_mutex_lock(&err_mtx) == EDEADLK);
    CHECK(pthread_mutex_unlock(&err_mtx) == 0);
    CHECK(pthread_mutex_destroy(&err_mtx) == 0);
    CHECK(pthread_mutexattr_destroy(&mattr) == 0);

    struct timespec abstime;
    CHECK(clock_gettime(CLOCK_REALTIME, &abstime) == 0);
    abstime.tv_nsec += 1000 * 1000;
    if (abstime.tv_nsec >= 1000000000L) {
        abstime.tv_sec += 1;
        abstime.tv_nsec -= 1000000000L;
    }
    CHECK(pthread_mutex_lock(&st.mtx) == 0);
    CHECK(pthread_cond_timedwait(&st.go_cv, &st.mtx, &abstime) == ETIMEDOUT);
    CHECK(pthread_mutex_unlock(&st.mtx) == 0);
}

struct futex_wait_arg {
    uint32_t *word;
    int op;
    uint32_t val;
    uint32_t mask;
    atomic_int *entered;
    atomic_int *woken;
};

static void *futex_waiter_thread(void *raw) {
    struct futex_wait_arg *arg = raw;
    atomic_fetch_add_explicit(arg->entered, 1, memory_order_acq_rel);
    long rc;
    if ((arg->op & 0x7f) == FUTEX_WAIT_BITSET) {
        rc = SYS(SYS_futex, arg->word, arg->op, arg->val, NULL, NULL, arg->mask);
    } else {
        rc = SYS(SYS_futex, arg->word, arg->op, arg->val, NULL, NULL, 0);
    }
    CHECK(rc == 0);
    atomic_fetch_add_explicit(arg->woken, 1, memory_order_acq_rel);
    return NULL;
}

static void spin_briefly(void) {
    struct timespec ts = { .tv_sec = 0, .tv_nsec = 2 * 1000 * 1000 };
    nanosleep(&ts, NULL);
}

static void check_raw_futex_ops(void) {
    _Alignas(8) uint8_t raw_buf[16] = {0};
    uint32_t word1 = 7;
    uint32_t word2 = 0;
    struct timespec zero_ts = { .tv_sec = 0, .tv_nsec = 0 };

    CHECK(SYS(SYS_futex, raw_buf + 1, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0) == -EINVAL);
    CHECK(SYS(SYS_futex, (void *)4, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0) == -EFAULT);
    CHECK(SYS(SYS_futex, (void *)(~3UL), FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0) == -EFAULT);
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAIT_PRIVATE, 8, NULL, NULL, 0) == -EAGAIN);
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAIT_PRIVATE, 7, &zero_ts, NULL, 0) == -ETIMEDOUT);
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 7, NULL, NULL, 0)
          == -EINVAL);
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0)
          == -EINVAL);

    /* Shared vs private futex isolation on the same virtual address. */
    atomic_int entered = 0, woken = 0;
    struct futex_wait_arg shared_arg = {
        .word = &word1,
        .op = FUTEX_WAIT,
        .val = 7,
        .mask = 0,
        .entered = &entered,
        .woken = &woken,
    };
    pthread_t shared_th;
    CHECK(pthread_create(&shared_th, NULL, futex_waiter_thread, &shared_arg) == 0);
    while (atomic_load(&entered) < 1) {
        sched_yield();
    }
    spin_briefly();
    /* Private wake must NOT wake a shared waiter! */
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0) == 0);
    CHECK(atomic_load(&woken) == 0);
    /* Shared wake wakes the shared waiter. */
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAKE, 1, NULL, NULL, 0) == 1);
    CHECK(pthread_join(shared_th, NULL) == 0);
    CHECK(atomic_load(&woken) == 1);

    /* WAIT_BITSET / WAKE_BITSET selective wakeup. */
    entered = 0;
    atomic_int woken_b1 = 0, woken_b2 = 0;
    struct futex_wait_arg b1 = {
        .word = &word1,
        .op = FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
        .val = 7,
        .mask = 0x1,
        .entered = &entered,
        .woken = &woken_b1,
    };
    struct futex_wait_arg b2 = {
        .word = &word1,
        .op = FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
        .val = 7,
        .mask = 0x2,
        .entered = &entered,
        .woken = &woken_b2,
    };
    pthread_t th_b1, th_b2;
    CHECK(pthread_create(&th_b1, NULL, futex_waiter_thread, &b1) == 0);
    CHECK(pthread_create(&th_b2, NULL, futex_waiter_thread, &b2) == 0);
    while (atomic_load(&entered) < 2) {
        sched_yield();
    }
    spin_briefly();
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG, 2, NULL, NULL, 0x4) == 0);
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG, 2, NULL, NULL, 0x2) == 1);
    CHECK(pthread_join(th_b2, NULL) == 0);
    CHECK(atomic_load(&woken_b2) == 1);
    CHECK(atomic_load(&woken_b1) == 0);
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG, 2, NULL, NULL, 0x1) == 1);
    CHECK(pthread_join(th_b1, NULL) == 0);
    CHECK(atomic_load(&woken_b1) == 1);

    /* CMP_REQUEUE (returns woken + requeued) and REQUEUE (returns woken). */
    entered = 0;
    woken = 0;
    struct futex_wait_arg rq_args[3];
    pthread_t rq_threads[3];
    for (int i = 0; i < 3; i++) {
        rq_args[i] = (struct futex_wait_arg){
            .word = &word1,
            .op = FUTEX_WAIT_PRIVATE,
            .val = 7,
            .mask = 0,
            .entered = &entered,
            .woken = &woken,
        };
        CHECK(pthread_create(&rq_threads[i], NULL, futex_waiter_thread, &rq_args[i]) == 0);
    }
    while (atomic_load(&entered) < 3) {
        sched_yield();
    }
    spin_briefly();
    CHECK(SYS(SYS_futex, &word1, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG, 1, (void *)2, &word2, 99)
          == -EAGAIN);
    CHECK(SYS(SYS_futex, &word1, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG, 1, (void *)2, &word2, 7)
          == 3);
    while (atomic_load(&woken) < 1) {
        sched_yield();
    }
    CHECK(SYS(SYS_futex, &word1, FUTEX_WAKE_PRIVATE, 10, NULL, NULL, 0) == 0);
    CHECK(SYS(SYS_futex, &word2, FUTEX_WAKE_PRIVATE, 10, NULL, NULL, 0) == 2);
    for (int i = 0; i < 3; i++) {
        CHECK(pthread_join(rq_threads[i], NULL) == 0);
    }
    CHECK(atomic_load(&woken) == 3);

    entered = 0;
    woken = 0;
    for (int i = 0; i < 2; i++) {
        CHECK(pthread_create(&rq_threads[i], NULL, futex_waiter_thread, &rq_args[i]) == 0);
    }
    while (atomic_load(&entered) < 2) {
        sched_yield();
    }
    spin_briefly();
    /* Both FUTEX_REQUEUE and FUTEX_CMP_REQUEUE delegate to futex_requeue,
     * which returns task_count (woken + requeued = 2). */
    CHECK(SYS(SYS_futex, &word1, FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG, 1, (void *)1, &word2, 0)
          == 2);
    CHECK(SYS(SYS_futex, &word2, FUTEX_WAKE_PRIVATE, 10, NULL, NULL, 0) == 1);
    for (int i = 0; i < 2; i++) {
        CHECK(pthread_join(rq_threads[i], NULL) == 0);
    }
    CHECK(atomic_load(&woken) == 2);
}

struct robust_abrupt_state {
    pthread_mutex_t mtx;
    atomic_int locked;
    atomic_int waiter_ready;
};

static void *robust_abrupt_owner(void *raw) {
    struct robust_abrupt_state *st = raw;
    CHECK(pthread_mutex_lock(&st->mtx) == 0);
    atomic_store_explicit(&st->locked, 1, memory_order_release);
    while (atomic_load_explicit(&st->waiter_ready, memory_order_acquire) == 0) {
        sched_yield();
    }
    spin_briefly();
    /* Bypass musl's userspace pthread_exit walk so the kernel's
     * exit_robust_list marks FUTEX_OWNER_DIED and wakes the shared waiter. */
    for (;;) {
        SYS(SYS_exit, 0);
    }
}

static void *robust_abrupt_waiter(void *raw) {
    struct robust_abrupt_state *st = raw;
    while (atomic_load_explicit(&st->locked, memory_order_acquire) == 0) {
        sched_yield();
    }
    atomic_store_explicit(&st->waiter_ready, 1, memory_order_release);
    int rc = pthread_mutex_lock(&st->mtx);
    CHECK(rc == EOWNERDEAD);
    CHECK(pthread_mutex_consistent(&st->mtx) == 0);
    CHECK(pthread_mutex_unlock(&st->mtx) == 0);
    return NULL;
}

static void check_robust_lists(void) {
    void *head = NULL;
    size_t len = 0;
    CHECK(SYS(SYS_set_robust_list, &head, 8) == -EINVAL);
    CHECK(SYS(SYS_get_robust_list, -1, &head, &len) == -ESRCH);
    CHECK(SYS(SYS_get_robust_list, 0x3fffffff, &head, &len) == -ESRCH);
    CHECK(SYS(SYS_get_robust_list, 0, UNMAPPED, &len) == -EFAULT);
    CHECK(SYS(SYS_get_robust_list, 0, &head, &len) == 0);
    CHECK(len == sizeof(struct robust_list_head));

    /* 1. Normal pthread_exit robust mutex recovery. */
    pthread_mutexattr_t attr;
    CHECK(pthread_mutexattr_init(&attr) == 0);
    CHECK(pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) == 0);
    CHECK(pthread_mutexattr_setpshared(&attr, PTHREAD_PROCESS_SHARED) == 0);

    struct robust_abrupt_state st = { .locked = 0, .waiter_ready = 0 };
    CHECK(pthread_mutex_init(&st.mtx, &attr) == 0);
    CHECK(pthread_mutexattr_destroy(&attr) == 0);

    /* 2. Kernel exit_robust_list recovery on raw SYS_exit while a sibling
     * thread is blocked in pthread_mutex_lock (shared FUTEX_WAIT). */
    pthread_t owner_th, waiter_th;
    /* Detach owner_th because it terminates via raw SYS_exit without updating
     * musl's userspace detach_state; kernel CLONE_CHILD_CLEARTID still clears
     * its tid word cleanly. */
    pthread_attr_t dattr;
    CHECK(pthread_attr_init(&dattr) == 0);
    CHECK(pthread_attr_setdetachstate(&dattr, PTHREAD_CREATE_DETACHED) == 0);
    CHECK(pthread_create(&owner_th, &dattr, robust_abrupt_owner, &st) == 0);
    CHECK(pthread_attr_destroy(&dattr) == 0);

    CHECK(pthread_create(&waiter_th, NULL, robust_abrupt_waiter, &st) == 0);
    CHECK(pthread_join(waiter_th, NULL) == 0);

    CHECK(pthread_mutex_lock(&st.mtx) == 0);
    CHECK(pthread_mutex_unlock(&st.mtx) == 0);
}

static void check_clone3_validation(void) {
    size_t page_size = (size_t)sysconf(_SC_PAGESIZE);
    struct clone_args args;
    memset(&args, 0, sizeof(args));
    CHECK(SYS(SYS_clone3, &args, 32) == -EINVAL);
    CHECK(SYS(SYS_clone3, &args, page_size * 2) == -E2BIG);
    CHECK(SYS(SYS_clone3, UNMAPPED, sizeof(args)) == -EFAULT);

    /* CLONE_DETACHED (0x00400000) is rejected by clone3. */
    args.flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD
        | CLONE_SYSVSEM | 0x00400000ULL;
    CHECK(SYS(SYS_clone3, &args, sizeof(args)) == -EINVAL);

    /* Non-zero exit_signal with CLONE_THREAD is rejected with EINVAL. */
    args.flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM;
    args.exit_signal = 17;
    CHECK(SYS(SYS_clone3, &args, sizeof(args)) == -EINVAL);
}

struct shootdown_spinner_state {
    atomic_int running;
    atomic_int started;
};

static void *shootdown_spinner(void *raw) {
    struct shootdown_spinner_state *st = raw;
    size_t page_size = (size_t)sysconf(_SC_PAGESIZE);
    uint8_t *scratch = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(scratch != MAP_FAILED);
    atomic_fetch_add_explicit(&st->started, 1, memory_order_acq_rel);
    uint8_t tick = 1;
    while (atomic_load_explicit(&st->running, memory_order_relaxed)) {
        scratch[0] = tick++;
        atomic_signal_fence(memory_order_seq_cst);
    }
    CHECK(munmap(scratch, page_size) == 0);
    return NULL;
}

static void check_smp_tlb_shootdown(void) {
    struct shootdown_spinner_state st = { .running = 1, .started = 0 };
    pthread_t spinners[2];
    for (int i = 0; i < 2; i++) {
        CHECK(pthread_create(&spinners[i], NULL, shootdown_spinner, &st) == 0);
    }
    while (atomic_load_explicit(&st.started, memory_order_acquire) < 2) {
        sched_yield();
    }

    size_t page_size = (size_t)sysconf(_SC_PAGESIZE);
    for (int iter = 0; iter < 32; iter++) {
        uint8_t *pages = mmap(NULL, page_size * 2, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(pages != MAP_FAILED);
        pages[0] = 0x5a;
        pages[page_size] = 0xa5;
        CHECK(madvise(pages, page_size, MADV_DONTNEED) == 0);
        CHECK(pages[0] == 0);
        CHECK(pages[page_size] == 0xa5);
        CHECK(mprotect(pages + page_size, page_size, PROT_READ) == 0);
        CHECK(pages[page_size] == 0xa5);
        CHECK(munmap(pages, page_size * 2) == 0);
    }

    atomic_store_explicit(&st.running, 0, memory_order_release);
    for (int i = 0; i < 2; i++) {
        CHECK(pthread_join(spinners[i], NULL) == 0);
    }
}

/* Mode 2: main calls pthread_exit while a worker is still alive and finishes
 * the process with status 0. */
static void *after_main_worker(void *arg) {
    atomic_int *started = arg;
    atomic_store_explicit(started, 1, memory_order_release);
    struct timespec ts = { .tv_sec = 0, .tv_nsec = 15 * 1000 * 1000 };
    nanosleep(&ts, NULL);
    const char msg[] = "worker completed after main pthread_exit\n";
    CHECK(write(STDOUT_FILENO, msg, sizeof(msg) - 1) == (ssize_t)(sizeof(msg) - 1));
    return NULL;
}

static void run_main_pthread_exit(void) {
    static atomic_int started = 0;
    pthread_t th;
    pthread_attr_t attr;
    CHECK(pthread_attr_init(&attr) == 0);
    CHECK(pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED) == 0);
    CHECK(pthread_create(&th, &attr, after_main_worker, &started) == 0);
    CHECK(pthread_attr_destroy(&attr) == 0);
    while (atomic_load_explicit(&started, memory_order_acquire) == 0) {
        sched_yield();
    }
    pthread_exit(NULL);
}

/* Mode 3: non-main thread calls exit_group(42) while main and another sibling
 * are parked in futex(WAIT). */
static void *parked_forever_worker(void *arg) {
    uint32_t *word = arg;
    SYS(SYS_futex, word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    return NULL;
}

static void *exit_group_worker(void *arg) {
    (void)arg;
    spin_briefly();
    const char msg[] = "non-main exit_group\n";
    CHECK(write(STDOUT_FILENO, msg, sizeof(msg) - 1) == (ssize_t)(sizeof(msg) - 1));
    for (;;) {
        SYS(SYS_exit_group, 42);
    }
}

static void run_group_exit(void) {
    static uint32_t park_word = 0;
    pthread_t parked_th, exit_th;
    CHECK(pthread_create(&parked_th, NULL, parked_forever_worker, &park_word) == 0);
    CHECK(pthread_create(&exit_th, NULL, exit_group_worker, NULL) == 0);
    SYS(SYS_futex, &park_word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    _exit(99);
}

/* Mode 4: worker warms a writable TLB entry on its processor, main narrows the
 * page to PROT_READ with mprotect (shooting down the remote TLB), and the
 * worker's next store raises SIGSEGV (139). */
struct shootdown_fault_state {
    volatile uint8_t *page;
    atomic_int warmed;
    atomic_int narrowed;
};

static void *shootdown_fault_worker(void *raw) {
    struct shootdown_fault_state *st = raw;
    st->page[0] = 0x11;
    atomic_store_explicit(&st->warmed, 1, memory_order_release);
    while (atomic_load_explicit(&st->narrowed, memory_order_acquire) == 0) {
        /* Stay in user mode on this processor while main runs mprotect, so an
         * active-slice IPI / broadcast shootdown is exercised directly. */
    }
    const char msg[] = "end: sibling write after mprotect ro\n";
    CHECK(write(STDOUT_FILENO, msg, sizeof(msg) - 1) == (ssize_t)(sizeof(msg) - 1));
    st->page[0] = 0x22;
    _exit(98);
}

static void run_shootdown_fault(void) {
    size_t page_size = (size_t)sysconf(_SC_PAGESIZE);
    uint8_t *page = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(page != MAP_FAILED);

    struct shootdown_fault_state st = {
        .page = page,
        .warmed = 0,
        .narrowed = 0,
    };
    pthread_t th;
    CHECK(pthread_create(&th, NULL, shootdown_fault_worker, &st) == 0);
    while (atomic_load_explicit(&st.warmed, memory_order_acquire) == 0) {
        sched_yield();
    }
    CHECK(mprotect(page, page_size, PROT_READ) == 0);
    atomic_store_explicit(&st.narrowed, 1, memory_order_release);

    uint32_t park = 0;
    SYS(SYS_futex, &park, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    _exit(97);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "suite";
    int expect_four_cpus = argc > 2 && strcmp(argv[2], "smp") == 0;

    if (strcmp(mode, "suite") == 0) {
        check_identity_and_scheduling(expect_four_cpus);
        check_create_join_and_tls();
        check_detached_threads();
        check_mutex_and_condvar();
        check_raw_futex_ops();
        check_robust_lists();
        check_clone3_validation();
        check_smp_tlb_shootdown();
        puts("pthreads suite: ok");
        return 0;
    }
    if (strcmp(mode, "main-pthread-exit") == 0) {
        run_main_pthread_exit();
        return 0;
    }
    if (strcmp(mode, "group-exit") == 0) {
        run_group_exit();
        return 0;
    }
    if (strcmp(mode, "shootdown-fault") == 0) {
        run_shootdown_fault();
        return 0;
    }

    dprintf(2, "unknown pthreads mode: %s\n", mode);
    return 2;
}
