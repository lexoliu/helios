/* Linux counterpart of tools/wasi-apps/hello: print one line, optionally
 * hold until stdin closes so a batch of processes stays resident. */
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "bench_metrics.h"

int main(int argc, char **argv) {
    int hold = 0;
    if (argc == 2 && strcmp(argv[1], "hold") == 0) {
        hold = 1;
    } else if (argc != 1) {
        fprintf(stderr, "usage: hello [hold]\n");
        return 2;
    }
    if (fputs("hello\n", stdout) == EOF || fflush(stdout) != 0) {
        die("stdout");
    }
    if (hold) {
        char sink[64];
        for (;;) {
            ssize_t read_bytes = read(STDIN_FILENO, sink, sizeof sink);
            if (read_bytes == 0) {
                break;
            }
            if (read_bytes < 0) {
                die("stdin");
            }
        }
    }
    return 0;
}
