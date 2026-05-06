#!/usr/bin/env bash
# Run a memory-heavy command (typically `tree-sitter generate` or `tree-sitter test`)
# under an adaptive RLIMIT_AS cap derived from /proc/meminfo at runtime.
#
# Why: `tree-sitter generate` materialises the entire LR/GLR parse table in JS-host
# memory before writing parser.c. On a grammar with many GLR conflict declarations
# the table can balloon to 10-20+ GB and OOM the system. `systemd-run --user --scope
# -p MemoryMax=...` silently no-ops on most distros (no memory delegation to the
# user manager), so use prlimit — RLIMIT_AS is enforced by the kernel.
#
# Policy (every threshold scales with MemTotal so the script works on a 16 GiB
# laptop and a 256 GiB workstation alike):
#   - HEADROOM = max(HEADROOM_MIN_MB, HEADROOM_FRAC * MemTotal)  reserved for the rest of the system
#   - CEILING  = CEILING_FRAC * MemTotal                         absolute upper bound on cap
#   - cap      = clamp(MemAvailable - HEADROOM, MIN_CAP_MB, CEILING)
#   - refuse   if cap < MIN_CAP_MB                                (not enough free memory to bother)
# Every threshold is overridable via env vars.

set -euo pipefail

# Knobs — fractions are integer percent of MemTotal; floors are absolute MB.
HEADROOM_FRAC_PCT="${HEADROOM_FRAC_PCT:-25}" # reserve 25% of total for the rest of the system
HEADROOM_MIN_MB="${HEADROOM_MIN_MB:-4096}"   # but never less than 4 GiB
CEILING_FRAC_PCT="${CEILING_FRAC_PCT:-50}"   # never cap above 50% of total
MIN_CAP_MB="${MIN_CAP_MB:-2048}"             # below 2 GiB tree-sitter generate just SIGKILLs anyway

usage() {
	cat <<EOF
usage: $(basename "$0") <command> [args...]

Run <command> with an adaptive RLIMIT_AS cap derived from /proc/meminfo.

env knobs (all optional):
  HEADROOM_FRAC_PCT  reserve this % of MemTotal for the rest of the system (default 25)
  HEADROOM_MIN_MB    floor for headroom in MB (default 4096)
  CEILING_FRAC_PCT   never cap above this % of MemTotal (default 50)
  MIN_CAP_MB         refuse if the computed cap would be below this many MB (default 2048)

examples:
  $(basename "$0") tree-sitter test
  $(basename "$0") tree-sitter generate
  HEADROOM_FRAC_PCT=40 $(basename "$0") tree-sitter test    # extra-conservative
EOF
}

case "${1:-}" in
	-h | --help)
		usage
		exit 0
		;;
	'')
		usage >&2
		exit 64
		;;
esac

read -r total_kb avail_kb < <(awk '
	/^MemTotal:/     { t=$2 }
	/^MemAvailable:/ { a=$2 }
	END              { print t, a }
' /proc/meminfo)

if [ -z "${total_kb:-}" ] || [ -z "${avail_kb:-}" ]; then
	echo "ts-bounded: cannot read MemTotal/MemAvailable from /proc/meminfo" >&2
	exit 1
fi

total_mb=$((total_kb / 1024))
avail_mb=$((avail_kb / 1024))

# Compute thresholds from MemTotal.
headroom_from_pct=$((total_mb * HEADROOM_FRAC_PCT / 100))
if [ "$headroom_from_pct" -gt "$HEADROOM_MIN_MB" ]; then
	headroom_mb="$headroom_from_pct"
else
	headroom_mb="$HEADROOM_MIN_MB"
fi
ceiling_mb=$((total_mb * CEILING_FRAC_PCT / 100))

cap_mb=$((avail_mb - headroom_mb))
if [ "$cap_mb" -gt "$ceiling_mb" ]; then cap_mb="$ceiling_mb"; fi

if [ "$cap_mb" -lt "$MIN_CAP_MB" ]; then
	printf 'ts-bounded: refusing — MemTotal=%dMB MemAvailable=%dMB headroom=%dMB → cap=%dMB < MIN_CAP_MB=%dMB.\n' \
		"$total_mb" "$avail_mb" "$headroom_mb" "$cap_mb" "$MIN_CAP_MB" >&2
	echo 'ts-bounded: free memory or override MIN_CAP_MB / HEADROOM_FRAC_PCT and retry.' >&2
	exit 1
fi

cap_bytes=$((cap_mb * 1024 * 1024))

printf 'ts-bounded: MemTotal=%dMB MemAvailable=%dMB headroom=%dMB ceiling=%dMB cap=%dMB cmd=%s\n' \
	"$total_mb" "$avail_mb" "$headroom_mb" "$ceiling_mb" "$cap_mb" "$*" >&2

exec prlimit --as="$cap_bytes" --rss="$cap_bytes" -- "$@"
