#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
trap 'rm -r -- "${work}"' EXIT
mkdir "${work}/bin"

cat >"${work}/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
case "$1" in
	metadata)
		printf '{"packages":[{"name":"%s","version":"1.2.3"}]}\n' "${STUB_CRATE:-actup-core}"
		;;
	publish)
		echo "$*" >>"${CALL_LOG}"
		answer="$(head -n 1 "${PLAN}")"
		sed -i 1d "${PLAN}"
		case "${answer}" in
			ok) echo "Uploaded actup-core v1.2.3" ;;
			429) echo "error: the remote server responded with an error (status 429 Too Many Requests)"; exit 101 ;;
			missing) echo "error: no matching package named \`actup-core\` found"; exit 101 ;;
			exists) echo "error: crate version \`1.2.3\` is already uploaded"; exit 101 ;;
			indexed) echo "error: crate actup-core@1.2.3 already exists on crates.io index"; exit 101 ;;
			lookalike) echo "Packaged 12 files, 429.1KiB"; echo "error[E0429]: \`self\` imports are only allowed within a { } list"; exit 101 ;;
			*) echo "error: failed to verify package tarball"; exit 101 ;;
		esac
		;;
esac
CARGO

cat >"${work}/bin/curl" <<'CURL'
#!/usr/bin/env bash
url="${*: -1}"
echo "${url}" >>"${CALL_LOG}"
[[ -f "${INDEX}" ]] || exit 22
cat "${INDEX}"
CURL

cat >"${work}/bin/sleep" <<'SLEEP'
#!/usr/bin/env bash
echo "sleep $1" >>"${CALL_LOG}"
SLEEP
chmod +x "${work}/bin/cargo" "${work}/bin/curl" "${work}/bin/sleep"

failures=0
pass() { echo "ok - $1"; }
fail() {
	echo "not ok - $1"
	failures=$((failures + 1))
}

publish() {
	local plan="$1" index="$2"
	shift 2
	printf '%s' "${plan}" | tr ' ' '\n' >"${work}/plan"
	rm -f "${work}/index"
	if [[ -n "${index}" ]]; then
		printf '%s\n' "${index}" >"${work}/index"
	fi
	: >"${work}/calls"
	env PATH="${work}/bin:${PATH}" PLAN="${work}/plan" INDEX="${work}/index" CALL_LOG="${work}/calls" \
		CRATE=actup-core RELEASE_TAG=v1.2.3 CARGO_REGISTRY_TOKEN=t RETRY_WAIT=600 PROPAGATION_WAIT=30 MAX_RETRIES=3 \
		"$@" "${here}/run.sh" >"${work}/out" 2>&1
}

calls() { tr '\n' ';' <"${work}/calls"; }

if publish "" '{"name":"actup-core","vers":"1.2.3"}' \
	&& grep -q "skip actup-core@1.2.3: already on crates.io" "${work}/out" \
	&& [[ "$(calls)" == "https://index.crates.io/ac/tu/actup-core;" ]]; then
	pass "a version in the sparse index is skipped"
else
	fail "a version in the sparse index is skipped"
fi

if publish "ok" '{"name":"actup-core","vers":"1.2.2"}' \
	&& grep -q "published actup-core@1.2.3" "${work}/out" \
	&& [[ "$(calls)" == "https://index.crates.io/ac/tu/actup-core;publish -p actup-core --locked;" ]]; then
	pass "a version missing from the index is published"
else
	fail "a version missing from the index is published"
fi

if publish "429 missing ok" "" \
	&& [[ "$(calls)" == "https://index.crates.io/ac/tu/actup-core;publish -p actup-core --locked;sleep 600;publish -p actup-core --locked;sleep 30;publish -p actup-core --locked;" ]]; then
	pass "a rate limit and a missing dependency are retried after their waits"
else
	fail "a rate limit and a missing dependency are retried after their waits"
fi

for answer in exists indexed; do
	if publish "${answer}" "" && grep -q "skip actup-core@1.2.3: already published" "${work}/out"; then
		pass "an upload that lost a race is skipped (${answer})"
	else
		fail "an upload that lost a race is skipped (${answer})"
	fi
done

if ! publish "lookalike ok" "" \
	&& grep -q "publishing actup-core@1.2.3 failed" "${work}/out" \
	&& [[ "$(grep -c '^publish' "${work}/calls")" -eq 1 ]]; then
	pass "a build error that mentions 429 fails at once"
else
	fail "a build error that mentions 429 fails at once"
fi

if ! publish "429 429 429 ok" "" \
	&& grep -q "rate limited after 3 attempts" "${work}/out" \
	&& [[ "$(grep -c '^publish' "${work}/calls")" -eq 3 ]]; then
	pass "retries stop after the attempt limit"
else
	fail "retries stop after the attempt limit"
fi

if ! publish "broken ok" "" \
	&& grep -q "publishing actup-core@1.2.3 failed" "${work}/out" \
	&& [[ "$(grep -c '^publish' "${work}/calls")" -eq 1 ]]; then
	pass "any other publish error fails at once"
else
	fail "any other publish error fails at once"
fi

if ! publish "ok" "" RELEASE_TAG=v9.9.9 \
	&& grep -q "tag v9.9.9 does not match actup-core 1.2.3" "${work}/out" && [[ ! -s "${work}/calls" ]]; then
	pass "a tag for another version fails before any request"
else
	fail "a tag for another version fails before any request"
fi

if ! publish "ok" "" CRATE=actup-nope \
	&& grep -q "the workspace has no crate actup-nope" "${work}/out" && [[ ! -s "${work}/calls" ]]; then
	pass "an unknown crate fails before any request"
else
	fail "an unknown crate fails before any request"
fi

for pair in a:1/a ab:2/ab abc:3/a/abc Ab-Cd:ab/-c/ab-cd; do
	name="${pair%%:*}"
	if publish "" '{"vers":"1.2.3"}' CRATE="${name}" STUB_CRATE="${name}" \
		&& [[ "$(calls)" == "https://index.crates.io/${pair#*:};" ]]; then
		pass "the index path of ${name} is ${pair#*:}"
	else
		fail "the index path of ${name} is ${pair#*:}"
	fi
done

if ((failures > 0)); then
	echo "${failures} failed"
	exit 1
fi
echo "all passed"
