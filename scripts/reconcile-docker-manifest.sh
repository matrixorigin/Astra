#!/usr/bin/env bash
# Create an immutable Docker tag from verified candidate digests, then prove
# that its Linux platform-to-digest mapping matches the requested build matrix.

set -euo pipefail

if [[ $# -ne 5 ]]; then
    echo "usage: $0 <image> <tag> <matrix-json> <digest-dir> <reject|verify>" >&2
    exit 2
fi

image_name="$1"
image_tag="$2"
matrix_json="$3"
digest_dir="$4"
existing_policy="$5"

case "${existing_policy}" in
    reject|verify) ;;
    *)
        echo "existing-tag policy must be reject or verify" >&2
        exit 2
        ;;
esac

for command_name in docker python3 sha256sum; do
    command -v "${command_name}" >/dev/null 2>&1 || {
        echo "${command_name} is required" >&2
        exit 1
    }
done

if [[ ! -d "${digest_dir}" ]]; then
    echo "candidate digest directory does not exist: ${digest_dir}" >&2
    exit 1
fi

mapfile -t expected_platform_names < <(
    python3 -c '
import json, sys
document = json.load(sys.stdin)
platforms = document.get("include", [])
if not platforms:
    raise SystemExit("empty build matrix")
for entry in platforms:
    platform = entry.get("platform")
    if not isinstance(platform, str) or not platform:
        raise SystemExit("invalid platform in build matrix")
    print(platform)
' <<< "${matrix_json}" | sort -u
)
expected_source_count="$(
    python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("include", [])))' \
        <<< "${matrix_json}"
)"
if [[ "${#expected_platform_names[@]}" -ne "${expected_source_count}" ]]; then
    echo "build matrix contains duplicate platform names" >&2
    exit 1
fi

shopt -s nullglob
digest_files=("${digest_dir}"/*)
if [[ "${#digest_files[@]}" -ne "${expected_source_count}" ]]; then
    echo "expected ${expected_source_count} candidate digests, found ${#digest_files[@]}" >&2
    exit 1
fi

platform_digest_set() {
    local reference="$1"
    local manifest_json manifest_platforms image_json platform digest

    manifest_json="$(docker buildx imagetools inspect "${reference}" --format '{{json .Manifest}}')"
    manifest_platforms="$(
        python3 -c '
import json, re, sys
document = json.load(sys.stdin)
manifests = document.get("manifests")
if not isinstance(manifests, list):
    print("__SINGLE__")
    raise SystemExit
found = []
unexpected = []
for manifest in manifests:
    platform = manifest.get("platform", {})
    if platform.get("os") == "linux" and platform.get("architecture") in {"amd64", "arm64"}:
        architecture = platform["architecture"]
        digest = manifest.get("digest")
        if not isinstance(digest, str) or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None:
            raise SystemExit("image manifest has an invalid digest")
        found.append(f"linux/{architecture}={digest}")
    elif not (
        platform.get("os") == "unknown"
        and platform.get("architecture") == "unknown"
        and manifest.get("annotations", {}).get("vnd.docker.reference.type")
            == "attestation-manifest"
    ):
        unexpected.append(platform)
if unexpected:
    raise SystemExit(f"index contains unexpected platforms: {unexpected}")
if not found:
    raise SystemExit("index contains no supported Linux image manifest")
print("\n".join(sorted(set(found))))
' <<< "${manifest_json}"
    )"
    if [[ "${manifest_platforms}" != "__SINGLE__" ]]; then
        printf '%s\n' "${manifest_platforms}"
        return
    fi

    image_json="$(docker buildx imagetools inspect "${reference}" --format '{{json .Image}}')"
    platform="$(
        python3 -c '
import json, sys
image = json.load(sys.stdin)
if image.get("os") != "linux" or image.get("architecture") not in {"amd64", "arm64"}:
    raise SystemExit("unsupported single-platform image")
print("linux/" + image["architecture"])
' <<< "${image_json}"
    )"
    digest="sha256:$(docker buildx imagetools inspect "${reference}" --raw | sha256sum | awk '{print $1}')"
    printf '%s=%s\n' "${platform}" "${digest}"
}

sources=()
candidate_platforms_file="$(mktemp "${RUNNER_TEMP:-/tmp}/astra-platforms.XXXXXX")"
trap 'rm -f "${candidate_platforms_file}"' EXIT HUP INT TERM

for digest_file in "${digest_files[@]}"; do
    if [[ ! -f "${digest_file}" ]]; then
        echo "candidate digest entry is not a regular file: ${digest_file}" >&2
        exit 1
    fi
    digest="$(basename "${digest_file}")"
    if [[ ! "${digest}" =~ ^[0-9a-f]{64}$ ]]; then
        echo "invalid candidate digest filename: ${digest}" >&2
        exit 1
    fi
    source="${image_name}@sha256:${digest}"
    sources+=("${source}")
    mapfile -t source_platforms < <(platform_digest_set "${source}")
    if [[ "${#source_platforms[@]}" -ne 1 ]]; then
        echo "candidate ${source} must contain exactly one supported Linux platform" >&2
        exit 1
    fi
    printf '%s\n' "${source_platforms[0]}" >> "${candidate_platforms_file}"
done
sort -u -o "${candidate_platforms_file}" "${candidate_platforms_file}"

mapfile -t candidate_platform_names < <(cut -d= -f1 "${candidate_platforms_file}" | sort -u)
if [[ "$(printf '%s\n' "${expected_platform_names[@]}")" != "$(printf '%s\n' "${candidate_platform_names[@]}")" ]]; then
    echo "candidate platforms do not match the requested build matrix" >&2
    diff \
        <(printf '%s\n' "${expected_platform_names[@]}") \
        <(printf '%s\n' "${candidate_platform_names[@]}") || true
    exit 1
fi

target="${image_name}:${image_tag}"
target_exists=false
if inspect_output="$(docker buildx imagetools inspect "${target}" 2>&1)"; then
    target_exists=true
elif ! grep -Eqi '(: not found|manifest unknown|name unknown|HTTP 404|status[^0-9]*404)' \
    <<< "${inspect_output}"; then
    echo "could not safely determine whether ${target} exists:" >&2
    printf '%s\n' "${inspect_output}" >&2
    exit 1
fi

if [[ "${target_exists}" == true ]]; then
    if [[ "${existing_policy}" == "reject" ]]; then
        echo "${target} already exists and will not be overwritten" >&2
        exit 1
    fi
    echo "${target} already exists; verifying its immutable platform set."
else
    docker buildx imagetools create --tag "${target}" "${sources[@]}"
fi

actual_platforms_file="$(mktemp "${RUNNER_TEMP:-/tmp}/astra-published-platforms.XXXXXX")"
trap 'rm -f "${candidate_platforms_file}" "${actual_platforms_file}"' EXIT HUP INT TERM
platform_digest_set "${target}" > "${actual_platforms_file}"
sort -u -o "${actual_platforms_file}" "${actual_platforms_file}"

if ! cmp -s "${candidate_platforms_file}" "${actual_platforms_file}"; then
    echo "published manifest ${target} does not match the verified candidates" >&2
    diff "${candidate_platforms_file}" "${actual_platforms_file}" || true
    exit 1
fi

echo "verified ${target}:"
sed 's/^/  /' "${actual_platforms_file}"
