#!/usr/bin/env bash
# Require the retained candidate set owned by a release's recorded Actions run.

set -euo pipefail

repository="${1:-}"
owner_run_id="${2:-}"
if [[ ! "${repository}" =~ ^[^/]+/[^/]+$ ]] || [[ ! "${owner_run_id}" =~ ^[0-9]+$ ]]; then
    echo "usage: $0 OWNER/REPO RUN_ID" >&2
    exit 2
fi

if ! retained_artifacts="$(
    gh api \
        "repos/${repository}/actions/runs/${owner_run_id}/artifacts?per_page=100" \
        --paginate --jq '.artifacts[] | select(.expired == false) | .name'
)"; then
    echo "Could not inspect retained candidates from release owner run ${owner_run_id}." >&2
    exit 1
fi

required_artifacts=(
    release-client-assets
    release-digest-linux-amd64
    release-digest-linux-arm64
)
for required_artifact in "${required_artifacts[@]}"; do
    match_count="$(grep -Fxc "${required_artifact}" <<< "${retained_artifacts}" || true)"
    if [[ "${match_count}" -ne 1 ]]; then
        echo "Release owner run ${owner_run_id} must retain exactly one ${required_artifact}; found ${match_count}." >&2
        echo "Recovery will not substitute newly built output for immutable release candidates." >&2
        echo "Publish a patch version instead." >&2
        exit 1
    fi
done
