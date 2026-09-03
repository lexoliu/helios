/* Linux counterpart of tools/wasi-apps/pipe-echo: write back every chunk
 * read from stdin until EOF, one read at a time. */
#include <stdio.h>
#include <unistd.h>

#include "bench_metrics.h"

#define CHUNK_BYTES (64 * 1024)

static char buffer[CHUNK_BYTES];

int main(int argc, char **argv) {
    (void)argv;
    if (argc != 1) {
        fprintf(stderr, "pipe-echo takes no arguments\n");
        return 2;
    }
    for (;;) {
        ssize_t read_bytes = read(STDIN_FILENO, buffer, sizeof buffer);
        if (read_bytes == 0) {
            return 0;
        }
        if (read_bytes < 0) {
            die("stdin");
        }
        size_t written = 0;
        while (written < (size_t)read_bytes) {
            ssize_t wrote = write(STDOUT_FILENO, buffer + written, (size_t)read_bytes - written);
            if (wrote < 0) {
                die("stdout");
            }
            written += (size_t)wrote;
        }
    }
}
