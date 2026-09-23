#!/usr/bin/env bash
# Clean up lrmux server processes and socket files.
# Usage: ./scripts/clean_sessions.sh

set -euo pipefail

# Kill any running lrmux server processes.
pkill -f "target/debug/lrmux" 2>/dev/null || true
sleep 0.5

# Remove lrmux socket files from /tmp.
rm -f /tmp/lrmux-*/default 2>/dev/null || true
rm -f /tmp/lrmux-*.log 2>/dev/null || true

# Remove lrmux runtime directories if empty.
rmdir /tmp/lrmux-* 2>/dev/null || true

echo "lrmux sessions cleaned."
