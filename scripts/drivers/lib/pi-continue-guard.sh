#!/usr/bin/env bash
# The client binary pi-anthropic.sh hands external-agent-cli.sh: `pi`, with
# a refusal in front of `-c`.
#
# pi's `-c` continues the most recent session and, when there is none,
# silently starts a fresh one. external-agent-cli.sh passes the continue
# flag through untouched, so without this a turn 1 that recorded no
# session would let turn 2 run as a new one-shot, and a case labelled
# multi-turn would land N first turns. Every other argument, `--version`
# included, reaches pi unchanged.
#
# ANTHROPIC_BASE_URL, which external-agent-cli.sh exports for every client
# it drives, is dropped here: pi never reads it, and a client environment
# without it is what shows the daemon was reached through models.json.

set -eu

. "$(cd "$(dirname "$0")" && pwd)/pi.sh"

for arg in "$@"; do
  if [ "$arg" = "-c" ] && ! pi_has_session; then
    echo "driver: pi -c has no recorded session, so nothing to resume; independent one-shots are not a multi-turn capture" >&2
    exit 1
  fi
done

unset ANTHROPIC_BASE_URL
exec "$PI_BIN" "$@"
