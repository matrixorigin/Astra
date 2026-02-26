# Scripts Directory

Organized scripts for development, setup, operations, and security.

## Directory Structure

```
scripts/
├── dev/                    # 🔨 Development scripts
│   ├── init.py            # Environment initialization
│   ├── start-api.sh       # Start API server
│   ├── stop-api.sh        # Stop API server
│   └── cleanup_test_dbs.py # Test database cleanup
│
├── setup/                  # 🏗️ Initialization scripts
│   └── init_prompts.py    # Prompt initialization
│
├── ops/                    # 🚀 Operations scripts
│   ├── health_check.sh    # Health check
│   ├── backup.sh          # Backup
│   └── restore.sh         # Restore
│
└── security/               # 🔒 Security scripts
    ├── check_security.py  # Security configuration check
    └── rotate_keys.py     # Key rotation
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

---

## Setup Scripts (setup/)

### init_prompts.py

Initialize system prompts.

**Usage:**
```bash
python scripts/setup/init_prompts.py
```

**Note:** Database tables are created automatically by `init_db()` in `api/database.py` via SQLAlchemy `Base.metadata.create_all()`. No manual SQL scripts needed.

---

## Operations Scripts (ops/)

### health_check.sh / backup.sh / restore.sh

Production operations scripts. See individual files for usage.

---

## Security Scripts (security/)

### check_security.py

Check security configuration before deployment.

**Usage:**
```bash
python scripts/security/check_security.py
```

### rotate_keys.py

Rotate encryption keys.

---

## Usage Examples

### Development Workflow

```bash
# 1. Initialize environment
python scripts/dev/init.py

# 2. Start development (tables auto-created on API startup)
make dev-start
```

### Pre-Deployment Checks

```bash
python scripts/security/check_security.py
```
