#!/usr/bin/env bash
#
# Runs Periscope for a long time and watches it, for the Phase 6 acceptance
# criterion "no crash in a 4-hour session with active watches and log tailing".
#
# Samples resident memory every 30 seconds, and stops the moment the process is
# gone — a soak that reports "finished" after the app died an hour in would be
# worse than no soak at all.
#
# Usage: packaging/soak.sh <minutes> [extra scope args...]

set -euo pipefail

minutes="${1:-240}"
shift || true

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="$root/target/release/scope"
report="$root/target/soak-$(date +%Y%m%d-%H%M%S).log"

if [[ ! -x "$binary" ]]; then
	echo "no release binary at $binary — cargo build --release --bin scope" >&2
	exit 1
fi

echo "==> soaking for ${minutes}m; report: $report"
"$binary" --perf "$@" &
pid=$!

{
	echo "soak started $(date -u +%Y-%m-%dT%H:%M:%SZ) pid=$pid minutes=$minutes args=$*"
	echo "elapsed_s rss_kb cpu_pct"
} >"$report"

deadline=$((SECONDS + minutes * 60))
started=$SECONDS
status="completed"

while ((SECONDS < deadline)); do
	if ! kill -0 "$pid" 2>/dev/null; then
		status="DIED"
		break
	fi

	# `ps` reports RSS in kilobytes on macOS and Linux alike.
	read -r rss cpu <<<"$(ps -o rss=,pcpu= -p "$pid" | tr -s ' ')" || true
	echo "$((SECONDS - started)) ${rss:-0} ${cpu:-0}" >>"$report"
	sleep 30
done

if [[ "$status" == "completed" ]]; then
	kill "$pid" 2>/dev/null || true
	wait "$pid" 2>/dev/null || true
fi

{
	echo "soak $status after $((SECONDS - started))s"
} >>"$report"

echo "==> soak $status"
tail -3 "$report"
[[ "$status" == "completed" ]]
