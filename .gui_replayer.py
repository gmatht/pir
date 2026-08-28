"""
GUI Replayer — Windows-only test runner for the NWG backend.

Launches corro.exe with a .corro file, simulates keyboard input via
PostMessage (bypasses UIPI which blocks SendInput from background
processes), captures stdout/stderr, and reports pass/fail.

Usage:
    python .gui_replayer.py --test recrec5
    python .gui_replayer.py --test recrec6
    python .gui_replayer.py --list
"""

import argparse
import glob
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

try:
    import ctypes
    from ctypes import wintypes
    HAS_WIN32 = True
except ImportError:
    HAS_WIN32 = False

# Windows API constants
WM_KEYDOWN = 0x0100
WM_KEYUP = 0x0101
WM_SYSKEYDOWN = 0x0104
WM_SYSKEYUP = 0x0105
KF_ALTDOWN = 0x2000


def _post(hwnd, msg, wparam, lparam=0):
    """Post a message to the given window."""
    if not HAS_WIN32 or hwnd is None:
        return False
    ok = ctypes.windll.user32.PostMessageW(hwnd, msg, wparam, lparam)
    if not ok:
        print(f"  WARN: PostMessageW({hwnd}, 0x{msg:04x}, 0x{wparam:04x}) failed (returned {ok})")
    return ok


def key_down(hwnd, vk_code, syskey=False):
    """Post a key-down message to the window."""
    msg = WM_SYSKEYDOWN if syskey else WM_KEYDOWN
    _post(hwnd, msg, vk_code, 0)


def key_up(hwnd, vk_code, syskey=False):
    """Post a key-up message to the window."""
    msg = WM_SYSKEYUP if syskey else WM_KEYUP
    _post(hwnd, msg, vk_code, 0)


def send_key(hwnd, vk_code, syskey=False):
    """Post a single key press (down+up)."""
    key_down(hwnd, vk_code, syskey)
    time.sleep(0.05)
    key_up(hwnd, vk_code, syskey)
    time.sleep(0.05)


def send_text(hwnd, text):
    """Send a string of text via keystrokes (lowercase only, no Shift)."""
    for ch in text:
        vk = ord(ch.upper())
        send_key(hwnd, vk)


def send_enter(hwnd):
    send_key(hwnd, 0x0D)  # VK_RETURN


def send_escape(hwnd):
    send_key(hwnd, 0x1B)  # VK_ESCAPE


def send_tab(hwnd):
    send_key(hwnd, 0x09)  # VK_TAB


WM_CLOSE = 0x0010


def send_close(hwnd):
    """Post WM_CLOSE to the window to trigger the registered close handler."""
    _post(hwnd, WM_CLOSE, 0, 0)


def send_alt_f(hwnd):
    """Send Alt+F key sequence to set the NWG seq_alt_f flag.

    This activates the Rust-level Alt+F detector so that a subsequent
    Q keystroke (sent via the normal key path to the focused child)
    triggers save_before_quit.  We do NOT send Q here — the replayer
    sends all keystrokes including Q through the normal input path so
    that the Rust handle_key function processes them in order.

    Alt down uses WM_SYSKEYDOWN to set the Alt modifier.  F uses regular
    WM_KEYDOWN so TranslateMessage generates WM_CHAR('f') rather than
    WM_SYSCHAR('f'), avoiding the NWG menu accelerator.
    """
    key_down(hwnd, 0x12, syskey=True)   # VK_MENU, WM_SYSKEYDOWN
    time.sleep(0.1)
    key_down(hwnd, 0x46, syskey=False)  # VK_F, WM_KEYDOWN
    time.sleep(0.05)
    key_up(hwnd, 0x46, syskey=False)    # VK_F up
    time.sleep(0.05)
    key_up(hwnd, 0x12, syskey=True)     # VK_MENU up, WM_SYSKEYUP


def send_down(hwnd):
    send_key(hwnd, 0x28)  # VK_DOWN


def send_right(hwnd):
    send_key(hwnd, 0x27)  # VK_RIGHT


def find_corro_window():
    """Find the HWND of the corro window by enumerating windows."""
    if not HAS_WIN32:
        return None
    found_hwnd = [None]
    def enum_callback(hwnd, _lparam):
        buf = ctypes.create_unicode_buffer(256)
        length = ctypes.windll.user32.GetWindowTextW(hwnd, buf, 256)
        if length > 0 and "corro" in buf.value.lower():
            found_hwnd[0] = hwnd
            return False
        return True
    callback = ctypes.CFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)
    ctypes.windll.user32.EnumWindows(callback(enum_callback), 0)
    return found_hwnd[0]


def _launch_and_wait(binary, test_file):
    """Launch corro, wait for window, and return (proc, hwnd).

    Stderr is NOT piped: the debug binary can produce enough eprintln! output
    to fill the 4KB Windows pipe buffer, which deadlocks the process before
    communicate() is called.  By letting stderr flow to the real stderr we
    avoid the deadlock entirely.  The caller can still observe stderr output
    (it appears inline on the console).
    """
    proc = subprocess.Popen(
        [binary, "--gui", test_file],
        stdout=subprocess.PIPE,
        stderr=None,
    )
    # Retry finding the window: initial 1.5s, then up to 5 more at 1s intervals
    hwnd = None
    for attempt in range(6):
        hwnd = find_corro_window()
        if hwnd:
            break
        if attempt == 0:
            time.sleep(1.5)
        else:
            time.sleep(1.0)
    if hwnd:
        # Bring to foreground for reliable focus (though PostMessage
        # bypasses foreground requirements).
        ctypes.windll.user32.SetForegroundWindow(hwnd)
        ctypes.windll.user32.BringWindowToTop(hwnd)
        time.sleep(0.5)
    else:
        print("WARNING: corro window not found")
    return proc, hwnd


def _cleanup(proc, label="", hwnd=None):
    """Try to gracefully terminate corro, then force kill if needed.
    Returns True if the process exited cleanly (returncode == 0).

    If hwnd is provided, tries Alt+F+Q keystrokes before force-killing.

    Note: stderr is NOT piped (see _launch_and_wait), so any stderr
    output from the child appears directly on the parent's stderr.
    """
    elapsed = 0.0
    import time as _time
    t0 = _time.time()

    # Check if hwnd is still valid before any PostMessageW calls
    hwnd_valid = False
    if hwnd is not None and HAS_WIN32:
        hwnd_valid = ctypes.windll.user32.IsWindow(hwnd) != 0
        if not hwnd_valid:
            print(f"  [{label}] hwnd={hwnd} is no longer a valid window")

    # First try: communicate with 8s timeout
    try:
        stdout, _stderr = proc.communicate(timeout=8)
        elapsed = _time.time() - t0
        ok = proc.returncode == 0
        if not ok:
            print(f"  [{label}] exit code={proc.returncode} elapsed={elapsed:.1f}s")
        if stdout:
            sys.stdout.write(stdout.decode("utf-8", errors="replace"))
        return ok
    except subprocess.TimeoutExpired:
        elapsed = _time.time() - t0
        print(f"  [{label}] timed out after {elapsed:.1f}s, trying Alt+F+Q fallback")

    # Second try: if we have a valid HWND, send Alt+F+Q as fallback quit mechanism
    if hwnd_valid:
        print(f"  [{label}] sending Alt+F+Q fallback")
        send_alt_f(hwnd)
        _time.sleep(0.3)
        send_key(hwnd, 0x51)  # VK_Q
        _time.sleep(0.5)
        try:
            stdout, _stderr = proc.communicate(timeout=6)
            elapsed = _time.time() - t0
            ok = proc.returncode == 0
            if not ok:
                print(f"  [{label}] Alt+F+Q: exit code={proc.returncode} elapsed={elapsed:.1f}s")
            if stdout:
                sys.stdout.write(stdout.decode("utf-8", errors="replace"))
            if ok:
                return True
        except subprocess.TimeoutExpired:
            elapsed = _time.time() - t0
            print(f"  [{label}] Alt+F+Q also timed out")
    elif hwnd is not None:
        print(f"  [{label}] skipping Alt+F+Q fallback (window no longer valid)")

    # Third try: WM_CLOSE again (in case previous attempt was dropped)
    if hwnd_valid:
        print(f"  [{label}] re-sending WM_CLOSE")
        send_close(hwnd)
        _time.sleep(0.5)
        try:
            stdout, _stderr = proc.communicate(timeout=4)
            elapsed = _time.time() - t0
            ok = proc.returncode == 0
            if stdout:
                sys.stdout.write(stdout.decode("utf-8", errors="replace"))
            if ok:
                return True
        except subprocess.TimeoutExpired:
            elapsed = _time.time() - t0
            print(f"  [{label}] WM_CLOSE retry also timed out")
    elif hwnd is not None:
        print(f"  [{label}] skipping WM_CLOSE retry (window no longer valid)")

    # Finally force-kill
    elapsed = _time.time() - t0
    print(f"  [{label}] force-killing after {elapsed:.1f}s total")
    proc.kill()
    try:
        stdout, _stderr = proc.communicate(timeout=3)
        if stdout:
            sys.stdout.write(stdout.decode("utf-8", errors="replace"))
    except subprocess.TimeoutExpired:
        print(f"  [{label}] force-kill also timed out")
        proc.kill()
    return False


def run_test_recrec5(binary="target/release/corro.exe"):
    """Test: open subtotal-tiny, enter data, quit."""
    print(f"Running recrec5 test with {binary}")
    _restore_test_file()
    test_file = "docs/tests/subtotal-tiny.corro"
    output_file = "docs/tests/subtotal-tiny.corro"  # corro writes back to same file
    if not os.path.exists(test_file):
        print(f"ERROR: test file not found: {test_file}")
        return False

    # Record initial output size (should be small, just test data)
    initial_size = os.path.getsize(output_file) if os.path.exists(output_file) else 0

    proc, hwnd = _launch_and_wait(binary, test_file)
    if not hwnd:
        print("WARNING: could not find corro window, keystrokes may go elsewhere")
    else:
        print(f"Found corro window (hwnd={hwnd})")

    # Type "42" into A1 and press Enter
    time.sleep(0.3)
    send_text(hwnd, "42")
    time.sleep(0.2)
    send_enter(hwnd)
    time.sleep(0.3)

    # Navigate to B1 and type "hello"
    time.sleep(0.2)
    send_text(hwnd, "hello")
    time.sleep(0.2)
    send_enter(hwnd)
    time.sleep(0.3)

    # Quit via WM_CLOSE.  The NWG backend's raw WM_CLOSE handler
    # calls quit_main_loop() which posts WM_QUIT, causing the message
    # loop to exit cleanly.  This avoids the race condition with
    # Alt+F+Q where PostMessageW(WM_KEYUP/Q) fails because the app
    # already started quitting after processing WM_KEYDOWN/Q.
    time.sleep(0.3)
    send_close(hwnd)
    time.sleep(0.5)

    result = _cleanup(proc, "recrec5", hwnd)
    final_size = os.path.getsize(output_file) if os.path.exists(output_file) else 0
    output_exists = "yes" if os.path.exists(output_file) and final_size > initial_size else "no"
    print(f"  output_file={output_exists} initial={initial_size}b final={final_size}b")
    _restore_test_file()
    print("recrec5 done")
    return result


def run_test_recrec6(binary="target/release/corro.exe"):
    """Test: open subtotal-tiny, navigate cells, quit."""
    print(f"Running recrec6 test with {binary}")
    _restore_test_file()
    test_file = "docs/tests/subtotal-tiny.corro"
    if not os.path.exists(test_file):
        print(f"ERROR: test file not found: {test_file}")
        return False

    proc, hwnd = _launch_and_wait(binary, test_file)
    if not hwnd:
        print("WARNING: could not find corro window, keystrokes may go elsewhere")
    else:
        print(f"Found corro window (hwnd={hwnd})")

    # Navigate with arrow keys and Tab
    send_down(hwnd)
    time.sleep(0.2)
    send_right(hwnd)
    time.sleep(0.2)
    send_tab(hwnd)
    time.sleep(0.2)

    # Type data
    time.sleep(0.3)
    send_text(hwnd, "test")
    time.sleep(0.2)
    send_enter(hwnd)
    time.sleep(0.3)

    # Quit via WM_CLOSE (see recrec5 for rationale).
    time.sleep(0.3)
    send_close(hwnd)
    time.sleep(0.5)

    result = _cleanup(proc, "recrec6", hwnd)
    _restore_test_file()
    print("recrec6 done")
    return result


# The committed test data file has blank lines between SET commands
# (42 lines total: 21 SET/FILL + 21 blank).  These produce the same
# workbook state as the compact 21-line form because the parser skips
# blank lines.  We include blank lines here so that the restored file
# is byte-identical to the committed git version.
_CANONICAL_LINES = [
    "SET $1:A1 1",
    "",
    "SET $1:A2 2",
    "",
    "SET $1:B1 4",
    "",
    "SET $1:B2 5",
    "",
    "SET $1:C~1 =TOTAL",
    "",
    "SET $1:[A3 =TOTAL",
    "",
    "SET $1:B2 105",
    "",
    "SET $1:[A4 MAX",
    "",
    "SET $1:[A5 =TOTAL",
    "",
    "SET $1:[A4 AVERAGE",
    "",
    "SET $1:[A~1 01234567890123456789",
    "",
    "SET $1:[A~1 ",
    "",
    "SET $1:C1 d",
    "",
    "SET $1:C~1 asdf",
    "",
    "SET $1:C~1 =TOTAL",
    "",
    "$1:FILL C1=",
    "",
    "SET $1:C~1 =TOTAL",
    "",
    "SET $1:[A_1 =TOTAL",
    "",
    "SET $1:[A7 Extra",
    "",
    "SET $1:A7 1",
    "",
    "SET $1:B7 2",
    "",
]


def _make_writable(p):
    """Ensure file p is writable using multiple fallback methods."""
    import subprocess as _sp, os as _os, stat as _stat
    try:
        if sys.platform == "win32":
            # First try attrib -r (most reliable on Windows)
            _sp.run(["attrib", "-R", p], capture_output=True, text=True, timeout=5)
            # Also try PowerShell to clear ReadOnly
            _sp.run(
                ["powershell", "-Command",
                 f"Set-ItemProperty -LiteralPath '{p}' -Name IsReadOnly -Value $false"],
                capture_output=True, text=True, timeout=5)
        _os.chmod(p, _stat.S_IWRITE | _stat.S_IREAD)
    except Exception as e:
        print(f"WARN: _make_writable({p}) failed: {e}")


def _restore_test_file():
    """Restore docs/tests/subtotal-tiny.corro to canonical content
    and ensure it is writable.

    The corro binary modifies this file in-place when launched with --gui,
    appending SET commands.  We restore it before each test run to avoid
    accumulating extra SET commands that would cause golden-file mismatches.

    This function rewrites the file from the hardcoded _CANONICAL_LINES list
    so the content is always correct regardless of prior corruption.
    """
    path = "docs/tests/subtotal-tiny.corro"

    # Ensure file is writable BEFORE writing
    _make_writable(path)

    # Write canonical content
    try:
        with open(path, "w", newline="") as f:
            for line in _CANONICAL_LINES:
                f.write(line + "\r\n")
        print(f"  restored {path} to {len(_CANONICAL_LINES)} canonical lines")
    except Exception as e:
        print(f"WARN: _restore_test_file error writing {path}: {e}")
        return

    # Also restore the companion file test_rec5.corro (must be byte-identical
    # to subtotal-tiny.corro per check_vals::test_data_files_are_consistent).
    companion = "test_rec5.corro"
    _make_writable(companion)
    try:
        with open(companion, "w", newline="") as f:
            for line in _CANONICAL_LINES:
                f.write(line + "\r\n")
        print(f"  restored {companion} to {len(_CANONICAL_LINES)} canonical lines")
    except Exception as e:
        print(f"WARN: _restore_test_file error writing {companion}: {e}")
    # Verify the files are writable now
    for p in (path, companion):
        if os.path.exists(p):
            try:
                with open(p, "ab") as f:
                    pass
            except PermissionError as e:
                print(f"WARN: {p} still not writable after restore: {e}")


def main():
    _restore_test_file()
    parser = argparse.ArgumentParser(description="GUI Replayer for NWG tests")
    parser.add_argument("--test", choices=["recrec5", "recrec6", "all"], default="all")
    parser.add_argument("--binary", default="")
    parser.add_argument("--list", action="store_true", help="List available tests")
    args = parser.parse_args()

    if args.list:
        print("Available tests:")
        print("  recrec5  - Open subtotal-tiny, enter data, quit")
        print("  recrec6  - Open subtotal-tiny, navigate cells, quit")
        return 0

    if sys.platform != "win32":
        print("ERROR: .gui_replayer.py is Windows-only (uses Win32 PostMessage)")
        return 1

    if not HAS_WIN32:
        print("ERROR: ctypes not available (needed for Win32 API)")
        return 1

    binary = args.binary or os.environ.get("BIN", "")
    if binary:
        # Explicit binary path provided; use it as-is.
        pass
    elif os.path.exists("target/release/corro.exe"):
        binary = "target/release/corro.exe"
    elif os.path.exists("target/debug/corro.exe"):
        binary = "target/debug/corro.exe"
    else:
        print("ERROR: corro.exe not found (neither target/debug/ nor target/release/)")
        print("  Building with: cargo +nightly build --features gui")
        rc = subprocess.call(["cargo", "+nightly", "build", "--features", "gui"])
        if rc != 0:
            print("ERROR: build failed")
            return 1
        binary = "target/debug/corro.exe"
    if not os.path.exists(binary):
        print(f"ERROR: binary still not found at {binary}")
        return 1
    # Warn if binary may be stale
    bin_mtime = os.path.getmtime(binary)
    src_patterns = ["src/**/*.rs", "rustxWidgets/**/*.rs", "Cargo.toml", "rustxWidgets/rustxwidgets/Cargo.toml"]
    newest_src = 0
    for pat in src_patterns:
        for f in glob.glob(pat, recursive=True):
            try:
                mtime = os.path.getmtime(f)
                if mtime > newest_src:
                    newest_src = mtime
            except OSError:
                pass
    if newest_src > bin_mtime:
        print(f"  WARNING: binary is older than source files; consider rebuilding with:")
        print(f"    cargo +nightly build{' --release' if 'release' in binary else ''} --features gui")

    tests = []
    if args.test == "all":
        tests = [("recrec5", run_test_recrec5), ("recrec6", run_test_recrec6)]
    else:
        tests = [(args.test, {"recrec5": run_test_recrec5, "recrec6": run_test_recrec6}[args.test])]

    failed = 0
    for name, fn in tests:
        print(f"\n--- {name} ---")
        if fn(binary):
            print(f"  PASS: {name}")
        else:
            print(f"  FAIL: {name}")
            failed += 1

    return failed


if __name__ == "__main__":
    sys.exit(main())
