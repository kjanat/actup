#!/usr/bin/env bash
set -euo pipefail

tag="${RELEASE_TAG:?RELEASE_TAG required}"
image="${IMAGE:?IMAGE required}"
output="${GITHUB_OUTPUT:?GITHUB_OUTPUT required}"

version="${tag#v}"
if [[ "${version%%+*}" == *-* ]]; then
	prerelease=true
	dist_tag=next
else
	prerelease=false
	dist_tag=latest
fi

{
	echo "prerelease=${prerelease}"
	echo "dist-tag=${dist_tag}"
	echo "image-tags<<EOF"
	echo "${image}:${tag}"
	if [[ "${prerelease}" == false ]]; then
		echo "${image}:latest"
	fi
	echo "EOF"
} >>"${output}"
echo "${tag}: prerelease ${prerelease}, npm dist-tag ${dist_tag}"
