#!/usr/bin/env bash
set -euo pipefail

crate="${CRATE:?CRATE required}"
tag="${RELEASE_TAG:?RELEASE_TAG required}"
: "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN required}"
export CARGO_REGISTRY_TOKEN
# crates.io refills its new-crate allowance about once per ten minutes.
retry_wait="${RETRY_WAIT:-630}"
propagation_wait="${PROPAGATION_WAIT:-30}"
max_retries="${MAX_RETRIES:-8}"

version="$(cargo metadata --no-deps --format-version 1 \
	| jq -r --arg name "${crate}" '.packages[] | select(.name == $name) | .version')"
if [[ -z "${version}" ]]; then
	echo "::error::the workspace has no crate ${crate}"
	exit 1
fi
if [[ "${tag}" != "v${version}" ]]; then
	echo "::error::tag ${tag} does not match ${crate} ${version}"
	exit 1
fi

index_path() {
	local name="${1,,}"
	case "${#name}" in
		1) echo "1/${name}" ;;
		2) echo "2/${name}" ;;
		3) echo "3/${name:0:1}/${name}" ;;
		*) echo "${name:0:2}/${name:2:2}/${name}" ;;
	esac
}

published() {
	local body
	body="$(curl --fail --silent --connect-timeout 10 --max-time 30 \
		"https://index.crates.io/$(index_path "${crate}")")" || return 1
	jq -e --arg v "${version}" 'select(.vers == $v)' <<<"${body}" >/dev/null
}

if published; then
	echo "skip ${crate}@${version}: already on crates.io"
	exit 0
fi

attempt=0
while true; do
	attempt=$((attempt + 1))
	status=0
	output="$(cargo publish -p "${crate}" --locked 2>&1)" || status=$?
	printf '%s\n' "${output}"
	if ((status == 0)); then
		echo "published ${crate}@${version}"
		exit 0
	fi
	if grep -Eq 'is already uploaded|already exists on crates\.io index' <<<"${output}"; then
		echo "skip ${crate}@${version}: already published"
		exit 0
	fi
	if grep -Eq 'status 429 Too Many Requests' <<<"${output}"; then
		wait="${retry_wait}"
		reason="rate limited"
	elif grep -Eiq 'no matching package named' <<<"${output}"; then
		wait="${propagation_wait}"
		reason="a dependency is not in the index yet"
	else
		echo "::error::publishing ${crate}@${version} failed"
		exit 1
	fi
	if ((attempt >= max_retries)); then
		echo "::error::${crate}@${version}: ${reason} after ${max_retries} attempts"
		exit 1
	fi
	echo "${crate}@${version}: ${reason} (attempt ${attempt}/${max_retries}); waiting ${wait}s"
	sleep "${wait}"
done
