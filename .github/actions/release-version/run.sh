#!/usr/bin/env bash
set -euo pipefail

tag="${RELEASE_TAG:?RELEASE_TAG required}"
image="${IMAGE:?IMAGE required}"
output="${GITHUB_OUTPUT:?GITHUB_OUTPUT required}"

image_tag="${tag//+/_}"
if [[ ! "${image_tag}" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]]; then
	echo "::error::tag ${tag} gives the invalid image tag ${image_tag}"
	exit 1
fi

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
	echo "${image}:${image_tag}"
	if [[ "${prerelease}" == false ]]; then
		echo "${image}:latest"
	fi
	echo "EOF"
} >>"${output}"
echo "${tag}: prerelease ${prerelease}, npm dist-tag ${dist_tag}"
