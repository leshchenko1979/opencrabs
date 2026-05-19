#!/usr/bin/env python3
"""tg-mcp client for OpenCrabs tools.toml and tgproxy — fastmcp call, lean stdout."""

import json
import logging
import os
import subprocess
import sys
import time
import tomllib
from logging.handlers import RotatingFileHandler

MCP_JSON = os.environ.get("TG_MCP_CONFIG", "/etc/tg-mcp/mcp.json")
DEFAULT_URL = "https://tg-mcp.l1979.ru/v1/mcp"
OPENCRABS_CONFIG = os.environ.get("OPENCRABS_CONFIG", "/root/.opencrabs/config.toml")
DEFAULT_TIMEOUT = 30
LOG_DIR = os.environ.get("TG_MCP_LOG_DIR", "/var/log/tg-mcp")
LOG_FILE = os.environ.get("TG_MCP_LOG_FILE", os.path.join(LOG_DIR, "tg-mcp-call.log"))

logger = logging.getLogger("tg-mcp-call")


def _preview_max_len() -> int:
    if os.environ.get("TG_MCP_LOG_LEVEL", "").upper() == "DEBUG":
        return int(os.environ.get("TG_MCP_PREVIEW_MAX", "2048"))
    return int(os.environ.get("TG_MCP_PREVIEW_MAX", "500"))


def _preview(text: str, max_len: int | None = None) -> str:
    limit = max_len if max_len is not None else _preview_max_len()
    if len(text) <= limit:
        return text
    return text[:limit] + "…"


def _setup_logging() -> None:
    if logger.handlers:
        return
    logger.setLevel(logging.DEBUG)
    formatter = logging.Formatter(
        "%(asctime)s %(levelname)s %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )
    level_name = os.environ.get("TG_MCP_LOG_LEVEL", "INFO").upper()
    handler_level = getattr(logging, level_name, logging.INFO)
    try:
        os.makedirs(LOG_DIR, mode=0o755, exist_ok=True)
        handler = RotatingFileHandler(
            LOG_FILE,
            maxBytes=2 * 1024 * 1024,
            backupCount=3,
            encoding="utf-8",
        )
        handler.setLevel(handler_level)
        handler.setFormatter(formatter)
        logger.addHandler(handler)
    except OSError as exc:
        sys.stderr.write(f"tg-mcp-call: file logging disabled ({exc})\n")


def _exit(code: int) -> None:
    logger.info("exit_code=%d", code)
    sys.exit(code)


def _read_server_url() -> str:
    if url := os.environ.get("TG_MCP_URL", "").strip():
        return url
    if os.path.isfile(MCP_JSON):
        with open(MCP_JSON, encoding="utf-8") as f:
            data = json.load(f)
        servers = data.get("mcpServers") or {}
        for entry in servers.values():
            if isinstance(entry, dict) and entry.get("url"):
                return str(entry["url"])
    return DEFAULT_URL


def _read_bearer() -> str:
    if bearer := os.environ.get("TG_MCP_BEARER", "").strip():
        return bearer
    if not os.path.isfile(OPENCRABS_CONFIG):
        return ""
    with open(OPENCRABS_CONFIG, "rb") as f:
        mcp = tomllib.load(f).get("mcp") or {}
    return str(mcp.get("bearer") or "").strip()


def _drop_nulls(value: object) -> object:
    """Remove dict keys with None — tg-mcp rejects null for optional ints."""
    if isinstance(value, dict):
        return {k: _drop_nulls(v) for k, v in value.items() if v is not None}
    if isinstance(value, list):
        return [_drop_nulls(item) for item in value]
    return value


def _is_json_object_or_array(text: str) -> bool:
    text = text.strip()
    if not text or text[0] not in "{[":
        return False
    try:
        json.loads(text)
        return True
    except json.JSONDecodeError:
        return False


def _log_fastmcp_failure(tool: str, proc: subprocess.CompletedProcess[str]) -> None:
    err = (proc.stderr or proc.stdout or "fastmcp failed").strip()
    logger.error(
        "fastmcp failed tool=%s fastmcp_rc=%d stderr_preview=%r",
        tool,
        proc.returncode,
        _preview(err, 400),
    )


def _run_fastmcp(
    tool: str, args_json: str, bearer: str, server_url: str, use_json: bool
) -> subprocess.CompletedProcess[str]:
    cmd = [
        "fastmcp",
        "call",
        "--server-spec",
        server_url,
        "--auth",
        bearer,
        "--target",
        tool,
        "--input-json",
        args_json,
        "--timeout",
        str(int(os.environ.get("TG_MCP_TIMEOUT", DEFAULT_TIMEOUT))),
    ]
    if use_json:
        cmd.append("--json")
    return subprocess.run(
        cmd, capture_output=True, text=True, timeout=DEFAULT_TIMEOUT + 10
    )


def _lean_stdout(
    proc: subprocess.CompletedProcess[str],
    tool: str,
    args_json: str,
    bearer: str,
    server_url: str,
) -> str:
    if proc.returncode != 0:
        _log_fastmcp_failure(tool, proc)
        err = (proc.stderr or proc.stdout or "fastmcp failed").strip()
        raise RuntimeError(err)

    out = proc.stdout or ""
    if _is_json_object_or_array(out):
        return out

    logger.info("fallback_used=true tool=%s", tool)
    proc2 = _run_fastmcp(tool, args_json, bearer, server_url, use_json=True)
    if proc2.returncode != 0:
        _log_fastmcp_failure(tool, proc2)
        err = (proc2.stderr or proc2.stdout or "fastmcp failed").strip()
        raise RuntimeError(err)

    envelope = json.loads(proc2.stdout)
    if envelope.get("is_error"):
        raise RuntimeError(json.dumps(envelope))

    structured = envelope.get("structured_content")
    if structured is not None:
        return json.dumps(structured, ensure_ascii=False)

    content = envelope.get("content") or []
    if content and content[0].get("type") == "text":
        text = content[0].get("text", "")
        if _is_json_object_or_array(text):
            return text
        return json.dumps({"result": text}, ensure_ascii=False)

    return proc2.stdout


def main() -> None:
    _setup_logging()

    if len(sys.argv) < 2:
        print("usage: tg-mcp-call <tool> [json]", file=sys.stderr)
        _exit(2)

    tool = sys.argv[1]
    raw = sys.argv[2] if len(sys.argv) > 2 else "{}"
    try:
        payload = json.loads(raw)
        payload = _drop_nulls(payload)
        if not isinstance(payload, dict):
            raise json.JSONDecodeError("expected JSON object", raw, 0)
        args_json = json.dumps(payload, separators=(",", ":"))
    except json.JSONDecodeError as exc:
        logger.error(
            "invalid json args tool=%s err=%s raw_len=%d raw_preview=%r",
            tool,
            exc,
            len(raw),
            _preview(raw),
        )
        print(json.dumps({"error": f"invalid json args: {exc}"}))
        _exit(1)

    bearer = _read_bearer()
    if not bearer:
        logger.error("missing bearer tool=%s config_path=%s", tool, OPENCRABS_CONFIG)
        print(
            json.dumps({"error": "TG_MCP_BEARER unset and no [mcp] bearer in config"})
        )
        _exit(1)

    server_url = _read_server_url()
    logger.info(
        "call start tool=%s url=%s args=%s",
        tool,
        server_url,
        _preview(args_json),
    )
    started = time.monotonic()

    try:
        proc = _run_fastmcp(tool, args_json, bearer, server_url, use_json=False)
        result = _lean_stdout(proc, tool, args_json, bearer, server_url)
        elapsed_ms = int((time.monotonic() - started) * 1000)
        logger.info(
            "call ok tool=%s elapsed_ms=%d out_bytes=%d",
            tool,
            elapsed_ms,
            len(result.encode("utf-8")),
        )
        sys.stdout.write(result)
        _exit(0)
    except subprocess.TimeoutExpired:
        elapsed_ms = int((time.monotonic() - started) * 1000)
        logger.error("call timeout tool=%s elapsed_ms=%d", tool, elapsed_ms)
        print(json.dumps({"error": "fastmcp call timed out"}))
        _exit(1)
    except Exception as exc:
        elapsed_ms = int((time.monotonic() - started) * 1000)
        err_msg = _preview(str(exc), 800)
        logger.error(
            "call failed tool=%s elapsed_ms=%d error=%s", tool, elapsed_ms, err_msg
        )
        print(json.dumps({"error": str(exc)}))
        _exit(1)


if __name__ == "__main__":
    main()
