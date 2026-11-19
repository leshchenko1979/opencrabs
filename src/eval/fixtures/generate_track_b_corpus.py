#!/usr/bin/env python3
"""Assemble the Track B retrieval corpus from the binary's own reference docs.

Why a generator instead of a hand-written corpus: the fixture has to be a
faithful slice of what the product actually ships. `src/docs/**/*.md` is
compiled into the binary, so the corpus is derived from it rather than
invented, and regenerating after a doc change is one command instead of a
hand-merge.

Two rules matter, and both were learned the hard way on the first attempt:

1. A section must carry a BALANCED set of code fences. A fence left open makes
   the next document's `## ` headings look like fenced code, so the Rust
   splitter silently drops them — the first build of this corpus reported 292
   sections while the parser could only see 188, and 31 dataset labels resolved
   to nothing.
2. Balance AFTER truncation, never before. Bodies are capped at 2000 characters;
   truncating a fenced block opens a fence that was closed in the source.

Usage: python3 src/eval/fixtures/generate_track_b_corpus.py
"""

import collections
import glob
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
DOCS = os.path.join(ROOT, "src", "docs")
OUT = os.path.join(ROOT, "src", "eval", "fixtures", "track_b_corpus.md")

MAX_BODY = 2000

HEADER = """# Track B — reference documentation corpus

Generated from the binary's own embedded reference docs (`src/docs/**/*.md`) by
`src/eval/fixtures/generate_track_b_corpus.py` — do not edit by hand.

Each section heading carries its source document path so a label names exactly
one section; deeper headings are folded into the body so a section stays the
unit of retrieval. Every section carries a balanced set of code fences, and the
body cap is applied before the balance check, so a truncated block cannot leave
a fence open.
"""


def split_sections(text):
    """Split one document on `## ` headings, ignoring fences.

    Mirrors `brain_sections::split_sections` so the fixture is parsed the same
    way the product parses a memory file.
    """
    sections = []
    current = None
    in_fence = False
    for line in text.splitlines():
        if line.strip().startswith("```"):
            in_fence = not in_fence
        if not in_fence and line.startswith("## "):
            if current is not None:
                sections.append(current)
            current = {"heading": line[3:].strip(), "body": []}
        elif current is not None:
            current["body"].append(line)
    if current is not None:
        sections.append(current)
    return sections


def render_body(lines):
    """Fold deeper headings, cap the length, then balance the fences."""
    out = []
    fence = False
    for line in lines:
        if line.strip().startswith("```"):
            fence = not fence
        if not fence and line.startswith("###"):
            out.append("**" + line.lstrip("#").strip() + "**")
        else:
            out.append(line)
    body = "\n".join(out).strip()
    if len(body) > MAX_BODY:
        body = body[:MAX_BODY].rsplit("\n", 1)[0] + "\n…"
    # Count fence-OPENING LINES on the final text, never substring occurrences:
    # the parser toggles on a line whose stripped form starts with ``` and is
    # blind to ``` mentioned mid-line, so a count of substrings reports a parity
    # the parser does not share.
    toggles = sum(1 for line in body.splitlines() if line.strip().startswith("```"))
    if toggles % 2 == 1:
        body += "\n```"
    return body


def main():
    files = sorted(glob.glob(os.path.join(DOCS, "**", "*.md"), recursive=True))
    if not files:
        sys.exit(f"no documents under {DOCS}")

    parts = [HEADER]
    index = []
    for path in files:
        rel = os.path.relpath(path, DOCS)[: -len(".md")]
        with open(path, encoding="utf-8", errors="replace") as handle:
            text = handle.read()
        seen = collections.Counter()
        for section in split_sections(text):
            heading = section["heading"]
            seen[heading] += 1
            if seen[heading] > 1:
                heading = f"{heading} ({seen[heading]})"
            body = render_body(section["body"])
            if not body:
                continue
            sid = f"{rel} · {heading}"
            parts.append(f"## {sid}\n\n{body}\n")
            index.append(sid)

    if len(index) != len(set(index)):
        sys.exit("duplicate section ids — a label would be ambiguous")

    with open(OUT, "w", encoding="utf-8") as handle:
        handle.write("\n".join(parts))

    print(f"{len(index)} sections, {os.path.getsize(OUT)} bytes -> {OUT}")


if __name__ == "__main__":
    main()
