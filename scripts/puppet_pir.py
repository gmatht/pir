#!/usr/bin/env python3
"""Puppet pir through a pty: drive the REPL + menu, assert observable state.

Usage: puppet.py <workdir> <pidir> < PirBinary>
Each scenario runs the binary fresh; failures raise with the transcript tail.
"""
import os, pty, sys, time, select, errno, shutil

PIR = sys.argv[3] if len(sys.argv) > 3 else "/home/ai_pir/src/pir/target/debug/pir"

class Puppet:
    def __init__(self, workdir, pidir, extra_env=None, argv=None):
        self.workdir = workdir
        self.pidir = pidir
        self.extra_env = extra_env or {}
        self.argv = argv or [PIR]
        self.buf = b""
        self.pid = None
        self.fd = None

    def start(self):
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(self.workdir)
            env = dict(os.environ)
            env["PI_DIR"] = self.pidir
            env["TERM"] = "xterm-256color"
            env.update(self.extra_env)
            # no network turns in these scenarios; keep any proxy as-is
            os.execve(self.argv[0], self.argv, env)
        self.pid = pid
        self.fd = fd
        # 80x24 terminal
        import fcntl, termios, struct
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        # nonblocking reads
        import fcntl as F
        fl = F.fcntl(fd, F.F_GETFL)
        F.fcntl(fd, F.F_SETFL, fl | os.O_NONBLOCK)

    def _drain(self):
        try:
            while True:
                try:
                    chunk = os.read(self.fd, 65536)
                except BlockingIOError:
                    break
                if not chunk:
                    break
                self.buf += chunk
        except OSError as e:
            if e.errno != errno.EIO:
                raise

    def text(self):
        self._drain()
        return self.buf.decode("utf-8", "replace")

    def expect(self, needle, timeout=25, what=""):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.text():
                return
            time.sleep(0.2)
        tail = self.text()[-3000:]
        raise AssertionError(f"TIMEOUT waiting for {needle!r} {what}\n--- tail ---\n{tail}")

    def send(self, keys, pause=0.6):
        if isinstance(keys, str):
            keys = keys.encode()
        os.write(self.fd, keys)
        time.sleep(pause)

    def sendline(self, s):
        self.send(s.encode() + b"\n")

    def quit(self):
        # Robust exit: Esc (leave any menu), /exit, then Ctrl-D (EOF break).
        self.send("\x1b")
        time.sleep(0.8)
        self.sendline("/exit")
        try:
            self.wait_exit(timeout=10)
            return
        except AssertionError:
            pass
        self.send("\x04")
        self.wait_exit(timeout=15)

    def wait_exit(self, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            wpid, status = os.waitpid(self.pid, os.WNOHANG)
            if wpid:
                return status
            time.sleep(0.2)
        raise AssertionError("pir did not exit")

    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass

def fresh_dirs(tag):
    base = f"/tmp/pir-puppet-{tag}-{os.getpid()}"
    shutil.rmtree(base, ignore_errors=True)
    wt = os.path.join(base, "wt")
    pi = os.path.join(base, "pi")
    os.makedirs(os.path.join(wt))
    os.makedirs(os.path.join(pi, "agent", "sessions"))
    # seed provider catalog/creds from the real root config so startup resolves
    for f in ("models.json", os.path.join("agent", "auth.json"),
              os.path.join("agent", "settings.json"), os.path.join("agent", "models-store.json")):
        src = os.path.join("/root/.pi", f)
        if os.path.exists(src):
            dst = os.path.join(pi, f)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy(src, dst)
    return wt, pi, base

def scenario_no_narrow_userns_and_canary():
    """Root session: no narrow userns, can read foreign-owned files and write anywhere."""
    wt, pi, base = fresh_dirs("ns")
    foreign = os.path.join(wt, "foreign.txt")
    with open(foreign, "w") as f:
        f.write("foreign-secret\n")
    os.chown(foreign, 60000, 60000)  # no such user: pure foreign kuid
    curly = os.path.join(base, "CANARY")
    p = Puppet(wt, pi)
    p.start()
    try:
        p.expect("\u276f ", what="repl prompt")
        # 1. full init-like uid map (NOT narrow 0 0 1)
        p.sendline("!cat /proc/self/uid_map")
        p.expect("0          0 4294967295", what="full userns map")
        # 2. read the foreign-owned file as (real) root
        p.sendline(f"!cat {foreign}")
        p.expect("foreign-secret", what="read foreign-owned file")
        # 3. write a canary outside the worktree
        p.sendline(f"!echo canary-ok > {curly} && echo WROTE-IT")
        p.expect("WROTE-IT", what="write canary outside worktree")
        p.sendline("/exit")
        p.expect("To restart, run: pir --session", what="restart hint on quit")
        p.quit()
    finally:
        p.close()
    data = open(curly).read()
    assert data.strip() == "canary-ok", f"canary content wrong: {data!r}"
    print("PASS no-narrow-userns + foreign-read + canary-write")

def scenario_menu_save_and_global():
    """Drive /menu s -> toggle both quarantines off -> s saves; (d) saves global."""
    wt, pi, base = fresh_dirs("menu")
    sess_file = os.path.join(pi, "agent", "security.toml")
    p = Puppet(wt, pi)
    p.start()
    try:
        p.expect("\u276f ", what="repl prompt")
        p.sendline("/menu")
        time.sleep(1.0)
        t = p.text()
        assert "Security" in t, f"menu did not open\n{t[-2000:]}"
        p.send("s")  # Security hotkey
        p.expect("write-quarantine", what="security editor")
        p.send("j")  # row 1: write-quarantine
        p.send("l")  # toggle off
        p.send("j")  # row 2: project-quarantine
        p.send("l")  # toggle off
        p.send("s")  # save
        p.expect("security options saved to", what="save confirmation")
        p.expect("\u276f ", what="back at repl")
        p.sendline("/exit")
        p.quit()
    finally:
        p.close()
    body = open(sess_file).read()
    assert "security.quarantine = false" in body, f"session file not updated:\n{body}"
    assert "security.quarantine-project = false" in body, f"project flag not updated:\n{body}"
    print("PASS menu (s) persists both quarantine flags")

    # session 2: (d) writes the global file too
    p2 = Puppet(wt, pi)
    p2.start()
    try:
        p2.expect("\u276f ", what="repl prompt 2")
        p2.sendline("/menu")
        time.sleep(1.0)
        p2.send("s")
        p2.expect("write-quarantine", what="security editor 2")
        p2.send("d")  # save as global defaults
        p2.expect("global defaults", what="global save confirmation")
        p2.expect("\u276f ", what="back at repl 2")
        p2.sendline("/exit")
        p2.quit()
    finally:
        p2.close()
    print("PASS menu (d) reports global defaults")

def scenario_secure_flag_and_canary():
    """--secure strict: override line prints, root still writes the canary."""
    wt, pi, base = fresh_dirs("secure")
    curly = os.path.join(base, "CANARY-SECURE")
    p = Puppet(wt, pi, argv=[PIR, "--secure", "strict"])
    p.start()
    try:
        p.expect("security level for this session: strict (config unchanged", what="secure override line")
        p.expect("\u276f ", what="repl prompt")
        p.sendline(f"!echo secure-ok > {curly} && echo WROTE-SECURE")
        p.expect("WROTE-SECURE", what="canary under --secure strict")
        p.quit()
    finally:
        p.close()
    assert open(curly).read().strip() == "secure-ok"
    # config must be untouched by the session-only override
    sess = os.path.join(pi, "agent", "security.toml")
    assert not os.path.exists(sess), f"--secure must not write config, found {sess}"
    print("PASS --secure strict override + canary, config untouched")

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "secure"):
        scenario_secure_flag_and_canary()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "ns"):
        scenario_no_narrow_userns_and_canary()
    if which in ("all", "menu"):
        scenario_menu_save_and_global()
    print("ALL PUPPET SCENARIOS PASSED")
