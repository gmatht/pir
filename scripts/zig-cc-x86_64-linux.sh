#!/bin/sh
# Linker wrapper: route rustc's link step through `zig cc` targeting
# glibc 2.17 (CentOS 7 era), so the produced binary starts on any
# glibc >= 2.17 distro while still being built on a modern host.
#
# Requires: `zig` on PATH (https://ziglang.org/download).
# Tested with zig 0.16.0.
#
# The glibc floor is enforced by zig's bundled 2.17 headers/libs, not by
# the host toolchain — `objdump -T <binary> | grep -o 'GLIBC_[0-9.]*'`
# must top out at GLIBC_2.17 or below.
#
# The host multiarch lib dirs are appended so `-l<sys>` lookups for
# system libs rustc requests (e.g. libz, whose .so dev symlink lives
# outside zig's sysroot) resolve. Zig still prefers its bundled 2.17
# libc, so the floor holds — verify with the objdump command above.
set -e
ZIG="${ZIG:-zig}"
exec "$ZIG" cc -target x86_64-linux-gnu.2.17 \
    -L/lib/x86_64-linux-gnu -L/usr/lib/x86_64-linux-gnu "$@"
