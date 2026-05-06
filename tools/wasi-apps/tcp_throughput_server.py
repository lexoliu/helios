#!/usr/bin/env python3
import argparse
import socketserver
import threading


DEFAULT_CHUNK_BYTES = 1024 * 1024
DEFAULT_PAYLOAD_BYTES = 64 * 1024 * 1024
PAYLOAD_CHUNK = bytes(index & 0xFF for index in range(DEFAULT_CHUNK_BYTES))


class TcpThroughputServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(
        self,
        server_address: tuple[str, int],
        payload_bytes: int,
    ) -> None:
        self.payload_bytes = payload_bytes
        super().__init__(server_address, TcpThroughputHandler)


class TcpThroughputHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        remaining = self.server.payload_bytes
        while remaining:
            chunk = PAYLOAD_CHUNK[: min(remaining, len(PAYLOAD_CHUNK))]
            self.request.sendall(chunk)
            remaining -= len(chunk)


def start_tcp_throughput_server(
    host: str,
    port: int,
    payload_bytes: int,
) -> tuple[TcpThroughputServer, int]:
    server = TcpThroughputServer((host, port), payload_bytes)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, int(server.server_address[1])


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--payload-bytes", type=int, default=DEFAULT_PAYLOAD_BYTES)
    args = parser.parse_args()
    server = TcpThroughputServer((args.host, args.port), args.payload_bytes)
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
