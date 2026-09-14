#!/usr/bin/env python3
"""A minimal streamable-HTTP MCP server fixture: initialize + tools/list."""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

TOOLS = {"ping": {"description": "responds pong", "input_schema": {"type": "object"}}}

class Handler(BaseHTTPRequestHandler):
    def _send(self, status, body, session=False):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        if session:
            self.send_header("Mcp-Session-Id", "fixture-session-1")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length))
        method = request.get("method")
        request_id = request.get("id")
        if method == "initialize":
            self._send(200, {"jsonrpc": "2.0", "id": request_id, "result": {
                "protocolVersion": "2026-07-28",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "1"},
            }}, session=True)
        elif method == "notifications/initialized":
            self._send(200, {})
        elif method == "tools/list":
            self._send(200, {"jsonrpc": "2.0", "id": request_id, "result": {"tools": [
                {"name": name, "description": tool["description"],
                 "inputSchema": tool["input_schema"]}
                for name, tool in TOOLS.items()
            ]}})
        else:
            self._send(200, {"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "method not found"}})

    def log_message(self, *args):
        pass

HTTPServer(("127.0.0.1", 18080), Handler).serve_forever()
