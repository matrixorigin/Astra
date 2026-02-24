# CLI Commands Reference

Complete reference for mo-agent and mo-admin command-line interfaces.

## mo-agent (User CLI)

User-facing CLI for interacting with agents, managing sessions, and running conversations.

### Installation

```bash
# Install from source
pip install -e .

# Verify installation
mo-agent --help
```

### Global Options

```bash
mo-agent [OPTIONS] COMMAND [ARGS]

Options:
  --help     Show help message
  --version  Show version
```

### Commands

#### chat

Start interactive chat session.

```bash
mo-agent chat [OPTIONS]

Options:
  --user-id TEXT     User ID (default: current user)
  --agent-id TEXT    Agent ID to use
  --session-id TEXT  Resume existing session
  --model TEXT       Override default model

Examples:
  mo-agent chat
  mo-agent chat --user-id alice
  mo-agent chat --agent-id dev-agent
  mo-agent chat --session-id abc123 --user-id alice
```

#### model

Manage and view available models.

```bash
# List all models
mo-agent model list

# Show model details
mo-agent model show <model_name>

# Examples
mo-agent model list
mo-agent model show gpt-4
mo-agent model show claude-3-opus
```

#### skill

Manage skills (tools/capabilities).

```bash
# List all skills
mo-agent skill list

# Show skill details
mo-agent skill show <skill_id>

# Register new skill
mo-agent skill register <skill_file.json>

# Examples
mo-agent skill list
mo-agent skill show code_search
mo-agent skill register my_skill.json
```

#### session

Manage conversation sessions.

```bash
# List sessions
mo-agent session list [OPTIONS]

Options:
  --user-id TEXT     Filter by user
  --agent-id TEXT    Filter by agent
  --limit INTEGER    Max results (default: 10)

# Show session details
mo-agent session show <session_id>

# Close session
mo-agent session close <session_id>

# Delete session
mo-agent session delete <session_id>

# Examples
mo-agent session list
mo-agent session list --user-id alice --limit 20
mo-agent session show abc123
mo-agent session close abc123
```

#### replay

Replay a conversation session.

```bash
mo-agent replay <session_id> [OPTIONS]

Options:
  --sandbox TEXT     Run in sandbox (default: creates temp sandbox)
  --compare          Compare with original results
  --verbose          Show detailed output

Examples:
  mo-agent replay abc123
  mo-agent replay abc123 --sandbox test_env
  mo-agent replay abc123 --compare
```

#### health

Check system health.

```bash
mo-agent health

# Output:
# ✅ API Server: healthy
# ✅ Database: connected
# ✅ Redis: connected
# ✅ LLM Provider: configured
```

---

## mo-admin (Admin CLI)

Administrative CLI for system initialization, model management, and user administration.

### Installation

```bash
# Install from source
pip install -e .

# Verify installation
mo-admin --help
```

### Global Options

```bash
mo-admin [OPTIONS] COMMAND [ARGS]

Options:
  --help     Show help message
  --version  Show version
```

### Commands

#### init

Initialize the system (database, default models, etc.).

```bash
mo-admin init [OPTIONS]

Options:
  --force    Force re-initialization
  --skip-db  Skip database initialization

Examples:
  mo-admin init
  mo-admin init --force
```

#### model

Manage system-wide models.

```bash
# Add model
mo-admin model add <model_name> <provider> [OPTIONS]

Options:
  --scope TEXT       Scope: global, account, user (default: global)
  --scope-id TEXT    Scope ID (required for account/user scope)
  --config TEXT      JSON config string

# List models
mo-admin model list [OPTIONS]

Options:
  --scope TEXT       Filter by scope
  --provider TEXT    Filter by provider

# Remove model
mo-admin model remove <model_name> [OPTIONS]

Options:
  --scope TEXT       Scope: global, account, user
  --scope-id TEXT    Scope ID

# Examples
mo-admin model add gpt-4 openai --scope global
mo-admin model add claude-3 anthropic --scope account --scope-id acme
mo-admin model list
mo-admin model list --provider openai
mo-admin model remove gpt-4 --scope global
```

#### token

Manage API tokens (LLM providers, GitHub, etc.).

```bash
# Create token
mo-admin token create [OPTIONS]

Options:
  --type TEXT        Token type: llm, github, etc.
  --provider TEXT    Provider name (for LLM tokens)
  --scope TEXT       Scope: global, account, user
  --scope-id TEXT    Scope ID
  --value TEXT       Token value (prompted if not provided)

# List tokens
mo-admin token list [OPTIONS]

Options:
  --type TEXT        Filter by type
  --scope TEXT       Filter by scope

# Revoke token
mo-admin token revoke <token_id>

# Examples
mo-admin token create --type llm --provider openai --scope global
mo-admin token create --type github --scope user --scope-id alice
mo-admin token list
mo-admin token list --type llm
mo-admin token revoke abc123
```

#### user

Manage users.

```bash
# Create user
mo-admin user create <username> [OPTIONS]

Options:
  --email TEXT       User email
  --password TEXT    User password (prompted if not provided)
  --admin            Make user admin

# List users
mo-admin user list

# Show user details
mo-admin user show <username>

# Update user
mo-admin user update <username> [OPTIONS]

Options:
  --email TEXT       New email
  --password TEXT    New password
  --admin BOOLEAN    Admin status

# Delete user
mo-admin user delete <username>

# Examples
mo-admin user create alice --email alice@example.com
mo-admin user create bob --admin
mo-admin user list
mo-admin user show alice
mo-admin user update alice --admin true
mo-admin user delete bob
```

#### audit

View audit logs.

```bash
mo-admin audit logs [OPTIONS]

Options:
  --user TEXT        Filter by user
  --action TEXT      Filter by action
  --since TEXT       Start date (YYYY-MM-DD)
  --until TEXT       End date (YYYY-MM-DD)
  --limit INTEGER    Max results (default: 100)

# Examples
mo-admin audit logs
mo-admin audit logs --user alice
mo-admin audit logs --action login --since 2026-02-01
mo-admin audit logs --user alice --since 2026-02-01 --limit 50
```

#### config

Manage system configuration.

```bash
# Show configuration
mo-admin config show

# Set configuration value
mo-admin config set <key> <value>

# Get configuration value
mo-admin config get <key>

# Examples
mo-admin config show
mo-admin config set max_session_length 100
mo-admin config get max_session_length
```

---

## Configuration Files

### User Configuration

Location: `~/.mo-agent/config.yaml`

```yaml
# Default user ID
default_user_id: alice

# Default agent
default_agent_id: dev-agent

# API endpoint
api_url: http://localhost:8000

# Authentication
auth_token: <your_token>
```

### Admin Configuration

Location: `~/.mo-admin/config.yaml`

```yaml
# API endpoint
api_url: http://localhost:8000

# Admin credentials
admin_token: <admin_token>

# Database connection
database_url: mysql://root:111@localhost:6001/mo_agent
```

---

## Environment Variables

Both CLIs respect these environment variables:

```bash
# API endpoint
export MO_AGENT_API_URL=http://localhost:8000

# Authentication token
export MO_AGENT_TOKEN=<your_token>

# Default user ID
export MO_AGENT_USER_ID=alice

# Log level
export MO_AGENT_LOG_LEVEL=info
```

---

## Examples

### User Workflow

```bash
# 1. Start chat
mo-agent chat --user-id alice

# 2. List sessions
mo-agent session list --user-id alice

# 3. Replay a session
mo-agent replay abc123

# 4. Check health
mo-agent health
```

### Admin Workflow

```bash
# 1. Initialize system
mo-admin init

# 2. Add models
mo-admin model add gpt-4 openai --scope global
mo-admin model add claude-3 anthropic --scope global

# 3. Create users
mo-admin user create alice --email alice@example.com
mo-admin user create bob --admin

# 4. Add tokens
mo-admin token create --type llm --provider openai --scope global

# 5. View audit logs
mo-admin audit logs --since 2026-02-01
```

---

## Troubleshooting

### CLI Not Found

```bash
# Ensure package is installed
pip install -e .

# Or add to PATH
export PATH=$PATH:~/.local/bin
```

### Connection Errors

```bash
# Check API server is running
curl http://localhost:8000/health

# Set correct API URL
export MO_AGENT_API_URL=http://localhost:8000
```

### Authentication Errors

```bash
# Login to get token
curl -X POST http://localhost:8000/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","password":"secret123"}'

# Set token
export MO_AGENT_TOKEN=<your_token>
```

---

## See Also

- [API Reference](api-reference.md) - REST API documentation
- [Configuration Reference](configuration.md) - Environment variables
- [Development Workflow](../guides/development-workflow.md) - Development guide
