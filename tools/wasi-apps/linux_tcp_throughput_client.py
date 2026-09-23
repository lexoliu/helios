#!/usr/bin/env python3
import argparse
import socket


BUFFER_BYTES = 1024 * 1024
UPLOAD_CHUNK = bytes(index & 0xFF for index in range(256 * 1024))
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


def send_exact(host: str, port: int, total_bytes: int) -> int:
    written = 0
    with socket.create_connection((host, port), timeout=DEFAULT_TIMEOUT_SECONDS) as sock:
        sock.settimeout(DEFAULT_TIMEOUT_SECONDS)
        while written < total_bytes:
            length = min(total_bytes - written, len(UPLOAD_CHUNK))
            sock.sendall(UPLOAD_CHUNK[:length])
            written += length
    return written


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--label", default=None)
    parser.add_argument("host")
    parser.add_argument("port", type=int)
    parser.add_argument("expected_bytes", type=int)
    parser.add_argument("mode", nargs="?", choices=["up"], default=None)
    args = parser.parse_args()
    if args.mode == "up":
        total = send_exact(args.host, args.port, args.expected_bytes)
        label = args.label or "tcp-upload"
    else:
        total = receive_exact(args.host, args.port, args.expected_bytes)
        label = args.label or "tcp-throughput"
    print(f"{label}:{total}")


if __name__ == "__main__":
    main()
