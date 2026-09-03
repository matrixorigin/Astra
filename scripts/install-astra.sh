#!/usr/bin/env sh
# Install a released Astra CLI binary from matrixorigin/Astra.
#
# Usage:
#   curl --proto '=https' --tlsv1.2 -fsSL \
#     https://raw.githubusercontent.com/matrixorigin/Astra/main/scripts/install-astra.sh | sh
#   curl ... | sh -s -- --version 0.1.0 --dir "$HOME/.local/bin"

set -eu

REPOSITORY="matrixorigin/Astra"
BINARY="astra"
VERSION=""
INSTALL_DIR="${ASTRA_INSTALL_DIR:-}"
DRY_RUN=false

info() { printf '%s\n' "> $*"; }
fail() { printf '%s\n' "error: $*" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: install-astra.sh [OPTIONS]

Options:
  -v, --version VERSION  Install vVERSION instead of the latest stable release
  -d, --dir PATH         Install directory (default: /usr/local/bin or ~/.local/bin)
  -n, --dry-run          Print the selected release and destination without installing
  -y, --yes              Accepted for compatibility; installation is non-interactive
  -h, --help             Show this help

Environment:
  ASTRA_INSTALL_DIR      Default install directory; --dir takes precedence
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -v|--version)
            [ "$#" -ge 2 ] || fail "$1 requires a value"
            VERSION="$2"
            shift 2
            ;;
        -d|--dir)
            [ "$#" -ge 2 ] || fail "$1 requires a value"
            INSTALL_DIR="$2"
            shift 2
            ;;
        -n|--dry-run)
            DRY_RUN=true
            shift
            ;;
        -y|--yes)
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

for command_name in awk curl grep install mktemp sort tar; do
    command -v "$command_name" >/dev/null 2>&1 || fail "${command_name} is required"
done

case "$(uname -s)" in
    Linux) os="linux" ;;
    Darwin) os="darwin" ;;
    *) fail "unsupported operating system: $(uname -s)" ;;
esac

case "$(uname -m)" in
    x86_64|amd64) arch="amd64" ;;
    arm64|aarch64) arch="arm64" ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
esac

curl_download() {
    curl --proto '=https' --tlsv1.2 --retry 3 --retry-delay 1 -fL "$@"
}

if [ -z "$VERSION" ]; then
    info "Resolving the latest stable Astra release"
    release_json=$(curl_download -sS "https://api.github.com/repos/${REPOSITORY}/releases/latest") \
        || fail "no stable Astra GitHub Release is available"
    tag=$(printf '%s\n' "$release_json" | sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$tag" ] || fail "the latest GitHub Release did not contain a tag"
    VERSION=${tag#v}
else
    VERSION=${VERSION#v}
    tag="v${VERSION}"
fi

if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
    fail "invalid release version: ${VERSION}"
fi
[ "$tag" = "v${VERSION}" ] || fail "unsupported release tag: ${tag}"

if [ -z "$INSTALL_DIR" ]; then
    if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
        INSTALL_DIR="/usr/local/bin"
    else
        [ -n "${HOME:-}" ] || fail "HOME is unset; choose an install directory with --dir"
        INSTALL_DIR="${HOME}/.local/bin"
    fi
fi

target="${os}-${arch}"
archive="${BINARY}-v${VERSION}-${target}.tar.gz"
base_url="https://github.com/${REPOSITORY}/releases/download/${tag}"
archive_url="${base_url}/${archive}"
checksum_url="${archive_url}.sha256"

info "Release: ${tag} (${target})"
info "Source:  ${archive_url}"
info "Target:  ${INSTALL_DIR}/${BINARY}"

if [ "$DRY_RUN" = true ]; then
    exit 0
fi

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/astra-install.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

info "Downloading archive and checksum"
curl_download -sS -o "${tmp_dir}/${archive}" "$archive_url" \
    || fail "failed to download ${archive_url}"
curl_download -sS -o "${tmp_dir}/${archive}.sha256" "$checksum_url" \
    || fail "failed to download the required checksum"

expected=$(awk 'NR == 1 { print $1 }' "${tmp_dir}/${archive}.sha256")
if ! printf '%s\n' "$expected" | grep -Eq '^[0-9a-fA-F]{64}$'; then
    fail "the published checksum is invalid"
fi
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "${tmp_dir}/${archive}" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "${tmp_dir}/${archive}" | awk '{ print $1 }')
else
    fail "sha256sum or shasum is required"
fi
[ "$expected" = "$actual" ] || fail "checksum mismatch for ${archive}"
info "Checksum verified"

members=$(tar -tzf "${tmp_dir}/${archive}" | sort)
[ "$members" = "$(printf 'LICENSE\nastra')" ] \
    || fail "release archive must contain exactly astra and LICENSE at its root"
tar -xOf "${tmp_dir}/${archive}" "$BINARY" > "${tmp_dir}/${BINARY}"
chmod 0755 "${tmp_dir}/${BINARY}"
reported_version=$("${tmp_dir}/${BINARY}" --version 2>/dev/null) \
    || fail "downloaded ${BINARY} could not report its version"
case "$reported_version" in
    *"${VERSION}"*) ;;
    *) fail "downloaded binary reports '${reported_version}', expected ${VERSION}" ;;
esac

mkdir -p "$INSTALL_DIR"
if [ ! -w "$INSTALL_DIR" ]; then
    fail "${INSTALL_DIR} is not writable; choose a writable directory with --dir"
fi
install -m 0755 "${tmp_dir}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
info "Installed ${BINARY} ${VERSION} to ${INSTALL_DIR}/${BINARY}"

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *) info "Add ${INSTALL_DIR} to PATH before invoking ${BINARY}" ;;
esac
