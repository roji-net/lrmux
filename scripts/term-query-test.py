#!/usr/bin/env python3
"""Terminal query testbed for lrmux (no vim required).

Sends OSC/DSR queries the same way vim/neovim do, prints what came back
(or TIMEOUT), then a truecolor strip. Stay alive so you can inspect.

Usage inside lrmux:
  lrmux new-server -s tq -- 'python3 /path/to/term-query-test.py'
  # or in an existing pane:
  python3 scripts/term-query-test.py

Env:
  TERM_QUERY_TIMEOUT=1.0   seconds to wait per query (default 1.0)
"""

from __future__ import annotations

import os
import select
import sys
import termios
import time
import tty


def eprint(*args: object) -> None:
    print(*args, file=sys.stderr, flush=True)


def open_tty() -> tuple[int, list | None, bool]:
    """Return (fd, old_termios or None, owns_fd)."""
    try:
        fd = os.open("/dev/tty", os.O_RDWR | os.O_NOCTTY)
        old = termios.tcgetattr(fd)
        tty.setraw(fd)
        return fd, old, True
    except OSError:
        # Fallback: stdin/stdout are the PTY inside a multiplexer pane.
        fd = sys.stdin.fileno()
        old = termios.tcgetattr(fd)
        tty.setraw(fd)
        return fd, old, False


def restore_tty(fd: int, old: list | None, owns_fd: bool) -> None:
    if old is not None:
        termios.tcsetattr(fd, termios.TCSAFLUSH, old)
    if owns_fd:
        os.close(fd)


def read_until(
    fd: int,
    timeout: float,
    done,
) -> bytes:
    """Read until `done(buf)` is true or timeout. Returns bytes read (maybe empty)."""
    buf = bytearray()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        r, _, _ = select.select([fd], [], [], max(remaining, 0.0))
        if not r:
            break
        chunk = os.read(fd, 4096)
        if not chunk:
            break
        buf.extend(chunk)
        if done(buf):
            break
    return bytes(buf)


def osc_complete(buf: bytes) -> bool:
    # ESC ] ... BEL  or  ESC ] ... ESC \
    if b"\x1b]" not in buf:
        return False
    i = buf.find(b"\x1b]")
    rest = buf[i + 2 :]
    if b"\x07" in rest:
        return True
    if b"\x1b\\" in rest:
        return True
    return False


def cpr_complete(buf: bytes) -> bool:
    # ESC [ row ; col R
    if b"\x1b[" not in buf:
        return False
    i = buf.find(b"\x1b[")
    return b"R" in buf[i + 2 :]


def dsr_complete(buf: bytes) -> bool:
    # ESC [ 0 n  (or similar)
    if b"\x1b[" not in buf:
        return False
    i = buf.find(b"\x1b[")
    return b"n" in buf[i + 2 :]


def fmt_bytes(b: bytes) -> str:
    out = []
    for c in b:
        if c == 0x1B:
            out.append("ESC")
        elif c == 0x07:
            out.append("BEL")
        elif 32 <= c < 127:
            out.append(chr(c))
        else:
            out.append(f"\\x{c:02x}")
    return "".join(out)


def run_query(fd: int, name: str, query: bytes, complete, timeout: float) -> None:
    # Drain any pending input so a previous reply can't fake a success.
    while True:
        r, _, _ = select.select([fd], [], [], 0)
        if not r:
            break
        os.read(fd, 4096)

    os.write(fd, query)

    t0 = time.monotonic()
    raw = read_until(fd, timeout, complete)
    dt = (time.monotonic() - t0) * 1000.0

    if raw and complete(raw):
        print(f"OK   {name:12}  {dt:6.1f}ms  {fmt_bytes(raw)}", flush=True)
    elif raw:
        print(
            f"PARTIAL {name:8}  {dt:6.1f}ms  {fmt_bytes(raw)}",
            flush=True,
        )
    else:
        print(f"TIMEOUT {name:7}  {dt:6.1f}ms  (no reply)", flush=True)


def truecolor_strip() -> None:
    print(flush=True)
    print("Truecolor strip (smooth = 24-bit path OK):", flush=True)
    parts = []
    for i in range(0, 78):
        r = int(255 - (i * 255 / 77))
        g = int(i * 510 / 77)
        if g > 255:
            g = 510 - g
        b = int(i * 255 / 77)
        parts.append(f"\x1b[48;2;{r};{g};{b}m \x1b[0m")
    print("".join(parts), flush=True)
    print(flush=True)


def main() -> int:
    timeout = float(os.environ.get("TERM_QUERY_TIMEOUT", "1.0"))
    print("=== lrmux terminal query testbed ===", flush=True)
    print(
        f"TERM={os.environ.get('TERM', '')!r}  "
        f"COLORTERM={os.environ.get('COLORTERM', '')!r}  "
        f"timeout={timeout}s",
        flush=True,
    )
    print(flush=True)

    try:
        fd, old, owns = open_tty()
    except OSError as e:
        eprint(f"cannot open tty: {e}")
        return 1

    try:
        # Same queries vim uses at startup (BEL-terminated OSC).
        run_query(fd, "OSC 11 bg", b"\x1b]11;?\x07", osc_complete, timeout)
        run_query(fd, "OSC 10 fg", b"\x1b]10;?\x07", osc_complete, timeout)
        run_query(fd, "OSC 12 cursor", b"\x1b]12;?\x07", osc_complete, timeout)
        run_query(fd, "OSC 4;0", b"\x1b]4;0;?\x07", osc_complete, timeout)
        # ST-terminated variants (some apps use these).
        run_query(fd, "OSC 11 ST", b"\x1b]11;?\x1b\\", osc_complete, timeout)
        # Device status / cursor position (answered locally by lrmux).
        run_query(fd, "CSI 6n CPR", b"\x1b[6n", cpr_complete, timeout)
        run_query(fd, "CSI 5n DSR", b"\x1b[5n", dsr_complete, timeout)
        run_query(
            fd,
            "CSI 0c DA",
            b"\x1b[0c",
            lambda b: b"\x1b[" in b and b"c" in b[b.find(b"\x1b[") + 2 :],
            timeout,
        )
    finally:
        restore_tty(fd, old, owns)

    truecolor_strip()
    print("Legend:", flush=True)
    print("  OK      = got a complete reply (proxy or local answer worked)", flush=True)
    print("  TIMEOUT = nothing came back (query swallowed / not proxied)", flush=True)
    print("  PARTIAL = junk or incomplete sequence", flush=True)
    print(flush=True)
    print("Idle — Ctrl-C or close pane to exit.", flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        print("\nbye", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
