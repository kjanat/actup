#!/usr/bin/env bash
set -euo pipefail

if (($# < 1 || $# > 2)); then
	echo "usage: $0 <tag> [dir]" >&2
	exit 2
fi
tag="$1"
dir="${2:-$(dirname "${BASH_SOURCE[0]}")/workflows}"
if [[ ! "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
	echo "error: ${tag} is not a release tag like v1.2.3" >&2
	exit 2
fi
sha="$(git rev-parse --verify --quiet "${tag}^{commit}")" || {
	echo "error: no tag ${tag} in this repository" >&2
	exit 1
}

for file in "${dir}"/*.yml; do
	sed -E -i.bak \
		-e "s|kjanat/actup@[^[:space:]]+ # v[^[:space:]]+|kjanat/actup@${sha} # ${tag}|" \
		-e "s|^([[:space:]]*version: )v[^[:space:]]+$|\\1${tag}|" \
		"${file}"
	rm -- "${file}.bak"
	echo "${file}: kjanat/actup@${sha} # ${tag}"
done
