#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
trap 'rm -r -- "${work}"' EXIT

git_() {
	git -C "${work}/repo" -c user.name=actup -c user.email=actup@example.invalid \
		-c commit.gpgSign=false -c tag.gpgSign=false "$@"
}
mkdir -p "${work}/repo"
git_ init -q
git_ commit -q --allow-empty -m first
git_ tag v9.9.9
first="$(git_ rev-parse HEAD)"
git_ commit -q --allow-empty -m second
git_ tag v9.10.0-rc.1
second="$(git_ rev-parse HEAD)"

cp -R "${here}/workflows" "${work}/workflows"
examples=("${work}"/workflows/*.yml)
pin() { (cd "${work}/repo" && "${here}/pin.sh" "$@" "${work}/workflows") >"${work}/pin.log" 2>&1; }

failures=0
pass() { echo "ok - $1"; }
fail() {
	echo "not ok - $1"
	failures=$((failures + 1))
}

pins_everywhere() {
	local file
	for file in "${examples[@]}"; do
		grep -q "kjanat/actup@$1 # $2$" "${file}" && grep -q "version: $2$" "${file}" || return 1
	done
}

pin v9.9.9
if ! grep -rq "<release-sha>" "${work}/workflows" && pins_everywhere "${first}" v9.9.9; then
	pass "a tag replaces the placeholder, comment and version in every example"
else
	fail "a tag replaces the placeholder, comment and version in every example"
fi

other_pins() { grep -rh "actions/checkout@\|create-pull-request@" "$1" | sort; }
if diff <(other_pins "${here}/workflows") <(other_pins "${work}/workflows") >/dev/null; then
	pass "other pins stay as written"
else
	fail "other pins stay as written"
fi

pin v9.10.0-rc.1
if pins_everywhere "${second}" v9.10.0-rc.1 && ! grep -rq "${first}" "${work}/workflows"; then
	pass "a later tag replaces the earlier pin"
else
	fail "a later tag replaces the earlier pin"
fi

cp -R "${work}/workflows" "${work}/before"
status=0
pin latest || status=$?
if [[ "${status}" == 2 ]] && diff -r "${work}/before" "${work}/workflows" >/dev/null; then
	pass "a tag that is not a release tag is refused"
else
	fail "a tag that is not a release tag is refused"
fi

status=0
pin v1.2.3 || status=$?
if [[ "${status}" == 1 ]] && grep -q "no tag v1.2.3" "${work}/pin.log" \
	&& diff -r "${work}/before" "${work}/workflows" >/dev/null; then
	pass "a missing tag is refused"
else
	fail "a missing tag is refused"
fi

if ((failures > 0)); then
	echo "${failures} failed"
	exit 1
fi
echo "all passed"
