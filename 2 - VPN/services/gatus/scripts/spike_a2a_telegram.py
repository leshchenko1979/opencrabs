#!/usr/bin/env python3
"""Spike: Gatus-style trigger via OpenCrabs ops A2A + Telegram surfacing via AGENTS.md.

Run from Mac (repo root or script dir). Secrets read from Hermes at runtime — never logged.

Usage:
  spike_a2a_telegram.py health
  spike_a2a_telegram.py sessions
  spike_a2a_telegram.py baseline [--endpoint host-box5-spike]
  spike_a2a_telegram.py a2a-trigger [--endpoint host-box5-spike]
  spike_a2a_telegram.py stream-capture [--endpoint host-box5-spike]
  spike_a2a_telegram.py ssh-path
  spike_a2a_telegram.py poll-task <task_id>
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from typing import Any

ALIASES = {
    "host-box2": "vpn",
    "host-box3": "apps",
    "host-box4": "n8n",
    "host-box5": "local",
    "host-box2-spike": "vpn",
    "host-box3-spike": "apps",
    "host-box4-spike": "n8n",
    "host-box5-spike": "local",
}

A2A_PORT = 18791
A2A_INGRESS_POINTER = (
    'Follow AGENTS.md "[Gatus] alerts" '
    "(load_brain_file AGENTS.md + MEMORY.md if not loaded)."
)

SSH_HERMES = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "hermes"]
SSH_VPN = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "vpn"]


def log(msg: str) -> None:
    print(f"[spike] {msg}", flush=True)


def hermes_cmd(script: str) -> str:
    r = subprocess.run(
        SSH_HERMES + [script],
        capture_output=True,
        text=True,
        timeout=600,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"hermes failed ({r.returncode}): {r.stderr.strip() or r.stdout}"
        )
    return r.stdout


def vpn_cmd(script: str) -> str:
    r = subprocess.run(
        SSH_VPN + [script],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"vpn failed ({r.returncode}): {r.stderr.strip() or r.stdout}"
        )
    return r.stdout


def fetch_a2a_api_key() -> str:
    """Return Bearer token if [a2a] api_key is set; empty string if loopback auth is disabled."""
    out = hermes_cmd(
        "awk '/^\\[a2a\\]/ { in_a=1; next } /^\\[/ { in_a=0 } "
        'in_a && /^api_key/ { gsub(/.*=[[:space:]]*"/, ""); gsub(/".*$/, ""); print; exit }\' '
        "/root/.opencrabs/profiles/ops/config.toml /root/.opencrabs/profiles/ops/keys.toml 2>/dev/null"
    )
    return out.strip()


def build_gatus_text(
    endpoint: str,
    *,
    include_ingress_pointer: bool,
    synthetic: bool = True,
) -> str:
    base = endpoint.replace("-spike", "") if synthetic else endpoint
    alias = ALIASES.get(endpoint, ALIASES.get(base, "unknown"))
    lines = [
        f"[Gatus] {endpoint} TRIGGERED",
        f"SSH: {alias}",
        "Conditions: [SPIKE] synthetic test — ignore production impact",
        "Errors: spike harness",
    ]
    if include_ingress_pointer:
        lines.append(A2A_INGRESS_POINTER)
    else:
        lines.append(
            "Run: git -C /root/vds-servers pull --ff-only; host-diag; investigate. "
            "Fix if safe or ask me."
        )
    return "\n".join(lines)


def a2a_jsonrpc(method: str, params: dict[str, Any], *, api_key: str) -> dict[str, Any]:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    remote_path = "/tmp/spike-a2a-request.json"
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        f.write(body)
        local_path = f.name
    try:
        subprocess.run(
            ["scp", "-o", "BatchMode=yes", local_path, f"hermes:{remote_path}"],
            check=True,
            capture_output=True,
            text=True,
        )
        auth = f"-H 'Authorization: Bearer {api_key}' " if api_key else ""
        out = hermes_cmd(
            f"curl -sS -X POST http://127.0.0.1:{A2A_PORT}/a2a/v1 "
            f"-H 'Content-Type: application/json' {auth}"
            f"-d @{remote_path}"
        )
    finally:
        os.unlink(local_path)
    try:
        return json.loads(out)
    except json.JSONDecodeError as e:
        raise RuntimeError(f"non-JSON A2A response: {out[:500]}") from e


def extract_task_id(resp: dict[str, Any]) -> str | None:
    result = resp.get("result")
    if isinstance(result, dict):
        for key in ("id", "taskId", "task_id"):
            if key in result and result[key]:
                return str(result[key])
        task = result.get("task")
        if isinstance(task, dict) and task.get("id"):
            return str(task["id"])
    return None


def poll_task(
    task_id: str, api_key: str, *, max_wait: int = 600, interval: int = 10
) -> dict[str, Any]:
    deadline = time.time() + max_wait
    last: dict[str, Any] = {}
    while time.time() < deadline:
        last = a2a_jsonrpc("tasks/get", {"id": task_id}, api_key=api_key)
        state = ""
        if isinstance(last.get("result"), dict):
            res = last["result"]
            st = res.get("status")
            if isinstance(st, dict):
                state = str(st.get("state", "")).lower()
            else:
                state = str(st or res.get("state", "")).lower()
        log(f"tasks/get {task_id} state={state or '?'}")
        if state in ("completed", "failed", "canceled", "cancelled", "done"):
            return last
        time.sleep(interval)
    return last


def cmd_health() -> None:
    out = hermes_cmd(f"curl -sS http://127.0.0.1:{A2A_PORT}/a2a/health")
    print(out)


def cmd_sessions() -> None:
    out = hermes_cmd("opencrabs -p ops session list 2>/dev/null | tail -20")
    print(out)


def cmd_baseline(endpoint: str) -> None:
    api_key = fetch_a2a_api_key()
    log("sessions before:")
    cmd_sessions()
    text = build_gatus_text(endpoint, include_ingress_pointer=False)
    t0 = time.time()
    resp = a2a_jsonrpc(
        "message/send",
        {"message": {"role": "user", "parts": [{"text": text}]}},
        api_key=api_key,
    )
    log(f"message/send response ({time.time() - t0:.1f}s):")
    print(json.dumps(resp, indent=2))
    task_id = extract_task_id(resp)
    if task_id:
        log(f"task_id={task_id}")
        final = poll_task(task_id, api_key, max_wait=300)
        print(json.dumps(final, indent=2))
    log("sessions after:")
    cmd_sessions()
    log(
        "Watch Telegram @redevest_admin_tools_bot — baseline expects silence without A2A ingress."
    )


def cmd_a2a_trigger(endpoint: str) -> None:
    api_key = fetch_a2a_api_key()
    log("sessions before:")
    cmd_sessions()
    text = build_gatus_text(endpoint, include_ingress_pointer=True)
    t0 = time.time()
    resp = a2a_jsonrpc(
        "message/send",
        {"message": {"role": "user", "parts": [{"text": text}]}},
        api_key=api_key,
    )
    log(f"message/send ({time.time() - t0:.1f}s):")
    print(json.dumps(resp, indent=2))
    task_id = extract_task_id(resp)
    if not task_id:
        log("no task_id in response — check JSON manually")
        return
    log(f"task_id={task_id} — note Telegram ack time manually")
    t_ack = time.time()
    final = poll_task(task_id, api_key, max_wait=600, interval=15)
    elapsed = time.time() - t0
    log(f"task finished in {elapsed:.0f}s")
    print(json.dumps(final, indent=2))
    log("sessions after:")
    cmd_sessions()


def cmd_stream_capture(endpoint: str, out_path: str) -> None:
    api_key = fetch_a2a_api_key()
    text = build_gatus_text(endpoint, include_ingress_pointer=True)
    resp = a2a_jsonrpc(
        "message/stream",
        {"message": {"role": "user", "parts": [{"text": text}]}},
        api_key=api_key,
    )
    remote_out = "/tmp/a2a-spike-events.jsonl"
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(json.dumps(resp, indent=2))
        f.write("\n")
    log(f"stream response saved to {out_path} (if not SSE, inspect JSON)")
    subprocess.run(
        ["scp", "-o", "BatchMode=yes", f"hermes:{remote_out}", out_path + ".remote"],
        check=False,
    )


def cmd_ssh_path() -> None:
    log("H4: VPN -> Hermes SSH -> A2A health (fleet key on VPN)")
    health = vpn_cmd(
        "ssh -o BatchMode=yes -o ConnectTimeout=15 -p 18718 "
        "-i /root/.ssh/id_ed25519 root@132.243.213.9 "
        f"'curl -sS --max-time 10 http://127.0.0.1:{A2A_PORT}/a2a/health'"
    )
    print(health)
    log("H4: VPN docker gateway -> tunneled A2A health")
    tun = vpn_cmd(f"curl -sS --max-time 10 http://172.18.0.1:{A2A_PORT}/a2a/health")
    print(tun)


def cmd_poll_task(task_id: str) -> None:
    api_key = fetch_a2a_api_key()
    print(json.dumps(poll_task(task_id, api_key, max_wait=60, interval=5), indent=2))


def main() -> None:
    p = argparse.ArgumentParser(description="A2A + Telegram surfacing spike harness")
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("health", help="ops A2A health on Hermes")
    sub.add_parser("sessions", help="opencrabs -p ops session list (tail)")

    b = sub.add_parser("baseline", help="A2A send without A2A ingress pointer")
    b.add_argument("--endpoint", default="host-box5-spike")

    t = sub.add_parser("a2a-trigger", help="thin trigger + AGENTS.md pointer")
    t.add_argument("--endpoint", default="host-box5-spike")

    s = sub.add_parser("stream-capture", help="fallback: capture message/stream SSE")
    s.add_argument("--endpoint", default="host-box5-spike")
    s.add_argument(
        "--out",
        default=os.path.expanduser("~/a2a-spike-events.jsonl"),
    )

    sub.add_parser("ssh-path", help="test VPN->Hermes->A2A reachability")

    pt = sub.add_parser("poll-task")
    pt.add_argument("task_id")

    args = p.parse_args()
    try:
        if args.cmd == "health":
            cmd_health()
        elif args.cmd == "sessions":
            cmd_sessions()
        elif args.cmd == "baseline":
            cmd_baseline(args.endpoint)
        elif args.cmd == "a2a-trigger":
            cmd_a2a_trigger(args.endpoint)
        elif args.cmd == "stream-capture":
            cmd_stream_capture(args.endpoint, args.out)
        elif args.cmd == "ssh-path":
            cmd_ssh_path()
        elif args.cmd == "poll-task":
            cmd_poll_task(args.task_id)
    except (RuntimeError, subprocess.TimeoutExpired) as e:
        sys.exit(f"ERROR: {e}")


if __name__ == "__main__":
    main()
