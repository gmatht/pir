#!/usr/bin/env python3
"""Reproduce dialogs disappearing on screen resize: open /menu under a real
pty, shrink the pty (SIGWINCH -> crossterm Resize event), and dump the bytes.

Usage: pty_dialog_resize.py <out.raw> <pir-binary>
Then:  pty_dialog_resize.py --check <out.raw>
"""
import os, pty, select, subprocess, sys, time, fcntl, termios, struct


def set_winsize(fd, rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    time.sleep(0.7)  # let SIGWINCH propagate before labelling frames


def drain(master, f, secs):
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


def make_ctty():
    # New session AND controlling terminal: the kernel sends SIGWINCH to the
    # foreground group of the *controlling* terminal, and crossterm turns that
    # into Event::Resize. setsid() alone leaves no ctty, so no SIGWINCH ever
    # arrives and the dialog under test can never observe the resize.
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def main():
    # argv: <out.raw> [--open KEYS] <pir-binary> [args...]
    # --open KEYS: keystrokes to open the dialog under test, e.g. "/menu\r",
    #   "/model\r" (inline list), "/help\r", "?" ... default "/menu\r".
    args = sys.argv[1:]
    outfile = args.pop(0)
    opening = b"/menu\r"
    if args[:1] == ["--open"]:
        args.pop(0)
        opening = args.pop(0).encode().decode("unicode_escape").encode()
    pir = args
    master, slave = pty.openpty()
    set_winsize(slave, 24, 100)
    env = dict(os.environ)
    env.setdefault("TERM", "xterm-256color")
    env["NO_COLOR"] = "1"
    p = subprocess.Popen(
        pir, stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True,
        preexec_fn=make_ctty,
    )
    os.close(slave)
    f = open(outfile, "wb")

    def mark(label):
        f.write(("\n===== MARK %s =====\n" % label).encode())
        f.flush()

    # Wait for the REPL prompt (rustyline prints the ❯ prompt when ready)
    # before sending anything; typing during startup is swallowed.
    mark("startup")
    buf = b""
    t0 = time.time()
    while time.time() - t0 < 20 and "❯".encode("utf-8") not in buf:
        r, _, _ = select.select([master], [], [], 0.2)
        if r:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                break
            f.write(chunk)
            buf += chunk
    time.sleep(0.5)
    drain(master, f, 0.3)
    # Staged keys: split on \x01 (literal SOH) with a settle delay between
    # stages, so "/menu\r\x01h" opens the menu, waits for it to paint, then
    # presses h inside it.
    for i, stage in enumerate(opening.split(b"\x01")):
        if i > 0:
            time.sleep(1.2)
            drain(master, f, 0.3)
        os.write(master, stage)
    time.sleep(1.5)  # dialog paints (alt-screen or inline block)
    mark("dialog-100x24")
    drain(master, f, 1.0)

    set_winsize(master, 24, 50)   # vertical split: halve the width
    mark("resized-50x24")
    drain(master, f, 1.0)

    set_winsize(master, 12, 50)   # horizontal split too
    mark("resized-50x12")
    drain(master, f, 1.0)

    set_winsize(master, 24, 100)  # back to the original geometry
    mark("regrown-100x24")
    drain(master, f, 1.0)

    # Dismiss the dialog (Esc) then quit the REPL.
    os.write(master, b"\x1b")
    time.sleep(0.5)
    mark("dismissed")
    drain(master, f, 0.5)
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
    """The dialog must repaint at the NEW size after each resize: look for box
    frames (CUP-addressed rows with box-drawing borders) after every MARK. A
    dialog that vanished leaves no box frames after the resize mark."""
    import re
    d = open(outfile, "rb").read().decode("utf-8", "replace")
    parts = re.split(r"\n===== MARK ([a-z0-9-]+) =====\n", d)
    segs = {parts[i]: parts[i + 1] for i in range(1, len(parts), 2)}
    # A painted box frame: cursor-addressed write containing a top border run.
    frame_re = re.compile(u"\x1b\\[\\d+;\\d+H[^\n]*\u250c")
    ok = True
    for label in ["dialog-100x24", "resized-50x24", "resized-50x12", "regrown-100x24"]:
        body = segs.get(label, "")
        n = len(frame_re.findall(body))
        # Also measure the widest box border drawn in this segment.
        widths = sorted({len(m.group(0)) for m in re.finditer(u"\u2500{4,}", body)})
        widest = widths[-1] if widths else 0
        status = "frames=%d widest-rule=%d" % (n, widest)
        print("check [%s]: %s" % (label, status))
        if n == 0:
            print("  FAIL — no dialog frame painted in this segment (dialog gone?)")
            ok = False
    # The resized-narrow segments must show a NARROWER box than the wide ones:
    # proves the dialog re-read the size instead of replaying stale geometry.
    def widest(label):
        body = segs.get(label, "")
        ws = sorted({len(m.group(0)) for m in re.finditer(u"\u2500{4,}", body)})
        return ws[-1] if ws else 0
    w_full, w_half = widest("dialog-100x24"), widest("resized-50x24")
    if w_full and w_half and not (w_half < w_full):
        print("check: FAIL — narrow segment rule (%d) not narrower than full (%d); "
              "dialog did not redraw at the new size" % (w_half, w_full))
        ok = False
    print("check: %s" % ("PASS — dialog repainted at each size" if ok else "FAIL"))
    return 0 if ok else 1


if __name__ == "__main__":
    if len(sys.argv) > 2 and sys.argv[1] == "--check":
        sys.exit(check(sys.argv[2]))
    sys.exit(main())
