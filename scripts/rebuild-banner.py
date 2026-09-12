#!/usr/bin/env python3
"""Rebuild docs/pir-banner.png from docs/pir.png.

The banner is 25% larger than eighth-size (scale 5/32): 308x371 -> 48x58.
Downscaling a black-on-transparent silhouette with LANCZOS leaves bright
fringe pixels on the silhouette edge (ringing + greenish blend tints),
which render as a white outline on dark terminals. This script despills
them: iterate to fixpoint, recoloring every bright edge pixel (Rec. 601
luma > 90, 8-connected to transparency) toward the mean of its dark
opaque neighbours. Pixels are only recolored, never removed, so the
silhouette keeps its shape and anti-aliasing.

Requires: pillow, numpy, pngcrush. Run from the repo root:
    python3 scripts/rebuild-banner.py
"""

import shutil
import subprocess
import sys
from pathlib import Path

from PIL import Image
import numpy as np

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "docs" / "pir.png"
DST = ROOT / "docs" / "pir-banner.png"

# Eighth-size (1/8) plus 25%: 5/32.
SCALE = 5 / 32
LUMA_CUT = 90
ALPHA_CUT = 128
MAX_ROUNDS = 20


def luma(px: np.ndarray) -> np.ndarray:
    return (299 * px[..., 0] + 587 * px[..., 1] + 114 * px[..., 2]) // 1000


def main() -> int:
    if not SRC.exists():
        print(f"missing source: {SRC}", file=sys.stderr)
        return 1
    for tool in ("pngcrush",):
        if shutil.which(tool) is None:
            print(f"missing required tool: {tool}", file=sys.stderr)
            return 1

    src = Image.open(SRC).convert("RGBA")
    tw = int(src.width * SCALE + 0.5)
    th = int(src.height * SCALE + 0.5)
    print(f"source {src.width}x{src.height} -> banner {tw}x{th}")

    out = np.asarray(src.resize((tw, th), Image.LANCZOS)).copy().astype(int)
    H, W = out.shape[:2]
    total_fixed = 0
    for rnd in range(MAX_ROUNDS):
        lum = luma(out)
        op = out[..., 3] >= ALPHA_CUT
        t = ~op
        # 8-connectivity transparency adjacency.
        nt = np.zeros_like(t)
        nt[1:, :] |= t[:-1, :]
        nt[:-1, :] |= t[1:, :]
        nt[:, 1:] |= t[:, :-1]
        nt[:, :-1] |= t[:, 1:]
        nt[1:, 1:] |= t[:-1, :-1]
        nt[1:, :-1] |= t[:-1, 1:]
        nt[:-1, 1:] |= t[1:, :-1]
        nt[:-1, :-1] |= t[1:, 1:]
        ys, xs = np.where(op & nt & (lum > LUMA_CUT))
        fixed = 0
        for y, x in zip(ys.tolist(), xs.tolist()):
            acc = np.zeros(3)
            n = 0
            for dy in (-1, 0, 1):
                for dx in (-1, 0, 1):
                    if dx == 0 and dy == 0:
                        continue
                    ny, nx = y + dy, x + dx
                    if 0 <= ny < H and 0 <= nx < W and op[ny, nx] and lum[ny, nx] <= LUMA_CUT:
                        acc += out[ny, nx, :3]
                        n += 1
            if n:
                out[y, x, :3] = (acc / n).astype(int)
                fixed += 1
        total_fixed += fixed
        print(f"round {rnd}: candidates={len(ys)} fixed={fixed}")
        if fixed == 0:
            break
    else:
        print("warning: despill did not reach fixpoint", file=sys.stderr)

    print(f"recolored {total_fixed} fringe pixels, shape preserved "
          f"(opaque={(out[..., 3] >= ALPHA_CUT).sum()})")
    tmp = Path("/tmp/pir-banner-next.png")
    Image.fromarray(out.astype(np.uint8), "RGBA").save(tmp, optimize=True)
    subprocess.run(
        ["pngcrush", "-brute", "-q", str(tmp), str(DST)], check=True,
    )
    print(f"wrote {DST} ({DST.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
