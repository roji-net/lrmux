#!/usr/bin/env bash
# Truecolor (24-bit) smoke test for a terminal / multiplexer pane.
# A smooth rainbow gradient means RGB SGR (38;2 / 48;2) is reaching the glass.
# Banded / wrong / flat colors mean 256-color fallback or a broken path.

set -euo pipefail

echo "TERM=${TERM:-<unset>}  COLORTERM=${COLORTERM:-<unset>}"
echo

# Background gradient (classic awk rainbow).
awk 'BEGIN{
  s="/\\/\\/\\/\\/\\"; s=s s s s s s s s;
  for (colnum = 0; colnum<77; colnum++) {
    r = 255-(colnum*255/76);
    g = (colnum*510/76);
    b = (colnum*255/76);
    if (g>255) g = 510-g;
    printf "\033[48;2;%d;%d;%dm", r,g,b;
    printf "\033[38;2;%d;%d;%dm", 255-r,255-g,255-b;
    printf "%s\033[0m", substr(s,colnum+1,1);
  }
  printf "\n";
}'

echo

# Explicit RGB blocks: pure R/G/B + a few mid tones (easy to spot if quantized to 256).
printf 'blocks: '
for rgb in "255;0;0" "0;255;0" "0;0;255" "255;128;0" "128;0;255" "0;255;255" "255;255;0"; do
  printf '\033[48;2;%sm  \033[0m' "$rgb"
done
printf '\n\n'

# 0→255 red ramp (should be continuous, not ~6–32 steps).
printf 'red ramp: '
for i in $(seq 0 5 255); do
  printf '\033[48;2;%d;0;0m \033[0m' "$i"
done
printf '\n\n'

echo "If the rainbow and red ramp look smooth → 24-bit path OK."
echo "If you see chunky bands → not truecolor (or viewer quantized)."
echo
echo "In vim, also check:"
echo "  :set termguicolors?"
echo "  :echo \&termguicolors   \" must be 1"
echo "  :hi Comment             \" look for guifg=#rrggbb (not only ctermfg=N)"
