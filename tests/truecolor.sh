#!/bin/sh
# Truecolor test for lrmux.
# Generates truecolor output via Python (fast) and emits it directly.
# Exercises 24-bit RGB rendering: gradients, color blocks, and attributes.
# Fits on a 24-row terminal.

python3 -c '
import sys
out = []
out.append("=== lrmux truecolor test ===")
# 16 basic colors
row = "16: "
for i in range(8):
    row += "\033[3%dm\u2588\033[0m" % i
row += " "
for i in range(8):
    row += "\033[9%dm\u2588\033[0m" % i
out.append(row)
# 256-color (first 32)
row = "256: "
for i in range(32):
    row += "\033[38;5;%dm\u2588\033[0m" % i
out.append(row)
# Truecolor gradients (step 16 = 16 bars each, fits on one line)
for name, idx in [("R", 0), ("G", 1), ("B", 2)]:
    row = "TC%s: " % name
    for v in range(0, 256, 16):
        rgb = [0, 0, 0]
        rgb[idx] = v
        row += "\033[38;2;%d;%d;%dm\u2588\033[0m" % tuple(rgb)
    out.append(row)
# Rainbow (3 segments of 16 bars = 48 bars, fits on one line)
row = "RB: "
for i in range(0, 256, 16):
    row += "\033[38;2;255;%d;0m\u2588\033[0m" % i
for i in range(240, -1, -16):
    row += "\033[38;2;%d;255;0m\u2588\033[0m" % i
for i in range(0, 256, 16):
    row += "\033[38;2;0;%d;255m\u2588\033[0m" % i
out.append(row)
# Attributes
out.append("AT: \033[1mbold\033[0m \033[3mitalic\033[0m \033[4munderline\033[0m \033[7mreverse\033[0m")
# Truecolor bg + fg
out.append("BG: \033[48;2;30;30;60m \033[38;2;255;255;0mhello truecolor\033[0m ")
out.append("=== test complete ===")
sys.stdout.write("\n".join(out) + "\n")
'
