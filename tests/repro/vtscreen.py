#!/usr/bin/env python3
"""Minimal VT100 emulator good enough to render pir's footer zone from a raw
capture, so a resize bug can be *seen* rather than guessed at.

Usage: vtscreen.py <rawfile> [marker]
"""
import re, sys


class Screen:
    def __init__(self, rows=24, cols=100):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.r = self.c = 0          # 0-based cursor
        self.top, self.bot = 0, rows - 1  # scroll region (0-based, inclusive)
        self.saved = (0, 0)

    def _scroll_up(self):
        del self.grid[self.top]
        self.grid.insert(self.bot, [" "] * self.cols)

    def put(self, ch):
        if self.c >= self.cols:
            # Real terminals autowrap: writing past the last column moves to
            # the next line (scrolling the region if at its bottom). Modelling
            # this is essential — a zone painted at a stale, wider width wraps
            # onto the next row and that *is* the reported corruption.
            self.c = 0
            if self.r == self.bot:
                self._scroll_up()
            elif self.r < self.rows - 1:
                self.r += 1
        self.grid[self.r][self.c] = ch
        self.c += 1

    def feed(self, s):
        i = 0
        while i < len(s):
            ch = s[i]
            if ch == "\x1b":
                m = re.match(r"\x1b\[([0-9;?]*)([A-Za-z@`~])", s[i:])
                if m:
                    params, final = m.group(1), m.group(2)
                    self.csi(params, final)
                    i += m.end()
                    continue
                i += 1
                continue
            if ch == "\n":
                # LF: if at bottom of region, scroll
                if self.r == self.bot:
                    self._scroll_up()
                elif self.r < self.rows - 1:
                    self.r += 1
                i += 1
                continue
            if ch == "\r":
                self.c = 0
                i += 1
                continue
            self.put(ch)
            i += 1

    def csi(self, params, final):
        ps = [int(p) for p in params.split(";") if p.isdigit()]
        if final == "H" or final == "f":
            row = (ps[0] if len(ps) > 0 else 1) - 1
            col = (ps[1] if len(ps) > 1 else 1) - 1
            self.r = max(0, min(self.rows - 1, row))
            self.c = col
        elif final == "r":
            if len(ps) >= 2:
                self.top, self.bot = ps[0] - 1, ps[1] - 1
            elif not ps:
                self.top, self.bot = 0, self.rows - 1
        elif final == "K":
            for c in range(self.c, self.cols):
                self.grid[self.r][c] = " "
        elif final == "J":
            for rr in range(self.r, self.rows):
                for c in range(self.cols):
                    self.grid[rr][c] = " "
        elif final == "s":
            self.saved = (self.r, self.c)
        elif final == "u":
            self.r, self.c = self.saved

    def dump(self):
        return "\n".join("".join(row).rstrip() for row in self.grid)


def main():
    data = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
    marker = sys.argv[2] if len(sys.argv) > 2 else None
    # Split the capture into per-marker segments and feed each into a fresh
    # screen, so the marker lines themselves never land in the rendered grid.
    parts = re.split(r"\n===== MARK ([a-z0-9-]+) =====\n", data)
    if marker is not None:
        picked = None
        for i in range(1, len(parts), 2):
            if parts[i] == marker:
                picked = parts[i + 1]
        if picked is None:
            raise SystemExit("no such marker: %s" % marker)
        feed = picked
    else:
        feed = "".join(parts[i + 1] for i in range(1, len(parts), 2))
    s = Screen()
    s.feed(feed)
    print(s.dump())


if __name__ == "__main__":
    main()
