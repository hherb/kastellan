"""Tests for the loopback-TCP<->UDS relay shim (egress slice #2).

A fake UDS server stands in for the egress sidecar: it accepts a connection and
echoes everything it receives. The test connects to the shim's loopback TCP port
with a blocking socket and asserts bytes round-trip through the UDS.
"""
import socket
import tempfile
import threading
import os

from kastellan_worker_browser_driver.shim import ProxyShim


def _fake_uds_echo_server(uds_path: str, ready: threading.Event) -> threading.Thread:
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(uds_path)
    srv.listen(8)
    ready.set()

    def serve():
        while True:
            try:
                conn, _ = srv.accept()
            except OSError:
                return
            threading.Thread(target=_echo, args=(conn,), daemon=True).start()

    def _echo(conn):
        with conn:
            while True:
                data = conn.recv(4096)
                if not data:
                    return
                conn.sendall(data)

    t = threading.Thread(target=serve, daemon=True)
    t.start()
    return t


def test_shim_relays_bytes_through_uds():
    tmp = tempfile.mkdtemp()
    uds_path = os.path.join(tmp, "egress.sock")
    ready = threading.Event()
    _fake_uds_echo_server(uds_path, ready)
    assert ready.wait(timeout=5)

    shim = ProxyShim(uds_path)
    port = shim.start()
    try:
        assert isinstance(port, int) and port > 0
        c = socket.create_connection(("127.0.0.1", port), timeout=5)
        c.sendall(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
        got = c.recv(4096)
        assert got == b"CONNECT example.com:443 HTTP/1.1\r\n\r\n"
        c.close()
    finally:
        shim.stop()


def test_shim_handles_concurrent_connections():
    tmp = tempfile.mkdtemp()
    uds_path = os.path.join(tmp, "egress.sock")
    ready = threading.Event()
    _fake_uds_echo_server(uds_path, ready)
    assert ready.wait(timeout=5)

    shim = ProxyShim(uds_path)
    port = shim.start()
    try:
        conns = [socket.create_connection(("127.0.0.1", port), timeout=5) for _ in range(5)]
        for i, c in enumerate(conns):
            msg = f"hello-{i}".encode()
            c.sendall(msg)
            assert c.recv(4096) == msg
        for c in conns:
            c.close()
    finally:
        shim.stop()
