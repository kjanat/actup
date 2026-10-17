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
DEFAULT_CMD="${DEFAULT_CMD:-tree-sitter}"    # what bare-invocation runs

# ANSI foreground colors. Bars are drawn with U+2588 FULL BLOCK glyphs so the
# result reads as a progress bar rather than a wall of saturated background.
# Suppressed when stdout/stderr isn't a TTY so piping doesn't dump escape codes.
if [ -t 1 ] && [ -t 2 ]; then
	C_USED=$'\033[31m'    # red fg = currently in use
	C_PLANNED=$'\033[33m' # yellow fg = the cap we're about to allocate
	C_FREE=$'\033[32m'    # green fg = free / remaining
	C_TRACK=$'\033[2;37m' # dim grey = empty bar track
	C_RESET=$'\033[0m'
	C_DIM=$'\033[2m'
else
	C_USED='' C_PLANNED='' C_FREE='' C_TRACK='' C_RESET='' C_DIM=''
fi

# Format a megabyte count for display: "X.Y GB" when |mb| > 1024, else "N MB".
fmt_mb() {
	local mb="$1" abs sign='' gb_int gb_tenth
	abs="${mb#-}"
	if [ "$mb" != "$abs" ]; then sign='-'; fi
	if [ "$abs" -gt 1024 ]; then
		gb_int=$((abs / 1024))
		gb_tenth=$(((abs * 10 / 1024) % 10))
		printf '%s%d.%d GB' "$sign" "$gb_int" "$gb_tenth"
	else
		printf '%s%d MB' "$sign" "$abs"
	fi
}

# Print N FULL BLOCK glyphs (U+2588) in the given ANSI foreground color.
bar_segment() {
	local color="$1" n="$2" i
	[ "$n" -le 0 ] && return 0
	printf '%s' "$color"
	for ((i = 0; i < n; i++)); do printf '█'; done
	printf '%s' "$C_RESET"
}

# Render two horizontal usage bars: one for current state (used + free),
# one for projected state after the cap is allocated (used + planned + free).
# Each bar is BAR_WIDTH chars wide, proportional to total_mb. Segments are
# clamped non-negative and the free segment fills any rounding shortfall.
render_bars() {
	local bar_width=50
	local used_mb=$((total_mb - avail_mb))
	# Clamp planned segment for the projected bar — if we'd refuse, show 0 planned.
	local planned_mb="$cap_mb"
	[ "$planned_mb" -lt 0 ] && planned_mb=0

	local cur_used_n=$((used_mb * bar_width / total_mb))
	local cur_free_n=$((bar_width - cur_used_n))
	[ "$cur_used_n" -lt 0 ] && cur_used_n=0
	[ "$cur_free_n" -lt 0 ] && cur_free_n=0

	local proj_used_n=$((used_mb * bar_width / total_mb))
	local proj_planned_n=$((planned_mb * bar_width / total_mb))
	local proj_free_n=$((bar_width - proj_used_n - proj_planned_n))
	[ "$proj_used_n" -lt 0 ] && proj_used_n=0
	[ "$proj_planned_n" -lt 0 ] && proj_planned_n=0
	[ "$proj_free_n" -lt 0 ] && proj_free_n=0

	printf '  current:    '
	bar_segment "$C_USED" "$cur_used_n"
	bar_segment "$C_TRACK" "$cur_free_n"
	printf '  used %s / free %s\n' "$(fmt_mb "$used_mb")" "$(fmt_mb "$avail_mb")"

	printf '  after run:  '
	bar_segment "$C_USED" "$proj_used_n"
	bar_segment "$C_PLANNED" "$proj_planned_n"
	bar_segment "$C_TRACK" "$proj_free_n"
	printf '  cap %s, free after %s\n' \
		"$(fmt_mb "$planned_mb")" "$(fmt_mb "$((avail_mb - planned_mb))")"

	SEGMENTS=$bar_width
	printf '  legend:     '
	bar_segment "$C_USED" $((SEGMENTS / 5))
	printf ' in use  '
	bar_segment "$C_TRACK" $((SEGMENTS / 4))
	printf ' free  '
	bar_segment "$C_PLANNED" $((SEGMENTS / 4))
	printf ' %s  ' 'reserved'
	printf '\n'
}

# Resolve thresholds against /proc/meminfo. Sets globals: total_mb, avail_mb,
# headroom_mb, ceiling_mb, cap_mb, refuse_reason. cap_mb is clamped to ceiling
# but NOT yet to MIN_CAP_MB — caller decides whether to refuse based on that.
resolve() {
	local total_kb avail_kb
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

	local headroom_from_pct=$((total_mb * HEADROOM_FRAC_PCT / 100))
	if [ "$headroom_from_pct" -gt "$HEADROOM_MIN_MB" ]; then
		headroom_mb="$headroom_from_pct"
	else
		headroom_mb="$HEADROOM_MIN_MB"
	fi
	ceiling_mb=$((total_mb * CEILING_FRAC_PCT / 100))

	cap_mb=$((avail_mb - headroom_mb))
	if [ "$cap_mb" -gt "$ceiling_mb" ]; then cap_mb="$ceiling_mb"; fi

	refuse_reason=""
	if [ "$cap_mb" -lt "$MIN_CAP_MB" ]; then
		refuse_reason="cap=$(fmt_mb "$cap_mb") < MIN_CAP_MB=$(fmt_mb "$MIN_CAP_MB")"
	fi
}

# Print a knob row aligned via printf field widths so values + descriptions
# line up regardless of value-string length.
knob_row() {
	local name="$1" value="$2" desc="$3"
	printf '  %-18s = %-9s  — %s\n' "$name" "$value" "$desc"
}

usage() {
	resolve
	local headroom_origin
	if [ "$headroom_mb" -gt "$((total_mb * HEADROOM_FRAC_PCT / 100))" ]; then
		headroom_origin="MIN floor"
	else
		headroom_origin="${HEADROOM_FRAC_PCT}% of total"
	fi

	local cap_marker
	if [ -n "$refuse_reason" ]; then
		cap_marker=" ✗ would refuse: $refuse_reason"
	else
		cap_marker=' ✓'
	fi

	cat <<-EOF
		usage: $(basename "$0") [<command> [args...]]

		Run <command> with an adaptive RLIMIT_AS cap derived from /proc/meminfo.
		Bare invocation runs DEFAULT_CMD (currently: ${C_DIM}${DEFAULT_CMD}${C_RESET}).

		env knobs (current effective value shown — set via env var to override):
	EOF
	knob_row "HEADROOM_FRAC_PCT" "${HEADROOM_FRAC_PCT}%" "reserve this % of MemTotal for the rest of the system"
	knob_row "HEADROOM_MIN_MB" "$(fmt_mb "$HEADROOM_MIN_MB")" "floor for headroom (env input in MB)"
	knob_row "CEILING_FRAC_PCT" "${CEILING_FRAC_PCT}%" "never cap above this % of MemTotal"
	knob_row "MIN_CAP_MB" "$(fmt_mb "$MIN_CAP_MB")" "refuse if the computed cap would be below this (env input in MB)"
	knob_row "DEFAULT_CMD" "${DEFAULT_CMD}" "what bare invocation runs"

	cat <<-EOF

		resolved on this system right now:
		  MemTotal:      $(fmt_mb "$total_mb")
		  MemAvailable:  $(fmt_mb "$avail_mb")
		  headroom:      $(fmt_mb "$headroom_mb") (${headroom_origin})
		  ceiling:       $(fmt_mb "$ceiling_mb") (${CEILING_FRAC_PCT}% of total)
		  cap:           $(fmt_mb "$cap_mb")${cap_marker}

		memory bars:
	EOF
	render_bars

	cat <<-EOF

		examples:
		  $(basename "$0")                              # runs DEFAULT_CMD ($DEFAULT_CMD)
		  $(basename "$0") tree-sitter generate
		  HEADROOM_FRAC_PCT=40 $(basename "$0")         # extra-conservative
	EOF
}

case "${1:-}" in
	-h | --help)
		usage
		exit 0
		;;
esac

# If no args, fall back to DEFAULT_CMD. Word-split it so DEFAULT_CMD="tree-sitter test" works.
if [ "$#" -eq 0 ]; then
	# shellcheck disable=SC2086
	set -- $DEFAULT_CMD
fi

resolve

if [ -n "$refuse_reason" ]; then
	printf 'ts-bounded: refusing — MemTotal=%s MemAvailable=%s headroom=%s → %s.\n' \
		"$(fmt_mb "$total_mb")" "$(fmt_mb "$avail_mb")" "$(fmt_mb "$headroom_mb")" "$refuse_reason" >&2
	echo 'ts-bounded: free memory or override MIN_CAP_MB / HEADROOM_FRAC_PCT and retry.' >&2
	exit 1
fi

cap_bytes=$((cap_mb * 1024 * 1024))

# An unprivileged process cannot raise a hard limit it inherited from an outer prlimit.
for resource in as rss; do
	hard="$(prlimit --pid "$$" --"$resource" --output HARD --noheadings --raw)"
	if [ "$hard" != unlimited ] && [ "$hard" -lt "$cap_bytes" ]; then
		cap_bytes="$hard"
	fi
done
cap_mb=$((cap_bytes / 1024 / 1024))

printf 'ts-bounded: MemTotal=%s MemAvailable=%s headroom=%s ceiling=%s cap=%s cmd=%s\n' \
	"$(fmt_mb "$total_mb")" "$(fmt_mb "$avail_mb")" "$(fmt_mb "$headroom_mb")" "$(fmt_mb "$ceiling_mb")" "$(fmt_mb "$cap_mb")" "$*" >&2

# Render the same bars to stderr at runtime so you see exactly what's about to happen.
render_bars >&2

# Visual delimiter so wrapper output is unambiguously separated from the wrapped
# command's stdout/stderr. Width matches the bar width above for alignment.
printf '%s──────── ↓ %s ↓ ────────%s\n' "$C_DIM" "$*" "$C_RESET" >&2

exec prlimit --as="$cap_bytes" --rss="$cap_bytes" -- "$@"
