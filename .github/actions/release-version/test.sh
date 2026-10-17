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

expect v1.0.0+build-5 'prerelease=false
dist-tag=latest
image-tags<<EOF
ghcr.io/o/i:v1.0.0_build-5
ghcr.io/o/i:latest
EOF'

expect v1.0.0-beta+exp 'prerelease=true
dist-tag=next
image-tags<<EOF
ghcr.io/o/i:v1.0.0-beta_exp
EOF'

long="v1.0.0-$(printf 'a%.0s' {1..122})"
for tag in "v1.0.0~x" "release/v1" ".v1" "${long}"; do
	if ! decide "${tag}" && grep -q "::error::tag ${tag} gives the invalid image tag" "${work}/out" \
		&& [[ ! -s "${work}/output" ]]; then
		pass "${tag:0:20} fails before any output"
	else
		fail "${tag:0:20} fails before any output"
	fi
done

if ! LC_ALL=en_US.UTF-8 bash -c '[[ é =~ ^[A-Za-z]$ ]]'; then
	fail "en_US.UTF-8 matches é with [A-Za-z]; generate the locale with locale-gen en_US.UTF-8"
elif ! LC_ALL=en_US.UTF-8 decide "v1.0.0-é" \
	&& grep -q "::error::tag v1.0.0-é gives the invalid image tag" "${work}/out" \
	&& [[ ! -s "${work}/output" ]]; then
	pass "v1.0.0-é fails under en_US.UTF-8"
else
	fail "v1.0.0-é fails under en_US.UTF-8"
fi

if decide "${long%a}" && grep -q "image-tags<<EOF" "${work}/output"; then
	pass "a 128-character image tag passes"
else
	fail "a 128-character image tag passes"
fi

if ((failures > 0)); then
	echo "${failures} failed"
	exit 1
fi
echo "all passed"
