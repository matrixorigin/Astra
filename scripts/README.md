# Scripts Directory

Current scripts are shell-based and organized by responsibility.

## Layout

```text
scripts/
├── dev/
│   ├── init.sh
│   ├── start-api.sh
│   └── stop-api.sh
├── setup/
│   └── demo-init.sh
├── ops/
│   ├── backup.sh
│   ├── deploy.sh
│   ├── health_check.sh
│   └── restore.sh
└── install.sh
```

## Key Scripts

### `scripts/dev/init.sh`
Initializes local development configuration, generating required local secrets in `.env`, and prepares the Rust-first workflow behind `make dev-init`.

### `scripts/setup/demo-init.sh`
Sets up a demo environment and performs prerequisite checks.

### `scripts/install.sh`
Installs the published CLI package. This path still requires Python 3.11+ because it installs the packaged CLI for end users, not because the repository's server runtime is Python-based.

### `scripts/ops/*.sh`
Operational helpers for health checks, backup/restore, and deployment.
