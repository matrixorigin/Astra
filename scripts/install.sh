#!/usr/bin/env sh
# Install the astra CLI from GitHub Releases.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/matrixorigin/astra/main/scripts/install.sh | sh
#   curl -sSL https://gh-proxy.com/https://raw.githubusercontent.com/matrixorigin/astra/main/scripts/install.sh | sh
#   curl -sSL ... | sh -s -- -v v0.1.0 -y
#   curl -sSL ... | sh -s -- -d "$HOME/.local/bin"

set -eu

BINARY="astra"
REPO="${ASTRA_REPO:-matrixorigin/astra}"
VERSION="${ASTRA_VERSION:-latest}"
INSTALL_DIR="${ASTRA_INSTALL_DIR:-/usr/local/bin}"
if [ "${ASTRA_GHPROXY+x}" ]; then
  GHPROXY="$ASTRA_GHPROXY"
else
  GHPROXY="https://gh-proxy.com"
fi
YES=false
DRY_RUN=false

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Options:
  -v, --version TAG   Version to install (default: latest)
  -d, --dir DIR       Install directory (default: /usr/local/bin)
  -y, --yes           Skip confirmation prompt
  -n, --dry-run       Print download URL and exit
  -h, --help          Show this help

Environment:
  ASTRA_REPO          GitHub repo (default: matrixorigin/astra)
  ASTRA_VERSION       Version tag (default: latest)
  ASTRA_INSTALL_DIR   Install directory
  ASTRA_GHPROXY       Space-separated GitHub proxy prefixes (default: https://gh-proxy.com;
                      set to an empty string to disable proxy fallback)
EOF
}

info() {
  printf '%s\n' "$*"
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_arg() {
  [ "${2:-}" ] || die "$1 requires a value"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    -v|--version)
      need_arg "$1" "${2:-}"
      VERSION="$2"
      shift 2
      ;;
    -d|--dir)
      need_arg "$1" "${2:-}"
      INSTALL_DIR="$2"
      shift 2
      ;;
    -y|--yes)
      YES=true
      shift
      ;;
    -n|--dry-run)
      DRY_RUN=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

for cmd in curl tar install mktemp; do
  command -v "$cmd" >/dev/null 2>&1 || die "$cmd is required"
done

detect_target() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)

  case "$arch" in
    x86_64|amd64) arch="amd64" ;;
    aarch64|arm64) arch="arm64" ;;
    *) die "unsupported CPU architecture: $arch" ;;
  esac

  case "$os" in
    linux|darwin) printf '%s-%s\n' "$os" "$arch" ;;
    *) die "unsupported operating system: $os" ;;
  esac
}

proxy_url() {
  proxy="${1%/}"
  raw_url="$2"

  [ -n "$proxy" ] || return 1
  case "$proxy" in
    0|false|off|none) return 1 ;;
  esac

  printf '%s/%s\n' "$proxy" "$raw_url"
}

download_candidates() {
  raw_url="$1"

  printf '%s\n' "$raw_url"
  for proxy in $GHPROXY; do
    proxy_url "$proxy" "$raw_url" || true
  done
}

curl_stdout() {
  raw_url="$1"

  for url in $(download_candidates "$raw_url"); do
    if curl -fsSL --retry 2 --connect-timeout 10 --max-time 60 "$url"; then
      return 0
    fi
  done

  return 1
}

curl_effective_url() {
  raw_url="$1"

  for url in $(download_candidates "$raw_url"); do
    effective=$(
      curl -fsSLI -o /dev/null -w '%{url_effective}' \
        --retry 2 --connect-timeout 10 --max-time 60 "$url" 2>/dev/null || true
    )
    if [ -n "$effective" ]; then
      printf '%s\n' "$effective"
      return 0
    fi
  done

  return 1
}

download_file() {
  dest="$1"
  raw_url="$2"
  required="$3"
  first=true

  for url in $(download_candidates "$raw_url"); do
    if [ "$first" = true ]; then
      info "downloading $url"
    else
      warn "retrying through GitHub proxy: $url"
    fi

    if curl -fL --retry 3 --connect-timeout 15 --max-time 300 -o "$dest" "$url"; then
      return 0
    fi
    first=false
  done

  [ "$required" = true ] && die "failed to download $raw_url"
  return 1
}

resolve_tag() {
  if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ]; then
    tag=$(
      curl_effective_url "https://github.com/${REPO}/releases/latest" \
        | sed 's#.*/##'
    )
    if [ -n "$tag" ] && [ "$tag" != "latest" ]; then
      printf '%s\n' "$tag"
      return
    fi

    curl_stdout "https://api.github.com/repos/${REPO}/releases?per_page=1" \
      | sed -n 's|.*"tag_name": *"\([^"]*\)".*|\1|p' \
      | head -1
    return
  fi

  printf 'v%s\n' "${VERSION#v}"
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
    return
  fi
  return 1
}

confirm_install() {
  [ "$YES" = true ] && return 0
  [ -r /dev/tty ] || die "confirmation requires a terminal; rerun with --yes"

  printf 'Install %s %s to %s? [y/N] ' "$BINARY" "$TAG" "$INSTALL_DIR" > /dev/tty
  read -r answer < /dev/tty || exit 1
  case "$answer" in
    y|Y|yes|YES) ;;
    *) info "aborted"; exit 0 ;;
  esac
}

run_privileged() {
  if [ -n "${SUDO:-}" ]; then
    sudo "$@"
  else
    "$@"
  fi
}

TARGET=$(detect_target)
TAG=$(resolve_tag)
[ -n "$TAG" ] || die "could not resolve release version for ${REPO}"

VERSION_STR="${TAG#v}"
ARCHIVE="${BINARY}-v${VERSION_STR}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
URL="${BASE_URL}/${ARCHIVE}"
SUM_URL="${URL}.sha256"

if [ "$DRY_RUN" = true ]; then
  info "url:      $URL"
  info "checksum: $SUM_URL"
  info "target:   $TARGET"
  info "binary:   $BINARY"
  if [ -n "$GHPROXY" ]; then
    info "proxy fallback:"
    for url in $(download_candidates "$URL"); do
      [ "$url" = "$URL" ] && continue
      info "  $url"
    done
  fi
  exit 0
fi

if command -v "$BINARY" >/dev/null 2>&1; then
  installed=$("$BINARY" --version 2>/dev/null | sed -n 's/^astra \([^ ]*\).*$/\1/p' | head -1 || true)
  if [ "$installed" = "$VERSION_STR" ]; then
    info "$BINARY v$installed is already installed at $(command -v "$BINARY")"
    exit 0
  fi
fi

confirm_install

SUDO=""
if [ ! -d "$INSTALL_DIR" ]; then
  if ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
    command -v sudo >/dev/null 2>&1 || die "cannot create $INSTALL_DIR and sudo is unavailable"
    SUDO=sudo
    run_privileged mkdir -p "$INSTALL_DIR"
  fi
fi

if [ ! -w "$INSTALL_DIR" ]; then
  command -v sudo >/dev/null 2>&1 || die "$INSTALL_DIR is not writable and sudo is unavailable"
  SUDO=sudo
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

download_file "$TMP/$ARCHIVE" "$URL" true

if download_file "$TMP/$ARCHIVE.sha256" "$SUM_URL" false; then
  expected=$(awk '{print $1; exit}' "$TMP/$ARCHIVE.sha256")
  actual=$(sha256_file "$TMP/$ARCHIVE" || true)
  [ -n "$actual" ] || die "sha256sum or shasum is required to verify $ARCHIVE"
  [ "$expected" = "$actual" ] || die "checksum mismatch for $ARCHIVE"
  info "checksum verified"
else
  warn "checksum not found; installing without verification"
fi

tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
BIN_PATH="$TMP/$BINARY"
[ -f "$BIN_PATH" ] || die "$BINARY not found in $ARCHIVE"

run_privileged install -m 755 "$BIN_PATH" "$INSTALL_DIR/$BINARY"
info "installed $INSTALL_DIR/$BINARY"

case ":$PATH:" in
  *:"$INSTALL_DIR":*) ;;
  *) warn "$INSTALL_DIR is not in PATH" ;;
esac

"$INSTALL_DIR/$BINARY" --version || true
