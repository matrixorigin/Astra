# Scripts Directory

Scripts are organized by responsibility and kept dependency-light.

## Layout

```text
scripts/
├── ci/       # repository contracts and changed-path CI routing
├── dev/      # local API, Web, and Edge lifecycle helpers
├── e2e/      # capability and test-case manifest validators
├── harness/  # Terminal-Bench lifecycle and offline contract tests
├── load/     # capacity probes, mock provider, reports, and unit tests
├── ops/      # deployment, health, backup, and restore helpers
├── schema/   # schema inventory and its contract tests
├── setup/    # demo environment initialization
└── *.sh/*.py # release, production-baseline, and diagram utilities
```

## Offline Contract Tests

These checks need no service, database, model provider, or API credential. The
repository and routing checks always run; CI routes the remaining suites only
when their owning script area changes:

```sh
python3 scripts/ci/validate_repository.py
python3 scripts/ci/test_ci_scope.py
python3 -m unittest \
  scripts.harness.test_benchmark_model_seed \
  scripts.harness.test_case_history \
  scripts.harness.test_fresh_database_contract
bash scripts/harness/test_local_gateway_contract.sh
python3 -m unittest discover -s scripts/load -p 'test_*.py'
python3 scripts/schema/test_schema_inventory.py
python3 scripts/e2e/validate_capability_matrix.py
```

The repository validator covers local documentation links, JSON and shell
syntax, pinned GitHub Actions, mirrored agent instructions, accidental tracked
artifacts, executable script modes, and monitoring metric references.

The remaining Harness tests exercise Harbor integration contracts and require
the benchmark environment. Run the complete suite there with
`python3 -m unittest discover -s scripts/harness -p 'test_*.py'`.

## Key Scripts

### `scripts/harness/`

Owns the Terminal-Bench/Harbor benchmark lifecycle: preflight checks, fresh
database contracts, process supervision, sealed run snapshots, verifier
readiness, recovery metadata, and the current benchmark launcher. Its colocated
tests cover both dependency-free lifecycle contracts and Harbor integration.

Run the dependency-free harness tests without starting a benchmark:

```sh
python3 -m unittest \
  scripts.harness.test_benchmark_model_seed \
  scripts.harness.test_case_history \
  scripts.harness.test_fresh_database_contract
bash scripts/harness/test_local_gateway_contract.sh
```

See the [Terminal-Bench results](../README.md#terminal-bench-21) for the public
benchmark summary.

### `scripts/dev/init.sh`
Initializes local development configuration, generating required local secrets in `.env`, and prepares the Rust-first workflow behind `make dev-init`.

### `scripts/lib/env_file.sh`
Provides the canonical, non-evaluating reader and placeholder checks for Astra
environment templates. Source this helper instead of independently parsing
dotenv values in setup or deployment scripts.

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

`/chat/stream` requires an explicit `selected_model.model`. Pass `--model` for
the standard probe body, or provide a `--body-template` that includes
`selected_model.model`.

```sh
python3 scripts/load/multi_cli_capacity_probe.py --profile 100-cli --model qwen3.7-max --register-users --require-metrics
python3 scripts/load/multi_cli_capacity_probe.py --profile 500-cli --model qwen3.7-max --token-file tokens.json --require-distinct-users --require-metrics
```

For runtime capacity gates, prefer mock LLM for large concurrency so provider
RPM/TPM does not hide runtime, DB, SSE, or control-plane behavior:

```sh
python3 scripts/load/mock_openai_server.py --port 18080 --model-name capacity-mock --write-model-yaml tmp/capacity-mock-model.yaml
python3 scripts/load/multi_cli_capacity_probe.py --profile 500-cli --model capacity-mock --register-users --require-distinct-users --require-metrics --require-error-codes-for-failures --require-no-critical-ingestion-drops --require-no-run-control-errors --require-no-durable-event-errors --require-no-edge-dispatch-errors --output-dir tmp/capacity-probe/mock-strict-500
```

After a probe, extract the matching API log window and generate a DB verdict:

```sh
python3 scripts/load/extract_slow_sql.py --log api_server.log --start 2026-07-03T08:08:20.654Z --end 2026-07-03T08:10:06.434Z --output-dir tmp/capacity-probe/mock-strict-500
python3 scripts/load/db_capacity_report.py --probe-summary tmp/capacity-probe/mock-strict-500/summary.json --slow-sql-summary tmp/capacity-probe/mock-strict-500/slow-sql-summary.json --format markdown
```

`db_capacity_report.py` deliberately separates provider/harness failures,
admission limits, DB pressure, and DB saturation. Do not treat slow SQL count
alone as proof that production DB capacity is insufficient; a production claim
also needs multi-pod or staging/prod MatrixOne metrics.

### `scripts/load/cleanup_pressure_probe.py`
Runs ignored live MatrixOne cleanup pressure probes for the current retention
hot paths: `agent_message_queue`, `conversation_log`, and prompt retention.
The runner captures per-probe stdout/stderr and writes `summary.json` under
`tmp/cleanup-pressure/`.

The prompt probe mixes three classes in one run: expired inactive rows that must
be deleted, expired active-session rows that must be guarded, and fresh inactive
rows that must remain until the retention age is reached.

Use only against disposable test databases. The default database base contains
`test`, and the script refuses names that do not contain `test` or `smoke`.

```sh
make test-cleanup-pressure
python3 scripts/load/cleanup_pressure_probe.py --profile smoke
python3 scripts/load/cleanup_pressure_probe.py --profile pressure --queue-rows 20000 --csl-rows 20000 --prompt-rows 20000 --prompt-fresh-rows 512
```

### `scripts/load/durable_event_pressure_probe.py`
Runs an ignored live MatrixOne probe for the durable run-event persistence
budget. It writes concurrent completed runs with large synthetic streaming
outputs, then reports `agent_run_events` rows, estimated batch bytes, replay
rows, and compaction summary frequency under `tmp/durable-event-pressure/`.

This is a DB-layer pressure gate. It deliberately avoids real LLM calls so
provider quotas do not hide durable event row-amplification regressions. Use the
multi-CLI probe for end-to-end HTTP/SSE/provider behavior.

Use only against disposable test databases. The default database contains
`test`, and the script refuses names that do not contain `test` or `smoke`.

```sh
make test-durable-event-pressure
python3 scripts/load/durable_event_pressure_probe.py --profile smoke
python3 scripts/load/durable_event_pressure_probe.py --profile pressure --runs 100 --text-deltas 10000 --progress-rows 525
```

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

### `scripts/e2e/validate_capability_matrix.py`
Validates that every `system_test` name in the product capability matrix still
resolves to a real Rust function somewhere under `crates/`. This is an
offline, dependency-free guard against renamed or deleted evidence anchors:

```sh
python3 scripts/e2e/validate_capability_matrix.py
```

### `scripts/render_readme_diagrams.py`
Regenerates the README architecture diagrams under `docs/assets/diagrams/` as
matching light and dark SVG pairs. Standard library only:

```sh
python3 scripts/render_readme_diagrams.py
```

Edit the diagram definitions in this script rather than the generated SVG, then
commit the regenerated files.

### `scripts/render_readme_demo.py`
Regenerates the 20-second illustrative context-to-execution walkthrough
embedded near the top of the README. It uses the repository's Inconsolata
fonts and dark TUI theme colors, and requires Pillow:

```sh
python3 scripts/render_readme_demo.py
```

Keep the flow explicitly labeled as illustrative so the asset explains Astra's
runtime contract without being mistaken for a captured live session.

### `scripts/install-astra.sh`
Installs a checksum-verified `astra` CLI archive from this repository's GitHub Releases:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/matrixorigin/Astra/main/scripts/install-astra.sh | sh
```

The installer, source tag, release assets, and user documentation deliberately
live in the same repository. Checksums are mandatory; a missing or mismatched
checksum fails the installation.

### `scripts/validate-release-version.sh` and `scripts/verify-release-artifacts.sh`
The release workflows use these scripts as shared, locally testable gates.
The first requires every versioned workspace surface to match the Git tag. The
second requires the complete four-platform CLI archive set, verifies every
checksum and archive layout, and creates the aggregate checksum manifest.
`scripts/ci/test_release_contract.sh` exercises the success and tampering paths
without compiling binaries or publishing artifacts.

### `scripts/ops/*.sh`
Operational helpers for health checks, backup/restore, and deployment.
`deploy.sh [api-replicas]` validates and starts the canonical production
Compose profile using root `.env.production` (override the path with
`ASTRA_PRODUCTION_ENV_FILE`). `validate_production_env.sh` enforces required
values, immutable image selection, trusted CORS origins, and minimum secret
lengths without evaluating the environment file.
