#!/usr/bin/env python3
"""Host servers behind the raw-TCP benchmark workloads.

The direction of a transfer is a property of the endpoint, never of anything
a connection does: the download server streams its payload the moment a
connection is accepted, and the upload server discards what arrives until the
client closes. A server that told the two apart by waiting for silence would
put that wait inside every timed download.
"""

import argparse
import os
import socketserver
import threading
from pathlib import Path

from owned_tcp_server import OwnedThreadingTCPServer


DEFAULT_CHUNK_BYTES = 1024 * 1024
DEFAULT_PAYLOAD_BYTES = 64 * 1024 * 1024
PAYLOAD_CHUNK = bytes(index & 0xFF for index in range(DEFAULT_CHUNK_BYTES))
BEGUN_MARKER = "transfer-begun"
ENDED_MARKER = "transfer-ended"


class TransferProgress:
    """Marker files a guest watches through its host share.

    `transfer-begun` appears once a download has sent `begun_bytes`, and
    `transfer-ended` once the connection's handler has returned, for
    whatever reason. Each holds the byte count sent when it was written,
    and appears whole: it is written aside and renamed into place.
    """

    def __init__(self, directory: Path, begun_bytes: int) -> None:
        self.directory = directory
        self.begun_bytes = begun_bytes
        self.begun = False

    def sent(self, total: int) -> None:
        if not self.begun and total >= self.begun_bytes:
            self.begun = True
            self._mark(BEGUN_MARKER, total)

    def ended(self, total: int) -> None:
        self._mark(ENDED_MARKER, total)

    def _mark(self, name: str, total: int) -> None:
        staged = self.directory / f".{name}.staged"
        staged.write_text(f"{total}\n")
        os.replace(staged, self.directory / name)


class TcpDownloadServer(OwnedThreadingTCPServer):
    allow_reuse_address = True

    def __init__(
        self,
        server_address: tuple[str, int],
        payload_bytes: int,
        progress: TransferProgress | None = None,
    ) -> None:
        self.payload_bytes = payload_bytes
        self.progress = progress
        super().__init__(server_address, TcpDownloadHandler)


class TcpDownloadHandler(socketserver.BaseRequestHandler):
    def handle(self) -> None:
        progress = self.server.progress
        sent = 0
        try:
            while sent < self.server.payload_bytes:
                remaining = self.server.payload_bytes - sent
                chunk = PAYLOAD_CHUNK[: min(remaining, len(PAYLOAD_CHUNK))]
                self.request.sendall(chunk)
                sent += len(chunk)
                if progress is not None:
                    progress.sent(sent)
        finally:
            if progress is not None:
                progress.ended(sent)


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
    parser.add_argument(
        "--progress-dir",
        type=Path,
        help="directory to write the transfer-begun and transfer-ended markers into",
    )
    parser.add_argument(
        "--begun-bytes",
        type=int,
        help="bytes a download sends before its transfer-begun marker appears",
    )
    args = parser.parse_args()
    if (args.progress_dir is None) != (args.begun_bytes is None):
        parser.error("--progress-dir and --begun-bytes are given together or not at all")
    progress = (
        None
        if args.progress_dir is None
        else TransferProgress(args.progress_dir, args.begun_bytes)
    )
    download = TcpDownloadServer(
        (args.host, args.download_port), args.payload_bytes, progress
    )
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
