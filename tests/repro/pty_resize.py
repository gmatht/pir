#!/usr/bin/env python3
"""Reproduce the pane-halving corruption: run pir under a real pty, then
shrink the pty width (simulating a split pane halving) and dump the bytes.

Usage: pty_resize.py <out.raw> <cmd> [args...]
"""
import os, pty, select, subprocess, sys, time, fcntl, termios, struct


def set_winsize(fd, rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    time.sleep(0.7)  # let the change propagate before labelling frames


def main():
    outfile = sys.argv[1]
    cmd = sys.argv[2:]
    master, slave = pty.openpty()
    set_winsize(master, 24, 100)
    env = dict(os.environ)
    env.setdefault("TERM", "xterm-256color")
    env["NO_COLOR"] = "1"
    p = subprocess.Popen(
        cmd, stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True,
        preexec_fn=os.setsid,
    )
    os.close(slave)
    f = open(outfile, "wb")

    def drain(secs, label):
        # Mark FIRST, then capture: the marker delimits the frames that follow
        # it, so each segment holds exactly the frames painted in that phase.
        # Writing it last shifted every label by one phase (a settle-only bug
        # in this harness, not in pir).
        f.write(("\n===== MARK %s =====\n" % label).encode())
        f.flush()
        t0 = time.time()
        n = 0
        while time.time() - t0 < secs:
            r, _, _ = select.select([master], [], [], 0.1)
            if r:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                f.write(chunk)
                n += len(chunk)
        return n

    drain(1.0, "start-100x24")
    # halve the width, as when a vertical split closes
    set_winsize(master, 24, 50)
    drain(0.6, "resized-50x24")
    # change the height too: a horizontal split / window drag
    set_winsize(master, 12, 50)
    time.sleep(0.5)
    drain(0.6, "resized-50x12")
    set_winsize(master, 6, 50)
    time.sleep(0.5)
    drain(0.6, "resized-50x6")
    # back to the original geometry
    set_winsize(master, 24, 100)
    drain(0.6, "regrown-100x24")
    p.terminate()
    try:
        p.wait(timeout=2)
    except Exception:
        p.kill()
    f.close()
    os.close(master)
    print("bytes=%d" % os.path.getsize(outfile))
    return 0


def check(outfile):
    """Assert the tick re-reads the pane width after a resize.

    The widest signal is a *grow*: `regrown-100x24` puts the pane back at 100
    columns. A tick that cached its width at spinner start keeps painting the
    smaller rule (the reported corruption: the footer laid out for the old
    geometry), while a tick that re-reads paints the 100-col rule.

    NOTE: this only bites when ONE spinner spans the resize. A prompt like `hi`
    finishes a model round quickly, and each round builds a *new* spinner which
    re-reads the width — so the bug hides. Hold a single spinner across the
    resize with a real bash tool: `-m fake/slow 'FAKE: sleep 20'`.
    """
    import re
    d = open(outfile, "rb").read().decode("utf-8", "replace")
    parts = re.split(r"\n===== MARK ([a-z0-9-]+) =====\n", d)
    segs = {parts[i]: parts[i + 1] for i in range(1, len(parts), 2)}
    body = segs.get("regrown-100x24", "")
    rules = sorted({len(m.group(0)) for m in re.finditer(u"\u2500{4,}", body)})
    frames = body.count("\x1b[1;")
    if frames == 0:
        print("check: NO FRAMES after the regrow (spinner not alive across it) "
              "— rerun with a long-lived tool, e.g. 'FAKE: sleep 20'")
        return 2
    if 100 not in rules:
        print("check: FAIL — pane grew back to 100 cols but the tick painted "
              "only %s-wide rule(s); the width was not re-read after the resize"
              % rules)
        return 1
    print("check: PASS — pane 100 cols, rules painted %s (re-read the width)" % rules)
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "--check":
        sys.exit(check(sys.argv[2]))
    sys.exit(main())
