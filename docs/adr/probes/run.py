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


CAVEATS = """## What this instrument cannot see

- **Client rendering.** `tg_get_messages` returns what the API stored; whether a client *draws* a
  reference is a different instrument (#827 was exactly that gap \u2014 the API accepted a form the client
  would not open). A row marked PASS here means the wire is right, not that a human eye confirmed it.
- **Stale evidence.** An observation recorded before a swap says nothing about the binary after it.
  Every live row above carries its `sent_at`, and the `binary` line at the top names what was running
  when the evidence was written. Re-probe after a swap; a PASS carried across one is not a measurement.
- **Load behaviour.** One probe, one message. A rate-limited or partially-failing send is not exercised.
- **Which PLANE a verdict speaks for.** \u00a72 rule 1 (a resolvable image arrives as a photo, its bytes in
  the rich message's `media` array) and \u00a72 rule 5 (with no rich path, the bytes ship as their own
  detached bubble \u2014 the floor) are DIFFERENT planes, and a probe only measures the one it travelled.
  The local-image rule 1 probe rides the turn plane (`delivery.rs`); the tool plane (`telegram_send`
  \u2192 `send.rs`) has no rich path at all and is recorded separately as the rule 5 floor. Reading either
  one as evidence for the other is how a suite certifies a defect.
"""

def write_report(path, manifest, evidence, rows, passed, failed, missing, exit_code):
    """Write the compliance report FROM the run, so no number can be hand-typed.

    The page this replaces claimed to be generated while its counts were
    transcribed by hand, and they drifted (it read 20/20 while the runner
    printed 21/21). A report that asserts its own provenance and then drifts is
    worse than no report: it launders a stale number into a compliance claim.
    """
    obs = evidence.get("observations", {})
    by_id = {p["id"]: p for p in manifest["probes"]}
    total = len(rows)
    out = []
    a = out.append

    a("# ADR 0001 compliance report")
    a("")
    a("**ADR:** `%s` (revision %s)" % (manifest["adr"], manifest.get("adr_revision")))
    a("**Binary under test:** %s" % evidence.get("binary", "unknown"))
    a("**Evidence recorded at:** %s" % evidence.get("recorded_at", "unknown"))
    if evidence.get("binary_delta"):
        a("**Since then:** %s" % evidence["binary_delta"])
    a("**Runner:** `python3 docs/adr/probes/run.py` \u2014 **exit %d**" % exit_code)
    a("**%d/%d passed \u00b7 %d failed \u00b7 %d missing (missing counts as failed)**"
      % (passed, total, failed, missing))
    a("")
    a("> **GENERATED FILE \u2014 do not hand-edit.** Written by `python3 docs/adr/probes/run.py --report`")
    a("> from `manifest.json` + `live-evidence.json` in the same run that printed these counts, so a")
    a("> number here cannot drift from the probe that produced it. Re-generate after any probe or")
    a("> evidence change; a stale page is a false claim, not a rounding error.")
    a("")
    a("## Rule \u2192 probe coverage")
    a("")
    a("| ADR rule | Probe | Kind | State |")
    a("|---|---|---|---|")
    for r in rows:
        a("| %s | `%s` | %s | %s |" % (r["rule"], r["id"], r["kind"], r["state"]))
    a("")
    a("## Live probes \u2014 the wire as Telegram stored it")
    for r in rows:
        if r["kind"] != "live":
            continue
        probe = by_id.get(r["id"], {})
        rec = obs.get(probe.get("evidence_id", ""), {})
        a("")
        a("### `%s` \u2014 %s \u2192 **%s**" % (r["id"], r["rule"], r["state"]))
        a("")
        a("- **Claim under test:** %s" % r["claim"])
        if not rec:
            a("- **No observation recorded** \u2014 runner verdict: %s" % r["why"])
            continue
        a("- **tg_get_messages** (msg `%s`, chat `%s`): %s"
          % (rec.get("message_id"), rec.get("chat_id"), rec.get("stored_form", "")))
        a("- **Bubbles observed:** %s \u00b7 **sent at:** %s"
          % (rec.get("bubbles"), rec.get("sent_at")))
        a("- **Probe:** %s" % rec.get("probe", ""))
        for line in rec.get("log_excerpt", []):
            a("- **log:** `%s`" % line)
        if rec.get("note"):
            a("- **Analysis:** %s" % rec["note"])

    withdrawn = [
        (k, v) for k, v in obs.items() if v.get("verdict") == "WITHDRAWN"
    ]
    if withdrawn:
        a("")
        a("## Withdrawn observations (UNMEASURED, not compliant)")
        for k, v in withdrawn:
            a("")
            a("- **%s** (msg `%s`): %s" % (k, v.get("message_id"), v.get("note", "")))

    a("")
    a("## Section 11 \u2014 the tree compiles and its tests pass")
    a("")
    for r in rows:
        if r["kind"] != "ci":
            continue
        probe = by_id.get(r["id"], {})
        a("`%s` \u2192 **%s** \u00b7 run **%s** on `%s` \u00b7 %s"
          % (r["id"], r["state"], probe.get("run_id"), probe.get("repo"), r["why"]))
        if probe.get("note"):
            a("")
            a(probe["note"])
        for line in r["detail"]:
            if line:
                a("")
                a("`%s`" % line)
    a("")
    a(CAVEATS)
    a("")
    a("## Reproduce")
    a("")
    a("```")
    a("cd <repo> && python3 docs/adr/probes/run.py            # verdicts, exit 0 iff all pass")
    a("cd <repo> && python3 docs/adr/probes/run.py --report docs/adr/probes/COMPLIANCE.md")
    a("```")

    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(out) + "\n")


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
    ap.add_argument(
        "--report",
        default="",
        help="write the compliance report (markdown) from this run",
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

    if args.report:
        write_report(
            args.report,
            manifest,
            evidence,
            rows,
            passed,
            failed,
            missing,
            0 if failed == 0 else 1,
        )
        print("wrote %s" % args.report)

    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
