#!/usr/bin/env python3
"""Live-verify the opencode-keys extension: banner, /opencode-keys status,
rotate, rotate-back. Local-only (no network): the pool file is a temp copy.
Usage: puppet_opencode_keys.py <workdir> <pir-binary>
"""
import json, os, shutil, sys, tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from puppet_pir import Puppet as PirTerm


class P(PirTerm):
    pass


def main():
    workdir = sys.argv[1] if len(sys.argv) > 1 else "/tmp"
    pir = sys.argv[2] if len(sys.argv) > 2 else "./target/debug/pir"
    pidir = tempfile.mkdtemp(prefix="pir-ock-pidir-")
    agent = os.path.join(pidir, "agent")
    os.makedirs(agent)
    pool = {
        "keys": [
            {"name": "a", "key": "sk-test-a"},
            {"name": "b", "key": "sk-test-b"},
            {"name": "c", "key": "sk-test-c"},
        ],
        "activeKeyIndex": 0,
        "cooldownMinutes": 60,
        "watchdogEnabled": True,
    }
    with open(os.path.join(agent, "opencode-keys.json"), "w") as f:
        json.dump(pool, f, indent=2)
    # Minimal catalog so startup doesn't fall back to auth.json noise.
    store = {
        "opencode-go": {
            "models": [
                {
                    "id": "m1",
                    "api": "openai-completions",
                    "baseUrl": "https://opencode.ai/inference/go/v1",
                }
            ]
        }
    }
    with open(os.path.join(agent, "models-store.json"), "w") as f:
        json.dump(store, f)

    p = PirTerm(workdir, pidir, argv=[pir])
    p.start()
    try:
        # NOTE: the first `❯ ` prompt draw doesn't emit under a minimal pty
        # in this environment (pre-existing: stock scenarios stall the same
        # way here), but the REPL reads stdin fine — wait for the status bar
        # (which proves startup, including the banner, completed), then drive
        # commands and assert on their outputs.
        p.expect("workspace:", timeout=90, what="repl status bar")
        t = p.clean()
        assert "[opencode-keys] pool: active #0 (a)" in t, f"no banner:\n{t[-2000:]}"
        assert "3 of 3 usable" in t, f"no usable count:\n{t[-2000:]}"
        print("BANNER ok")

        p.sendline("/opencode-keys")
        p.expect("opencode-keys pool", timeout=15, what="status")
        t = p.clean()
        assert "#0 (a)" in t and "#2 (c)" in t, f"status lines:\n{t[-2000:]}"
        print("STATUS ok")

        p.sendline("/opencode-keys rotate")
        p.expect("rotated opencode key pool: active is now #1 (b)", timeout=15, what="rotate")
        print("ROTATE ok")

        # File state: activeKeyIndex persisted.
        with open(os.path.join(agent, "opencode-keys.json")) as f:
            v = json.load(f)
        assert v["activeKeyIndex"] == 1, f"active not persisted: {v['activeKeyIndex']}"
        assert v["watchdogEnabled"] is True, "foreign field dropped!"
        print("PERSIST ok")

        p.sendline("/opencode-keys rotate")
        p.expect("rotated opencode key pool: active is now #2 (c)", timeout=15, what="rotate2")
        p.sendline("/opencode-keys rotate")
        p.expect("rotated opencode key pool: active is now #0 (a)", timeout=15, what="wrap")
        print("WRAP ok")

        p.quit()
    finally:
        shutil.rmtree(pidir, ignore_errors=True)
    print("ALL OK")


main()
