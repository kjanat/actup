#!/usr/bin/env bash
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
trap 'rm -r -- "${work}"' EXIT

fixtures="${work}/releases"
mkdir -p "${work}/bin" "${work}/stage"

cat >"${work}/bin/curl" <<'CURL'
#!/usr/bin/env bash
output=""
url=""
while (($#)); do
	case "$1" in
		--output) output="$2"; shift 2 ;;
		--retry | --retry-delay) shift 2 ;;
		--*) shift ;;
		*) url="$1"; shift ;;
	esac
done
echo "${url}" >>"${CURL_LOG}"
file="${FIXTURES}/${url#https://github.com/}"
[[ -f "${file}" ]] || exit 22
cp "${file}" "${output}"
CURL
chmod +x "${work}/bin/curl"

cat >"${work}/stage/actup" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$@" >"${ACTUP_FAKE_ARGS}"
exit "${ACTUP_FAKE_EXIT:-0}"
FAKE
chmod +x "${work}/stage/actup"
cp "${work}/stage/actup" "${work}/stage/actup.exe"

publish() {
	local repo="$1" tag="$2" dir="${fixtures}/$1/releases/download/$2"
	mkdir -p "${dir}"
	tar -czf "${dir}/actup-${tag}-x86_64-unknown-linux-musl.tar.gz" -C "${work}/stage" actup
	tar -czf "${dir}/actup-${tag}-aarch64-apple-darwin.tar.gz" -C "${work}/stage" actup
	(cd "${work}/stage" && zip -q "${dir}/actup-${tag}-x86_64-pc-windows-msvc.zip" actup.exe)
	(cd "${dir}" && sha256sum actup-* >SHA256SUMS.txt)
	echo "${repo} ${tag}"
}

failures=0
pass() { echo "ok - $1"; }
fail() {
	echo "not ok - $1"
	failures=$((failures + 1))
}

install() {
	local out="${work}/output"
	: >"${out}"
	: >"${work}/curl.log"
	env PATH="${work}/bin:${PATH}" FIXTURES="${fixtures}" CURL_LOG="${work}/curl.log" \
		RUNNER_TEMP="${work}" GITHUB_OUTPUT="${out}" "$@" "${here}/install.sh" >"${work}/install.log" 2>&1
}

bin_of() { sed -n 's/^actup-bin=//p' "${work}/output"; }

publish kjanat/actup v1.2.3 >/dev/null
publish kjanat/actup v2.0.0-rc.1 >/dev/null

if install ACTUP_ACTION_REF=v1.2.3 ACTUP_REPOSITORY=kjanat/actup RUNNER_OS=Linux RUNNER_ARCH=X64 \
	&& [[ -x "$(bin_of)" ]] \
	&& grep -qx "https://github.com/kjanat/actup/releases/download/v1.2.3/actup-v1.2.3-x86_64-unknown-linux-musl.tar.gz" "${work}/curl.log"; then
	pass "a release tag ref installs the musl asset of that release"
else
	fail "a release tag ref installs the musl asset of that release"
fi

if install ACTUP_ACTION_REF=3d3c42e5aac5ba805825da76410c181273ba90b1 ACTUP_VERSION=v2.0.0-rc.1 ACTUP_REPOSITORY=kjanat/actup RUNNER_OS=macOS RUNNER_ARCH=ARM64 \
	&& [[ -x "$(bin_of)" ]] \
	&& grep -qx "https://github.com/kjanat/actup/releases/download/v2.0.0-rc.1/actup-v2.0.0-rc.1-aarch64-apple-darwin.tar.gz" "${work}/curl.log"; then
	pass "a SHA ref installs the version input"
else
	fail "a SHA ref installs the version input"
fi

if install ACTUP_ACTION_REF=v1.2.3 ACTUP_REPOSITORY=KJanat/Actup RUNNER_OS=Linux RUNNER_ARCH=X64 \
	&& [[ -x "$(bin_of)" ]] && grep -q "/download/v1.2.3/" "${work}/curl.log"; then
	pass "the repository name matches without regard to case"
else
	fail "the repository name matches without regard to case"
fi

if ! install ACTUP_ACTION_REF=v1.2.3 ACTUP_REPOSITORY=acme/wrapper RUNNER_OS=Linux RUNNER_ARCH=X64 \
	&& grep -q "set the version input" "${work}/install.log" && [[ ! -s "${work}/curl.log" ]]; then
	pass "a tag ref of an enclosing action needs the version input"
else
	fail "a tag ref of an enclosing action needs the version input"
fi

if install ACTUP_ACTION_REF=v9.9.9 ACTUP_REPOSITORY=acme/wrapper ACTUP_VERSION=v1.2.3 RUNNER_OS=Linux RUNNER_ARCH=X64 \
	&& [[ -x "$(bin_of)" ]] \
	&& [[ "$(wc -l <"${work}/curl.log")" -eq 2 ]] \
	&& grep -qx "https://github.com/kjanat/actup/releases/download/v1.2.3/SHA256SUMS.txt" "${work}/curl.log" \
	&& grep -qx "https://github.com/kjanat/actup/releases/download/v1.2.3/actup-v1.2.3-x86_64-unknown-linux-musl.tar.gz" "${work}/curl.log"; then
	pass "an enclosing action downloads the version input from kjanat/actup"
else
	fail "an enclosing action downloads the version input from kjanat/actup"
fi

if ! install ACTUP_ACTION_REF=v1.2.3 ACTUP_REPOSITORY= RUNNER_OS=Linux RUNNER_ARCH=X64 \
	&& grep -q "set the version input" "${work}/install.log"; then
	pass "an empty action repository needs the version input"
else
	fail "an empty action repository needs the version input"
fi

if ! install ACTUP_ACTION_REF=v9.9.9 ACTUP_REPOSITORY=kjanat/actup ACTUP_VERSION=v1.2.3 RUNNER_OS=Windows RUNNER_ARCH=X64 \
	&& grep -q "/download/v9.9.9/" "${work}/curl.log" && ! grep -q "/download/v1.2.3/" "${work}/curl.log"; then
	pass "a release tag ref wins over the version input"
else
	fail "a release tag ref wins over the version input"
fi

if install ACTUP_ACTION_REF= ACTUP_VERSION=v1.2.3 RUNNER_OS=Windows RUNNER_ARCH=X64 \
	&& [[ "$(bin_of)" == */unpacked/actup.exe && -x "$(bin_of)" ]]; then
	pass "a local action unpacks the Windows zip from kjanat/actup"
else
	fail "a local action unpacks the Windows zip from kjanat/actup"
fi

mkdir "${work}/no-unzip"
cp "${work}/bin/curl" "${work}/no-unzip/curl"
for tool in bash awk sha256sum mktemp mkdir tar gzip 7z chmod cp tr; do
	ln -s "$(command -v "${tool}")" "${work}/no-unzip/${tool}"
done
: >"${work}/output"
if env PATH="${work}/no-unzip" FIXTURES="${fixtures}" CURL_LOG="${work}/curl.log" RUNNER_TEMP="${work}" \
	GITHUB_OUTPUT="${work}/output" ACTUP_VERSION=v1.2.3 RUNNER_OS=Windows RUNNER_ARCH=X64 \
	"${here}/install.sh" >"${work}/install.log" 2>&1 \
	&& [[ -f "$(bin_of)" ]]; then
	pass "a runner without unzip unpacks the zip with 7z"
else
	fail "a runner without unzip unpacks the zip with 7z"
fi

for ref in main v1 v1.2; do
	if install ACTUP_ACTION_REF="${ref}" ACTUP_REPOSITORY=kjanat/actup RUNNER_OS=Linux RUNNER_ARCH=X64 \
		|| ! grep -q "the action ref '${ref}' of 'kjanat/actup' is not a release tag" "${work}/install.log" \
		|| [[ -s "${work}/output" || -s "${work}/curl.log" ]]; then
		fail "ref ${ref} without a version input fails"
	else
		pass "ref ${ref} without a version input fails"
	fi
done

if install ACTUP_VERSION=latest RUNNER_OS=Linux RUNNER_ARCH=X64 \
	|| ! grep -q "version 'latest' is not a release tag" "${work}/install.log" || [[ -s "${work}/curl.log" ]]; then
	fail "a version input that is not a release tag fails before any download"
else
	pass "a version input that is not a release tag fails before any download"
fi

if install ACTUP_VERSION=v1.2.3 RUNNER_OS=Linux RUNNER_ARCH=ARM64 \
	|| ! grep -q "actup-v1.2.3-aarch64-unknown-linux-musl.tar.gz$" "${work}/curl.log" || [[ -s "${work}/output" ]]; then
	fail "a missing asset fails"
else
	pass "a missing asset fails"
fi

if install ACTUP_VERSION=v1.2.3 RUNNER_OS=FreeBSD RUNNER_ARCH=X64 \
	|| ! grep -q "no actup release asset for runner FreeBSD/X64" "${work}/install.log"; then
	fail "an unsupported runner fails"
else
	pass "an unsupported runner fails"
fi

sums="${fixtures}/kjanat/actup/releases/download/v1.2.3/SHA256SUMS.txt"
cp "${sums}" "${work}/sums.orig"
sed -i 's/^[0-9a-f]\{64\}  actup-v1.2.3-x86_64-unknown-linux-musl/0000000000000000000000000000000000000000000000000000000000000000  actup-v1.2.3-x86_64-unknown-linux-musl/' "${sums}"
if install ACTUP_VERSION=v1.2.3 RUNNER_OS=Linux RUNNER_ARCH=X64 \
	|| ! grep -q "checksum mismatch for actup-v1.2.3-x86_64-unknown-linux-musl.tar.gz" "${work}/install.log" || [[ -s "${work}/output" ]]; then
	fail "a checksum mismatch refuses the binary"
else
	pass "a checksum mismatch refuses the binary"
fi
grep -v x86_64-unknown-linux-musl "${work}/sums.orig" >"${sums}"
if install ACTUP_VERSION=v1.2.3 RUNNER_OS=Linux RUNNER_ARCH=X64 \
	|| ! grep -q "SHA256SUMS.txt of v1.2.3 lists no actup-v1.2.3-x86_64-unknown-linux-musl.tar.gz" "${work}/install.log"; then
	fail "an asset missing from SHA256SUMS.txt is refused"
else
	pass "an asset missing from SHA256SUMS.txt is refused"
fi
cp "${work}/sums.orig" "${sums}"

run() {
	: >"${work}/output"
	env GITHUB_OUTPUT="${work}/output" ACTUP_BIN="${work}/stage/actup" ACTUP_FAKE_ARGS="${work}/args" "$@" "${here}/run.sh"
}

if run ACTUP_COMMAND=check ACTUP_ARGS="--strict  --format json" ACTUP_FAKE_EXIT=0 \
	&& [[ "$(cat "${work}/args")" == $'check\n--strict\n--format\njson' ]] \
	&& grep -qx "exit-code=0" "${work}/output"; then
	pass "run passes the command and whitespace-split arguments"
else
	fail "run passes the command and whitespace-split arguments"
fi

if run ACTUP_COMMAND=pin ACTUP_ARGS= ACTUP_FAKE_EXIT=0 && [[ "$(cat "${work}/args")" == pin ]]; then
	pass "run passes no arguments for empty args"
else
	fail "run passes no arguments for empty args"
fi

if run ACTUP_COMMAND=check ACTUP_ARGS=$'--strict\n--format json\n' ACTUP_FAKE_EXIT=0 \
	&& [[ "$(cat "${work}/args")" == $'check\n--strict\n--format\njson' ]]; then
	pass "run passes every line of multi-line args"
else
	fail "run passes every line of multi-line args"
fi

status=0
run ACTUP_COMMAND=check ACTUP_ARGS=--strict ACTUP_FAKE_EXIT=3 || status=$?
if [[ "${status}" == 3 ]] && grep -qx "exit-code=3" "${work}/output"; then
	pass "run records and returns a failing exit code"
else
	fail "run records and returns a failing exit code"
fi

if ((failures > 0)); then
	echo "${failures} failed"
	exit 1
fi
echo "all passed"
