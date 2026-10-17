#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
trap 'rm -r -- "${work}"' EXIT

failures=0
pass() { echo "ok - $1"; }
fail() {
	echo "not ok - $1"
	failures=$((failures + 1))
}

decide() {
	: >"${work}/output"
	env RELEASE_TAG="$1" IMAGE=ghcr.io/o/i GITHUB_OUTPUT="${work}/output" "${here}/run.sh" >"${work}/out" 2>&1
}

expect() {
	local tag="$1" expected="$2"
	if decide "${tag}" && [[ "$(<"${work}/output")" == "${expected}" ]]; then
		pass "${tag}"
	else
		fail "${tag}"
		cat "${work}/out" "${work}/output"
	fi
}

expect v0.2.0-rc.1 'prerelease=true
dist-tag=next
image-tags<<EOF
ghcr.io/o/i:v0.2.0-rc.1
EOF'

expect v0.2.0 'prerelease=false
dist-tag=latest
image-tags<<EOF
ghcr.io/o/i:v0.2.0
ghcr.io/o/i:latest
EOF'

expect v1.0.0-beta+exp 'prerelease=true
dist-tag=next
image-tags<<EOF
ghcr.io/o/i:v1.0.0-beta+exp
EOF'

if ((failures > 0)); then
	echo "${failures} failed"
	exit 1
fi
echo "all passed"
