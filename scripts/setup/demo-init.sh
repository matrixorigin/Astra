#!/bin/bash
set +e

# Color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

print_header() {
    echo -e "\n${CYAN}╔════════════════════════════════════════════════════════╗${NC}"
    echo -e "${CYAN}║${NC} $1"
    echo -e "${CYAN}╚════════════════════════════════════════════════════════╝${NC}\n"
}

print_step() {
    echo -e "${BLUE}▶${NC} $1"
}

print_success() {
    echo -e "${GREEN}✓${NC} $1"
}

print_error() {
    echo -e "${RED}✗${NC} $1"
}

# Check services
check_services() {
    print_step "Checking services..."
    
    for i in 1 2 3 4 5; do
        if python3 -c "import pymysql; pymysql.connect(host='127.0.0.1', port=6001, user='root', password='111')" >/dev/null 2>&1; then
            print_success "MatrixOne database running"
            break
        fi
        [ $i -eq 5 ] && { print_error "MatrixOne not responding"; exit 1; }
        sleep 1
    done
    
    if pgrep -f "python -m uvicorn api.main:app" >/dev/null 2>&1; then
        print_success "API server running"
    else
        print_error "API server not running"
        exit 1
    fi
    echo ""
}

# Register user - returns 0=success, 1=username taken, 2=password invalid, 3=email taken
register_user() {
    local username=$1
    local password=$2
    local email=$3
    
    output=$(NO_PROXY=localhost mo-agent register --username "$username" --password "$password" --email "$email" 2>&1)
    
    if [[ "$output" == *"Registered as"* ]]; then
        return 0
    elif [[ "$output" == *"already taken"* ]]; then
        return 1
    elif [[ "$output" == *"already registered"* ]]; then
        return 3
    elif [[ "$output" == *"at least 8 characters"* ]]; then
        return 2
    else
        return 4
    fi
}

# Create demo user with proper retry logic
create_demo_user() {
    while true; do
        echo ""
        echo -e "${BLUE}Demo User${NC}"
        read -p "  Username (default: demo): " username
        username=${username:-demo}
        
        while true; do
            read -sp "  Password (default: password): " password
            echo ""
            password=${password:-password}
            
            while true; do
                read -p "  Email: " email
                [ -z "$email" ] && { print_error "Email required"; continue; }
                
                print_step "Creating user: $username..."
                register_user "$username" "$password" "$email"
                result=$?
                
                case $result in
                    0)
                        print_success "User created"
                        echo "$username" "$password"
                        return 0
                        ;;
                    1)
                        print_error "Username '$username' is already taken"
                        echo ""
                        echo "Try a different username"
                        break 2
                        ;;
                    2)
                        print_error "Password must be at least 8 characters"
                        echo ""
                        echo "Enter a longer password"
                        break
                        ;;
                    3)
                        print_error "Email '$email' is already registered"
                        echo ""
                        echo "Try a different email"
                        continue
                        ;;
                    *)
                        print_error "Registration failed"
                        return 1
                        ;;
                esac
            done
        done
    done
}

# Menu
show_menu() {
    print_header "🚀 mo-agent-engine Demo Setup"
    
    # Show login status
    login_info=$(NO_PROXY=localhost mo-admin whoami 2>/dev/null)
    if [ $? -eq 0 ]; then
        username=$(echo "$login_info" | grep "Username:" | cut -d: -f2 | xargs)
        # Check if user has admin role
        if NO_PROXY=localhost mo-admin model list >/dev/null 2>&1; then
            echo -e "${GREEN}● Logged in:${NC} $username ${CYAN}(admin)${NC}"
        else
            echo -e "${GREEN}● Logged in:${NC} $username"
        fi
    else
        echo -e "${RED}○ Not logged in${NC}"
    fi
    echo ""
    
    echo "What would you like to do?"
    echo ""
    echo "  1) Login"
    echo "  2) Create demo user (for chatting)"
    echo "  3) Create first admin user"
    echo "  4) Configure models"
    echo "  5) Exit"
    echo ""
    read -p "Choose (1-5): " choice
}

# Demo user only
demo_user_only() {
    local username password
    
    while true; do
        echo ""
        echo -e "${BLUE}Demo User${NC}"
        read -p "  Username (default: demo): " username
        username=${username:-demo}
        
        while true; do
            read -sp "  Password (default: password): " password
            echo ""
            password=${password:-password}
            
            while true; do
                read -p "  Email: " email
                [ -z "$email" ] && { print_error "Email required"; continue; }
                
                print_step "Creating user: $username..."
                register_user "$username" "$password" "$email"
                result=$?
                
                case $result in
                    0)
                        print_success "User created"
                        
                        print_step "Logging in..."
                        if NO_PROXY=localhost mo-agent login --username "$username" --password "$password" 2>/dev/null >/dev/null; then
                            print_success "Logged in"
                            echo ""
                            print_success "Ready to chat: NO_PROXY=localhost mo-agent chat"
                            return 0
                        else
                            print_error "Login failed"
                            return 1
                        fi
                        ;;
                    1)
                        print_error "Username '$username' is already taken"
                        echo ""
                        echo "Try a different username"
                        break 2
                        ;;
                    2)
                        print_error "Password must be at least 8 characters"
                        echo ""
                        echo "Enter a longer password"
                        break
                        ;;
                    3)
                        print_error "Email '$email' is already registered"
                        echo ""
                        echo "Try a different email"
                        continue
                        ;;
                    *)
                        print_error "Registration failed"
                        return 1
                        ;;
                esac
            done
        done
    done
}

# Login function
login_user() {
    echo ""
    print_step "Login"
    echo ""
    
    # Show existing profiles with roles
    creds_file="$HOME/.mo-agent/credentials.json"
    if [ -f "$creds_file" ]; then
        echo "Saved profiles:"
        python3 -c "
import json
import subprocess
import os

try:
    with open('$creds_file') as f:
        data = json.load(f)
    profiles = data.get('profiles', {})
    current = data.get('current_profile', '')
    
    if profiles:
        for name in profiles:
            marker = '*' if name == current else ' '
            
            # Check if this profile has admin role
            env = os.environ.copy()
            env['NO_PROXY'] = 'localhost'
            result = subprocess.run(
                ['mo-admin', '--profile', name, 'model', 'list'],
                capture_output=True,
                env=env,
                timeout=2
            )
            role = '(admin)' if result.returncode == 0 else ''
            
            print(f'  {marker} {name} {role}')
except Exception as e:
    # Fallback: just show names
    try:
        with open('$creds_file') as f:
            data = json.load(f)
        profiles = data.get('profiles', {})
        current = data.get('current_profile', '')
        if profiles:
            for name in profiles:
                marker = '*' if name == current else ' '
                print(f'  {marker} {name}')
    except:
        pass
" 2>/dev/null
        echo ""
        
        # Get current profile separately
        current_profile=$(python3 -c "
import json
try:
    with open('$creds_file') as f:
        data = json.load(f)
    print(data.get('current_profile', ''), end='')
except:
    pass
" 2>/dev/null)
    fi
    
    if [ -n "$current_profile" ]; then
        read -p "Select profile (default: $current_profile): " username
        username=${username:-$current_profile}
    else
        read -p "Username: " username
    fi
    
    if [ -z "$username" ]; then
        print_error "Username required"
        read -p "Press Enter to continue..."
        return
    fi
    
    # Try to use existing token first
    print_step "Checking saved credentials for $username..."
    if NO_PROXY=localhost mo-admin --profile "$username" whoami >/dev/null 2>&1; then
        print_success "Already logged in as $username (using saved token)"
        print_success "Profile updated: $username"
    else
        echo ""
        print_step "Saved token invalid or expired, please login again"
        read -sp "Password: " password
        echo ""
        
        print_step "Logging in as $username..."
        if NO_PROXY=localhost mo-admin login --username "$username" --password "$password" >/dev/null 2>&1; then
            print_success "Logged in as $username"
            print_success "Profile updated: $username"
        else
            print_error "Login failed (check username/password)"
        fi
    fi
    
    echo ""
    read -p "Press Enter to continue..."
}

# Register model only
model_only() {
    echo ""
    print_error "Model registration requires admin access"
    echo ""
    echo "Steps to register models:"
    echo "  1. Create admin user (choose option 2 from menu)"
    echo "  2. Login as admin: NO_PROXY=localhost mo-admin login"
    echo "  3. Add model: NO_PROXY=localhost mo-admin model add <name> <provider> --scope global"
    echo ""
    echo "Or configure models in .env file (recommended for demo):"
    echo "  • Set OPENAI_API_KEY, ANTHROPIC_API_KEY, etc."
    echo "  • Models are auto-registered from environment"
    echo ""
    read -p "Press Enter to continue..."
}

# Configure models interactively
configure_models() {
    echo ""
    print_step "Configure Models"
    echo ""
    echo "Choose configuration method:"
    echo "  1) Add API keys to .env file (recommended)"
    echo "  2) Register models via CLI (requires admin login)"
    echo "  3) Back to menu"
    echo ""
    read -p "Choose (1-3): " method
    
    case $method in
        1)
            echo ""
            echo "Add these lines to your .env file:"
            echo ""
            echo "  OPENAI_API_KEY=sk-xxx"
            echo "  ANTHROPIC_API_KEY=sk-ant-xxx"
            echo "  DEEPSEEK_API_KEY=sk-xxx"
            echo ""
            echo "Then restart API:"
            echo "  make dev-api-restart"
            echo ""
            read -p "Press Enter to continue..."
            ;;
        2)
            # Check if logged in
            if ! NO_PROXY=localhost mo-admin whoami >/dev/null 2>&1; then
                print_error "Not logged in as admin"
                echo ""
                echo "Please login first (choose option 3)"
                echo ""
                read -p "Press Enter to continue..."
                return
            fi
            
            echo ""
            read -p "Model name (e.g., gpt-4o): " model_name
            read -p "Provider (e.g., openai): " provider
            
            if [ -z "$model_name" ] || [ -z "$provider" ]; then
                print_error "Model name and provider are required"
                read -p "Press Enter to continue..."
                return
            fi
            
            # Ask for API token
            echo ""
            read -sp "API token for $provider: " api_token
            echo ""
            
            if [ -z "$api_token" ]; then
                print_error "API token is required"
                read -p "Press Enter to continue..."
                return
            fi
            
            # Create token first
            echo ""
            print_step "Storing API token..."
            if echo "$api_token" | NO_PROXY=localhost mo-admin token create --type llm --provider "$provider" --scope global 2>&1 | grep -qi "created\|token"; then
                print_success "API token stored"
            else
                print_error "Failed to store API token"
                read -p "Press Enter to continue..."
                return
            fi
            
            # Then register model
            print_step "Registering model..."
            if NO_PROXY=localhost mo-admin model add "$model_name" "$provider" --scope global 2>&1 | grep -qi "registered"; then
                print_success "Model registered: $model_name ($provider)"
            else
                print_error "Failed to register model"
            fi
            echo ""
            read -p "Press Enter to continue..."
            ;;
        3)
            # Back to menu
            return
            ;;
        4)
            return
            ;;
        *)
            print_error "Invalid choice"
            read -p "Press Enter to continue..."
            ;;
    esac
}

# Create admin user
# Create admin user (first user only)
create_admin() {
    echo ""
    print_step "Creating first admin user..."
    echo ""
    
    # Check if this will be the first user
    user_count=$(python3 -c "from api.database import get_db_session; from sqlalchemy import text; \
                 db = next(get_db_session()); \
                 count = db.execute(text('SELECT COUNT(*) FROM users')).fetchone()[0]; \
                 print(count); db.close()" 2>&1 | tail -1)
    
    if [ "$user_count" -gt 0 ]; then
        print_error "Admin user already exists"
        echo ""
        echo "To create additional admin users:"
        echo "  1. Login as existing admin: NO_PROXY=localhost mo-admin login"
        echo "  2. Register new user: NO_PROXY=localhost mo-admin register"
        echo "  3. Grant admin role: NO_PROXY=localhost mo-admin user grant-role <username> mo_agent_admin"
        echo ""
        read -p "Press Enter to continue..."
        return 1
    fi
    
    # Register first user
    read -p "Username (default: admin): " username
    username=${username:-admin}
    
    while true; do
        read -sp "Password (min 8 chars): " password
        echo ""
        [ ${#password} -ge 8 ] && break
        print_error "Password must be at least 8 characters"
    done
    
    read -p "Email: " email
    [ -z "$email" ] && email="${username}@admin.local"
    
    print_step "Registering first user..."
    output=$(NO_PROXY=localhost mo-admin register --username "$username" --password "$password" --email "$email" 2>&1)
    
    if echo "$output" | grep -qi "registered"; then
        print_success "User created: $username"
        print_success "First user automatically granted admin role"
        
        echo ""
        print_step "Logging in as $username..."
        if NO_PROXY=localhost mo-admin login --username "$username" --password "$password" 2>/dev/null >/dev/null; then
            print_success "Logged in as $username"
            echo ""
            echo "Next steps:"
            echo "  • Configure models: Choose option 3 from menu"
        else
            print_error "Auto-login failed"
            echo "Login manually: NO_PROXY=localhost mo-admin login --username $username"
        fi
        echo ""
        read -p "Press Enter to continue..."
        return 0
    else
        print_error "Failed to create user"
        echo ""
        echo "Error details:"
        echo "$output"
        echo ""
        read -p "Press Enter to continue..."
        return 1
    fi
}

# Complete setup
complete_setup() {
    print_header "Complete Setup"
    
    # Step 1: Create demo user (reuse demo_user_only which handles everything)
    print_step "Step 1/1: Creating demo user and logging in..."
    echo ""
    
    if demo_user_only; then
        echo ""
        print_success "Setup complete!"
        echo ""
        echo "Next steps:"
        echo "  • Configure models in .env (OPENAI_API_KEY, etc.)"
        echo "  • Or register via admin: NO_PROXY=localhost mo-admin model add gpt-4o openai --scope global"
        echo "  • Start chatting: NO_PROXY=localhost mo-agent chat"
    else
        print_error "Setup failed"
        return 1
    fi
}

# Main
check_services

while true; do
    show_menu
    
    case $choice in
        1) login_user ;;
        2) demo_user_only ;;
        3) create_admin ;;
        4) configure_models ;;
        5) echo "Goodbye!"; exit 0 ;;
        *) print_error "Invalid choice" ;;
    esac
done
