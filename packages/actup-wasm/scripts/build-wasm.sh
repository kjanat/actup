#!/usr/bin/env bash
set -euo pipefail

# Build actup-wasm WASM artifact for the npm package.
#
# Output: packages/actup-wasm/wasm/
#   actup_wasm_bg.wasm  — optimized WASM binary
#   actup_wasm.js       — ESM glue (--target web)
#   actup_wasm.d.ts     — wasm-bindgen type declarations

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$PKG_DIR/../.." && pwd)"
OUT_DIR="$PKG_DIR/wasm"

echo "Building actup-wasm (--target web)..."
wasm-pack build "$REPO_ROOT/crates/wasm" \
	--target web \
	--out-dir "$OUT_DIR" \
	--out-name actup_wasm

# wasm-pack generates a package.json and .gitignore we don't need
rm -f "$OUT_DIR/package.json" "$OUT_DIR/.gitignore"

# Report size
WASM_FILE="$OUT_DIR/actup_wasm_bg.wasm"
if [ -f "$WASM_FILE" ]; then
	RAW_SIZE=$(wc -c <"$WASM_FILE")
	GZ_SIZE=$(gzip -c "$WASM_FILE" | wc -c)
	echo "WASM size: ${RAW_SIZE} bytes raw, ${GZ_SIZE} bytes gzipped"

	# Fail if gzipped > 500KB
	MAX_GZ=512000
	if [ "$GZ_SIZE" -gt "$MAX_GZ" ]; then
		echo "ERROR: gzipped WASM exceeds ${MAX_GZ} bytes (${GZ_SIZE})" >&2
		exit 1
	fi
fi

echo "Done. Output: $OUT_DIR"
