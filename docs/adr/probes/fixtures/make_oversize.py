#!/usr/bin/env python3
"""Generate the oversize-image fixture (>10 MiB) outside the repo.

The floor's size split (<=10 MiB -> `sendPhoto`, above -> `sendDocument`) needs a
structurally VALID image past the photo ceiling. A 12 MiB PNG does not belong in
git, so it is generated here and the probe reads it from disk: the fixture probe
`fixture-oversize-photo-valid` validates it structurally on every run, so a stale
or corrupt copy fails loudly instead of certifying the rule.

    python3 docs/adr/probes/fixtures/make_oversize.py [/tmp/oc-probes/oversize-photo.png]
"""
import os
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/oc-probes/oversize-photo.png"
CEILING = 10 * 1024 * 1024


def main() -> int:
    import numpy as np
    from PIL import Image

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    rng = np.random.default_rng(823)  # noise: incompressible, so the size is real
    arr = rng.integers(0, 256, size=(2048, 2048, 3), dtype=np.uint8)
    Image.fromarray(arr, "RGB").save(OUT, optimize=False)
    size = os.path.getsize(OUT)
    print(f"{OUT}: {size} bytes ({size / 1048576:.2f} MiB)")
    if size <= CEILING:
        print(f"UNDER the {CEILING}-byte ceiling — regenerate with a bigger array", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
