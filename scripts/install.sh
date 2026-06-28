#!/usr/bin/env sh
# Install astra binary from GitHub Releases.
# Usage:
#   curl -sSL https://raw.githubusercontent.com/matrixorigin/astra/main/scripts/install.sh | sh
#   curl -sSL ... | sh -s -- -v v0.1.0
#   curl -sSL ... | sh -s -- -y              # skip confirmation
#   curl -sSL ... | sh -s -- -d ~/.local/bin  # custom directory
#   curl -sSL ... | sh -s -- -n              # dry-run: print URL and exit
#
# Options:
#   -v, --version TAG      Version to install (default: latest release)
#   -d, --dir DIR          Install directory (default: /usr/local/bin, sudo if needed)
#   -y, --yes              Skip confirmation prompt
#   -n, --dry-run          Print download URL and exit
#   -h, --help             Show this help
#
# Env:
#   ASTRA_REPO             GitHub repo (default: matrixorigin/astra)
#   ASTRA_VERSION          Version tag (default: latest)
#   ASTRA_INSTALL_DIR      Install directory (overridden by -d)
#   ASTRA_GHPROXY          ghproxy base URL (default: https://ghfast.top)
#   ASTRA_ALLOW_DOWNGRADE  Set to 1 to allow downgrades

set -eu

# ── Colors ──────────────────────────────────────────────────────────

BOLD="$(tput bold 2>/dev/null || printf '')"
GREEN="$(tput setaf 2 2>/dev/null || printf '')"
YELLOW="$(tput setaf 3 2>/dev/null || printf '')"
BLUE="$(tput setaf 4 2>/dev/null || printf '')"
RED="$(tput setaf 1 2>/dev/null || printf '')"
DIM="$(tput dim 2>/dev/null || printf '')"
NC="$(tput sgr0 2>/dev/null || printf '')"

info()  { printf '%s\n' "${BOLD}>${NC} $*"; }
warn()  { printf '%s\n' "${YELLOW}! $*${NC}"; }
error() { printf '%s\n' "${RED}x $*${NC}" >&2; }
ok()    { printf '%s\n' "${GREEN}✓${NC} $*"; }

# ── Banner ──────────────────────────────────────────────────────────

cat << "EOF"

   █████╗ ███████╗████████╗██████╗  █████╗
  ██╔══██╗██╔════╝╚══██╔══╝██╔══██╗██╔══██╗
  ███████║███████╗   ██║   ██████╔╝███████║
  ██╔══██║╚════██║   ██║   ██╔══██╗██╔══██║
  ██║  ██║███████║   ██║   ██║  ██║██║  ██║
  ╚═╝  ╚═╝╚══════╝   ╚═╝   ╚═╝  ╚═╝╚═╝  ╚═╝
        Self-improving coding agent
EOF

# ── Prerequisites ───────────────────────────────────────────────────

if ! command -v curl >/dev/null 2>&1; then
  error "curl is required but not found"
  exit 1
fi

# ── Defaults ────────────────────────────────────────────────────────

REPO="${ASTRA_REPO:-matrixorigin/astra}"
VERSION="${ASTRA_VERSION:-}"
BINARY="astra"
INSTALL_DIR=""
DRY_RUN=false
FORCE=false
ALLOW_DOWNGRADE="${ASTRA_ALLOW_DOWNGRADE:-0}"

# ── Platform detection ──────────────────────────────────────────────

detect_target() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch="amd64" ;;
    aarch64|arm64) arch="arm64" ;;
    *) arch="" ;;
  esac
  case "$os" in
    linux|darwin)
      [ -n "$arch" ] && printf "%s-%s" "$os" "$arch" && return
      ;;
  esac
  printf ""
}

# ── Writability test ────────────────────────────────────────────────

test_writeable() {
  path="${1}/._astra_write_test"
  if touch "${path}" 2>/dev/null; then
    rm -f "${path}"
    return 0
  fi
  return 1
}

# ── Sudo elevation ─────────────────────────────────────────────────

elevate_priv() {
  if ! command -v sudo >/dev/null 2>&1; then
    error "Need write access to ${INSTALL_DIR} but 'sudo' not found"
    info "Either run as root, or use: -d ~/.local/bin"
    exit 1
  fi
  warn "Elevated permissions required to install to ${INSTALL_DIR}"
  if ! sudo -v; then
    error "Superuser not granted, aborting"
    exit 1
  fi
}

# ── PATH detection & shell config hints ─────────────────────────────

check_path() {
  dir="$1"
  case ":${PATH}:" in
    *:"${dir}":*) return 0 ;;
  esac
  return 1
}

print_path_hint() {
  dir="$1"
  printf '\n'
  warn "${dir} is not in your PATH"
  info "Add it by running one of:"
  printf '\n'

  shell_name="$(basename "${SHELL:-sh}")"
  case "$shell_name" in
    zsh)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.zshrc && source ~/.zshrc${NC}"
      ;;
    bash)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.bashrc && source ~/.bashrc${NC}"
      ;;
    fish)
      info "  ${BLUE}fish_add_path ${dir}${NC}"
      ;;
    *)
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.bashrc${NC}  (bash)"
      info "  ${BLUE}echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.zshrc${NC}   (zsh)"
      info "  ${BLUE}fish_add_path ${dir}${NC}                              (fish)"
      ;;
  esac
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
    return 0
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
    return 0
  fi
  return 1
}

# -- Confirmation -----------------------------------------------------------

confirm() {
  if [ "$FORCE" = true ]; then return 0; fi
  printf "%s " "$* ${BOLD}[y/N]${NC}"
  read -r yn < /dev/tty || return 1
  case "$yn" in
    [Yy]*) return 0 ;;
    *) return 1 ;;
  esac
}

# ── Parse args ──────────────────────────────────────────────────────

while [ $# -gt 0 ]; do
  case "$1" in
    -v|--version)      VERSION="$2"; shift 2 ;;
    -d|--dir)          INSTALL_DIR="$2"; shift 2 ;;
    -y|--yes)          FORCE=true; shift ;;
    -n|--dry-run)      DRY_RUN=true; shift ;;
    -h|--help)
      printf "Usage: install.sh [options]\n\n"
      printf "  -v, --version TAG      Version to install (default: latest)\n"
      printf "  -d, --dir DIR          Install directory (default: /usr/local/bin)\n"
      printf "  -y, --yes              Skip confirmation prompt\n"
      printf "  -n, --dry-run          Print download URL and exit\n"
      printf "  -h, --help             Show this help\n"
      printf "\nEnvironment:\n"
      printf "  ASTRA_REPO             GitHub repo (default: matrixorigin/astra)\n"
      printf "  ASTRA_VERSION          Version tag\n"
      printf "  ASTRA_INSTALL_DIR      Install directory\n"
      printf "  ASTRA_GHPROXY          ghproxy base URL\n"
      printf "  ASTRA_ALLOW_DOWNGRADE  Set to 1 to allow downgrades\n"
      exit 0
      ;;
    *) shift ;;
  esac
done

# ── Resolve target ──────────────────────────────────────────────────

TARGET=$(detect_target)
if [ -z "$TARGET" ]; then
  error "Unsupported platform: $(uname -s) $(uname -m)"
  exit 1
fi

# ── Resolve version ─────────────────────────────────────────────────

if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ]; then
  RESOLVED_TAG=$(curl -sSf -o /dev/null -w '%{redirect_url}' "https://github.com/${REPO}/releases/latest" 2>/dev/null | grep -oE '[^/]+$' || true)
  if [ -n "$RESOLVED_TAG" ]; then
    TAG="$RESOLVED_TAG"
  else
    TAG=$(curl -sSfL "https://api.github.com/repos/${REPO}/releases?per_page=20" 2>/dev/null \
      | sed -n 's|.*"tag_name": *"\(v[^"]*\)".*|\1|p' | head -1)
    [ -n "$TAG" ] || { error "No releases found at https://github.com/${REPO}/releases"; exit 1; }
  fi
else
  TAG="${VERSION#v}"
  TAG="v${TAG}"
fi

VERSION_STR="${TAG#v}"
ARCHIVE="${BINARY}-v${VERSION_STR}-${TARGET}.tar.gz"
GH_URL="https://github.com/${REPO}/releases/download/${TAG}/${ARCHIVE}"
GH_SUM_URL="${GH_URL}.sha256"
GHPROXY="${ASTRA_GHPROXY:-https://ghfast.top}"

if [ "$DRY_RUN" = true ]; then
  printf "URL:       %s\n" "$GH_URL"
  printf "Checksum:  %s\n" "$GH_SUM_URL"
  printf "Platform:  %s\n" "$TARGET"
  printf "Binary:    %s\n" "$BINARY"
  exit 0
fi

# ── Resolve install directory ───────────────────────────────────────

if [ -z "$INSTALL_DIR" ]; then
  INSTALL_DIR="${ASTRA_INSTALL_DIR:-/usr/local/bin}"
fi

# ── Check existing installation ─────────────────────────────────────

SKIP_DOWNLOAD=false
if command -v "${BINARY}" >/dev/null 2>&1; then
  INSTALLED_VERSION="$("${BINARY}" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)"
  if [ -n "$INSTALLED_VERSION" ]; then
    if [ "$INSTALLED_VERSION" = "$VERSION_STR" ] && [ "$ALLOW_DOWNGRADE" != "1" ]; then
      ok "${BINARY} v${INSTALLED_VERSION} already installed (latest)"
      SKIP_DOWNLOAD=true
      INSTALL_DIR="$(dirname "$(command -v "${BINARY}")")"
    else
      info "${BINARY} v${INSTALLED_VERSION} installed, upgrading to v${VERSION_STR}"
    fi
  fi
fi

# ── Show plan & confirm ────────────────────────────────────────────

if [ "$SKIP_DOWNLOAD" = false ]; then
  printf '\n'
  info "${BOLD}Version${NC}:   ${GREEN}v${VERSION_STR}${NC}"
  info "${BOLD}Platform${NC}:  ${GREEN}${TARGET}${NC}"
  info "${BOLD}Binary${NC}:    ${GREEN}${BINARY}${NC}"
  info "${BOLD}Directory${NC}: ${GREEN}${INSTALL_DIR}${NC}"
  printf '\n'

  if ! confirm "Install ${BINARY}?"; then
    info "Aborted"
    exit 0
  fi
fi

# ── Download ────────────────────────────────────────────────────────

if [ "$SKIP_DOWNLOAD" = false ]; then

  # Determine sudo requirement
  SUDO=""
  if ! test_writeable "$INSTALL_DIR" 2>/dev/null; then
    if [ ! -d "$INSTALL_DIR" ]; then
      if ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
        elevate_priv
        SUDO="sudo"
        $SUDO mkdir -p "$INSTALL_DIR"
      fi
    else
      elevate_priv
      SUDO="sudo"
    fi
  fi

  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT

  info "Downloading ${BLUE}${GH_URL}${NC}"
  if ! curl -fL# --max-time 30 --connect-timeout 15 --retry 3 -o "$TMP/$ARCHIVE" "$GH_URL" 2>/dev/null; then
    warn "Direct download failed, retrying via proxy: ${GHPROXY}"
    URL="${GHPROXY}/${GH_URL}"
    SUM_URL="${GHPROXY}/${GH_SUM_URL}"
    info "Downloading ${BLUE}${URL}${NC}"
    curl -fL# --max-time 30 --retry 3 -o "$TMP/$ARCHIVE" "$URL" || {
      error "Download failed — check that version '${TAG}' exists"
      info "Available releases: ${BLUE}https://github.com/${REPO}/releases${NC}"
      exit 1
    }
  else
    URL="$GH_URL"
    SUM_URL="$GH_SUM_URL"
  fi

  # ── Verify checksum ───────────────────────────────────────────────

  if curl -sSLf --max-time 15 --retry 2 -o "$TMP/SHA256SUMS.txt" "$SUM_URL" 2>/dev/null; then
    expected=$(cut -d' ' -f1 "$TMP/SHA256SUMS.txt")
    actual=$(sha256_file "$TMP/$ARCHIVE" || true)
    if [ -z "$actual" ]; then
      warn "No SHA-256 tool found (skipping verification)"
    elif [ "$expected" != "$actual" ]; then
      error "Checksum mismatch!"
      error "  expected: $expected"
      error "  got:      $actual"
      exit 1
    else
      ok "SHA-256 verified"
    fi
  else
    warn "Could not download checksum (skipping verification)"
  fi

  # ── Extract & install ─────────────────────────────────────────────

  info "Extracting ${ARCHIVE}..."
  tar -xzf "$TMP/$ARCHIVE" -C "$TMP"

  # The archive may contain just the binary, or binary in a subdirectory
  if [ -f "$TMP/${BINARY}" ]; then
    BIN_PATH="$TMP/${BINARY}"
  elif [ -f "$TMP/${BINARY}-v${VERSION_STR}-${TARGET}/${BINARY}" ]; then
    BIN_PATH="$TMP/${BINARY}-v${VERSION_STR}-${TARGET}/${BINARY}"
  else
    # Search for the binary in the extracted tree
    BIN_PATH=$(find "$TMP" -name "${BINARY}" -type f 2>/dev/null | head -1)
    if [ -z "$BIN_PATH" ]; then
      error "Binary '${BINARY}' not found in archive"
      info "Archive contents:"
      find "$TMP" -type f | sed 's|'"$TMP"'||' | while read -r f; do printf "  %s\n" "$f"; done
      exit 1
    fi
  fi

  $SUDO install -m 755 "$BIN_PATH" "$INSTALL_DIR/${BINARY}"
  ok "Installed ${INSTALL_DIR}/${BINARY}"
fi

# -- Post-install -----------------------------------------------------------

if ! check_path "$INSTALL_DIR"; then
  print_path_hint "$INSTALL_DIR"
fi

printf '\n'
ok "${GREEN}${BOLD}${BINARY} v${VERSION_STR} installation complete!${NC}"
printf '\n'
info "Get started:"
info "  ${BLUE}${BINARY} --help${NC}"
printf '\n'
