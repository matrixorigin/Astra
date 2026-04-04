#!/usr/bin/env bash
# mo-agent installer — installs via pipx, uv, or pip
set -euo pipefail

PACKAGE="astra-engine"
MIN_PYTHON="3.11"
BOLD='\033[1m'
GREEN='\033[0;32m'
RED='\033[0;31m'
DIM='\033[2m'
NC='\033[0m'

info()  { echo -e "${BOLD}$*${NC}"; }
ok()    { echo -e "${GREEN}✓${NC} $*"; }
fail()  { echo -e "${RED}✗${NC} $*"; exit 1; }

# --- Check Python ---
check_python() {
    for cmd in python3.12 python3.11 python3 python; do
        if command -v "$cmd" &>/dev/null; then
            local ver
            ver=$("$cmd" -c "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')")
            if "$cmd" -c "import sys; exit(0 if sys.version_info >= (3,11) else 1)" 2>/dev/null; then
                PYTHON="$cmd"
                ok "Python $ver ($cmd)"
                return 0
            fi
        fi
    done
    fail "Python >= $MIN_PYTHON required. Install from https://python.org"
}

# --- Install ---
install() {
    # Try pipx first
    if command -v pipx &>/dev/null; then
        info "Installing via pipx..."
        pipx install "$PACKAGE" && { ok "Installed via pipx"; return 0; } || true
    fi

    # Try uv
    if command -v uv &>/dev/null; then
        info "Installing via uv..."
        uv tool install "$PACKAGE" && { ok "Installed via uv"; return 0; } || true
    fi

    # Fallback to pip --user
    info "Installing via pip..."
    "$PYTHON" -m pip install --user "$PACKAGE" || fail "pip install failed"
    ok "Installed via pip"
}

# --- PATH check ---
check_path() {
    local bin_dir="$HOME/.local/bin"
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$bin_dir"; then
        echo ""
        info "Add to your shell profile:"
        echo -e "  ${DIM}export PATH=\"\$HOME/.local/bin:\$PATH\"${NC}"
        echo ""
    fi
}

# --- Main ---
echo ""
info "✦ mo-agent installer"
echo ""

check_python
install
check_path

echo ""
echo -e "${GREEN}${BOLD}✓ Installation complete!${NC}"
echo ""
echo "  Get started:"
echo -e "    ${BOLD}astra login${NC}"
echo -e "    ${BOLD}astra chat${NC}"
echo ""
