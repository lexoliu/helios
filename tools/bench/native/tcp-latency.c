/* Linux counterpart of tools/wasi-apps/tcp-latency: 16-byte round trips
 * against the host echo server over one TCP stream with Nagle disabled. */
#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "bench_metrics.h"

#define MESSAGE_BYTES 16

static const char USAGE[] = "usage: tcp-latency <ip-host> <port> <rounds>";

static void write_exact(int fd, const unsigned char *data, size_t len) {
    size_t written = 0;
    while (written < len) {
        ssize_t wrote = write(fd, data + written, len - written);
        if (wrote < 0) {
            die("write");
        }
        written += (size_t)wrote;
    }
}

static void read_exact(int fd, unsigned char *data, size_t len) {
    size_t received = 0;
    while (received < len) {
        ssize_t got = read(fd, data + received, len - received);
        if (got < 0) {
            die("read");
        }
        if (got == 0) {
            errno = 0;
            die("the echo server closed the connection");
        }
        received += (size_t)got;
    }
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "%s\n", USAGE);
        return 2;
    }
    uint64_t port = parse_count(argv[2], USAGE);
    uint64_t rounds = parse_count(argv[3], USAGE);
    if (port > 65535) {
        fprintf(stderr, "%s\n", USAGE);
        return 2;
    }

    struct sockaddr_in address;
    memset(&address, 0, sizeof address);
    address.sin_family = AF_INET;
    address.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, argv[1], &address.sin_addr) != 1) {
        fprintf(stderr, "%s\n", USAGE);
        return 2;
    }
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        die("socket");
    }
    int one = 1;
    if (setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one) != 0) {
        die("setsockopt TCP_NODELAY");
    }
    if (connect(fd, (struct sockaddr *)&address, sizeof address) != 0) {
        die("connect");
    }

    struct latency_samples samples;
    samples_init(&samples, (size_t)rounds);
    unsigned char message[MESSAGE_BYTES];
    unsigned char reply[MESSAGE_BYTES];
    for (uint64_t round = 0; round < rounds; round++) {
        memcpy(message, &round, sizeof round);
        memcpy(message + sizeof round, &round, sizeof round);
        uint64_t started = monotonic_nanos();
        write_exact(fd, message, sizeof message);
        read_exact(fd, reply, sizeof reply);
        samples_record(&samples, monotonic_nanos() - started);
        if (memcmp(message, reply, sizeof message) != 0) {
            errno = 0;
            die("the echo server returned a corrupted message");
        }
    }
    close(fd);

    printf("tcp-latency:%llu\n", (unsigned long long)rounds);
    samples_report(&samples, "rtt");
    return 0;
}
