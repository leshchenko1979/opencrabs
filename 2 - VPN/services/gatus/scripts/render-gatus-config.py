#!/usr/bin/env python3
"""Inject SSH private key into Gatus config at deploy time (key never committed)."""
from __future__ import annotations

import os
import sys
from pathlib import Path

try:
    import yaml
except ImportError:
    print("PyYAML required: pip3 install pyyaml", file=sys.stderr)
    sys.exit(1)


def main() -> None:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} path/to/config.yaml", file=sys.stderr)
        sys.exit(1)

    config_path = Path(sys.argv[1])
    key_path = Path(
        os.environ.get("GATUS_SSH_KEY", Path.home() / ".ssh" / "id_ed25519")
    ).expanduser()

    config = yaml.safe_load(config_path.read_text())
    private_key = key_path.read_text()

    for ep in config.get("endpoints", []):
        url = ep.get("url") or ""
        if not str(url).startswith("ssh://"):
            continue
        ssh = ep.setdefault("ssh", {})
        ssh["private-key"] = private_key

    sys.stdout.write(yaml.dump(config, default_flow_style=False, sort_keys=False))


if __name__ == "__main__":
    main()
