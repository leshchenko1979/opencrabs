#!/usr/bin/env python3
"""Inject SSH private key into Gatus config at deploy time (key never committed).

Line-based injection — preserves YAML structure and custom A2A JSON body verbatim.
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path


def inject_private_keys(config_text: str, private_key: str) -> str:
    key_lines = private_key.strip().splitlines()
    out: list[str] = []
    lines = config_text.splitlines(keepends=True)
    i = 0
    while i < len(lines):
        line = lines[i]
        out.append(line)
        if re.match(r"^      username:", line):
            window = "".join(lines[max(0, i - 8) : i + 1])
            if "ssh:" not in window:
                i += 1
                continue
            if i + 1 < len(lines) and re.match(r"^      private-key:", lines[i + 1]):
                i += 1
                continue
            out.append("      private-key: |\n")
            for kl in key_lines:
                out.append(f"        {kl}\n")
        i += 1
    return "".join(out)


def main() -> None:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} path/to/config.yaml", file=sys.stderr)
        sys.exit(1)

    config_path = Path(sys.argv[1])
    key_path = Path(
        os.environ.get("GATUS_SSH_KEY", Path.home() / ".ssh" / "id_ed25519")
    ).expanduser()

    sys.stdout.write(inject_private_keys(config_path.read_text(), key_path.read_text()))


if __name__ == "__main__":
    main()
