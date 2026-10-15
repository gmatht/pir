#!/usr/bin/env python
"""Run a command under a real pty, capture raw bytes for N seconds. (py2)

Usage: pty_run.py <outfile> <seconds> <cmd> [args...]
"""
from __future__ import print_function
import os, pty, select, subprocess, sys, time


def main():
    outfile = sys.argv[1]
    secs = float(sys.argv[2])
    cmd = sys.argv[3:]
    master, slave = pty.openpty()
    env = dict(os.environ)
    env.setdefault("TERM", "xterm-256color")
    p = subprocess.Popen(
        cmd, stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True,
        preexec_fn=os.setsid,
    )
    os.close(slave)
    t0 = time.time()
    total = 0
    reason = "timeout"
    f = open(outfile, "wb")
    while time.time() - t0 < secs:
        if p.poll() is not None:
            reason = "exit"
        r, _, _ = select.select([master], [], [], 0.2)
        if r:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                reason = "eof"
                break
            if not chunk:
                reason = "eof"
                break
            f.write(chunk)
            total += len(chunk)
        if reason == "exit":
            while True:
                r, _, _ = select.select([master], [], [], 0.1)
                if not r:
                    break
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                f.write(chunk)
                total += len(chunk)
            break
    if p.poll() is None:
        p.terminate()
        try:
            p.wait(timeout=2)
        except Exception:
            p.kill()
    f.close()
    os.close(master)
    print("reason=%s exit=%s bytes=%d secs=%.1f" % (reason, p.returncode, total, time.time() - t0))
    return 0


if __name__ == "__main__":
    sys.exit(main())
