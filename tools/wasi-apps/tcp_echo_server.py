#!/usr/bin/env python3
"""TCP echo server for the round-trip latency workloads.

Every byte received on a connection is written straight back. Nagle is
disabled on the accepted socket so a 16-byte reply leaves immediately;
the client side of the measurement is expected to do the same where its
socket API offers the option.
"""

import argparse
import socket
import socketserver
import threading


RECEIVE_BYTES = 64 * 1024


class TcpEchoServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class TcpEchoHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        self.request.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        while True:
            chunk = self.request.recv(RECEIVE_BYTES)
            if not chunk:
                return
            self.request.sendall(chunk)


def start_tcp_echo_server(host: str, port: int) -> tuple[TcpEchoServer, int]:
    server = TcpEchoServer((host, port), TcpEchoHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, int(server.server_address[1])


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
    server = TcpEchoServer((args.host, args.port), TcpEchoHandler)
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
