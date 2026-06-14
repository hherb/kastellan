"""A loopback-TCP <-> UDS relay so a headless Chromium can reach the egress
sidecar (egress slice #2).

Chromium speaks HTTP-proxy `CONNECT host:port` over a TCP socket; the egress
sidecar speaks the *same* CONNECT protocol over its Unix-domain socket. So this
shim is a dumb byte-pipe: accept a TCP connection on 127.0.0.1, open the UDS,
and splice bytes both ways. No HTTP parsing.

The browser-driver worker is synchronous (sync Playwright), so the relay runs on
its own background thread with a private asyncio event loop. The public API is
sync: `start()` returns the bound loopback port; `stop()` shuts it down.
"""
import asyncio
import threading
from typing import Optional


class ProxyShim:
    def __init__(self, uds_path: str):
        self._uds_path = uds_path
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._thread: Optional[threading.Thread] = None
        self._server: Optional[asyncio.AbstractServer] = None
        self._port: Optional[int] = None

    def start(self) -> int:
        """Start the relay on a background thread; return the bound TCP port."""
        if self._thread is not None:
            raise RuntimeError("ProxyShim already started")
        ready = threading.Event()
        err: list[BaseException] = []

        def run() -> None:
            loop = asyncio.new_event_loop()
            self._loop = loop
            asyncio.set_event_loop(loop)
            try:
                server = loop.run_until_complete(
                    asyncio.start_server(self._handle, host="127.0.0.1", port=0)
                )
                self._server = server
                self._port = server.sockets[0].getsockname()[1]
            except BaseException as e:  # noqa: BLE001 - surface to start()
                err.append(e)
                ready.set()
                return
            ready.set()
            loop.run_forever()
            loop.run_until_complete(server.wait_closed())
            loop.close()

        self._thread = threading.Thread(target=run, name="egress-shim", daemon=True)
        self._thread.start()
        if not ready.wait(timeout=10):
            raise RuntimeError("egress shim failed to start within 10s")
        if err:
            raise err[0]
        assert self._port is not None
        return self._port

    def stop(self) -> None:
        """Stop the relay and join its thread. Idempotent: safe to call when
        never started, and safe to call more than once."""
        loop = self._loop
        server = self._server
        thread = self._thread
        # Clear first so a concurrent/second stop() is a no-op.
        self._loop = None
        self._server = None
        self._thread = None
        if loop is None:
            return

        def _shutdown() -> None:
            if server is not None:
                server.close()
            loop.stop()

        if not loop.is_closed():
            try:
                loop.call_soon_threadsafe(_shutdown)
            except RuntimeError:
                pass  # loop already stopped/closed
        if thread is not None:
            thread.join(timeout=5)

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """One TCP client: open the UDS, splice both directions until either EOF."""
        try:
            uds_reader, uds_writer = await asyncio.open_unix_connection(self._uds_path)
        except OSError:
            writer.close()
            return
        try:
            await asyncio.gather(
                self._pipe(reader, uds_writer),
                self._pipe(uds_reader, writer),
            )
        finally:
            for w in (writer, uds_writer):
                try:
                    w.close()
                except Exception:  # noqa: BLE001
                    pass

    @staticmethod
    async def _pipe(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            while True:
                chunk = await reader.read(65536)
                if not chunk:
                    break
                writer.write(chunk)
                await writer.drain()
        except (ConnectionError, OSError):
            pass
        finally:
            try:
                writer.write_eof()
            except (OSError, RuntimeError):
                pass
