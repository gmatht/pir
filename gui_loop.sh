#!/usr/bin/env bash
# GUI backend test runner.
# Tests GTK3/GTK4 (via WSL) and NWG (Windows native) backends.
#
# Usage:
#   ./gui_loop.sh                  # Run all available backend tests
#   ./gui_loop.sh --nwg-only      # Run only NWG (Windows native) tests
#   ./gui_loop.sh --gtk-only      # Run only GTK tests (requires WSL)
#   ./gui_loop.sh --check-wsl     # Check WSL prerequisites only
#   ./gui_loop.sh --list          # List available test scenarios

set -eo pipefail
cd "$(dirname "$0")"

PASS=0
FAIL=0
SKIP=0

pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
skip() { SKIP=$((SKIP+1)); echo "  SKIP: $1"; }

# Parse args
NWG_ONLY=false
GTK_ONLY=false
CHECK_WSL=false
LIST_ONLY=false
for arg in "$@"; do
    case "$arg" in
        --nwg-only) NWG_ONLY=true ;;
        --gtk-only) GTK_ONLY=true ;;
        --check-wsl) CHECK_WSL=true ;;
        --list) LIST_ONLY=true ;;
    esac
done

if [ "$LIST_ONLY" = true ]; then
    echo "Available test scenarios:"
    echo "  NWG build & tests      — Rust build with --features gui + integration tests"
    echo "  NWG replayer tests     — Python-based GUI replayer (Windows + python3 only)"
    echo "  WSL/GTK prerequisites  — Check WSL Rust nightly & GTK library availability"
    echo "  GTK build (via WSL)    — Rust build with --features gui inside WSL"
    echo "  GTK tests (via WSL)    — Rust integration tests inside WSL"
    echo "  WASM build             — Cross-compile check for wasm32-unknown-unknown"
    exit 0
fi

# ---------------------------------------------------------------------------
# Prerequisite checks
# ---------------------------------------------------------------------------

HAS_WSL=false
HAS_WSL_BASH=false
HAS_PYTHON=false
PYTHON=""
HAS_CARGO=false
HAS_NIGHTLY=false
HAS_WSL_CARGO=false
HAS_WSL_NIGHTLY=false
WSL_GTK_AVAILABLE=false
WSL_GTK_VERSION=""

if command -v wsl.exe &>/dev/null; then
    HAS_WSL=true
    # Wrap wsl.exe to prevent MSYS2 path conversion (which would convert
    # WSL paths like /mnt/d/... to D:\..., breaking the command).
    wsl() { MSYS2_ARG_CONV_EXCL="*" wsl.exe "$@"; }
    if wsl bash --version &>/dev/null; then
        HAS_WSL_BASH=true
    fi
fi

if command -v python3 &>/dev/null; then
    HAS_PYTHON=true
    PYTHON="python3"
elif command -v python &>/dev/null; then
    HAS_PYTHON=true
    PYTHON="python"
fi

if command -v cargo &>/dev/null || which cargo &>/dev/null 2>&1; then
    HAS_CARGO=true
fi

# Check for Rust nightly toolchain on the host (used by all cargo +nightly commands)
HAS_NIGHTLY=false
if command -v rustup &>/dev/null; then
    if rustup toolchain list 2>/dev/null | grep -q nightly; then
        HAS_NIGHTLY=true
    fi
fi

# Select cargo command: prefer nightly, fall back to default
CARGO_CMD="cargo"
if [ "$HAS_NIGHTLY" = true ]; then
    CARGO_CMD="cargo +nightly"
fi

# Deep WSL prerequisite checks
if [ "$HAS_WSL" = true ] && [ "$HAS_WSL_BASH" = true ]; then
    # Determine path to cargo inside WSL (may not be in non-interactive PATH)
    WSL_CARGO="cargo"
    WSL_RUSTUP="rustup"
    HAS_WSL_CARGO=false
    if wsl bash -c "command -v cargo &>/dev/null" 2>/dev/null; then
        HAS_WSL_CARGO=true
    elif wsl bash -c '[ -x "$HOME/.cargo/bin/cargo" ]' 2>/dev/null; then
        HAS_WSL_CARGO=true
        # Resolve $HOME inside WSL now, so the path is absolute and
        # won't be host-expanded when used later inside double quotes.
        WSL_CARGO=$(wsl bash -c 'echo "$HOME/.cargo/bin/cargo"')
        WSL_RUSTUP=$(wsl bash -c 'echo "$HOME/.cargo/bin/rustup"')
    elif wsl bash -c '[ -x /root/.cargo/bin/cargo ]' 2>/dev/null; then
        HAS_WSL_CARGO=true
        WSL_CARGO=/root/.cargo/bin/cargo
        WSL_RUSTUP=/root/.cargo/bin/rustup
    fi
    # Check for Rust nightly toolchain in WSL
    HAS_WSL_NIGHTLY=false
    if wsl bash -c 'command -v rustup &>/dev/null' 2>/dev/null; then
        # rustup is in PATH
        if wsl bash -c 'rustup toolchain list 2>/dev/null | grep -q nightly' 2>/dev/null; then
            HAS_WSL_NIGHTLY=true
        fi
    elif [ "$HAS_WSL_CARGO" = true ]; then
        # rustup not in PATH but cargo dir is known; use detected path.
        if wsl bash -c "$WSL_RUSTUP toolchain list 2>/dev/null | grep -q nightly" 2>/dev/null; then
            HAS_WSL_NIGHTLY=true
        fi
    fi
    # Check for GTK libraries in WSL (dlopen-able at runtime)
    WSL_GTK_CHECK=$(wsl bash -c '
        has_lib() { ldconfig -p 2>/dev/null | grep -q "$1" || [ -f "$1" ] || [ -f "/usr/lib/$1" ] || [ -f "/usr/lib64/$1" ] || [ -f "/usr/lib/x86_64-linux-gnu/$1" ] || [ -f "/lib/$1" ] || [ -f "/lib64/$1" ] || [ -f "/lib/x86_64-linux-gnu/$1" ]; }
        if has_lib "libgtk-4.so"; then echo "gtk4"
        elif has_lib "libgtk-3.so"; then echo "gtk3"
        else echo "none"
        fi
    ' 2>/dev/null || echo "unknown")
    if [ "$WSL_GTK_CHECK" = "gtk4" ] || [ "$WSL_GTK_CHECK" = "gtk3" ]; then
        WSL_GTK_AVAILABLE=true
        WSL_GTK_VERSION=$WSL_GTK_CHECK
    fi
fi

echo "=== GUI Backend Test Suite ==="
echo "Host:       $(uname -s 2>/dev/null || echo unknown)"
echo "WSL:        $HAS_WSL  WSL bash: $HAS_WSL_BASH  WSL cargo: $HAS_WSL_CARGO  WSL nightly: $HAS_WSL_NIGHTLY"
echo "WSL GTK:    $WSL_GTK_VERSION"
echo "Python:     $HAS_PYTHON  Cargo: $HAS_CARGO  Nightly: $HAS_NIGHTLY  Cargo cmd: ${CARGO_CMD/cargo +nightly/+nightly}"
echo ""

# ---------------------------------------------------------------------------
# Check WSL prerequisites (--check-wsl or always)
# ---------------------------------------------------------------------------

if [ "$CHECK_WSL" = true ] || ([ "$GTK_ONLY" = false ] && [ "$NWG_ONLY" = false ]); then
    if [ "$HAS_WSL" = true ] && [ "$HAS_WSL_BASH" = true ]; then
        echo "--- WSL prerequisite check ---"
        WSL_PREREQ_FAIL=0

        if [ "$HAS_WSL_CARGO" = false ]; then
            echo "  WARNING: cargo not installed in WSL."
            echo "    Install with: wsl bash -c 'curl --proto \"=https\" --tlsv1.2 -sSf https://sh.rustup.rs | sh'"
            WSL_PREREQ_FAIL=1
        fi
        if [ "$HAS_WSL_NIGHTLY" = false ]; then
            echo "  WARNING: Rust nightly toolchain not installed in WSL."
            echo "    Install with: wsl bash -c 'rustup toolchain install nightly'"
            WSL_PREREQ_FAIL=1
        fi
        if [ "$WSL_GTK_AVAILABLE" = false ] && [ "$WSL_GTK_CHECK" != "unknown" ]; then
            echo "  WARNING: GTK libraries not found in WSL (runtime dlopen will fail)."
            echo "    Install with: wsl bash -c 'sudo apt-get install libgtk-4-dev libcairo2-dev'"
            echo "    Or for GTK3:  wsl bash -c 'sudo apt-get install libgtk-3-dev libcairo2-dev'"
            WSL_PREREQ_FAIL=1
        fi
        if [ "$WSL_GTK_CHECK" = "unknown" ]; then
            echo "  WARNING: Could not check GTK library availability in WSL."
            echo "    (ldconfig or file search failed — GTK may or may not be available at runtime)"
            WSL_PREREQ_FAIL=1
        fi
        # Check DISPLAY
        WSL_DISPLAY=$(wsl bash -c 'echo "${DISPLAY:-unset}"' 2>/dev/null || echo "unset")
        if [ "$WSL_DISPLAY" = "unset" ]; then
            echo "  INFO: DISPLAY not set in WSL (required for GTK runtime tests, not for build)."
            echo "    Set with: wsl bash -c 'export DISPLAY=:0'"
        fi

        if [ "$WSL_PREREQ_FAIL" -eq 0 ]; then
            pass "WSL prerequisites"
        else
            fail "WSL prerequisites (see warnings above)"
        fi
        echo ""
    else
        skip "WSL prerequisite check (WSL/bash not available)"
        echo ""
    fi
fi

# ---------------------------------------------------------------------------
# Build (NWG on Windows, skip if --gtk-only)
# ---------------------------------------------------------------------------

if [ "$GTK_ONLY" = false ] && [ "$HAS_CARGO" = true ]; then
    echo "--- Building GUI backend (NWG) ---"
    # Build release first (needed for NWG replayer tests)
    if $CARGO_CMD build --release --features gui 2>&1; then
        pass "Build release (gui features)"
    else
        fail "Build release (gui features)"
    fi
    if $CARGO_CMD build --features gui 2>&1; then
        pass "Build debug (gui features)"
    else
        fail "Build debug (gui features)"
    fi
    echo ""

    # Restore test data files from git before running tests.
    git checkout -- docs/tests/subtotal-tiny.corro test_rec5.corro 2>/dev/null || true
    for f in docs/tests/subtotal-tiny.corro test_rec5.corro; do
        if [ -f "$f" ]; then
            head -n 42 "$f" > /tmp/$(basename "$f").clean 2>/dev/null || true
            cp -f /tmp/$(basename "$f").clean "$f" 2>/dev/null || true
            chmod +w "$f" 2>/dev/null || true
            rm -f /tmp/$(basename "$f").clean 2>/dev/null || true
        fi
    done
    if [[ $OSTYPE == "msys" || $OSTYPE == "cygwin" || -n "${WINDIR:-}" ]]; then
        for f in docs/tests/subtotal-tiny.corro test_rec5.corro; do
            if [ -f "$f" ]; then
                cmd.exe /c "attrib -R $f" 2>/dev/null || true
            fi
        done
    fi
    if [ -f test_rec5.corro ]; then
        chmod +w test_rec5.corro 2>/dev/null || true
    fi

    # Rust integration tests (NWG on Windows)
    echo "--- Running recording replay tests (test_tiny5 / test_tiny6) ---"
    if $CARGO_CMD test --test test_tiny5 2>&1; then
        pass "recrec5 (test_tiny5)"
    else
        fail "recrec5 (test_tiny5)"
    fi

    if $CARGO_CMD test --test test_tiny6 2>&1; then
        pass "recrec6 (test_tiny6)"
    else
        fail "recrec6 (test_tiny6)"
    fi
    echo ""

    # GUI-specific Rust tests
    echo "--- Running GUI-specific Rust tests ---"
    for t in check_vals gui_enter_text_creates_file check_agg_gui check_gui_imports quit_alt_f_q; do
        if $CARGO_CMD test --features gui --test "$t" 2>&1; then
            pass "$t"
        else
            fail "$t"
        fi
    done
    # Cross-backend consistency tests (pattern verification, no live GUI needed)
    if $CARGO_CMD test --features gui --test gtk_todo_tests 2>&1; then
        pass "gtk_todo_tests (cross-backend consistency)"
    else
        fail "gtk_todo_tests (cross-backend consistency)"
    fi
    echo ""
elif [ "$GTK_ONLY" = false ]; then
    skip "Rust build and tests (cargo not available)"
    echo ""
fi

# ---------------------------------------------------------------------------
# NWG replayer tests (Windows only)
# ---------------------------------------------------------------------------

if [ "$GTK_ONLY" = false ] && [[ $OSTYPE == "msys" || $OSTYPE == "cygwin" || -n "${WINDIR:-}" ]]; then
    echo "--- NWG replayer tests ---"
    if [ "$HAS_PYTHON" = true ] && [ -f .gui_replayer.py ]; then
        if BIN="target/release/corro.exe" $PYTHON .gui_replayer.py --test recrec5 2>&1; then
            pass "NWG replayer: recrec5"
        else
            fail "NWG replayer: recrec5"
        fi
        if BIN="target/release/corro.exe" $PYTHON .gui_replayer.py --test recrec6 2>&1; then
            pass "NWG replayer: recrec6"
        else
            fail "NWG replayer: recrec6"
        fi
    else
        skip "NWG replayer tests (no python or .gui_replayer.py)"
    fi
elif [ "$GTK_ONLY" = false ]; then
    skip "NWG replayer tests (not Windows)"
fi
echo ""

# ---------------------------------------------------------------------------
# GTK tests via WSL (Linux/WSL only)
# ---------------------------------------------------------------------------

if [ "$NWG_ONLY" = false ] && [ "$HAS_WSL" = true ] && [ "$HAS_WSL_BASH" = true ] && [ "$HAS_WSL_CARGO" = true ]; then
    echo "--- GTK3/GTK4 tests via WSL ---"

    # When running inside WSL (host OS is Linux), pwd is already a valid Linux path.
    # When running on Windows (MINGW/MSYS/CYGWIN), convert from /d/... to /mnt/d/...
    HOST_OS="$(uname -s)"
    if [ "${HOST_OS#Linux}" != "$HOST_OS" ]; then
        # Already inside WSL/Linux — pwd is a native Linux path
        WSL_PWD="$(pwd)"
    else
        # On Windows — Git Bash gives /d/...; WSL needs /mnt/d/...
        WSL_PWD="/mnt/$(pwd | sed 's|^/\([a-zA-Z]\)/|\L\1/|')"
    fi

    # Check nightly availability in WSL
    if [ "$HAS_WSL_NIGHTLY" = false ]; then
        echo "  WARNING: Rust nightly not installed in WSL."
        echo "    Run: wsl bash -c 'rustup toolchain install nightly'"
        echo "    Skipping GTK build..."
        skip "GTK build (nightly not installed in WSL)"
    else
        echo "  WSL nightly: OK"
        echo "  WSL GTK libs: $WSL_GTK_VERSION"
        echo "  WSL PWD: $WSL_PWD"

        # Build with GTK feature inside WSL; capture error output for diagnostics
        GTK_BUILD_OUT=$(wsl bash -c "cd '$WSL_PWD' && DISPLAY=:0 $WSL_CARGO +nightly build --features gui" 2>&1) && {
            pass "GTK build"
        } || {
            fail "GTK build"
            echo "  Build output (last 20 lines):"
            echo "$GTK_BUILD_OUT" | tail -20
            echo "  TIP: Check WSL prerequisites with: ./gui_loop.sh --check-wsl"
            echo "  TIP: Run manually: wsl bash -c \"cd '$WSL_PWD' && $WSL_CARGO +nightly build --features gui\""
        }

        # Run Rust tests inside WSL
        if wsl bash -c "cd '$WSL_PWD' && DISPLAY=:0 $WSL_CARGO +nightly test --test test_tiny5" 2>&1; then
            pass "GTK recrec5"
        else
            fail "GTK recrec5"
        fi
        if wsl bash -c "cd '$WSL_PWD' && DISPLAY=:0 $WSL_CARGO +nightly test --test test_tiny6" 2>&1; then
            pass "GTK recrec6"
        else
            fail "GTK recrec6"
        fi
        # Cross-backend consistency tests (Linux-specific test gui_spreadsheet_scrollbars runs only on Linux)
        CROSS_BACKEND_OUT=$(wsl bash -c "cd '$WSL_PWD' && DISPLAY=:0 $WSL_CARGO +nightly test --features gui --test gtk_todo_tests" 2>&1) && {
            pass "GTK cross-backend consistency (gtk_todo_tests)"
        } || {
            fail "GTK cross-backend consistency (gtk_todo_tests)"
            echo "  Test output (last 20 lines):"
            echo "$CROSS_BACKEND_OUT" | tail -20
            echo "  TIP: Run manually: wsl bash -c \"cd '$WSL_PWD' && $WSL_CARGO +nightly test --features gui --test gtk_todo_tests\""
        }
        # terminal_parity requires pancurses+ratatui+tmux; run if available
        if wsl bash -c "command -v tmux &>/dev/null" 2>/dev/null; then
            TERM_PARITY_OUT=$(wsl bash -c "cd '$WSL_PWD' && DISPLAY=:0 $WSL_CARGO +nightly test --features combined-gui,ratatui --test terminal_parity" 2>&1) && {
                pass "GTK pancurses/ratatui parity (terminal_parity)"
            } || {
                fail "GTK pancurses/ratatui parity (terminal_parity)"
                echo "  Test output (last 20 lines):"
                echo "$TERM_PARITY_OUT" | tail -20
                echo "  TIP: Run manually: wsl bash -c \"cd '$WSL_PWD' && $WSL_CARGO +nightly test --features combined-gui,ratatui --test terminal_parity\""
            }
        else
            skip "GTK pancurses/ratatui parity (tmux not available in WSL)"
        fi
    fi
elif [ "$NWG_ONLY" = false ] && [ "$HAS_WSL" = true ] && [ "$HAS_WSL_BASH" = true ]; then
    skip "GTK tests (cargo not installed in WSL)"
elif [ "$NWG_ONLY" = false ]; then
    skip "GTK tests (WSL/bash not available)"
fi

# ---------------------------------------------------------------------------
# WASM build (cross-compile check)
# ---------------------------------------------------------------------------

if [ "$NWG_ONLY" = false ] && [ "$GTK_ONLY" = false ]; then
    echo "--- WASM build ---"
    if [ "$HAS_CARGO" = true ]; then
        if rustup target list --toolchain nightly 2>/dev/null | grep -q "wasm32-unknown-unknown (installed)" || [ "$HAS_NIGHTLY" = false ]; then
            if $CARGO_CMD build --target wasm32-unknown-unknown --features wasm --no-default-features 2>&1; then
                pass "WASM build"
            else
                fail "WASM build"
            fi
        else
            skip "WASM build (wasm32-unknown-unknown target not installed)"
        fi
    else
        skip "WASM build (cargo not available)"
    fi
fi
echo ""

echo ""
echo "=== Results: $PASS passed, $FAIL failed, $SKIP skipped ==="
exit $FAIL
