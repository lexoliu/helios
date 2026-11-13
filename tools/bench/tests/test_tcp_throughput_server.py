"""The host servers behind `tcp-throughput` and `tcp-upload`.

A download connection used to be told from an upload one by waiting a
quarter of a second for the client to speak, so every timed download began
with that silence in both the Helios and the Linux column (#748). The
direction is now the endpoint's: the download server owns no wait at all,
and the upload server is a sink.
"""

from __future__ import annotations

import socket

import pytest

from helios_bench import WASI_APPS_ROOT
from helios_bench.wasi_apps import gap_bench

PAYLOAD_BYTES = 3 * 1024 * 1024 + 17


def server_module():
    gap_bench()  # puts tools/wasi-apps on sys.path
    import tcp_throughput_server

    return tcp_throughput_server


class RefusingRequest:
    """A connection that fails the test if the handler reads or waits on it."""

    def __init__(self) -> None:
        self.sent = bytearray()

    def recv(self, *_args) -> bytes:
        raise AssertionError("the download handler waited for the client to speak")

    def settimeout(self, *_args) -> None:
        raise AssertionError("the download handler set a timeout")

    def sendall(self, data: bytes) -> None:
        self.sent += data


def test_download_handler_streams_the_payload_without_reading_or_waiting() -> None:
    module = server_module()
    request = RefusingRequest()
    handler = module.TcpDownloadHandler.__new__(module.TcpDownloadHandler)
    handler.request = request
    handler.server = type("Server", (), {"payload_bytes": PAYLOAD_BYTES, "progress": None})()
    handler.handle()
    assert len(request.sent) == PAYLOAD_BYTES
    assert bytes(request.sent[: module.DEFAULT_CHUNK_BYTES]) == module.PAYLOAD_CHUNK


def test_download_progress_marks_the_transfer_begun_and_then_ended(tmp_path) -> None:
    module = server_module()
    request = RefusingRequest()
    handler = module.TcpDownloadHandler.__new__(module.TcpDownloadHandler)
    handler.request = request
    begun_bytes = 2 * module.DEFAULT_CHUNK_BYTES
    handler.server = type(
        "Server",
        (),
        {
            "payload_bytes": PAYLOAD_BYTES,
            "progress": module.TransferProgress(tmp_path, begun_bytes),
        },
    )()
    handler.handle()
    assert (tmp_path / module.BEGUN_MARKER).read_text() == f"{begun_bytes}\n"
    assert (tmp_path / module.ENDED_MARKER).read_text() == f"{PAYLOAD_BYTES}\n"
    assert sorted(path.name for path in tmp_path.iterdir()) == [
        module.BEGUN_MARKER,
        module.ENDED_MARKER,
    ]


def test_a_download_cut_short_marks_it_ended_but_never_begun(tmp_path) -> None:
    module = server_module()

    class ResettingRequest(RefusingRequest):
        def sendall(self, data: bytes) -> None:
            raise ConnectionResetError("the guest went away")

    handler = module.TcpDownloadHandler.__new__(module.TcpDownloadHandler)
    handler.request = ResettingRequest()
    handler.server = type(
        "Server",
        (),
        {
            "payload_bytes": PAYLOAD_BYTES,
            "progress": module.TransferProgress(tmp_path, module.DEFAULT_CHUNK_BYTES),
        },
    )()
    with pytest.raises(ConnectionResetError):
        handler.handle()
    assert not (tmp_path / module.BEGUN_MARKER).exists()
    assert (tmp_path / module.ENDED_MARKER).read_text() == "0\n"


def test_download_server_sends_to_a_client_that_never_writes() -> None:
    module = server_module()
    server, port = module.start_tcp_download_server("127.0.0.1", 0, PAYLOAD_BYTES)
    try:
        received = 0
        with socket.create_connection(("127.0.0.1", port), timeout=10) as client:
            while chunk := client.recv(1024 * 1024):
                received += len(chunk)
        assert received == PAYLOAD_BYTES
    finally:
        server.shutdown()
        server.server_close()


def test_upload_server_discards_until_the_client_closes() -> None:
    module = server_module()
    server, port = module.start_tcp_upload_server("127.0.0.1", 0)
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=10) as client:
            for _ in range(8):
                client.sendall(module.PAYLOAD_CHUNK)
            client.shutdown(socket.SHUT_WR)
            # The sink never answers; it closes once the client has.
            assert client.recv(1) == b""
    finally:
        server.shutdown()
        server.server_close()


def test_the_server_keeps_no_timing_constant() -> None:
    source = (WASI_APPS_ROOT / "tcp_throughput_server.py").read_text()
    assert "timeout" not in source
