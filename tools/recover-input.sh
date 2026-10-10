#!/usr/bin/env bash
# recover-input.sh - force a monitor back to a known input.
#
# Why this exists: DDC input switching is fire-and-forget. If a switch lands on
# an input with no signal, some monitors hold that input (showing "no signal")
# instead of falling back, and the host that issued the switch can be left
# unable to see its own screen. This script re-asserts an input repeatedly,
# across fresh processes, because a DDC write wedges the channel for the rest of
# the issuing process (see docs/hardware-notes.md 2.1).
#
# usage: tools/recover-input.sh [input] [attempts]
#        tools/recover-input.sh 0x10 20
set -u

INPUT="${1:-0x10}"
ATTEMPTS="${2:-20}"
HELPER="$(dirname "$0")/../target/release/dmux-ddc-helper"

if [ ! -x "$HELPER" ]; then
  echo "helper not found at $HELPER -- build first: cargo build --release" >&2
  exit 1
fi

echo "re-asserting input $INPUT up to $ATTEMPTS times (one process per attempt)..."
for i in $(seq 1 "$ATTEMPTS"); do
  out=$("$HELPER" set-input --index=0 --value="$INPUT" 2>/dev/null)
  case "$out" in
    *'"ok":true'*) echo "  attempt $i: write acknowledged" ;;
    *)             echo "  attempt $i: no answer yet" ;;
  esac
  # a short pause lets the monitor settle; a readback needs a fresh process
  sleep 2
  cur=$("$HELPER" status --index=0 2>/dev/null | sed -n 's/.*"current_input":"\([^"]*\)".*/\1/p')
  if [ -n "$cur" ]; then
    echo "  readback: $cur"
    if [ "$(echo "$cur" | tr 'A-Z' 'a-z')" = "$(echo "$INPUT" | tr 'A-Z' 'a-z')" ]; then
      echo "monitor is back on $INPUT"
      exit 0
    fi
  fi
done
echo "gave up. Try the monitor's own OSD input menu, or power-cycle the display." >&2
exit 1
