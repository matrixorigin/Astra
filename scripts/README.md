# Scripts Directory

Organized scripts for development, setup, operations, and security.

## Directory Structure

```
scripts/
├── dev/                    # 🔨 Development scripts
│   ├── init.py            # Environment initialization
│   └── cleanup_test_dbs.py # Test database cleanup
│
├── setup/                  # 🏗️ Initialization scripts
│   ├── init_database.py   # Database initialization
│   ├── init_prompts.py    # Prompt initialization
│   └── sql/               # SQL initialization files
│       ├── init-agent-config.sql
│       └── init-rbac.sql
│
├── ops/                    # 🚀 Operations scripts
│   └── deploy.sh          # Production deployment
│
└── security/               # 🔒 Security scripts
    └── check_security.py  # Security configuration check
```

---

## Development Scripts (dev/)

### init.py

Initialize development environment.

**Usage:**
```bash
python scripts/dev/init.py
```

**What it does:**
- Generates `TOKEN_ENCRYPTION_KEY` if missing
- Fixes common configuration errors (e.g., `OPENAI_AKI_KEY` → `OPENAI_API_KEY`)
- Validates LLM provider/model compatibility
- Updates `.env` file automatically

**Called by:** `make dev-init`

### cleanup_test_dbs.py

Clean up test databases.

**Usage:**
```bash
python scripts/dev/cleanup_test_dbs.py
```

**What it does:**
- Removes test databases created during testing
- Cleans up temporary data

---

## Setup Scripts (setup/)

### init_database.py

Initialize database schema and default data.

**Usage:**
```bash
python scripts/setup/init_database.py
```

**What it does:**
- Creates database schema
- Loads initial configuration from SQL files
- Sets up RBAC (Role-Based Access Control)

**Called by:** `make db-init`

### init_prompts.py

Initialize system prompts.

**Usage:**
```bash
python scripts/setup/init_prompts.py
```

**What it does:**
- Loads default system prompts
- Initializes prompt templates

### SQL Files (setup/sql/)

**init-agent-config.sql:**
- Agent configuration tables
- Default agent settings

**init-rbac.sql:**
- Role-Based Access Control setup
- Default roles and permissions

---

## Operations Scripts (ops/)

### deploy.sh

Production deployment script.

**Usage:**
```bash
./scripts/ops/deploy.sh
```

**What it does:**
- Builds Docker images
- Deploys to production
- Runs health checks

**Environment variables:**
- `DEPLOY_ENV` - Deployment environment (staging, production)
- `DOCKER_REGISTRY` - Docker registry URL

---

## Security Scripts (security/)

### check_security.py

Check security configuration before deployment.

**Usage:**
```bash
python scripts/security/check_security.py
```

**What it checks:**
- ✅ Strong encryption keys (not default values)
- ✅ No default passwords
- ✅ HTTPS enabled in production
- ✅ CORS properly configured
- ✅ Rate limiting enabled
- ✅ API keys not in code
- ✅ Database access restricted

**Exit codes:**
- `0` - All checks passed
- `1` - Security issues found

**Called by:** Production deployment pipeline

---

## Usage Examples

### Development Workflow

```bash
# 1. Initialize environment
python scripts/dev/init.py

# 2. Setup database
python scripts/setup/init_database.py

# 3. Initialize prompts
python scripts/setup/init_prompts.py

# 4. Start development
make dev-start
```

### Pre-Deployment Checks

```bash
# 1. Run security check
python scripts/security/check_security.py

# 2. If passed, deploy
./scripts/ops/deploy.sh
```

### Cleanup

```bash
# Clean test databases
python scripts/dev/cleanup_test_dbs.py
```

---

## Adding New Scripts

### Guidelines

1. **Choose the right directory:**
   - `dev/` - Development and testing scripts
   - `setup/` - One-time initialization scripts
   - `ops/` - Production operations (deploy, backup, restore)
   - `security/` - Security checks and audits

2. **Make scripts executable:**
   ```bash
   chmod +x scripts/ops/new_script.sh
   ```

3. **Add shebang:**
   ```python
   #!/usr/bin/env python3
   ```
   ```bash
   #!/bin/bash
   ```

4. **Document in this README:**
   - Add script description
   - Document usage
   - List environment variables
   - Provide examples

5. **Add to Makefile if appropriate:**
   ```makefile
   .PHONY: my-command
   my-command:
       python scripts/dev/my_script.py
   ```

---

## Script Dependencies

### Python Scripts

All Python scripts require:
- Python 3.11+
- Project dependencies installed (`pip install -e .`)

### Shell Scripts

Shell scripts require:
- Bash 4.0+
- Docker and Docker Compose (for deployment scripts)

---

## Troubleshooting

### Script Not Found

```bash
# Ensure you're in project root
cd /path/to/mo-agent

# Run script with full path
python scripts/dev/init.py
```

### Permission Denied

```bash
# Make script executable
chmod +x scripts/ops/deploy.sh

# Run script
./scripts/ops/deploy.sh
```

### Import Errors

```bash
# Install dependencies
pip install -e .

# Verify installation
pip list | grep mo-agent
```

---

## See Also

- [Development Workflow](../docs/guides/development-workflow.md) - Development guide
- [Makefile Commands](../docs/reference/makefile-commands.md) - Available commands
- [Configuration](../docs/reference/configuration.md) - Environment variables
