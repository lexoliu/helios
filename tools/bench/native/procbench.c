/* Linux counterpart of programs/procbench with the same subcommands:
 *
 *   startup <n> <child> [args..]         posix_spawn n children at once,
 *                                        time to first stdout byte each,
 *                                        RSS while all are resident
 *   spawn-wait <n> <child> [args..]      spawn, drain, wait, n times
 *   pingpong <rounds> <bytes> <child>..  message round trips over pipes
 *   stream <total> <child> [args..]      push total bytes through the child
 *
 * The child is any argv, so the same binary drives native children and
 * `wasmtime run --allow-precompiled <cwasm>` children for the
 * Linux+Wasmtime column. */
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include "bench_metrics.h"

extern char **environ;

#define STREAM_CHUNK_BYTES (64 * 1024)
#define READ_CHUNK_BYTES (64 * 1024)

static const char USAGE[] =
    "usage: procbench startup <n> <child> [args..] | spawn-wait <n> <child> [args..] | "
    "pingpong <rounds> <bytes> <child> [args..] | stream <total-bytes> <child> [args..]";

struct child {
    pid_t pid;
    int stdin_fd;
    int stdout_fd;
};

/* Spawns argv with fresh pipes on stdin and stdout; stderr is inherited. */
static struct child spawn_child(char **argv) {
    int stdin_pipe[2];
    int stdout_pipe[2];
    if (pipe(stdin_pipe) != 0 || pipe(stdout_pipe) != 0) {
        die("pipe");
    }
    posix_spawn_file_actions_t actions;
    posix_spawn_file_actions_init(&actions);
    posix_spawn_file_actions_adddup2(&actions, stdin_pipe[0], STDIN_FILENO);
    posix_spawn_file_actions_adddup2(&actions, stdout_pipe[1], STDOUT_FILENO);
    posix_spawn_file_actions_addclose(&actions, stdin_pipe[0]);
    posix_spawn_file_actions_addclose(&actions, stdin_pipe[1]);
    posix_spawn_file_actions_addclose(&actions, stdout_pipe[0]);
    posix_spawn_file_actions_addclose(&actions, stdout_pipe[1]);
    struct child child;
    int rc = posix_spawn(&child.pid, argv[0], &actions, NULL, argv, environ);
    posix_spawn_file_actions_destroy(&actions);
    if (rc != 0) {
        errno = rc;
        die("posix_spawn");
    }
    close(stdin_pipe[0]);
    close(stdout_pipe[1]);
    child.stdin_fd = stdin_pipe[1];
    child.stdout_fd = stdout_pipe[0];
    return child;
}

static void wait_child(pid_t pid) {
    int status = 0;
    if (waitpid(pid, &status, 0) < 0) {
        die("waitpid");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        errno = 0;
        fprintf(stderr, "child exited with status %d\n", status);
        exit(1);
    }
}

static void write_exact(int fd, const unsigned char *data, size_t len) {
    size_t written = 0;
    while (written < len) {
        ssize_t wrote = write(fd, data + written, len - written);
        if (wrote < 0) {
            die("write to child");
        }
        written += (size_t)wrote;
    }
}

static void read_exact(int fd, unsigned char *data, size_t len) {
    size_t received = 0;
    while (received < len) {
        ssize_t got = read(fd, data + received, len - received);
        if (got < 0) {
            die("read from child");
        }
        if (got == 0) {
            errno = 0;
            die("the child closed its stdout early");
        }
        received += (size_t)got;
    }
}

static uint64_t drain(int fd) {
    static unsigned char buffer[READ_CHUNK_BYTES];
    uint64_t total = 0;
    for (;;) {
        ssize_t got = read(fd, buffer, sizeof buffer);
        if (got < 0) {
            die("read from child");
        }
        if (got == 0) {
            return total;
        }
        total += (uint64_t)got;
    }
}

/* Resident set size of one process in bytes, from /proc/<pid>/statm. */
static uint64_t resident_bytes(pid_t pid) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/statm", (int)pid);
    FILE *statm = fopen(path, "r");
    if (statm == NULL) {
        die("open /proc/<pid>/statm");
    }
    unsigned long size_pages = 0;
    unsigned long resident_pages = 0;
    if (fscanf(statm, "%lu %lu", &size_pages, &resident_pages) != 2) {
        errno = 0;
        die("parse /proc/<pid>/statm");
    }
    fclose(statm);
    return (uint64_t)resident_pages * (uint64_t)sysconf(_SC_PAGESIZE);
}

static void startup(uint64_t count, char **child_argv) {
    struct child *children = calloc((size_t)count, sizeof *children);
    uint64_t *spawned_at = calloc((size_t)count, sizeof *spawned_at);
    struct pollfd *fds = calloc((size_t)count, sizeof *fds);
    if (children == NULL || spawned_at == NULL || fds == NULL) {
        die("calloc");
    }
    struct latency_samples samples;
    samples_init(&samples, (size_t)count);

    uint64_t batch_started = monotonic_nanos();
    for (uint64_t index = 0; index < count; index++) {
        spawned_at[index] = monotonic_nanos();
        children[index] = spawn_child(child_argv);
        fds[index].fd = children[index].stdout_fd;
        fds[index].events = POLLIN;
    }
    uint64_t pending = count;
    while (pending > 0) {
        if (poll(fds, (nfds_t)count, -1) < 0) {
            die("poll");
        }
        uint64_t now = monotonic_nanos();
        for (uint64_t index = 0; index < count; index++) {
            if (fds[index].fd < 0 || (fds[index].revents & (POLLIN | POLLHUP)) == 0) {
                continue;
            }
            unsigned char first[READ_CHUNK_BYTES];
            ssize_t got = read(fds[index].fd, first, sizeof first);
            if (got < 0) {
                die("read from child");
            }
            if (got == 0) {
                errno = 0;
                die("a child closed its stdout before producing output");
            }
            samples_record(&samples, now - spawned_at[index]);
            fds[index].fd = -1;
            pending--;
        }
    }
    uint64_t batch_elapsed = monotonic_nanos() - batch_started;

    uint64_t resident_total = 0;
    for (uint64_t index = 0; index < count; index++) {
        resident_total += resident_bytes(children[index].pid);
    }
    for (uint64_t index = 0; index < count; index++) {
        close(children[index].stdin_fd);
        drain(children[index].stdout_fd);
        close(children[index].stdout_fd);
        wait_child(children[index].pid);
    }

    printf("instance-startup:%llu\n", (unsigned long long)count);
    samples_report(&samples, "first_output");
    printf("bench.batch_ms=%.3f\n", (double)batch_elapsed / 1e6);
    printf("bench.memory_per_instance_bytes=%llu\n", (unsigned long long)(resident_total / count));
    printf("bench.live_instances=%llu\n", (unsigned long long)count);
    printf("bench.instance_memory_bytes=%llu\n", (unsigned long long)(resident_total / count));
}

static void spawn_wait(uint64_t count, char **child_argv) {
    struct latency_samples samples;
    samples_init(&samples, (size_t)count);
    for (uint64_t index = 0; index < count; index++) {
        uint64_t started = monotonic_nanos();
        struct child child = spawn_child(child_argv);
        close(child.stdin_fd);
        drain(child.stdout_fd);
        close(child.stdout_fd);
        wait_child(child.pid);
        samples_record(&samples, monotonic_nanos() - started);
    }
    printf("spawn-wait:%llu\n", (unsigned long long)count);
    samples_report(&samples, "spawn_wait");
}

static void pingpong(uint64_t rounds, uint64_t bytes, char **child_argv) {
    struct child child = spawn_child(child_argv);
    unsigned char *message = malloc((size_t)bytes);
    unsigned char *reply = malloc((size_t)bytes);
    if (message == NULL || reply == NULL) {
        die("malloc");
    }
    struct latency_samples samples;
    samples_init(&samples, (size_t)rounds);
    for (uint64_t round = 0; round < rounds; round++) {
        for (uint64_t index = 0; index < bytes; index++) {
            message[index] = (unsigned char)(round + index);
        }
        uint64_t started = monotonic_nanos();
        write_exact(child.stdin_fd, message, (size_t)bytes);
        read_exact(child.stdout_fd, reply, (size_t)bytes);
        samples_record(&samples, monotonic_nanos() - started);
        if (memcmp(message, reply, (size_t)bytes) != 0) {
            errno = 0;
            die("the child returned a corrupted message");
        }
    }
    close(child.stdin_fd);
    drain(child.stdout_fd);
    close(child.stdout_fd);
    wait_child(child.pid);

    printf("pipe-pingpong:%llu\n", (unsigned long long)rounds);
    samples_report(&samples, "rtt");
}

struct stream_writer {
    int fd;
    uint64_t total;
};

static void *run_stream_writer(void *argument) {
    struct stream_writer *writer = argument;
    static unsigned char chunk[STREAM_CHUNK_BYTES];
    uint64_t written = 0;
    while (written < writer->total) {
        uint64_t len = writer->total - written;
        if (len > sizeof chunk) {
            len = sizeof chunk;
        }
        for (uint64_t index = 0; index < len; index++) {
            chunk[index] = (unsigned char)(written + index);
        }
        write_exact(writer->fd, chunk, (size_t)len);
        written += len;
    }
    close(writer->fd);
    return NULL;
}

static void stream(uint64_t total, char **child_argv) {
    struct child child = spawn_child(child_argv);
    struct stream_writer writer = {child.stdin_fd, total};
    uint64_t started = monotonic_nanos();
    pthread_t thread;
    int rc = pthread_create(&thread, NULL, run_stream_writer, &writer);
    if (rc != 0) {
        errno = rc;
        die("pthread_create");
    }
    uint64_t echoed = drain(child.stdout_fd);
    uint64_t elapsed = monotonic_nanos() - started;
    rc = pthread_join(thread, NULL);
    if (rc != 0) {
        errno = rc;
        die("pthread_join");
    }
    close(child.stdout_fd);
    wait_child(child.pid);
    if (echoed != total) {
        errno = 0;
        fprintf(stderr, "the child echoed %llu bytes, expected %llu\n",
                (unsigned long long)echoed, (unsigned long long)total);
        exit(1);
    }
    printf("pipe-stream:%llu\n", (unsigned long long)total);
    report_mib_per_second("mib_per_s", total, elapsed);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "%s\n", USAGE);
        return 2;
    }
    const char *command = argv[1];
    if (strcmp(command, "startup") == 0 && argc >= 4) {
        startup(parse_count(argv[2], USAGE), argv + 3);
    } else if (strcmp(command, "spawn-wait") == 0 && argc >= 4) {
        spawn_wait(parse_count(argv[2], USAGE), argv + 3);
    } else if (strcmp(command, "pingpong") == 0 && argc >= 5) {
        pingpong(parse_count(argv[2], USAGE), parse_count(argv[3], USAGE), argv + 4);
    } else if (strcmp(command, "stream") == 0 && argc >= 4) {
        stream(parse_count(argv[2], USAGE), argv + 3);
    } else {
        fprintf(stderr, "%s\n", USAGE);
        return 2;
    }
    return 0;
}
