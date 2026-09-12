#!/usr/bin/env python3
"""Puppet pir through a pty: drive the REPL + menu, assert observable state.

Usage: puppet.py <workdir> <pidir> < PirBinary>
Each scenario runs the binary fresh; failures raise with the transcript tail.
"""
import os, pty, sys, time, select, errno, shutil, codecs, unicodedata

PIR = sys.argv[3] if len(sys.argv) > 3 else "/home/ai_pir/src/pir/target/debug/pir"


class Term:
    """Minimal terminal emulator: tracks grid + cursor over the escape
    repertoire pir emits (CUP/CUU/CUD/CUF/CUB/CNL/CPL/ED/EL/SGR/save/
    restore/bracketed-paste/OSC/BEL). Feeds raw bytes; decodes incrementally."""

    def __init__(self, rows=24, cols=80):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.r, self.c = 0, 0
        self.saved = None
        self._dec = codecs.getincrementaldecoder("utf-8")()
        self._esc = b""

    def feed(self, data: bytes):
        text = self._dec.decode(data, False)
        i = 0
        while i < len(text):
            ch = text[i]
            if self._esc:
                self._esc += ch.encode("utf-8", "ignore")
                if ch == "\x07":
                    self._esc = b""  # OSC terminator
                elif ch.isalpha() or ch in "~":
                    seq = self._esc[1:]
                    if seq.startswith(b"["):
                        seq = seq[1:]
                    self._handle_csi(seq)
                    self._esc = b""
                i += 1
                continue
            if ch == "\x1b":
                nxt = text[i + 1:i + 2]
                if nxt == "7":
                    self.saved = (self.r, self.c)
                    i += 2
                elif nxt == "8":
                    if self.saved:
                        self.r, self.c = self.saved
                    i += 2
                elif nxt == "]":
                    self._esc = b"\x1b]"  # OSC, skip to BEL
                    i += 2
                else:
                    self._esc = b"\x1b"
                    i += 1
                continue
            if ch == "\n":
                self.r += 1
                self.c = 0
                if self.r >= self.rows:
                    self.grid.pop(0)
                    self.grid.append([" "] * self.cols)
                    self.r = self.rows - 1
            elif ch == "\r":
                self.c = 0
            elif ch == "\b":
                self.c = max(0, self.c - 1)
            elif ch == "\x07":
                pass
            elif ch == "\x00":
                pass
            else:
                w = 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
                if self.c + w > self.cols:
                    self.r += 1
                    self.c = 0
                    if self.r >= self.rows:
                        self.grid.pop(0)
                        self.grid.append([" "] * self.cols)
                        self.r = self.rows - 1
                self.grid[self.r][self.c] = ch
                self.c += w
            i += 1

    def _handle_csi(self, seq: bytes):
        try:
            s = seq.decode("ascii")
        except UnicodeDecodeError:
            return
        final = s[-1:]
        params = s[:-1].lstrip("?")
        nums = [int(x) if x else 0 for x in params.split(";") if x != ""] if params else []
        n = nums[0] if nums else 0
        if final in ("H", "f"):
            r = (nums[0] if len(nums) > 0 and nums[0] else 1) - 1
            c = (nums[1] if len(nums) > 1 and nums[1] else 1) - 1
            self.r = min(max(r, 0), self.rows - 1)
            self.c = min(max(c, 0), self.cols - 1)
        elif final == "A":
            self.r = max(0, self.r - (n or 1))
        elif final == "B":
            self.r = min(self.rows - 1, self.r + (n or 1))
        elif final == "C":
            self.c = min(self.cols - 1, self.c + (n or 1))
        elif final == "D":
            self.c = max(0, self.c - (n or 1))
        elif final == "G":
            self.c = min(max((n or 1) - 1, 0), self.cols - 1)
        elif final == "K":
            if n == 2:
                self.grid[self.r] = [" "] * self.cols
            elif n == 1:
                for x in range(0, self.c + 1):
                    self.grid[self.r][x] = " "
            else:
                for x in range(self.c, self.cols):
                    self.grid[self.r][x] = " "
        elif final == "J":
            if n == 2:
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
            else:
                for y in range(self.r, self.rows):
                    start = self.c if y == self.r else 0
                    for x in range(start, self.cols):
                        self.grid[y][x] = " "
        elif final == "s":
            self.saved = (self.r, self.c)
        elif final == "u":
            if self.saved:
                self.r, self.c = self.saved
        # SGR (m), show/hide cursor, paste mode, etc: no grid effect.

    def row_text(self, r):
        return "".join(self.grid[r]).rstrip()

    def pos(self):
        return (self.r, self.c)

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

    def raw(self):
        self._drain()
        return bytes(self.buf)

    def expect(self, needle, timeout=25, what=""):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.text():
                return
            time.sleep(0.2)
        tail = self.text()[-3000:]
        raise AssertionError(f"TIMEOUT waiting for {needle!r} {what}\n--- tail ---\n{tail}")

    def clean(self):
        import re
        t = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", self.text())
        # Terminal repaints separate prompt and echo with bare \r (same
        # visual row); normalize so "❯ hi" reads contiguous.
        t = t.replace("\r\n", "\n").replace("\r", "")
        return t

    def expect_clean(self, needle, timeout=25, what=""):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.clean():
                return
            time.sleep(0.2)
        raise AssertionError(f"TIMEOUT waiting for clean {needle!r} {what}\n--- tail ---\n{self.clean()[-3000:]}")

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

    def wait_exit(self, timeout=20, drain=True):
        end = time.time() + timeout
        while time.time() < end:
            # Drain throughout: an undrained pty buffer (64K) fills in ~20s
            # of spinner ticks and then every writer blocks forever — a
            # harness artifact that masks the real state. Draining keeps the
            # wait honest.
            if drain:
                self._drain()
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

def scenario_tui_idle_prompt():
    """--tui real binary: the idle frame on the wire contains the prompt."""
    wt, pi, base = fresh_dirs("tui")
    import shutil as _sh
    feat = "/home/ai_pir/src/pir/target/debug/pir"
    p = Puppet(wt, pi, argv=[feat, "--tui"])
    p.start()
    try:
        p.expect("\u276f", timeout=30, what="tui idle prompt on the wire")
        p.send("\x04")  # ctrl-d exits the TUI
        p.wait_exit(timeout=15)
    finally:
        p.close()
    print("PASS --tui idle frame carries the prompt")

def scenario_input_zone():
    """Idle prompt is a boxed zone: top hrule / prompt+cursor / footer.

    Asserts stream order: a full-width hrule, then the ❯ prompt line
    (typed text lands right after ❯, proving the cursor sits on the
    prompt row), then the footer status line (bottom border) restored
    after submit."""
    import re
    wt, pi, base = fresh_dirs("zone")
    p = Puppet(wt, pi)
    p.start()
    try:
        p.expect("\u276f ", what="repl prompt")
        # type without submitting: echo must land adjacent to the prompt
        p.send("hi")
        p.expect_clean("\u276f hi", what="typed text on the prompt line")
        # submit -> the footer status line (bottom border) is redrawn below
        p.send("\n")
        time.sleep(1.0)
        t = p.clean()
        hrules = [m.start() for m in re.finditer(r"─{20,}", t)]
        prompt_at = t.find("\u276f hi")
        assert prompt_at > 0, "prompt line missing"
        assert any(h < prompt_at for h in hrules), "top hrule must precede the prompt"
        footer_at = t.find("workspace:", prompt_at)
        assert footer_at > prompt_at, "footer (bottom border) must follow submit"
        p.quit()
    finally:
        p.close()
    print("PASS idle input zone hrules + cursor row")

def scenario_fake_turn_midturn_carry():
    """Real turn via fake/slow: mid-turn draft visible, partial carried.

    FAKE: sleep 12 keeps the turn alive; type 'hel' mid-turn (footer must
    show ❯ hel); after the turn ends the idle prompt reopens prefilled,
    so Enter submits 'hel' and the fake's echo ('you said "hel"') proves
    the carry end-to-end."""
    wt, pi, base = fresh_dirs("turn")
    env = {"PIR_FAKE_MODEL": "1", "PI_FULL_AUTO": "1"}
    p = Puppet(wt, pi, extra_env=env, argv=[PIR, "-m", "fake/slow"])
    p.start()
    try:
        p.expect("\u276f ", what="repl prompt")
        p.sendline("FAKE: sleep 12")
        p.expect("sleep 12", timeout=20, what="tool echo")
        # mid-turn: footer must show the draft next to the prompt
        p.send("hel")
        p.expect_clean("\u276f hel", timeout=20, what="mid-turn draft visible")
        # turn end: conclusion streams, then idle reopens prefilled
        p.expect("fake: tools done", timeout=40, what="turn conclusion")
        time.sleep(2.0)  # spinner dead, footer erased, idle prompt up
        # submit the carried line; the fake echoes it back verbatim
        p.send("\n")
        p.expect('you said "hel"', timeout=30, what="carried line submitted")
        p.quit()
    finally:
        p.close()
    print("PASS fake turn: mid-turn draft + carry")

def scenario_thinking_keeps_prompt():
    """Thinking phase must keep the prompt visible.

    Guards against the idle-prefill false pass (a draft sighting only
    counts while the turn is still alive): the draft must appear BEFORE
    any turn-end marker AND more thinking must stream AFTER it."""
    wt, pi, base = fresh_dirs("think")
    env = {"PIR_FAKE_MODEL": "1", "PI_FULL_AUTO": "1"}
    words = " ".join(f"thought{i}" for i in range(50))
    p = Puppet(wt, pi, extra_env=env, argv=[PIR, "-m", "fake/slow"])
    p.start()
    try:
        p.expect("\u276f ", what="repl prompt")
        p.sendline(f"FAKE: think {words}")
        p.expect("thought0", timeout=30, what="thinking streams")
        time.sleep(2.0)  # well inside the ~7.5s thinking phase
        p.send("zz")
        # draft must show promptly (footer ticks every 80ms)...
        deadline = time.time() + 5.0
        seen_at = None
        while time.time() < deadline:
            t = p.clean()
            if "\u276f zz" in t:
                # ...and the turn must still be alive (no usage line yet).
                if "out tokens" not in t:
                    seen_at = len(t)
                    break
                raise AssertionError("draft only appeared after turn end (idle prefill, not mid-turn)")
            time.sleep(0.2)
        assert seen_at is not None, "no draft visible at any point mid-thinking"
        # ...and more thinking must stream AFTER the sighting (turn alive).
        deadline = time.time() + 15.0
        proved = False
        while time.time() < deadline:
            t = p.clean()
            later = t.find("thought45", seen_at)
            if later > 0:
                proved = True
                break
            if "out tokens" in t:
                break
            time.sleep(0.3)
        assert proved, "thinking did not continue after the draft sighting (turn was already over)"
        time.sleep(12.0)  # let the thinking turn finish
        p.quit()
    finally:
        p.close()
    print("PASS thinking phase keeps the prompt")

def scenario_midturn_zone_rows():
    """Mid-turn footer is a 3-line zone on 3 distinct rows (row-aware).

    Tracks CUP sequences during a fake sleep turn: the zone must address
    rows 22/23/24 (24-row pty) in order per tick, with the ❯ draft drawn
    on the middle row. Order-only assertions cannot see row collapse —
    this is the test that catches single-line regressions."""
    import re
    wt, pi, base = fresh_dirs("rows")
    env = {"PIR_FAKE_MODEL": "1", "PI_FULL_AUTO": "1"}
    p = Puppet(wt, pi, extra_env=env, argv=[PIR, "-m", "fake/slow"])
    p.start()
    try:
        p.expect("\u276f ", what="repl prompt")
        p.sendline("FAKE: sleep 10")
        p.expect("sleep 10", timeout=20, what="tool echo")
        p.send("zz")
        deadline = time.time() + 12.0
        proved = False
        while time.time() < deadline:
            raw = p.text()
            cups = [(m.start(), int(m.group(1))) for m in re.finditer(r"\x1b\[(\d+);\d+H", raw)]
            rows = [r for _, r in cups]
            # find a 22,23,24 triple (one spinner tick) with ❯ zz on row 23
            for i in range(len(cups) - 2):
                r0, r1, r2 = rows[i], rows[i + 1], rows[i + 2]
                if (r0, r1, r2) == (22, 23, 24):
                    seg_end = cups[i + 2][0]
                    seg = raw[cups[i + 1][0]:seg_end]
                    seg = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", seg).replace("\r\n", "\n").replace("\r", "")
                    if "\u276f zz" in seg:
                        proved = True
                        break
            if proved:
                break
            if "out tokens" in p.clean():
                raise AssertionError("turn ended before any 3-row zone frame with the draft")
            time.sleep(0.3)
        assert proved, "no 3-row zone frame (22/23/24 + ❯ zz on 23) seen mid-turn"
        p.quit()
    finally:
        p.close()
    print("PASS mid-turn zone occupies 3 rows")

def scenario_cursor_parked_midturn():
    """Hardware cursor sits on the prompt row for a quiet turn.

    During a fake sleep turn (no worker output in flight) every sample
    must show the cursor parked just past ❯ on the prompt row, with
    hrules above and below. Relative asserts only (row found by content)
    so absolute drift cannot fake a pass."""
    wt, pi, base = fresh_dirs("park")
    env = {"PIR_FAKE_MODEL": "1", "PI_FULL_AUTO": "1"}
    p = Puppet(wt, pi, extra_env=env, argv=[PIR, "-m", "fake/slow"])
    p.start()
    try:
        p.expect("\u276f ", what="repl prompt")
        # Long sleep: the tool echo (» bash) marks turn start; sampling must
        # happen while the turn is alive (a progress display like
        # "running 10s" would mean the turn is nearly over — never use it
        # as a start marker).
        p.sendline("FAKE: sleep 30")
        p.expect("bash  sleep 30", timeout=20, what="tool started")
        time.sleep(2.0)  # let ticks park the cursor (tool then runs silent)
        t = Term()
        fed = 0
        samples = []
        for _ in range(15):
            raw = p.raw()
            t.feed(raw[fed:])
            fed = len(raw)
            samples.append(t.pos())
            time.sleep(0.2)
        assert "out tokens" not in p.clean(), "turn ended mid-sampling (window too late)"
        # cursor constant across all samples ...
        assert len(set(samples)) == 1, f"cursor wandered mid-turn: {sorted(set(samples))}"
        r, c = samples[0]
        prow = t.row_text(r)
        assert prow.startswith("❯"), f"cursor not on the prompt row: {t.pos()} {prow!r}"
        assert c == 3 - 1, f"cursor not just past ❯ (col {c})"
        assert t.row_text(r - 1).startswith("─"), "no hrule above the prompt"
        assert t.row_text(r + 1).strip().replace("─", "") == "", "no clean hrule below"
        p.quit()
    finally:
        p.close()
    print("PASS cursor parked on prompt mid-turn")

def scenario_wire_parity():
    """Mock-server parity: record what pir sends on the wire (§3.6).

    Runs pir one-shot against a local mock OpenAI endpoint and asserts the
    recorded request bodies: pir identity + shape sections present, user
    text verbatim, tool schemas shipped, bash tool callable end-to-end."""
    import json as _json
    import socket as _socket
    import subprocess as _sp
    wt, pi, base = fresh_dirs("parity")
    # A CLAUDE.md in the worktree must reach the model (pi parity).
    with open(os.path.join(wt, "CLAUDE.md"), "w") as f:
        f.write("# WireMarker\nAlways mention pineapples.\n")
    port = 8799
    log = os.path.join(base, "requests.jsonl")
    store = {
        "providers": [{
            "id": "local", "name": "local", "api": "openai",
            "baseUrl": f"http://127.0.0.1:{port}", "apiKey": "test",
            "models": [{"id": "mock", "context": 200000, "maxTokens": 8192}],
        }]
    }
    with open(os.path.join(pi, "agent", "models-store.json"), "w") as f:
        _json.dump(store, f)
    mock = os.path.join("/home/ai_pir/src/pir/scripts", "mock_server.py")
    srv = _sp.Popen([sys.executable, mock, str(port), log],
                    stdout=_sp.DEVNULL, stderr=_sp.DEVNULL)
    try:
        deadline = time.time() + 10.0
        while time.time() < deadline:
            try:
                s = _socket.create_connection(("127.0.0.1", port), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.2)
        env = {"PI_FULL_AUTO": "1"}
        # round 1: tool call end-to-end over the wire
        p = Puppet(wt, pi, extra_env=env,
                   argv=[PIR, "-m", "local/mock", "MOCK: tool echo wire-ok"])
        p.start()
        try:
            status = p.wait_exit(timeout=90)
        finally:
            p.close()
        assert status == 0, f"one-shot tool turn exited {status}"
        recs = [ _json.loads(l) for l in open(log) if l.strip() ]
        assert recs, "mock recorded no requests"
        first = recs[0]["body"]
        system = first.get("system", "") or _sys_text(first)
        assert "You are pir" in system, "identity missing from system prompt"
        assert "Available tools:" in system, "Available tools section missing"
        assert "Guidelines:" in system, "Guidelines section missing"
        assert "Current working directory:" in system, "cwd trailer missing"
        def _utext(m):
            c = m.get("content", "")
            if isinstance(c, str):
                return c
            if isinstance(c, list):
                return " ".join(x.get("text", "") for x in c if isinstance(x, dict))
            return ""
        user_texts = [_utext(m) for m in first.get("messages", [])
                       if m.get("role") == "user"]
        assert any("MOCK: tool echo wire-ok" in t for t in user_texts), \
            f"user text not verbatim: {user_texts}"
        assert "pineapples" in system, "CLAUDE.md content missing from system prompt"
        tools = first.get("tools", [])
        assert any(t.get("function", {}).get("name") == "bash" for t in tools), \
            "bash tool schema missing from request"
        # round 2: plain text turn resolves and exits clean
        p2 = Puppet(wt, pi, extra_env=env,
                    argv=[PIR, "-m", "local/mock", "MOCK: text hello-wire"])
        p2.start()
        try:
            status2 = p2.wait_exit(timeout=90)
        finally:
            p2.close()
        assert status2 == 0, f"one-shot text turn exited {status2}"
        print("PASS wire parity: system shape + verbatim user + tool schemas")
    finally:
        srv.terminate()


def _sys_text(body):
    # system prompt may arrive as a messages[0] system entry instead
    for m in body.get("messages", []):
        if m.get("role") == "system" and isinstance(m.get("content"), str):
            return m["content"]
    return ""

def scenario_pi_vs_pir():
    """Drive pi AND pir against the same mock; diff per the §3.6 matrix.

    Match: identical user text, Available tools + Guidelines sections in
    both, cwd trailer in both, AGENTS.md marker content in both.
    Diverge: identity lines and tool-name sets differ per agent."""
    import json as _json
    import socket as _socket
    import subprocess as _sp
    wt, pi, base = fresh_dirs("pivspir")
    with open(os.path.join(wt, "AGENTS.md"), "w") as f:
        f.write("# SharedMarker\nAlways be kind.\n")
    port = 18799
    log = os.path.join(base, "requests.jsonl")
    mock = "/home/ai_pir/src/pir/scripts/mock_server.py"
    for p in (8799, port):
        _sp.run(["pkill", "-f", f"mock_server.py {p}"],
                capture_output=True)
    time.sleep(0.5)
    srv = _sp.Popen([sys.executable, mock, str(port), log],
                    stdout=_sp.DEVNULL, stderr=_sp.DEVNULL)
    bodies = {}
    try:
        deadline = time.time() + 10.0
        while time.time() < deadline:
            try:
                s = _socket.create_connection(("127.0.0.1", port), timeout=1)
                s.close()
                break
            except OSError:
                time.sleep(0.2)
        prompt = "MOCK: text same-prompt"
        # --- pi ---
        pihome = os.path.join(base, "pihome")
        os.makedirs(os.path.join(pihome, ".pi", "agent"), exist_ok=True)
        with open(os.path.join(pihome, ".pi", "agent", "models.json"), "w") as f:
            _json.dump({"providers": {"localmock": {
                "baseUrl": f"http://127.0.0.1:{port}/v1",
                "api": "openai-completions", "apiKey": "test",
                "compat": {"supportsDeveloperRole": False, "supportsReasoningEffort": False},
                "models": [{"id": "mock"}]} }}, f)
        open(log, "w").write("")
        pi_env = dict(os.environ, HOME=pihome, PI_TELEMETRY="0")
        r = _sp.run(["pi", "--api-key", "test", "--model", "localmock/mock",
                     "-p", prompt, "--no-session"],
                    cwd=wt, env=pi_env, capture_output=True, text=True, timeout=120)
        assert r.returncode == 0, f"pi failed: {r.stderr[-500:]}"
        bodies["pi"] = [_json.loads(l) for l in open(log) if l.strip()][-1]["body"]
        # --- pir ---
        store = {"providers": [{
            "id": "local", "name": "local", "api": "openai",
            "baseUrl": f"http://127.0.0.1:{port}", "apiKey": "test",
            "models": [{"id": "mock", "context": 200000, "maxTokens": 8192}]}]}
        with open(os.path.join(pi, "agent", "models-store.json"), "w") as f:
            _json.dump(store, f)
        open(log, "w").write("")
        pir_env = dict(os.environ, PI_DIR=pi, PI_FULL_AUTO="1")
        r = _sp.run([PIR, "-m", "local/mock", prompt],
                    cwd=wt, env=pir_env, capture_output=True, text=True, timeout=120)
        assert r.returncode == 0, f"pir failed: {r.stderr[-500:]}"
        bodies["pir"] = [_json.loads(l) for l in open(log) if l.strip()][-1]["body"]
    finally:
        srv.terminate()
    # --- matrix: MATCH rows ---
    texts = {}
    for who, b in bodies.items():
        ts = []
        for m in b.get("messages", []):
            if m.get("role") != "user":
                continue
            c = m.get("content", "")
            if isinstance(c, str):
                ts.append(c)
            elif isinstance(c, list):
                ts.append(" ".join(x.get("text", "") for x in c if isinstance(x, dict)))
        texts[who] = " ".join(ts)
    assert texts["pi"].strip() == texts["pir"].strip(), \
        f"user text differs:\npi: {texts['pi'][:120]!r}\npir: {texts['pir'][:120]!r}"
    systems = {}
    for who, b in bodies.items():
        s = ""
        for m in b.get("messages", []):
            if m.get("role") == "system":
                c = m.get("content", "")
                s = c if isinstance(c, str) else " ".join(
                    x.get("text", "") for x in c if isinstance(x, dict))
        systems[who] = s
    for who, s in systems.items():
        assert "Available tools:" in s, f"{who}: tools section missing"
        assert "Guidelines:" in s, f"{who}: guidelines section missing"
        assert "Current working directory:" in s, f"{who}: cwd trailer missing"
        assert "Always be kind." in s, f"{who}: AGENTS.md marker missing"
    # --- matrix: DIVERGE rows ---
    assert "operating inside pi" in systems["pi"], "pi identity changed?"
    assert "You are pir" in systems["pir"], "pir identity missing"
    assert "operating inside pi" not in systems["pir"]
    pi_tools = {t.get("function", {}).get("name") for t in bodies["pi"].get("tools", [])}
    pir_tools = {t.get("function", {}).get("name") for t in bodies["pir"].get("tools", [])}
    assert "read" in pi_tools and "read_file" not in pi_tools, f"pi tools: {pi_tools}"
    assert "read_file" in pir_tools and "read" not in pir_tools, f"pir tools: {pir_tools}"
    print("PASS pi-vs-pir: user/shape/context match, identity/tools diverge")

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "pivspir"):
        scenario_pi_vs_pir()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "parity"):
        scenario_wire_parity()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "parked"):
        scenario_cursor_parked_midturn()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "zonerows"):
        scenario_midturn_zone_rows()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "thinking"):
        scenario_thinking_keeps_prompt()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "turn"):
        scenario_fake_turn_midturn_carry()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "zone"):
        scenario_input_zone()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "tui"):
        scenario_tui_idle_prompt()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "secure"):
        scenario_secure_flag_and_canary()
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("all", "ns"):
        scenario_no_narrow_userns_and_canary()
    if which in ("all", "menu"):
        scenario_menu_save_and_global()
    print("ALL PUPPET SCENARIOS PASSED")
