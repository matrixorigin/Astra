#!/usr/bin/env bash
# Exercise draft-aware, fail-closed GitHub Release resolution without network access.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/astra-release-lookup.XXXXXX")"
trap 'rm -rf "$fixture_root"' EXIT HUP INT TERM

fake_bin="${fixture_root}/bin"
mkdir -p "$fake_bin"
cat > "${fake_bin}/gh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

[[ "${1:-}" == api ]]
[[ "${2:-}" == 'repos/matrixorigin/Astra/releases?per_page=100' ]]
[[ " $* " == *' --paginate '* ]]
[[ " $* " == *' --jq '* ]]

case "${FAKE_GH_MODE:?}" in
    published)
        printf 'v0.1.0\tfalse\t100\nv0.2.0\tfalse\t123\n'
        ;;
    draft)
        printf 'v0.2.0\ttrue\t124\n'
        ;;
    empty)
        ;;
    http403)
        echo 'gh: Forbidden (HTTP 403)' >&2
        exit 1
        ;;
    http404)
        echo 'gh: Not Found (HTTP 404)' >&2
        exit 1
        ;;
    duplicate)
        printf 'v0.2.0\ttrue\t124\nv0.2.0\tfalse\t125\n'
        ;;
    invalid-draft)
        printf 'v0.2.0\tnull\t124\n'
        ;;
    invalid-id)
        printf 'v0.2.0\ttrue\tnot-a-number\n'
        ;;
    *)
        echo "unknown fake mode: ${FAKE_GH_MODE}" >&2
        exit 2
        ;;
esac
SH
chmod 0755 "${fake_bin}/gh"

resolve() {
    PATH="${fake_bin}:${PATH}" FAKE_GH_MODE="$1" \
        "${repo_root}/scripts/resolve-github-release.sh" matrixorigin/Astra v0.2.0
}

[[ "$(resolve published)" == $'published\t123' ]]
[[ "$(resolve draft)" == $'draft\t124' ]]
[[ "$(resolve empty)" == $'none\t' ]]

for mode in http403 http404 duplicate invalid-draft invalid-id; do
    if resolve "$mode" >/dev/null 2>&1; then
        echo "release lookup unexpectedly accepted ${mode}" >&2
        exit 1
    fi
done
