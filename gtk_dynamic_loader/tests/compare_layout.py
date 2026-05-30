#!/usr/bin/env python3
"""Compare widget layout geometries between GTK3 and GTK4 runs.

Usage:
    python3 compare_layout.py

Runs `cargo test --test layout_test` twice (default/GTK4 and GTK3),
parses the JSON output, and reports any positions or sizes that differ
between the two GTK versions beyond the given tolerance.

Widgets with `hexpand=True` in GTK3 (like buttons and fx_label) are
expected to be wider; these are listed separately.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GTK4_CANDIDATES = ["libgtk-4.so", "libgtk-4.so.1"]
GTK3_CANDIDATES = ["libgtk-3.so", "libgtk-3.so.0"]
TOLERANCE_PX = 1

# Widgets expected to be wider in GTK3 due to pack_start(expand=1, fill=1)
HEXPAND_WIDGETS = {"open_btn", "save_btn", "fx_label"}


def run_cargo(env_extra: dict) -> str:
    cmd = ["cargo", "test", "--test", "layout_test", "--", "--nocapture"]
    env = {**subprocess.os.environ, **env_extra}
    proc = subprocess.run(
        cmd,
        cwd=str(ROOT),
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    return proc.stdout


def extract_json(output: str) -> dict:
    """Find the first JSON object in stdout after LAYOUT_RESULT marker."""
    start = output.find("{")
    end = output.rfind("}")
    if start == -1 or end == -1:
        raise RuntimeError("No JSON found in output")
    return json.loads(output[start:end+1])


def parent_relative(widget: str, data: dict) -> dict:
    """Compute position relative to parent.

    GTK4 returns coordinates relative to the immediate parent.
    GTK3 includes ancestor offsets, so we subtract parent coords.
    """
    w = data[widget]
    version = data["version"]
    if version == "gtk4":
        return {"x": w["x"], "y": w["y"], "width": w["width"], "height": w["height"]}
    else:
        return {
            "x": w["x"] - w["parent_x"],
            "y": w["y"] - w["parent_y"],
            "width": w["width"],
            "height": w["height"],
        }


def main():
    print("=" * 60)
    print("GTK Layout Comparison Test")
    print("=" * 60)

    # Run GTK3
    print("\n[1/2] Running with default GTK (expected GTK4)...")
    gtk4_out = run_cargo({})
    gtk4 = extract_json(gtk4_out)
    gtk4_ver = gtk4.get("version", "unknown")
    print(f"      Detected: {gtk4_ver}")

    # Run GTK3
    print("[2/2] Running with GTK_DLOPEN_PREFER_GTK3=1...")
    gtk3_out = run_cargo({"GTK_DLOPEN_PREFER_GTK3": "1"})
    gtk3 = extract_json(gtk3_out)
    gtk3_ver = gtk3.get("version", "unknown")
    print(f"      Detected: {gtk3_ver}")

    if gtk4_ver == gtk3_ver:
        print("\n⚠  Both runs loaded the same GTK version — check your environment.")
        sys.exit(1)

    widgets = sorted(k for k in gtk4 if k != "version")

    max_name = max(len(w) for w in widgets)
    max_name = max(max_name, 5)

    # Header
    header = f"  {'Widget':<{max_name}}  {'dx':>4} {'dy':>4}  {'dw':>4} {'dh':>4}  {'OK?'}"
    sep = "  " + "-" * (max_name + 28)

    print(f"\n  Comparing {gtk4_ver} vs {gtk3_ver} (parent-relative positions, "
          f"tolerance {TOLERANCE_PX}px)")
    print()

    ok_count = 0
    diff_count = 0
    hexpand_diffs = []

    for w in widgets:
        r4 = parent_relative(w, gtk4)
        r3 = parent_relative(w, gtk3)
        dx = r4["x"] - r3["x"]
        dy = r4["y"] - r3["y"]
        dw = r4["width"] - r3["width"]
        dh = r4["height"] - r3["height"]

        pos_ok = abs(dx) <= TOLERANCE_PX and abs(dy) <= TOLERANCE_PX
        size_ok = abs(dw) <= TOLERANCE_PX and abs(dh) <= TOLERANCE_PX
        is_ok = pos_ok and size_ok

        if is_ok:
            ok_count += 1
            status = "✓"
        else:
            if w in HEXPAND_WIDGETS:
                hexpand_diffs.append((w, dx, dy, dw, dh))
                status = "⟐"  # expected expand difference
            else:
                diff_count += 1
                status = "✗"

        print(f"  {status} {w:<{max_name}}  {dx:>4} {dy:>4}  {dw:>4} {dh:>4}  {status}")

    print()
    print(f"  OK: {ok_count}, Diff: {diff_count}, Hexpand (expected): {len(hexpand_diffs)}")
    print()

    if hexpand_diffs:
        print("  Expected GTK3 expand differences (pack_start with expand=1):")
        for w, dx, dy, dw, dh in hexpand_diffs:
            parts = []
            if abs(dx) > TOLERANCE_PX: parts.append(f"x shifted by {dx}px")
            if abs(dy) > TOLERANCE_PX: parts.append(f"y shifted by {dy}px")
            if abs(dw) > TOLERANCE_PX: parts.append(f"wider by {dw}px (GTK3 larger)")
            if abs(dh) > TOLERANCE_PX: parts.append(f"taller by {dh}px (GTK3 larger)")
            detail = ", ".join(parts) if parts else "no change"
            print(f"    • {w:<20}  {detail}")

    if diff_count > 0:
        print(f"\n  ❌ {diff_count} widget(s) have unexpected position/size differences!")
        sys.exit(1)
    else:
        print("  ✅ All widget positions and sizes match within tolerance.")
        sys.exit(0)


if __name__ == "__main__":
    main()
