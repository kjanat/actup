#!/usr/bin/env bash
set -euo pipefail

repository="kjanat/actup"
release_tag='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'
action_repository="$(printf '%s' "${ACTUP_REPOSITORY:-}" | tr '[:upper:]' '[:lower:]')"
if [[ "${action_repository}" == "${repository}" && "${ACTUP_ACTION_REF:-}" =~ $release_tag ]]; then
	tag="${ACTUP_ACTION_REF}"
elif [[ -n "${ACTUP_VERSION:-}" ]]; then
	if [[ ! "${ACTUP_VERSION}" =~ $release_tag ]]; then
		echo "::error::version '${ACTUP_VERSION}' is not a release tag like v1.2.3"
		exit 1
	fi
	tag="${ACTUP_VERSION}"
else
	echo "::error::set the version input to a release tag like v1.2.3: the action ref '${ACTUP_ACTION_REF:-}' of '${ACTUP_REPOSITORY:-}' is not a release tag of ${repository}"
	exit 1
fi

case "${RUNNER_OS:-}/${RUNNER_ARCH:-}" in
	Linux/X64) target=x86_64-unknown-linux-musl ext=.tar.gz exe="" ;;
	Linux/ARM64) target=aarch64-unknown-linux-musl ext=.tar.gz exe="" ;;
	macOS/X64) target=x86_64-apple-darwin ext=.tar.gz exe="" ;;
	macOS/ARM64) target=aarch64-apple-darwin ext=.tar.gz exe="" ;;
	Windows/X64) target=x86_64-pc-windows-msvc ext=.zip exe=.exe ;;
	Windows/ARM64) target=aarch64-pc-windows-msvc ext=.zip exe=.exe ;;
	*)
		echo "::error::no actup release asset for runner ${RUNNER_OS:-unknown}/${RUNNER_ARCH:-unknown}"
		exit 1
		;;
esac

base="https://github.com/${repository}/releases/download/${tag}"
asset="actup-${tag}-${target}${ext}"
dest="$(mktemp -d "${RUNNER_TEMP}/actup-bin.XXXXXX")"
mkdir "${dest}/unpacked"

fetch() {
	curl --fail --location --silent --show-error --retry 3 --retry-delay 2 --output "$2" "$1"
}
fetch "${base}/${asset}" "${dest}/${asset}"
fetch "${base}/SHA256SUMS.txt" "${dest}/SHA256SUMS.txt"

expected="$(awk -v name="${asset}" '$2 == name || $2 == "*" name { print $1; exit }' "${dest}/SHA256SUMS.txt")"
if [[ -z "${expected}" ]]; then
	echo "::error::SHA256SUMS.txt of ${tag} lists no ${asset}"
	exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
	actual="$(sha256sum "${dest}/${asset}" | awk '{ print $1 }')"
else
	actual="$(shasum -a 256 "${dest}/${asset}" | awk '{ print $1 }')"
fi
if [[ "${expected}" != "${actual}" ]]; then
	echo "::error::checksum mismatch for ${asset}: SHA256SUMS.txt lists ${expected}, the download hashes to ${actual}"
	exit 1
fi
echo "Verified ${asset} (${actual})"

if [[ "${ext}" == .zip ]]; then
	if command -v unzip >/dev/null 2>&1; then
		unzip -q "${dest}/${asset}" -d "${dest}/unpacked"
	else
		7z x -y -bd -o"${dest}/unpacked" "${dest}/${asset}" >/dev/null
	fi
else
	tar -xzf "${dest}/${asset}" -C "${dest}/unpacked"
fi

bin="${dest}/unpacked/actup${exe}"
if [[ ! -f "${bin}" ]]; then
	echo "::error::${asset} holds no actup${exe}"
	exit 1
fi
chmod +x "${bin}"
echo "actup-bin=${bin}" >>"${GITHUB_OUTPUT}"
