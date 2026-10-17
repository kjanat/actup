#!/usr/bin/env bash
set +e -uo pipefail

read -r -d '' -a extra <<<"${ACTUP_ARGS:-}"
"${ACTUP_BIN}" "${ACTUP_COMMAND}" ${extra[@]+"${extra[@]}"}
code=$?

echo "exit-code=${code}" >>"${GITHUB_OUTPUT}"
exit "${code}"
