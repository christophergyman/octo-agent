#!/bin/bash
# octo run: build, ensure daemon, run TUI. Daemon stays alive after TUI quits.
# Usage: ./run.sh
set -e
cd "$(dirname "$0")"

cargo build -q 2>&1 | tail -n 5

# Start daemon if port is down, detached so closing this terminal keeps it alive.
# Mac has no setsid, so nohup plus redirect stdin is the portable detach.
# TUI also auto spawns, this is just faster feedback.
if ! ./target/debug/octo-d --health >/dev/null 2>&1; then
  echo "starting octo-d..."
  nohup ./target/debug/octo-d > /tmp/octo-d.log 2>&1 < /dev/null &
  disown 2>/dev/null || true
  for i in $(seq 1 50); do
    ./target/debug/octo-d --health >/dev/null 2>&1 && break
    sleep 0.1
  done
fi
./target/debug/octo-d --health

# Foreground TUI. Esc or /quit leaves, daemon keeps running.
exec ./target/debug/octo-tui
