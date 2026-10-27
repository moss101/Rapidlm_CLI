#!/usr/bin/env python3
"""The CI key-isolation proxy: the model credential never reaches the
checkout.

The pattern: `rapid exec` in CI runs against repository-controlled code —
prompts, scripts, playbooks — any of which could otherwise read the
provider key from the environment or the config it runs with. Instead the
key lives ONLY in this proxy's environment. The runner starts this proxy
with the secret, writes rapid a config that points at the proxy with a
fixed dummy token, and the proxy:

  * forwards requests to exactly ONE upstream provider origin,
  * injects `Authorization: Bearer <the real key>` — replacing whatever
    the caller sent, so the dummy token rapid holds is useless outside,
  * refuses every other host (a hijacked run cannot pivot the proxy into
    an open relay), and
  * never logs headers.

Stdlib only; single-threaded; bounded request bodies. Good enough for one
CI job, deliberately too small to hide anything in.

Usage: key-proxy.py <listen-port> <upstream-base-url>
  The real key comes from the KEY environment variable.
"""

import http.server
import os
import socket
import sys
import urllib.parse

MAX_BODY = 32 * 1024 * 1024
UPSTREAM: str = ""
KEY: str = ""


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # headers are never logged
        sys.stderr.write("key-proxy: %s %s\n" % (self.command, self.path))

    def _upstream(self):
        parsed = urllib.parse.urlsplit(UPSTREAM)
        default_port = 443 if parsed.scheme == "https" else 80
        addr = (parsed.hostname, parsed.port or default_port)
        try:
            sock = socket.create_connection(addr, timeout=30)
        except OSError as err:
            self._fail(502, f"upstream unreachable: {err}")
            return None
        return sock, parsed.path.rstrip("/")

    def _fail(self, code: int, reason: str) -> None:
        body = reason.encode()
        self.send_response(code)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")  # one request per connection
        self.end_headers()
        self.wfile.write(body)

    def do_CONNECT(self):  # refused: there is exactly one upstream
        self._fail(403, "CONNECT is not supported; this proxy fronts one provider")

    def do_GET(self) -> None:
        self._relay(body=b"")

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        if length > MAX_BODY:
            self._fail(413, "request over the size bound")
            return
        self._relay(body=self.rfile.read(length) if length else b"")

    def _relay(self, body: bytes) -> None:
        if not KEY:
            self._fail(500, "the KEY environment variable is empty")
            return
        upstream = self._upstream()
        if upstream is None:
            return
        sock, base = upstream
        # Only ever this proxy's own upstream origin: the path the caller
        # names rides under it, whatever host it claims.
        path = self.path
        request = (
            f"{self.command} {base}{path} HTTP/1.1\r\n"
            f"Host: {urllib.parse.urlsplit(UPSTREAM).netloc}\r\n"
            "Authorization: Bearer " + KEY + "\r\n"
            "Content-Type: application/json\r\n"
            f"Content-Length: {len(body)}\r\n"
            "Connection: close\r\n\r\n"
        ).encode() + body
        try:
            sock.sendall(request)
            response = b""
            while True:
                chunk = sock.recv(65536)
                if not chunk:
                    break
                response += chunk
                if len(response) > MAX_BODY:
                    self._fail(502, "upstream response over the size bound")
                    return
        finally:
            sock.close()
        if not response:
            self._fail(502, "the upstream closed without answering")
            return
        head, _, payload = response.partition(b"\r\n\r\n")
        lines = head.split(b"\r\n")
        try:
            status = int(lines[0].split()[1])
        except (IndexError, ValueError):
            self._fail(502, "the upstream's answer is not HTTP")
            return
        self.send_response(status)
        for line in lines[1:]:
            name = line.split(b":", 1)[0].strip().lower()
            # Hop-by-hop and authorization headers are ours, not the
            # caller's; everything else passes through.
            if name in (b"connection", b"transfer-encoding", b"content-length", b"authorization"):
                continue
            self.send_header(line.split(b":", 1)[0].decode(), line.split(b":", 1)[1].strip().decode("latin-1"))
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")  # one request per connection
        self.end_headers()
        self.wfile.write(payload)


def main() -> None:
    global UPSTREAM, KEY
    if len(sys.argv) != 3:
        sys.stderr.write(__doc__)
        sys.exit(2)
    port = int(sys.argv[1])
    UPSTREAM = sys.argv[2].rstrip("/")
    KEY = os.environ.get("KEY", "")
    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
