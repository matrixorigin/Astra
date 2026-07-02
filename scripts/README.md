# Scripts Directory

Scripts are organized by responsibility and kept dependency-light.

## Layout

```text
scripts/
├── dev/
│   ├── init.sh
│   ├── start-api.sh
│   └── stop-api.sh
├── load/
│   └── multi_cli_capacity_probe.py
├── schema/
│   └── schema_inventory.py
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

### `scripts/load/multi_cli_capacity_probe.py`
Runs a stdlib-only concurrent `POST /chat/stream` SSE capacity probe for the
100 CLI / 500 CLI rollout model. The probe records per-request JSONL, samples
`/metrics`, and writes a summary under `tmp/capacity-probe/`.

Use `--auth-token` or `--token-file` for existing users. Use `--register-users`
only against a disposable environment; distinct user testing requires distinct
real access tokens because the runtime derives ownership from auth, not from a
request body field.

### `scripts/schema/schema_inventory.py`
Builds a stdlib-only inventory of static production `CREATE TABLE IF NOT EXISTS`
DDL across current schema owners, not just `storage.rs`. It reports owner,
source line, column count, primary keys, index count, AUTO_INCREMENT columns,
duplicate table names, FK usage, and explicit semantic metadata for audited
tables. AUTO_INCREMENT tables also include write-profile, owner-boundary,
hotspot-risk, and replacement-guidance fields.

Useful checks:

```sh
python3 scripts/schema/schema_inventory.py --fail-on-duplicates --fail-on-foreign-keys --output tmp/schema-inventory.json
python3 scripts/schema/test_schema_inventory.py
```

### Public CLI Installer
The published `astra` CLI installer is owned by the public `matrixorigin/astra-suite` repository:

```sh
curl -sSL https://raw.githubusercontent.com/matrixorigin/astra-suite/main/scripts/install-astra.sh | sh
```

Keep installer behavior there so the public install path, documentation, and release assets stay in one repository.

### `scripts/ops/*.sh`
Operational helpers for health checks, backup/restore, and deployment.
