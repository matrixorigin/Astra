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
└── README.md
```

## Key Scripts

### `scripts/dev/init.sh`
Initializes local development configuration, generating required local secrets in `.env`, and prepares the Rust-first workflow behind `make dev-init`.

### `scripts/setup/demo-init.sh`
Sets up a demo environment and performs prerequisite checks.

### Public CLI Installer
The published `astra` CLI installer is owned by the public `matrixorigin/astra-suite` repository:

```sh
curl -sSL https://raw.githubusercontent.com/matrixorigin/astra-suite/main/scripts/install.sh | sh
```

Keep installer behavior there so the public install path, documentation, and release assets stay in one repository.

### `scripts/ops/*.sh`
Operational helpers for health checks, backup/restore, and deployment.
