#!/usr/bin/env python3
import argparse
import socket


BUFFER_BYTES = 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 30.0


def receive_exact(host: str, port: int, expected_bytes: int) -> int:
    buffer = bytearray(BUFFER_BYTES)
    view = memoryview(buffer)
    total = 0
    with socket.create_connection((host, port), timeout=DEFAULT_TIMEOUT_SECONDS) as sock:
        sock.settimeout(DEFAULT_TIMEOUT_SECONDS)
        while True:
            read = sock.recv_into(view)
            if read == 0:
                break
            total += read
    if total != expected_bytes:
        raise SystemExit(f"tcp stream delivered {total} bytes, expected {expected_bytes}")
    return total


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--label", default="tcp-throughput")
    parser.add_argument("host")
    parser.add_argument("port", type=int)
    parser.add_argument("expected_bytes", type=int)
    args = parser.parse_args()
    total = receive_exact(args.host, args.port, args.expected_bytes)
    print(f"{args.label}:{total}")


if __name__ == "__main__":
    main()
