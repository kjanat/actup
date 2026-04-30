#!/usr/bin/env bash
set -euo pipefail

# Usage: ./generate_icon.sh [options] <bg_color> <logo_path> <output_path>
#
# Examples:
#   ./generate_icon.sh "#0d1117" branding/icon.svg branding/icon-128.png
#   ./generate_icon.sh none branding/icon.svg branding/icon-128.png
#   ./generate_icon.sh -s 256 -p 15 "#0d1117" icon.svg icon-256.png
#   ./generate_icon.sh -r 20 green icon.svg icon-rounded.png

CANVAS_SIZE=128
PADDING_PCT=20
CORNER_RADIUS=0

usage() {
	cat <<-EOF
		Usage: $0 [options] <bg_color> <logo_path> <output_path>

		Arguments:
		  bg_color     Background color hex (e.g. "#0d1117") or "none" for transparent
		  logo_path    Path to SVG logo
		  output_path  Output PNG path

		Options:
		  -s SIZE      Canvas size in px (default: ${CANVAS_SIZE})
		  -p PERCENT   Padding percentage (default: ${PADDING_PCT})
		  -r RADIUS    Corner radius in px, 0 = square (default: ${CORNER_RADIUS})
		  -h           Show this help
	EOF
	exit "${1:-1}"
}

while getopts ":s:p:r:h" opt; do
	case "${opt}" in
		s) CANVAS_SIZE="${OPTARG}" ;;
		p) PADDING_PCT="${OPTARG}" ;;
		r) CORNER_RADIUS="${OPTARG}" ;;
		h) usage 0 ;;
		:)
			echo "Error: -${OPTARG} requires an argument" >&2
			exit 1
			;;
		*)
			echo "Error: unknown option -${OPTARG}" >&2
			usage
			;;
	esac
done
shift $((OPTIND - 1))

(($# == 3)) || usage

BG_COLOR="$1"
LOGO_PATH="$2"
OUTPUT_PATH="$3"
MAX_LOGO_SIZE=$((CANVAS_SIZE * (100 - PADDING_PCT) / 100))

require_cmd() {
	command -v "$1" >/dev/null 2>&1 || {
		echo "Error: $1 not found. Install: $2" >&2
		exit 1
	}
}

require_cmd rsvg-convert "brew install librsvg / apt install librsvg2-bin"

if command -v magick >/dev/null 2>&1; then
	IM_CMD="magick"
else
	require_cmd convert "brew install imagemagick / apt install imagemagick"
	IM_CMD="convert"
fi

[[ -f "${LOGO_PATH}" ]] || {
	echo "Error: file not found: ${LOGO_PATH}" >&2
	exit 1
}
mkdir -p "$(dirname "${OUTPUT_PATH}")"

TEMP_PNG="$(mktemp /tmp/icon_logo_XXXXXX.png)"
trap 'rm -f "${TEMP_PNG}"' EXIT

echo "Generating ${OUTPUT_PATH} (${CANVAS_SIZE}×${CANVAS_SIZE}, padding ${PADDING_PCT}%)..."

rsvg-convert \
	--width="${MAX_LOGO_SIZE}" \
	--height="${MAX_LOGO_SIZE}" \
	--keep-aspect-ratio \
	"${LOGO_PATH}" \
	-o "${TEMP_PNG}"

compose_args=(
	-size "${CANVAS_SIZE}x${CANVAS_SIZE}" "canvas:${BG_COLOR}"
	"${TEMP_PNG}" -gravity center -composite
)

# Apply rounded corners if requested
if ((CORNER_RADIUS > 0)); then
	compose_args+=(
		\( +clone -alpha extract
		-draw "fill black polygon 0,0 0,${CORNER_RADIUS} ${CORNER_RADIUS},0
			        fill white circle ${CORNER_RADIUS},${CORNER_RADIUS} ${CORNER_RADIUS},0"
		\( +clone -flip \) -compose Multiply -composite
		\( +clone -flop \) -compose Multiply -composite
		\) -alpha off -compose CopyOpacity -composite
	)
fi

"${IM_CMD}" "${compose_args[@]}" "${OUTPUT_PATH}"

echo "Done: ${OUTPUT_PATH}"
