#!/bin/bash
# Smart interactive demo setup for mo-agent-engine.
#
# Design principles:
#   1. Detect current state — skip what's already done
#   2. Guide linearly — no menu loops, walk through steps in order
#   3. Sensible defaults — minimize typing
#   4. Idempotent — safe to re-run at any point
#   5. Fail fast with clear remediation
set -euo pipefail

# ── Colours & output helpers ────────────────────────────────────

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'
BLUE='\033[0;34m'; CYAN='\033[0;36m'; DIM='\033[2m'; BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${BLUE}▸${NC} $*"; }
ok()    { echo -e "${GREEN}✓${NC} $*"; }
warn()  { echo -e "${YELLOW}!${NC} $*"; }
err()   { echo -e "${RED}✗${NC} $*"; }
dim()   { echo -e "${DIM}$*${NC}"; }
header(){ echo -e "\n${BOLD}${CYAN}$*${NC}\n"; }

ask() {
    # ask "prompt" "default" → sets REPLY
    local prompt=$1 default=${2:-}
    if [ -n "$default" ]; then
        read -rp "  $prompt [${default}]: " REPLY
        REPLY=${REPLY:-$default}
    else
        read -rp "  $prompt: " REPLY
    fi
}

ask_secret() {
    local prompt=$1 default=${2:-}
    if [ -n "$default" ]; then
        read -rsp "  $prompt [********]: " REPLY; echo
        REPLY=${REPLY:-$default}
    else
        read -rsp "  $prompt: " REPLY; echo
    fi
}

confirm() {
    # confirm "question" → returns 0/1
    local prompt=$1
    read -rp "  $prompt [Y/n]: " REPLY
    [[ -z "$REPLY" || "$REPLY" =~ ^[Yy] ]]
}

# ── Proxy helper ─────────────────────────────────────────────────
# CLI tools (mo-agent, mo-admin) now auto-bypass proxy for localhost.
# Only curl still needs NO_PROXY.
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}localhost,127.0.0.1"

# ── State detection helpers ─────────────────────────────────────

CREDS_FILE="$HOME/.mo-agent/credentials.json"

db_reachable() {
    python3 -c "
import pymysql
pymysql.connect(host='127.0.0.1', port=6001, user='root', password='111')
" >/dev/null 2>&1
}

api_reachable() {
    curl -sf http://localhost:8000/health >/dev/null 2>&1
}

_db_query() {
    # Run a single-expression DB query, suppress matrixone client logs
    python3 -c "
import sys, os, logging
os.environ['MO_SQL_LOG_MODE'] = 'off'
for _n in ['matrixone','matrixone.client']:
    _l = logging.getLogger(_n); _l.setLevel(99); _l.propagate = False; _l.handlers = [logging.NullHandler()]
sys.path.insert(0,'.')
from api.database import get_db_session
from sqlalchemy import text
db = next(get_db_session())
$1
db.close()
" 2>/dev/null || echo "${2:-0}"
}

user_count() {
    _db_query "print(db.execute(text('SELECT COUNT(*) FROM users')).fetchone()[0])"
}

has_admin() {
    _db_query "
n = db.execute(text(
    \"SELECT COUNT(*) FROM user_roles ur JOIN roles r ON ur.role_id = r.role_id \"
    \"WHERE r.role_name = 'mo_agent_admin'\"
)).fetchone()[0]
print(n)
"
}

model_count() {
    _db_query "
try:
    n = db.execute(text('SELECT COUNT(*) FROM llm_models WHERE is_active=1')).fetchone()[0]
    print(n)
except:
    print(0)
"
}

has_llm_token() {
    # Check if any LLM API key is configured in .env (non-placeholder)
    grep -qE '^(OPENAI_API_KEY|ANTHROPIC_API_KEY|DEEPSEEK_API_KEY)=.{8,}' .env 2>/dev/null
}

profile_logged_in() {
    # Check if a profile's saved token is still valid
    local profile=$1
    mo-agent --profile "$profile" whoami >/dev/null 2>&1
}

current_profile() {
    python3 -c "
import json, pathlib
p = pathlib.Path('$CREDS_FILE')
if p.exists():
    d = json.loads(p.read_text())
    print(d.get('current_profile',''))
" 2>/dev/null
}

saved_profiles() {
    python3 -c "
import json, pathlib
p = pathlib.Path('$CREDS_FILE')
if p.exists():
    d = json.loads(p.read_text())
    for name in d.get('profiles', {}):
        print(name)
" 2>/dev/null
}

# ── Register + login helper (returns 0 on success) ─────────────

do_register() {
    local username=$1 password=$2 email=$3
    local out
    out=$(mo-agent register --username "$username" --password "$password" --email "$email" 2>&1)
    local rc=$?
    if [ $rc -eq 0 ] && echo "$out" | grep -qi "registered"; then
        return 0
    fi
    # Parse error
    if echo "$out" | grep -qi "already taken\|already exists"; then
        echo "USERNAME_TAKEN"; return 1
    elif echo "$out" | grep -qi "already registered"; then
        echo "EMAIL_TAKEN"; return 1
    elif echo "$out" | grep -qi "at least 8 characters"; then
        echo "PASSWORD_SHORT"; return 1
    fi
    echo "UNKNOWN: $out"; return 1
}

do_login() {
    local username=$1 password=$2 tool=${3:-mo-agent}
    $tool login --username "$username" --password "$password" >/dev/null 2>&1
}

# ── Step 0: Pre-flight checks ──────────────────────────────────

preflight() {
    header "🚀 mo-agent-engine — Interactive Setup"

    info "Checking prerequisites..."

    # Database
    if db_reachable; then
        ok "MatrixOne database reachable"
    else
        err "MatrixOne database not reachable (localhost:6001)"
        echo ""
        echo "  Start it with:  make dev-deps-up"
        echo "  Then re-run:    make dev-setup-demo"
        exit 1
    fi

    # API
    if api_reachable; then
        ok "API server reachable"
    else
        err "API server not reachable (localhost:8000)"
        echo ""
        echo "  Start it with:  make dev-start"
        echo "  Then re-run:    make dev-setup-demo"
        exit 1
    fi

    echo ""
}

# ── Step 1: Admin user ──────────────────────────────────────────

step_admin() {
    header "Step 1 · Admin User"

    local admin_count
    admin_count=$(has_admin)

    if [ "$admin_count" -gt 0 ]; then
        ok "Admin user already exists — skipping"
        dim "  (To manage admins: mo-admin user grant-role <user> mo_agent_admin)"
        return 0
    fi

    info "No admin user found. The first registered user gets admin role."
    echo ""

    local username password email
    ask "Admin username" "admin"
    username=$REPLY

    ask_secret "Password (min 8 chars)" "admin123"
    password=$REPLY
    while [ ${#password} -lt 8 ]; do
        warn "Password must be at least 8 characters"
        ask_secret "Password (min 8 chars)"
        password=$REPLY
    done

    ask "Email" "${username}@local.dev"
    email=$REPLY

    info "Creating admin user: $username"
    local result
    result=$(do_register "$username" "$password" "$email") || true

    case "$result" in
        USERNAME_TAKEN)
            warn "Username '$username' already exists — trying login instead"
            ;;
        EMAIL_TAKEN)
            warn "Email already registered — trying login instead"
            ;;
        PASSWORD_SHORT)
            err "Password too short"; return 1
            ;;
        UNKNOWN:*)
            err "Registration failed: ${result#UNKNOWN: }"; return 1
            ;;
        *)
            ok "Admin user created: $username"
            ;;
    esac

    info "Logging in as $username (admin profile)..."
    if do_login "$username" "$password" "mo-admin"; then
        ok "Logged in as admin"
    else
        warn "Auto-login failed — you can login later: mo-admin login"
    fi
}

# ── Step 2: Demo user (for chatting) ────────────────────────────

step_demo_user() {
    header "Step 2 · Chat User"

    # Check if already logged in with a valid profile
    local cur
    cur=$(current_profile)
    if [ -n "$cur" ] && profile_logged_in "$cur"; then
        ok "Already logged in as '$cur'"
        if ! confirm "Create a separate demo user?"; then
            return 0
        fi
    fi

    local total
    total=$(user_count)
    if [ "$total" -gt 1 ]; then
        info "$total users exist. You can login to an existing account or create a new one."
        if ! confirm "Create a new demo user?"; then
            # Offer login to existing
            ask "Username to login as" "$cur"
            local username=$REPLY
            if profile_logged_in "$username"; then
                ok "Already authenticated as $username"
                return 0
            fi
            ask_secret "Password"
            if do_login "$username" "$REPLY" "mo-agent"; then
                ok "Logged in as $username"
                return 0
            else
                err "Login failed"
                return 1
            fi
        fi
    fi

    echo ""
    local username password email
    ask "Username" "demo"
    username=$REPLY

    ask_secret "Password" "demo1234"
    password=$REPLY
    while [ ${#password} -lt 8 ]; do
        warn "Password must be at least 8 characters"
        ask_secret "Password"
        password=$REPLY
    done

    ask "Email" "${username}@local.dev"
    email=$REPLY

    info "Creating user: $username"
    local result
    result=$(do_register "$username" "$password" "$email") || true

    case "$result" in
        USERNAME_TAKEN)
            warn "Username '$username' already exists — trying login"
            ;;
        EMAIL_TAKEN)
            warn "Email already registered — trying login"
            ;;
        PASSWORD_SHORT)
            err "Password too short"; return 1
            ;;
        UNKNOWN:*)
            err "Registration failed: ${result#UNKNOWN: }"; return 1
            ;;
        *)
            ok "User created: $username"
            ;;
    esac

    info "Logging in..."
    if do_login "$username" "$password" "mo-agent"; then
        ok "Logged in as $username"
    else
        err "Login failed"
        return 1
    fi
}

# ── Step 3: Model configuration ────────────────────────────────

step_models() {
    header "Step 3 · Register LLM Model"

    # Check if any models exist
    local count
    count=$(_db_query "
try:
    n = db.execute(text('SELECT COUNT(*) FROM llm_models')).fetchone()[0]
    print(n)
except:
    print(0)
")

    if [ "$count" -gt 0 ]; then
        local active
        active=$(_db_query "print(db.execute(text('SELECT COUNT(*) FROM llm_models WHERE is_active=1')).fetchone()[0])")
        ok "$count model(s) registered ($active active)"
        echo ""
        _db_query "
rows = db.execute(text('SELECT model_name, provider, is_active FROM llm_models ORDER BY created_at')).fetchall()
print(f\"  {'Model':<30} {'Provider':<15} {'Status'}\")
print(f\"  {'-'*30} {'-'*15} {'-'*8}\")
for r in rows:
    status = '✓ active' if r[2] else '✗ inactive'
    print(f'  {r[0]:<30} {r[1]:<15} {status}')
"
        echo ""
        if confirm "Register another model?"; then
            : # fall through
        else
            return 0
        fi
    else
        info "No models registered yet. Let's add one so you can chat."
    fi

    local provider model_name base_url=""
    while true; do
        echo ""
        echo "  Supported providers:"
        echo "    1) OpenAI       (gpt-4o, gpt-4o-mini)"
        echo "    2) DeepSeek     (deepseek-chat)"
        echo "    3) Anthropic    (claude-3-5-sonnet)"
        echo "    4) Custom       (OpenAI-compatible endpoint)"
        echo "    5) Skip"
        echo ""
        read -rp "  Choose [1-5, default=2]: " provider_choice
        provider_choice=${provider_choice:-2}

        case "$provider_choice" in
            1|openai)    provider="openai";    model_name="gpt-4o"; break ;;
            2|deepseek)  provider="deepseek";  model_name="deepseek-chat"; break ;;
            3|anthropic) provider="anthropic"; model_name="claude-3-5-sonnet-20241022"; break ;;
            4|custom)
                ask "Provider name" "custom"
                provider=$REPLY
                ask "Model name"
                model_name=$REPLY
                ask "Base URL (OpenAI-compatible)"
                base_url=$REPLY
                break ;;
            5|skip) info "Skipping — register models later with: mo-admin model add"; return 0 ;;
            *) warn "Invalid choice '$provider_choice' — please enter 1-5" ;;
        esac
    done

    ask "Model name" "$model_name"
    model_name=$REPLY

    ask_secret "API key for $provider"
    local api_key=$REPLY

    if [ -z "$api_key" ]; then
        warn "Empty key — skipping"
        return 0
    fi

    # Ensure mo-admin is authenticated before registering
    if ! mo-admin model list >/dev/null 2>&1; then
        warn "Admin session expired or not logged in"
        info "Please login as admin to register models:"
        local admin_user admin_pass
        ask "Admin username" "admin"
        admin_user=$REPLY
        ask_secret "Admin password"
        admin_pass=$REPLY
        if ! do_login "$admin_user" "$admin_pass" "mo-admin"; then
            err "Admin login failed — register models later with: mo-admin login && mo-admin model add"
            return 1
        fi
        ok "Logged in as admin"
    fi

    info "Registering $model_name ($provider) and validating connectivity..."
    local output base_url_args=()
    [[ -n "$base_url" ]] && base_url_args=(--base-url "$base_url")
    output=$(mo-admin model add "$model_name" "$provider" --api-key "$api_key" "${base_url_args[@]}" 2>&1) || true
    echo "$output"

    if echo "$output" | grep -q "INACTIVE"; then
        warn "Model registered but connectivity failed — check your API key"
    fi
}

# ── Summary ─────────────────────────────────────────────────────

summary() {
    header "✅ Setup Complete"

    local cur
    cur=$(current_profile)

    echo "  Current profile:  ${cur:-<none>}"
    echo "  API server:       http://localhost:8000"
    echo "  API docs:         http://localhost:8000/docs"
    echo ""
    echo "  Quick start:"
    echo "    mo-agent chat                    # start chatting"
    echo "    mo-agent chat --model gpt-4o     # use specific model"
    echo "    mo-admin model list              # list available models"
    echo ""
}

# ── Main ────────────────────────────────────────────────────────

main() {
    preflight
    step_admin
    step_demo_user
    step_models
    summary
}

main "$@"
