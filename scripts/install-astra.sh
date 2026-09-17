#!/usr/bin/env sh
# Install the released Astra CLI and Edge/User Runner from matrixorigin/Astra.
#
# Usage:
#   curl --proto '=https' --tlsv1.2 -fsSL \
#     https://raw.githubusercontent.com/matrixorigin/Astra/main/scripts/install-astra.sh | sh
#   curl ... | sh -s -- --version 0.1.0 --dir "$HOME/.local/bin"

set -eu

REPOSITORY="matrixorigin/Astra"
BINARIES="astra astra-edge"
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

for command_name in awk cp curl grep install mktemp mv sort tar; do
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
        || fail "no stable Astra GitHub Release is available; see https://github.com/${REPOSITORY}/releases"
    tag=$(printf '%s\n' "$release_json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$tag" ] || fail "the latest GitHub Release did not contain a tag"
    VERSION=${tag#v}
else
    VERSION=${VERSION#v}
    tag="v${VERSION}"
fi

if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' \
    || ! awk -v version="$VERSION" '
        BEGIN {
            separator = index(version, "-")
            core = separator ? substr(version, 1, separator - 1) : version
            if (split(core, identifiers, ".") != 3) exit 1
            for (i = 1; i <= 3; i++) {
                if (identifiers[i] != "0" && identifiers[i] ~ /^0/) exit 1
            }
            if (!separator) exit 0
            prerelease = substr(version, separator + 1)
            count = split(prerelease, identifiers, ".")
            for (i = 1; i <= count; i++) {
                if (identifiers[i] == "") exit 1
                if (identifiers[i] ~ /^[0-9]+$/ \
                    && identifiers[i] != "0" \
                    && identifiers[i] ~ /^0/) exit 1
            }
        }
    ' </dev/null; then
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
archive="astra-v${VERSION}-${target}.tar.gz"
base_url="https://github.com/${REPOSITORY}/releases/download/${tag}"
archive_url="${base_url}/${archive}"
checksum_url="${archive_url}.sha256"

info "Release: ${tag} (${target})"
info "Source:  ${archive_url}"
info "Target:  ${INSTALL_DIR}/astra and ${INSTALL_DIR}/astra-edge"

show_next_steps() {
    printf '\n'
    info "Next: start the matching Astra Server stack"
    printf '%s\n' \
        "  git clone --branch ${tag} --depth 1 https://github.com/${REPOSITORY}.git Astra-${VERSION}" \
        "  cd Astra-${VERSION}" \
        "  make stack-setup"
    info "Already have a Server? Run: astra login"
}

if [ "$DRY_RUN" = true ]; then
    show_next_steps
    exit 0
fi

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/astra-install.XXXXXX")
install_stage=""
backup_stage=""
installation_started=false
installation_committed=false
cleanup() {
    if [ "$installation_started" = true ] && [ "$installation_committed" != true ]; then
        restore_failed=false
        for binary in $BINARIES; do
            if [ -e "${backup_stage}/${binary}" ] || [ -L "${backup_stage}/${binary}" ]; then
                mv -f "${backup_stage}/${binary}" "${INSTALL_DIR}/${binary}" \
                    || restore_failed=true
            else
                rm -f "${INSTALL_DIR}/${binary}" || restore_failed=true
            fi
        done
        if [ "$restore_failed" = true ]; then
            printf '%s\n' \
                "error: installation failed and automatic rollback was incomplete; inspect ${INSTALL_DIR}/astra and ${INSTALL_DIR}/astra-edge" >&2
        else
            printf '%s\n' "error: installation failed; previous Astra clients were restored" >&2
        fi
    fi
    rm -rf "$tmp_dir"
    if [ -n "$install_stage" ]; then
        rm -rf "$install_stage"
    fi
    if [ -n "$backup_stage" ]; then
        rm -rf "$backup_stage"
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

info "Downloading archive and checksum"
curl_download -sS -o "${tmp_dir}/${archive}" "$archive_url" \
    || fail "failed to download ${archive_url}"
curl_download -sS -o "${tmp_dir}/${archive}.sha256" "$checksum_url" \
    || fail "failed to download the required checksum"

checksum_record="${tmp_dir}/${archive}.sha256"
checksum_lines=$(awk 'END { print NR }' "$checksum_record")
read -r expected checksum_name checksum_extra < "$checksum_record"
if [ "$checksum_lines" -ne 1 ] \
    || ! printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' \
    || [ "$checksum_name" != "$archive" ] \
    || [ -n "${checksum_extra:-}" ]; then
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
[ "$members" = "$(printf 'LICENSE\nastra\nastra-edge')" ] \
    || fail "release archive must contain exactly astra, astra-edge, and LICENSE at its root"
for binary in $BINARIES; do
    tar -xOf "${tmp_dir}/${archive}" "$binary" > "${tmp_dir}/${binary}"
    chmod 0755 "${tmp_dir}/${binary}"
    reported_version=$("${tmp_dir}/${binary}" --version 2>/dev/null) \
        || fail "downloaded ${binary} could not report its version"
    [ "$reported_version" = "${binary} ${VERSION}" ] \
        || fail "downloaded ${binary} reports '${reported_version}', expected ${binary} ${VERSION}"
done

mkdir -p "$INSTALL_DIR" || fail "could not create ${INSTALL_DIR}"
if [ ! -w "$INSTALL_DIR" ]; then
    fail "${INSTALL_DIR} is not writable; choose a writable directory with --dir"
fi
install_stage=$(mktemp -d "${INSTALL_DIR}/.astra-install.XXXXXX")
backup_stage=$(mktemp -d "${INSTALL_DIR}/.astra-backup.XXXXXX")
for binary in $BINARIES; do
    install -m 0755 "${tmp_dir}/${binary}" "${install_stage}/${binary}"
    destination="${INSTALL_DIR}/${binary}"
    if [ -e "$destination" ] || [ -L "$destination" ]; then
        if [ ! -f "$destination" ] && [ ! -L "$destination" ]; then
            fail "${destination} exists but is not a regular file or symlink"
        fi
        cp -pP "$destination" "${backup_stage}/${binary}"
    fi
done
installation_started=true
mv -f "${install_stage}/astra-edge" "${INSTALL_DIR}/astra-edge"
# Move the CLI last so a visible new `astra` always has its matching Runner.
mv -f "${install_stage}/astra" "${INSTALL_DIR}/astra"
installation_committed=true
info "Installed astra and astra-edge ${VERSION} to ${INSTALL_DIR}"

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *) info "Add ${INSTALL_DIR} to PATH before invoking Astra" ;;
esac
show_next_steps
