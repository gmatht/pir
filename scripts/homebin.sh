#!/bin/bash
# Install pir into ~/bin as a portable glibc-2.17 release binary.
#
# Builds with `cargo-zigbuild --target x86_64-unknown-linux-gnu.2.17`,
# so the binary starts on any glibc >= 2.17 distro (CentOS 7 era through
# current Ubuntu/Fedora). TLS is NOT bundled: it comes from the host's
# libcurl.so.4 at runtime via lsb-curl (dlopen) — no static OpenSSL/curl,
# no host libssl needed at build time.
#
# Requires: `cargo-zigbuild` on PATH (`pip install cargo-zigbuild`)
# and `zig` on PATH (snap `zig`, or the `ziglang` pip bundle).
# Verify the floor after install:
#   objdump -T ~/bin/pir | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1
set -e
DEST=~/bin/pir
TARGET=x86_64-unknown-linux-gnu.2.17
# `zig` may live outside a minimal PATH (snap / pip bundle); fall back
# to well-known locations before giving up.
if ! command -v zig >/dev/null 2>&1 || ! zig version >/dev/null 2>&1; then
    # Remove broken zig entries from PATH before searching.
    # Snap's zig is commonly broken (disconnected mount); strip /snap/bin
    # so cargo-zigbuild doesn't rediscover it after we set up a working one.
    PATH=$(echo "$PATH" | tr ':' '\n' | grep -v '^/snap/bin$' | paste -sd:)
    export PATH
    for z in /usr/local/bin/zig-pip "$HOME/.local/bin/python-zig" /snap/bin/zig; do
        if [ -x "$z" ] && "$z" version >/dev/null 2>&1; then
            # If the binary isn't named "zig", symlink it into ~/bin so
            # cargo-zigbuild (which searches PATH for "zig") can find it.
            if [ "$(basename "$z")" != "zig" ]; then
                mkdir -p "$HOME/bin"
                ln -sf "$z" "$HOME/bin/zig"
                export PATH="$HOME/bin:$PATH"
            else
                export PATH="$(dirname "$z"):$PATH"
            fi
            break
        fi
    done
fi
if ! command -v cargo-zigbuild >/dev/null 2>&1; then
    echo "error: 'cargo-zigbuild' not found on PATH" >&2
    echo "  install: pip install cargo-zigbuild  (then ensure ~/.local/bin is on PATH," >&2
    echo "  or symlink it somewhere global: ln -s ~/.local/bin/cargo-zigbuild /usr/local/bin/)" >&2
    exit 1
fi
# No vendored C TLS code remains, but keep the clean forgiving: a stale
# host-CC cache is harmless, a missing package is not an error.
cargo clean -p lsb-curl -p lsb-loader 2>/dev/null || true
cargo zigbuild --release --target "$TARGET"
# NOTE: zigbuild normalizes the dotted triple for the output dir.
BIN="target/x86_64-unknown-linux-gnu/release/pir"
ls -lh "$BIN"
mkdir -p "$(dirname "$DEST")"
if [ -e "$DEST" ]; then
    i=1
    while [ -e "$DEST$i" ]
    do i=$((i+1))
    done
    mv "$DEST" "$DEST$i"
fi
cp "$BIN" "$DEST"
ls -lh $DEST
objdump -T $DEST | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1
