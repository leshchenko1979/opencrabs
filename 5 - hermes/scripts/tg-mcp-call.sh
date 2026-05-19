#!/usr/bin/env python3
"""Lightweight tg-mcp client for OpenCrabs tools.toml (fastmcp call → remote HTTP MCP).

Replaces /root/.local/bin/mcp tg-mcp … (14MB proxy; slow/hangs on 1GB Hermes).
Usage: tg-mcp-call <tool_name> '<json-args>'
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys

DEFAULT_CONFIG = "/root/.opencrabs/config.toml"
DEFAULT_URL = "https://tg-mcp.l1979.ru/v1/mcp"
DEFAULT_TIMEOUT = "30"


def _read_bearer() -> str:
    bearer = os.environ.get("TG_MCP_BEARER", "").strip()
    if bearer:
        return bearer
    config = os.environ.get("TG_MCP_CONFIG", DEFAULT_CONFIG)
    if not os.path.isfile(config):
        return ""
    with open(config, encoding="utf-8") as f:
        for line in f:
            m = re.match(r'^bearer\s*=\s*"([^"]+)"', line.strip())
            if m:
                return m.group(1)
    return ""


def main() -> None:
    if len(sys.argv) < 2:
        print("usage: tg-mcp-call <tool> [json]", file=sys.stderr)
        sys.exit(2)
    tool = sys.argv[1]
    raw = sys.argv[2] if len(sys.argv) > 2 else "{}"
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:
        print(json.dumps({"error": f"invalid json args: {exc}"}))
        sys.exit(1)

    bearer = _read_bearer()
    if not bearer:
        print(json.dumps({"error": "TG_MCP_BEARER unset and no bearer in config"}))
        sys.exit(1)

    url = os.environ.get("TG_MCP_URL", DEFAULT_URL)
    timeout = os.environ.get("TG_MCP_TIMEOUT", DEFAULT_TIMEOUT)
    cmd = [
        "fastmcp",
        "call",
        "--server-spec",
        url,
        "--auth",
        bearer,
        "--target",
        tool,
        "--input-json",
        json.dumps(payload, separators=(",", ":")),
        "--timeout",
        timeout,
        "--json",
    ]
    subprocess.run(cmd, check=True)


if __name__ == "__main__":
    main()
