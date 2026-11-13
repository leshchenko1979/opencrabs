#!/usr/bin/env python3
"""Run the ADR 0001 probe manifest.

Every normative rule in `docs/adr/0001-telegram-local-file-and-image-delivery.md`
maps to at least one probe here. Three kinds:

  static  a shell command run from the repo root; passes on exit 0. It asserts
          the code that implements the rule is present and shaped as the rule
          says (e.g. a document is emitted in LINK syntax, never image syntax).

  live    a message sent through the real daemon delivery path and read back
          with tg_get_messages. The observation is recorded in
          `live-evidence.json` with the message id, the stored form and the log
          line that accompanied it; this runner re-validates that record, so a
          claim without evidence is a FAIL, and a stale evidence file is caught
          by the `recorded_at` stamp it prints.

  ci      a GitHub Actions conclusion, read from the API.

Exit 0 iff every probe passes. A probe with no evidence at all reports MISSING
and fails, because "not yet measured" is not "compliant".

Usage:
    python3 docs/adr/probes/run.py [--repo DIR] [--json OUT] [--allow-missing]
"""

import argparse
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
MANIFEST = os.path.join(HERE, "manifest.json")
EVIDENCE = os.path.join(HERE, "live-evidence.json")


def load(path):
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)


def run_static(cmd, repo):
    proc = subprocess.run(
        ["bash", "-c", cmd], cwd=repo, capture_output=True, text=True
    )
    detail = (proc.stdout + proc.stderr).strip().splitlines()
    return proc.returncode == 0, f"exit {proc.returncode}", detail[:4]


def run_ci(probe):
    run_id = probe.get("run_id") or 0
    if not run_id:
        return None, "no run id recorded", []
    repo = probe.get("repo", "leshchenko1979/opencrabs")
    proc = subprocess.run(
        [
            "gh", "run", "view", str(run_id), "-R", repo,
            "--json", "status,conclusion",
            "--jq", '.status + " " + (.conclusion // "-")',
        ],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        return False, f"gh failed: {proc.stderr.strip()[:120]}", []
    verdict = proc.stdout.strip().replace("\r", "")
    return verdict == "completed success", f"run {run_id}: {verdict}", [verdict]


def check_fixture(probe, repo):
    """Structurally validate a live probe's input file.

    A live probe is only as good as the file it feeds the delivery path, and
    `file(1)` is not an instrument for this: it reads the PNG IHDR and stops,
    so it reports `PNG image data, 16 x 16, 8-bit/color RGBA` for a file whose
    IEND chunk is displaced by one stray byte. Measured 2026-11-13: the
    `live-l6` probe recorded `116 bytes, valid` from exactly that output, and
    the `IMAGE_PROCESS_FAILED` it reported as a product defect was the API
    correctly refusing a truncated PNG (#829, withdrawn).

    So the checks are structural: the signature, every chunk's CRC32, the file
    ending exactly on the IEND chunk, and — for a non-interlaced truecolour PNG
    — the IDAT inflating to exactly `h * (1 + w * 4)` bytes.
    """
    import binascii
    import struct
    import zlib

    path = os.path.join(repo, probe["path"])
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError as exc:
        return False, f"unreadable: {exc}", [path]
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        return False, "not a PNG signature", [path]

    offset = 8
    idat = b""
    chunks = []
    while offset < len(data):
        if offset + 8 > len(data):
            return False, f"truncated header at {offset}", [f"size {len(data)}"]
        (length,) = struct.unpack(">I", data[offset : offset + 4])
        kind = data[offset + 4 : offset + 8]
        end = offset + 12 + length
        if end > len(data):
            return False, (
                f"chunk {kind!r} declares {length} bytes at {offset}, "
                f"file ends at {len(data)}"
            ), [f"size {len(data)}"]
        payload = data[offset + 8 : offset + 8 + length]
        (crc,) = struct.unpack(">I", data[offset + 8 + length : end])
        if crc != (binascii.crc32(kind + payload) & 0xFFFFFFFF):
            return False, f"CRC mismatch in {kind!r}", [f"size {len(data)}"]
        chunks.append(kind.decode("ascii", "replace"))
        if kind == b"IDAT":
            idat += payload
        offset = end
    if offset != len(data):
        return False, f"trailing bytes after last chunk ({len(data) - offset})", []
    if chunks[-1:] != ["IEND"]:
        return False, f"does not end on IEND (ends {chunks[-1:]})", []

    width, height, depth, colour, _, _, interlace = struct.unpack(
        ">IIBBBBB", data[16:29]
    )
    detail = [
        f"{width}x{height} depth={depth} colour={colour} interlace={interlace}",
        f"{len(data)} bytes, chunks {chunks}",
    ]
    if interlace == 0 and colour == 6 and depth == 8:
        try:
            raw = zlib.decompress(idat)
        except zlib.error as exc:
            return False, f"IDAT does not inflate: {exc}", detail
        expected = height * (1 + width * 4)
        if len(raw) != expected:
            return False, f"inflates to {len(raw)}, expected {expected}", detail
        detail.append(f"inflates to {len(raw)} = expected")
    return True, f"{width}x{height}, CRCs OK, ends on IEND", detail


def check_live(probe, evidence):
    """A live probe passes only on a recorded observation that names the wire.

    The record must carry: the chat and message id it was read back from, the
    stored form observed, and at least one log excerpt or API field. A record
    whose `verdict` is not PASS fails; an absent record is MISSING, not a pass.
    """
    entries = evidence.get("observations", {})
    key = probe.get("evidence_id", "")
    rec = entries.get(key)
    if not rec:
        return None, f"no observation recorded for {key}", []
    verdict = rec.get("verdict")
    # A WITHDRAWN record is neither a pass nor a violation: the measurement it
    # rested on was itself faulty (e.g. a corrupt probe fixture), so the rule is
    # UNMEASURED and owes a fresh probe. Reading it as PASS would launder a bad
    # measurement into compliance; reading it as FAIL would keep a defect open
    # that the bytes do not support.
    if verdict == "WITHDRAWN":
        return None, f"{key} withdrawn — owes a re-probe", [rec.get("note", "")[:120]]
    ok = verdict == "PASS"
    detail = [
        f"msg {rec.get('message_id')} @ {rec.get('chat_id')}",
        f"stored: {rec.get('stored_form', '')[:90]}",
    ]
    for line in rec.get("log_excerpt", [])[:2]:
        detail.append(f"log: {line[:90]}")
    if rec.get("note"):
        detail.append(rec["note"][:120])
    return ok, f"{key} verdict {rec.get('verdict')}", detail


def main():
    ap = argparse.ArgumentParser()
    # HERE is <repo>/docs/adr/probes, so the repo root is three levels up.
    ap.add_argument("--repo", default=os.path.dirname(os.path.dirname(os.path.dirname(HERE))))
    ap.add_argument("--json", default="")
    ap.add_argument(
        "--allow-missing",
        action="store_true",
        help="report MISSING without failing (pre-ship run)",
    )
    args = ap.parse_args()

    manifest = load(MANIFEST)
    evidence = load(EVIDENCE) if os.path.exists(EVIDENCE) else {"observations": {}}

    rows = []
    failed = 0
    missing = 0
    for probe in manifest["probes"]:
        kind = probe["kind"]
        if kind == "static":
            ok, why, detail = run_static(probe["cmd"], args.repo)
        elif kind == "live":
            ok, why, detail = check_live(probe, evidence)
        elif kind == "fixture":
            ok, why, detail = check_fixture(probe, args.repo)
        elif kind == "ci":
            ok, why, detail = run_ci(probe)
        else:
            ok, why, detail = False, f"unknown kind {kind}", []

        if ok is None:
            state = "MISSING"
            missing += 1
            if not args.allow_missing:
                failed += 1
        elif ok:
            state = "PASS"
        else:
            state = "FAIL"
            failed += 1
        rows.append(
            {
                "id": probe["id"],
                "rule": probe["rule"],
                "claim": probe["claim"],
                "kind": kind,
                "state": state,
                "why": why,
                "detail": detail,
            }
        )

    width = max(len(r["id"]) for r in rows) + 2
    for r in rows:
        print(f"{r['state']:<8}{r['id']:<{width}}{r['rule']:<16}{r['why']}")
        for line in r["detail"]:
            if line:
                print(f"{'':<8}  · {line}")

    total = len(rows)
    passed = total - failed - (missing if args.allow_missing else 0)
    print(
        f"\n{passed}/{total} passed · {failed} failed · {missing} missing"
        + ("" if args.allow_missing else " (missing counts as failed)")
    )
    if evidence.get("recorded_at"):
        print(f"live evidence recorded at {evidence['recorded_at']}")

    if args.json:
        with open(args.json, "w", encoding="utf-8") as fh:
            json.dump(
                {
                    "adr": manifest["adr"],
                    "adr_revision": manifest.get("adr_revision"),
                    "generated_at": evidence.get("recorded_at"),
                    "rows": rows,
                    "passed": passed,
                    "failed": failed,
                    "missing": missing,
                },
                fh,
                indent=2,
            )
        print(f"wrote {args.json}")

    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
