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
    echo "What would you like to do?"
    echo ""
    echo "  1) Complete setup (admin + token + model + demo user)"
    echo "  2) Create demo user only"
    echo "  3) Register model only"
    echo "  4) Exit"
    echo ""
    read -p "Choose (1-4): " choice
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

# Register model only
model_only() {
    local model_name provider
    
    echo ""
    echo -e "${BLUE}Register Model${NC}"
    read -p "  Model name (e.g., gpt-4o): " model_name
    read -p "  Provider (e.g., openai): " provider
    
    if [ -z "$model_name" ] || [ -z "$provider" ]; then
        print_error "Model name and provider are required"
        return 1
    fi
    
    echo ""
    echo "Registering model..."
    
    if NO_PROXY=localhost mo-admin model add "$model_name" "$provider" --scope global 2>&1; then
        print_success "Model registered: $model_name ($provider)"
    else
        print_error "Failed to register model"
        return 1
    fi
}

# Complete setup
complete_setup() {
    echo ""
    print_error "Complete setup not yet fully implemented"
    echo ""
    echo "For now, use option 2 to create demo user"
    echo "Configure models in .env file"
}

# Main
check_services
show_menu

case $choice in
    1) complete_setup ;;
    2) demo_user_only ;;
    3) model_only ;;
    4) echo "Goodbye!"; exit 0 ;;
    *) print_error "Invalid choice"; exit 1 ;;
esac
