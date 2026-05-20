#!/usr/bin/env python3
"""Normalize provider API keys in keys.toml and sync all providers across profiles."""

from __future__ import annotations

import re
import sys
from pathlib import Path

PROVIDERS = ("minimax", "openrouter", "gemini")
LEGACY_SECTIONS = {
    "minimax": ("providers.minimax", "minimax"),
    "openrouter": ("providers.openrouter", "openrouter"),
    "gemini": ("providers.gemini", "google"),
}
PROVIDER_KEY_SECTIONS = {f"providers.{name}" for name in PROVIDERS} | set(PROVIDERS)

DEFAULT_KEYS = Path("/root/.opencrabs/keys.toml")
OPS_KEYS = Path("/root/.opencrabs/profiles/ops/keys.toml")
DEFAULT_CONFIG = Path("/root/.opencrabs/config.toml")
OPS_CONFIG = Path("/root/.opencrabs/profiles/ops/config.toml")

OPENROUTER_CONFIG_BLOCK = """
[providers.openrouter]
enabled = false
default_model = "openai/gpt-oss-120b:free"
"""


def extract_key(content: str, sections: tuple[str, ...]) -> str | None:
    for section in sections:
        pattern = rf"^\[{re.escape(section)}\]\s*\n(.*?)(?=^\[|\Z)"
        match = re.search(pattern, content, re.MULTILINE | re.DOTALL)
        if not match:
            continue
        key_match = re.search(r'^api_key\s*=\s*"([^"]+)"', match.group(1), re.MULTILINE)
        if key_match:
            return key_match.group(1)
    return None


def read_provider_keys(path: Path) -> dict[str, str | None]:
    content = path.read_text()
    return {name: extract_key(content, LEGACY_SECTIONS[name]) for name in PROVIDERS}


def strip_provider_sections(content: str) -> list[str]:
    preserved: list[str] = []
    lines = content.splitlines()
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped[1:-1]
            if section in PROVIDER_KEY_SECTIONS or section.startswith("providers."):
                index += 1
                while index < len(lines):
                    next_stripped = lines[index].strip()
                    if next_stripped.startswith("[") and next_stripped.endswith("]"):
                        break
                    index += 1
                continue
        preserved.append(lines[index])
        index += 1
    return preserved


def write_provider_keys(
    path: Path, keys: dict[str, str | None], header: str = ""
) -> dict[str, bool]:
    content = path.read_text() if path.exists() else ""
    preserved = strip_provider_sections(content)

    output_lines: list[str] = []
    if header:
        output_lines.append(header.rstrip())
        output_lines.append("")
    output_lines.extend(preserved)
    while output_lines and not output_lines[-1].strip():
        output_lines.pop()

    output = "\n".join(output_lines).rstrip() + "\n\n"
    present: dict[str, bool] = {}
    for name in PROVIDERS:
        key = keys.get(name)
        if not key:
            present[name] = False
            continue
        output += f"[providers.{name}]\n"
        output += f'api_key = "{key}"\n\n'
        present[name] = True

    path.write_text(output.rstrip() + "\n")
    path.chmod(0o600)
    return present


def fix_config_google_alias(path: Path) -> bool:
    content = path.read_text()
    if "[providers.google]" not in content:
        return False
    content = content.replace("[providers.google]", "[providers.gemini]")
    content = re.sub(
        r"\n\[providers\.gemini\]\nenabled = false\n",
        "\n",
        content,
        count=1,
    )
    path.write_text(content)
    return True


def ensure_openrouter_config(path: Path) -> bool:
    content = path.read_text()
    if "[providers.openrouter]" in content:
        return False
    marker = "[providers.minimax]"
    if marker not in content:
        return False
    insert_at = content.index(marker)
    end = content.find("\n[", insert_at + 1)
    if end == -1:
        end = len(content)
    updated = content[:end] + OPENROUTER_CONFIG_BLOCK + content[end:]
    path.write_text(updated)
    return True


def main() -> int:
    if not DEFAULT_KEYS.exists():
        print(f"ERROR: missing {DEFAULT_KEYS}", file=sys.stderr)
        return 1

    canonical = read_provider_keys(DEFAULT_KEYS)
    write_provider_keys(DEFAULT_KEYS, canonical)

    if OPS_KEYS.exists():
        write_provider_keys(OPS_KEYS, canonical)
        print(
            f"synced ops keys from default: { {k: bool(v) for k, v in canonical.items()} }"
        )
    else:
        print(f"WARN: missing {OPS_KEYS}", file=sys.stderr)

    for path in (DEFAULT_CONFIG, OPS_CONFIG):
        if not path.exists():
            print(f"SKIP missing {path}", file=sys.stderr)
            continue
        alias_fixed = fix_config_google_alias(path)
        openrouter_added = ensure_openrouter_config(path)
        print(
            f"config {path}: google→gemini={alias_fixed}, openrouter_added={openrouter_added}"
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
