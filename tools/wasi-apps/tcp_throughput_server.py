#!/usr/bin/env python3
"""Host servers behind the raw-TCP benchmark workloads.

The direction of a transfer is a property of the endpoint, never of anything
a connection does: the download server streams its payload the moment a
connection is accepted, and the upload server discards what arrives until the
client closes. A server that told the two apart by waiting for silence would
put that wait inside every timed download.
"""

import argparse
import socketserver
import threading

from owned_tcp_server import OwnedThreadingTCPServer


DEFAULT_CHUNK_BYTES = 1024 * 1024
DEFAULT_PAYLOAD_BYTES = 64 * 1024 * 1024
PAYLOAD_CHUNK = bytes(index & 0xFF for index in range(DEFAULT_CHUNK_BYTES))


class TcpDownloadServer(OwnedThreadingTCPServer):
    allow_reuse_address = True

    def __init__(
        self,
        server_address: tuple[str, int],
        payload_bytes: int,
    ) -> None:
        self.payload_bytes = payload_bytes
        super().__init__(server_address, TcpDownloadHandler)


class TcpDownloadHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        remaining = self.server.payload_bytes
        while remaining:
            chunk = PAYLOAD_CHUNK[: min(remaining, len(PAYLOAD_CHUNK))]
            self.request.sendall(chunk)
            remaining -= len(chunk)


class TcpUploadServer(OwnedThreadingTCPServer):
    allow_reuse_address = True

    def __init__(self, server_address: tuple[str, int]) -> None:
        super().__init__(server_address, TcpUploadHandler)


class TcpUploadHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        while self.request.recv(DEFAULT_CHUNK_BYTES):
            pass


def serve_in_thread(server: socketserver.BaseServer) -> tuple[socketserver.BaseServer, int]:
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, int(server.server_address[1])


def start_tcp_download_server(
    host: str,
    port: int,
    payload_bytes: int,
) -> tuple[TcpDownloadServer, int]:
    return serve_in_thread(TcpDownloadServer((host, port), payload_bytes))


def start_tcp_upload_server(host: str, port: int) -> tuple[TcpUploadServer, int]:
    return serve_in_thread(TcpUploadServer((host, port)))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--download-port", type=int, required=True)
    parser.add_argument("--upload-port", type=int, required=True)
    parser.add_argument("--payload-bytes", type=int, default=DEFAULT_PAYLOAD_BYTES)
    args = parser.parse_args()
    download = TcpDownloadServer((args.host, args.download_port), args.payload_bytes)
    upload = TcpUploadServer((args.host, args.upload_port))
    serve_in_thread(upload)
    try:
        download.serve_forever()
    finally:
        download.server_close()
        upload.shutdown()
        upload.server_close()


if __name__ == "__main__":
    main()
