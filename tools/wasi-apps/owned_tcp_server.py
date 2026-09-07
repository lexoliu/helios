import socket
import socketserver
import threading
from contextlib import suppress


class RequestOwnership:
    def __init__(self, *args, **kwargs):
        self._request_lock = threading.Lock()
        self._requests: set[socket.socket] = set()
        self._closing = False
        super().__init__(*args, **kwargs)

    def process_request(self, request, client_address):
        with self._request_lock:
            closing = self._closing
            if not closing:
                self._requests.add(request)
        if closing:
            self.shutdown_request(request)
        else:
            super().process_request(request, client_address)

    def shutdown_request(self, request):
        try:
            super().shutdown_request(request)
        finally:
            with self._request_lock:
                self._requests.discard(request)

    def _close_requests(self):
        with self._request_lock:
            self._closing = True
            requests = tuple(self._requests)
        for request in requests:
            with suppress(OSError):
                request.shutdown(socket.SHUT_RDWR)

    def shutdown(self):
        self._close_requests()
        super().shutdown()

    def server_close(self):
        self._close_requests()
        super().server_close()


class OwnedTCPServer(RequestOwnership, socketserver.TCPServer):
    pass


class OwnedThreadingTCPServer(RequestOwnership, socketserver.ThreadingTCPServer):
    daemon_threads = False
