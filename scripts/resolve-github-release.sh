#!/usr/bin/env bash
# Resolve a GitHub Release by tag, including drafts that are not available
# through the releases/tags endpoint.

set -euo pipefail

repository="${1:-}"
source_tag="${2:-}"
if [[ -z "$repository" || -z "$source_tag" ]]; then
    echo "usage: resolve-github-release.sh OWNER/REPO TAG" >&2
    exit 2
fi

if ! release_list="$(
    gh api "repos/${repository}/releases?per_page=100" \
        --paginate --jq '.[] | [.tag_name, .draft, .id] | @tsv'
)"; then
    echo "Could not safely determine whether GitHub Release ${source_tag} exists." >&2
    exit 1
fi

state=none
release_id=""
matches=0
while IFS=$'\t' read -r release_tag release_draft candidate_id; do
    [[ "$release_tag" == "$source_tag" ]] || continue
    matches=$((matches + 1))
    case "$release_draft" in
        true) candidate_state=draft ;;
        false) candidate_state=published ;;
        *)
            echo "GitHub Release ${source_tag} has an invalid draft state: ${release_draft:-<empty>}." >&2
            exit 1
            ;;
    esac
    case "$candidate_id" in
        '' | *[!0-9]*)
            echo "GitHub Release ${source_tag} has an invalid release ID: ${candidate_id:-<empty>}." >&2
            exit 1
            ;;
    esac
    state="$candidate_state"
    release_id="$candidate_id"
done <<< "$release_list"

if [[ "$matches" -gt 1 ]]; then
    echo "GitHub returned ${matches} releases for ${source_tag}; refusing an ambiguous publication." >&2
    exit 1
fi

printf '%s\t%s\n' "$state" "$release_id"
