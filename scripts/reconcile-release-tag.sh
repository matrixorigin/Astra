#!/usr/bin/env bash
# Create or verify one immutable annotated release tag and its owning Actions run.

set -euo pipefail

mode="${1:-}"
repository="${2:-}"
source_tag="${3:-}"
source_sha="${4:-}"
owner_run_id="${5:-}"
default_branch="${6:-}"
expected_tag_object="${7:-}"

if [ "$#" -ne 7 ] || [[ ! "${mode}" =~ ^(create|verify)$ ]] \
    || [ -z "${repository}" ] || [ -z "${source_tag}" ] \
    || [ -z "${source_sha}" ] || [ -z "${owner_run_id}" ]; then
    echo "Usage: $0 <create|verify> <repository> <tag> <source-sha> <owner-run-id> <default-branch> <expected-tag-object>" >&2
    exit 2
fi
if [ "${mode}" = "create" ] && [ -z "${default_branch}" ]; then
    echo "A default branch is required when creating ${source_tag}." >&2
    exit 2
fi
if [ "${mode}" = "verify" ] && [ -z "${expected_tag_object}" ]; then
    echo "An expected tag object is required when verifying ${source_tag}." >&2
    exit 2
fi

server_url="${GITHUB_SERVER_URL:-https://github.com}"
run_marker="Release-Run: ${server_url}/${repository}/actions/runs/${owner_run_id}"
remote_tag_object="$(
    git ls-remote origin "refs/tags/${source_tag}" |
        awk 'NR == 1 { print $1 }'
)"

if [ -z "${remote_tag_object}" ]; then
    if [ "${mode}" = "verify" ]; then
        echo "Release tag ${source_tag} disappeared after candidate verification." >&2
        exit 1
    fi

    current_default_sha="$(
        git ls-remote origin "refs/heads/${default_branch}" |
            awk 'NR == 1 { print $1 }'
    )"
    if [ -z "${current_default_sha}" ]; then
        echo "Could not resolve the current ${default_branch} head before creating ${source_tag}." >&2
        exit 1
    fi
    if [ "${current_default_sha}" != "${source_sha}" ]; then
        git fetch --no-tags origin \
            "refs/heads/${default_branch}:refs/remotes/origin/${default_branch}"
        if ! git merge-base --is-ancestor "${source_sha}" "${current_default_sha}"; then
            echo "Release source ${source_sha} is no longer in ${default_branch} history." >&2
            echo "No tag was created." >&2
            exit 1
        fi
        if git diff --quiet "${source_sha}" "${current_default_sha}" -- .github/workflows; then
            :
        elif [ "$?" -eq 1 ]; then
            echo "${default_branch} advanced with workflow changes during candidate verification." >&2
            echo "The workflow token cannot tag the now-historical source ${source_sha}." >&2
            echo "Start a new release run from the current protected branch; no tag was created." >&2
            exit 1
        else
            echo "Could not verify workflow changes between ${source_sha} and ${current_default_sha}." >&2
            echo "No tag was created." >&2
            exit 1
        fi
    fi

    tag_message="$(printf 'Astra %s\n\n%s' "${source_tag}" "${run_marker}")"
    if ! remote_tag_object="$(
        gh api --method POST "repos/${repository}/git/tags" \
            -f tag="${source_tag}" \
            -f message="${tag_message}" \
            -f object="${source_sha}" \
            -f type=commit \
            --jq .sha
    )"; then
        echo "GitHub refused to create ${source_tag}." >&2
        echo "If ${default_branch} just gained workflow changes, restart from its current head; otherwise verify release-token and tag-ruleset permissions." >&2
        exit 1
    fi
    if ! gh api --method POST "repos/${repository}/git/refs" \
        -f ref="refs/tags/${source_tag}" \
        -f sha="${remote_tag_object}" >/dev/null; then
        observed_tag_object="$(
            git ls-remote origin "refs/tags/${source_tag}" |
                awk 'NR == 1 { print $1 }'
        )"
        if [ "${observed_tag_object}" != "${remote_tag_object}" ]; then
            echo "Could not create immutable release ref ${source_tag}." >&2
            exit 1
        fi
    fi
elif [ "${mode}" = "verify" ] && [ "${remote_tag_object}" != "${expected_tag_object}" ]; then
    echo "Release tag ${source_tag} changed after it was verified." >&2
    exit 1
fi

tag_json="$(gh api "repos/${repository}/git/tags/${remote_tag_object}")" || {
    echo "Existing ${source_tag} is not an annotated release tag owned by this run." >&2
    exit 1
}
TAG_JSON="${tag_json}" python3 - "${source_sha}" "${run_marker}" <<'PY'
import json
import os
import sys

tag = json.loads(os.environ["TAG_JSON"])
expected_source, expected_marker = sys.argv[1:]
message = tag.get("message", "")
markers = [line for line in message.splitlines() if "Release-Run: " in line]
if tag.get("object", {}).get("sha") != expected_source or markers != [expected_marker]:
    print("Release tag is not owned by this run and source.", file=sys.stderr)
    raise SystemExit(1)
PY

printf '%s\n' "${remote_tag_object}"
