#!/usr/bin/env python3
"""HTTP bridge: Gatus custom webhook → tg-mcp send_message → admin bot."""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

ENV_FILE = os.environ.get("ENV_FILE", "/etc/gatus-bridge.env")
LISTEN_HOST = os.environ.get("LISTEN_HOST", "0.0.0.0")
LISTEN_PORT = int(os.environ.get("LISTEN_PORT", "9081"))
ROUTE = "/gatus/alert"
MCP_URL = os.environ.get("MCP_URL", "https://tg-mcp.l1979.ru/v1/mcp")
MCP_CHAT_ID = os.environ.get("MCP_CHAT_ID", "@redevest_admin_tools_bot")

ALIASES = {
    "host-box2": "vpn",
    "host-box3": "apps",
    "host-box4": "n8n",
    "host-box5": "local",
}


def load_env() -> None:
    if not os.path.isfile(ENV_FILE):
        return
    with open(ENV_FILE, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, _, val = line.partition("=")
            os.environ.setdefault(key.strip(), val.strip())


def build_message(payload: dict) -> str | None:
    status = str(payload.get("status", ""))
    if "TRIGGERED" not in status.upper():
        return None
    endpoint = payload.get("endpoint", "unknown")
    alias = ALIASES.get(endpoint, "unknown")
    return (
        f"[Gatus] {endpoint} TRIGGERED\n"
        f"SSH: {alias}\n"
        f"Conditions: {payload.get('conditions', '')}\n"
        f"Errors: {payload.get('errors', '')}\n"
        "Run: git -C /root/vds-servers pull --ff-only; host-diag; investigate. "
        "Fix if safe or ask me."
    )


def mcp_send(message: str) -> None:
    bearer = os.environ.get("TG_MCP_BEARER", "")
    if not bearer:
        raise RuntimeError("TG_MCP_BEARER not set")
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "send_message",
                "arguments": {"chat_id": MCP_CHAT_ID, "message": message},
            },
        }
    ).encode()
    req = urllib.request.Request(
        MCP_URL,
        data=body,
        headers={
            "Authorization": f"Bearer {bearer}",
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        resp.read()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args) -> None:
        sys.stderr.write("[gatus-bridge] " + (fmt % args) + "\n")

    def do_GET(self) -> None:
        if self.path == "/":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"ok")
        else:
            self.send_error(404)

    def do_POST(self) -> None:
        if self.path != ROUTE:
            self.send_error(404)
            return
        secret = self.headers.get("X-Gatus-Secret", "")
        expected = os.environ.get("GATUS_BRIDGE_SECRET", "")
        if not expected or secret != expected:
            self.send_error(401)
            return
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length else b""
        try:
            payload = json.loads(raw.decode() or "{}")
        except json.JSONDecodeError:
            payload = {}
        sys.stderr.write(f"[gatus-bridge] body: {raw.decode(errors='replace')}\n")
        msg = build_message(payload)
        if msg:
            mcp_send(msg)
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")


def notify_test() -> None:
    mcp_send("[Gatus] bridge notify-test — ignore")
    print(f"notify-test sent to {MCP_CHAT_ID}")


def serve() -> None:
    load_env()
    if not os.environ.get("TG_MCP_BEARER"):
        sys.exit("TG_MCP_BEARER not set")
    httpd = HTTPServer((LISTEN_HOST, LISTEN_PORT), Handler)
    sys.stderr.write(f"[gatus-bridge] listening {LISTEN_HOST}:{LISTEN_PORT}{ROUTE}\n")
    httpd.serve_forever()


def main() -> None:
    cmd = sys.argv[1] if len(sys.argv) > 1 else "serve"
    load_env()
    if cmd == "serve":
        serve()
    elif cmd == "notify-test":
        notify_test()
    else:
        sys.exit(f"Usage: {sys.argv[0]} serve|notify-test")


if __name__ == "__main__":
    main()
