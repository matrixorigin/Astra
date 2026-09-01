#!/usr/bin/env python3
"""Create and verify a one-use MatrixOne database contract for Harbor.

The contract records that a unique database did not exist before seeding and
then proves, immediately before Harbor starts, that the selected DeepSeek
route is healthy and no durable runtime/session/work state exists.  It contains
no database or provider credentials.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import secrets
import stat
import subprocess
import sys
import tempfile
from datetime import UTC, datetime, timedelta
from pathlib import Path


SCHEMA_VERSION = 2
# The scored config records the complete selector; the fresh database only
# needs the registered base model.  Keep the historical benchmark default but
# make an explicitly selected model/mode comparison verifiable too.
EXPECTED_MODEL = os.environ.get("ASTRA_HARNESS_MODEL_BASE", "deepseek-v4-flash").strip()
EXPECTED_THINKING_MODE = os.environ.get(
    "ASTRA_HARNESS_MODEL_THINKING", "high"
).strip()
CONSUMPTION_DIRECTORY_NAME = "consumption"
DATABASE_LOCK_PREFIX = "astra-terminal-bench-database-"
TEST_FIXTURE_TABLES = (
    "agent_events",
    "agent_runs",
    "agent_sessions",
    "harness_runs",
    "session_transcript_items",
    "work_items",
    "works",
)
BOOT_METADATA_TABLES = frozenset(
    {
        "astra_schema_contracts",
        "astra_schema_table_contracts",
        "infra_llm_models",
        "maintenance_sweep_cursors",
        "preview_template_registry",
        "raw_ref_scheme_registry",
        "sweeper_leases",
        # A fresh scored database is seeded through the production admin API.
        # Its single bootstrap administrator is control-plane state, not a
        # user task/session/work mutation. Keep this narrow and count-bound
        # below so a reused user population cannot be mistaken for a fresh
        # run.
        "auth_users",
        "auth_roles",
        "auth_user_roles",
        "auth_refresh_tokens",
    }
)

CONDITIONAL_SCHEMA_STARTUP_OWNERS = frozenset(
    {"ensure_llm_provider_admission_schema_if_configured"}
)
# MatrixOne follows MySQL's 64-byte identifier ABI.  Keep benchmark database
# identities within that limit so a fresh-run proof cannot trigger a partial
# CREATE DATABASE side effect followed by an opaque server error.
DATABASE_RE = re.compile(r"astra_tb_[a-zA-Z0-9_]{1,55}")


class ContractError(RuntimeError):
    pass


def _required_env(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        raise ContractError(f"{name} is required")
    return value


def _source_revision(repo: Path) -> str:
    completed = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "HEAD"],
        check=False,
        capture_output=True,
        text=True,
        timeout=5,
        env={"PATH": os.environ.get("PATH", "/usr/bin:/bin")},
    )
    revision = completed.stdout.strip()
    if completed.returncode != 0 or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ContractError("cannot resolve a valid source revision")
    return revision


def _database_name(value: str) -> str:
    if DATABASE_RE.fullmatch(value) is None:
        raise ContractError(
            "database must be a unique astra_tb_* identifier of at most 64 ASCII bytes containing only letters, digits, and underscores"
        )
    return value


def _database_identity_sha256(database: str) -> str:
    """Bind a proof to the credential-free MatrixOne database identity.

    All accepted host spellings are loopback aliases.  Canonicalizing them to
    one value is deliberately conservative: two launchers cannot evade the
    lifecycle lease merely by spelling the same local endpoint differently.
    """
    host = _required_env("MATRIXONE_HOST")
    if host not in {"127.0.0.1", "localhost", "::1"}:
        raise ContractError(
            "fresh benchmark database proof requires a loopback MatrixOne endpoint"
        )
    port = _required_env("MATRIXONE_PORT")
    if not port.isdigit() or not 1 <= int(port) <= 65535:
        raise ContractError("MATRIXONE_PORT must be a decimal TCP port")
    user = _required_env("MATRIXONE_USER")
    canonical = json.dumps(
        {
            "schema": "astra.harness.matrixone_database_identity.v1",
            "host": "loopback",
            "port": int(port),
            "user": user,
            "database": database,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return hashlib.sha256(canonical).hexdigest()


def _mysql_rows(sql: str, database: str | None = None) -> list[list[str]]:
    host = _required_env("MATRIXONE_HOST")
    if host not in {"127.0.0.1", "localhost", "::1"}:
        raise ContractError(
            "fresh benchmark database proof requires a loopback MatrixOne endpoint"
        )
    port = _required_env("MATRIXONE_PORT")
    if not port.isdigit() or not 1 <= int(port) <= 65535:
        raise ContractError("MATRIXONE_PORT must be a decimal TCP port")
    user = _required_env("MATRIXONE_USER")
    password = _required_env("MATRIXONE_PASSWORD")
    argv = [
        os.environ.get("ASTRA_MYSQL_CLIENT", "mysql"),
        "--protocol=TCP",
        f"-h{host}",
        f"-P{port}",
        f"-u{user}",
        "--skip-ssl",
        "--batch",
        "--skip-column-names",
    ]
    if database is not None:
        argv.append(database)
    argv.extend(["-e", sql])
    environment = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "MYSQL_PWD": password,
    }
    try:
        completed = subprocess.run(
            argv,
            check=False,
            capture_output=True,
            text=True,
            timeout=15,
            env=environment,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ContractError(
            f"MatrixOne proof query failed to start: {error}"
        ) from error
    if completed.returncode != 0:
        detail = completed.stderr.strip() or "mysql client returned non-zero"
        raise ContractError(f"MatrixOne proof query failed: {detail}")
    return [line.split("\t") for line in completed.stdout.splitlines() if line]


def _database_exists(database: str) -> bool:
    rows = _mysql_rows(
        "SELECT COUNT(*) FROM information_schema.schemata "
        f"WHERE schema_name='{database}'"
    )
    return rows == [["1"]]


def _load_schema_inventory(repo: Path):
    script = repo / "scripts" / "schema" / "schema_inventory.py"
    if not script.is_file():
        raise ContractError(
            f"canonical schema inventory generator is missing: {script}"
        )
    module_name = "astra_harness_schema_inventory"
    spec = importlib.util.spec_from_file_location(module_name, script)
    if spec is None or spec.loader is None:
        raise ContractError("cannot load canonical schema inventory generator")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    try:
        spec.loader.exec_module(module)
        return module
    except Exception as error:
        raise ContractError(
            f"cannot generate canonical schema inventory: {error}"
        ) from error
    finally:
        sys.modules.pop(module_name, None)


def _canonical_schema_manifest(repo: Path) -> dict:
    inventory_module = _load_schema_inventory(repo)
    inventory = inventory_module.build_inventory(repo)
    tables = inventory.get("tables")
    summary = inventory.get("summary")
    if not isinstance(tables, list) or not isinstance(summary, dict):
        raise ContractError("canonical schema inventory output is malformed")
    names = [row.get("table") for row in tables if isinstance(row, dict)]
    if (
        not names
        or len(names) != len(tables)
        or not all(
            isinstance(name, str) and re.fullmatch(r"[a-z][a-z0-9_]*", name)
            for name in names
        )
        or len(names) != len(set(names))
    ):
        raise ContractError(
            "canonical schema inventory has invalid or duplicate tables"
        )
    if summary.get("unique_table_count") != len(names):
        raise ContractError(
            "canonical schema inventory summary does not match its tables"
        )
    source_rows = inventory.get("schema_sources")
    if not isinstance(source_rows, list) or not source_rows:
        raise ContractError("canonical schema source manifest is empty")
    conditional_tables = sorted(
        row["table"]
        for row in tables
        if row.get("startup_owner") in CONDITIONAL_SCHEMA_STARTUP_OWNERS
    )
    manifest = {
        "schema": "astra.harness.canonical_schema_inventory.v2",
        "tables": sorted(names),
        "conditional_tables": conditional_tables,
        "sources": sorted(
            {
                str(row.get("path"))
                for row in source_rows
                if isinstance(row, dict) and isinstance(row.get("path"), str)
            }
        ),
    }
    manifest["sha256"] = hashlib.sha256(
        json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    return manifest


def _canonical_schema_inventory(repo: Path) -> set[str]:
    return set(_canonical_schema_manifest(repo)["tables"])


def _validate_closed_schema_counts(
    canonical: set[str],
    actual: set[str],
    counts: dict[str, int],
    *,
    optional_absent: set[str] | frozenset[str] = frozenset(),
) -> None:
    _validate_closed_schema_presence(canonical, actual, optional_absent)
    if set(counts) != actual:
        raise ContractError("database count inventory does not match the live schema")
    invalid = {
        name: count
        for name, count in counts.items()
        if not isinstance(count, int) or count < 0
    }
    if invalid:
        raise ContractError(f"database count inventory is invalid: {invalid}")
    contaminated = {
        name: count
        for name, count in counts.items()
        if name not in BOOT_METADATA_TABLES and count != 0
    }
    if contaminated:
        raise ContractError(f"database contains non-bootstrap state: {contaminated}")
    expected_boot_counts = {
        "astra_schema_contracts": 1,
        "infra_llm_models": 1,
        "maintenance_sweep_cursors": 1,
        "preview_template_registry": 37,
        "raw_ref_scheme_registry": 9,
        "sweeper_leases": 1,
    }
    for table, expected in expected_boot_counts.items():
        if counts.get(table) != expected:
            raise ContractError(
                f"bootstrap table {table} has {counts.get(table)!r} rows; expected {expected}"
            )
    # A server may have only performed schema initialization, or it may have
    # also registered the one admin required to load and probe the selected
    # model. Both are legitimate pre-run seed states; any larger/different
    # identity population is contamination. The absent-before-seed proof is
    # still the authority that prevents accepting a reused database.
    for table, expected in {
        "auth_users": 1,
        "auth_roles": 2,
        "auth_user_roles": 2,
        "auth_refresh_tokens": 1,
    }.items():
        if counts.get(table, 0) not in {0, expected}:
            raise ContractError(
                f"bootstrap auth table {table} has {counts.get(table)!r} rows; expected 0 or {expected}"
            )
    if counts.get("astra_schema_table_contracts", 0) <= 0:
        raise ContractError("bootstrap table astra_schema_table_contracts is empty")


def _validate_closed_schema_presence(
    canonical: set[str],
    actual: set[str],
    optional_absent: set[str] | frozenset[str],
) -> None:
    invalid_optional = sorted(set(optional_absent) - canonical)
    if invalid_optional:
        raise ContractError(
            f"conditional schema inventory is not canonical: {invalid_optional}"
        )
    optional_present = actual & set(optional_absent)
    if optional_present and optional_present != set(optional_absent):
        missing_optional = sorted(set(optional_absent) - optional_present)
        raise ContractError(
            f"conditional database schema is incomplete; missing tables: {missing_optional}"
        )
    unknown = sorted(actual - canonical)
    missing = sorted(canonical - actual - set(optional_absent))
    if unknown:
        raise ContractError(f"database contains unknown canonical tables: {unknown}")
    if missing:
        raise ContractError(f"database schema is incomplete; missing tables: {missing}")


def _core_schema_contract_version(repo: Path) -> str:
    source = (repo / "crates" / "services" / "src" / "storage.rs").read_text(
        encoding="utf-8"
    )
    match = re.search(
        r'pub const CORE_SCHEMA_CONTRACT_VERSION: &str = "([^"]+)";', source
    )
    if match is None:
        raise ContractError("cannot resolve the canonical core schema contract version")
    return match.group(1)


def _canonical_baseline_registry_rows(repo: Path) -> dict[str, list[list[str]]]:
    """Derive exact startup seeds from the same checked-in Rust constants.

    Timestamps are deliberately excluded.  Every semantic column, including
    the JSON strings written by startup, is compared byte-for-byte.
    """
    storage = (repo / "crates" / "services" / "src" / "storage.rs").read_text(
        encoding="utf-8"
    )
    context = (repo / "crates" / "services" / "src" / "context_manifest.rs").read_text(
        encoding="utf-8"
    )
    raw_block = re.search(
        r"for \(scheme, resolver, backing, access_check, example\) in \[(.*?)\n\s*\] \{",
        storage,
        re.DOTALL,
    )
    if raw_block is None:
        raise ContractError("cannot resolve raw-ref startup seed inventory")
    raw_rows = [
        [*values, "1"]
        for values in re.findall(
            r'\(\s*"([^"]+)",\s*"([^"]+)",\s*"([^"]+)",\s*"([^"]+)",\s*"([^"]+)"\s*,?\s*\)',
            raw_block.group(1),
            re.DOTALL,
        )
    ]
    if len(raw_rows) != 9 or len({row[0] for row in raw_rows}) != len(raw_rows):
        raise ContractError("raw-ref startup seed inventory is not canonical")

    template_block = re.search(
        r"pub const BASELINE_PREVIEW_TEMPLATES:.*?= &\[(.*?)\n\];",
        context,
        re.DOTALL,
    )
    weights_block = re.search(
        r"pub fn preview_template_fts_field_weights.*?match normalize_version \{(.*?)\n\s*\}\n\}\n\n#\[",
        context,
        re.DOTALL,
    )
    if template_block is None or weights_block is None:
        raise ContractError("cannot resolve preview-template startup seed inventory")
    templates = re.findall(
        r'\("([^"]+)",\s*([0-9]+),\s*"([^"]+)"\)',
        template_block.group(1),
    )
    weights = dict(
        re.findall(
            r'"([^"]+)"\s*=>\s*(?:\{\s*)?r#"([^\n]+)"#',
            weights_block.group(1),
        )
    )
    default_match = re.search(r'_\s*=>\s*r#"([^\n]+)"#', weights_block.group(1))
    if not templates or default_match is None:
        raise ContractError("preview-template startup seed inventory is empty")
    default_weights = default_match.group(1)
    preview_rows = [
        [
            tool_name,
            "v1",
            "active",
            max_preview_bytes,
            "tool_output_preview",
            "[]",
            weights.get(normalize_version, default_weights),
            normalize_version,
            "{}",
        ]
        for tool_name, max_preview_bytes, normalize_version in templates
    ]
    if len({row[0] for row in preview_rows}) != len(preview_rows):
        raise ContractError("preview-template startup seed inventory has duplicates")
    return {
        "raw_ref_scheme_registry": sorted(raw_rows),
        "preview_template_registry": sorted(preview_rows),
    }


def _validate_baseline_registry_rows(database: str, repo: Path) -> dict[str, object]:
    expected = _canonical_baseline_registry_rows(repo)
    observed = {
        "raw_ref_scheme_registry": _mysql_rows(
            "SELECT scheme, resolver_name, backing_store, access_check, "
            "canonical_example, CAST(is_active AS CHAR) "
            "FROM raw_ref_scheme_registry ORDER BY scheme",
            database,
        ),
        "preview_template_registry": _mysql_rows(
            "SELECT tool_name, version, status, CAST(max_preview_bytes AS CHAR), "
            "default_chunk_type, first_class_columns_json, fts_field_weights_json, "
            "normalize_version, schema_json "
            "FROM preview_template_registry ORDER BY tool_name, version",
            database,
        ),
    }
    result: dict[str, object] = {}
    for table in sorted(expected):
        if observed[table] != expected[table]:
            raise ContractError(
                f"bootstrap table {table} differs from its exact source-owned baseline"
            )
        serialized = json.dumps(
            observed[table], sort_keys=True, separators=(",", ":")
        ).encode()
        result[f"{table}_count"] = len(observed[table])
        result[f"{table}_sha256"] = hashlib.sha256(serialized).hexdigest()
    return result


def _canonical_runtime_system_baseline(repo: Path) -> dict[str, object]:
    source_paths = (
        "crates/runtime/src/server/sweeper_lease.rs",
        "crates/runtime/src/server/tool_invocation_compactor.rs",
    )
    sources = {path: (repo / path).read_text(encoding="utf-8") for path in source_paths}
    sweeper_source = sources[source_paths[0]]
    cursor_source = sources[source_paths[1]]
    lease = re.search(
        r"pub\(crate\) fn spawn_runtime_sweepers\(.*?"
        r'lease_name:\s*"([^"]+)"\.to_string\(\),',
        sweeper_source,
        re.DOTALL,
    )
    pod_prefix = re.search(
        r'unwrap_or_else\(\|\|\s*format!\("([^"]*)\{\}",\s*Uuid::new_v4\(\)\)\)',
        sweeper_source,
    )
    ttl = re.search(r"const SWEEPER_LEASE_TTL_SECS: u64 = ([0-9]+);", sweeper_source)
    cursor_name = re.search(
        r'const COMPACTION_CURSOR_NAME: &str = "([^"]+)";', cursor_source
    )
    cursor_epoch = re.search(
        r'const COMPACTION_CURSOR_EPOCH: &str = "([^"]+)";', cursor_source
    )
    if any(
        match is None for match in (lease, pod_prefix, ttl, cursor_name, cursor_epoch)
    ):
        raise ContractError("cannot resolve the source-owned runtime system baseline")
    contract = {
        "schema": "astra.harness.runtime_system_baseline.v1",
        "sweeper_name": lease.group(1),
        "sweeper_owner_prefix": pod_prefix.group(1),
        "sweeper_lease_ttl_seconds": int(ttl.group(1)),
        "maintenance_cursor_name": cursor_name.group(1),
        "maintenance_cursor_epoch": cursor_epoch.group(1),
        "sources": [
            {
                "path": path,
                "sha256": hashlib.sha256(sources[path].encode()).hexdigest(),
            }
            for path in source_paths
        ],
    }
    return contract


def _validate_runtime_system_baseline(database: str, repo: Path) -> dict[str, object]:
    expected = _canonical_runtime_system_baseline(repo)
    sweeper_rows = _mysql_rows(
        "SELECT sweeper_name, owner_pod_id, CAST(expires_at AS CHAR), "
        "CAST(version AS CHAR), CAST(created_at AS CHAR), CAST(updated_at AS CHAR) "
        "FROM sweeper_leases ORDER BY sweeper_name",
        database,
    )
    cursor_rows = _mysql_rows(
        "SELECT sweep_name, CAST(cursor_updated_at AS CHAR), cursor_user_id, "
        "cursor_run_id, CAST(scan_generation AS CHAR), CAST(created_at AS CHAR), "
        "CAST(updated_at AS CHAR) FROM maintenance_sweep_cursors ORDER BY sweep_name",
        database,
    )
    if len(sweeper_rows) != 1 or len(sweeper_rows[0]) != 6:
        raise ContractError(
            "bootstrap table sweeper_leases differs from its exact runtime baseline"
        )
    sweeper = sweeper_rows[0]
    configured_owner = os.environ.get("ASTRA_POD_ID", "").strip()
    owner_pattern = re.compile(
        re.escape(str(expected["sweeper_owner_prefix"]))
        + r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}"
    )
    owner_valid = (
        sweeper[1] == configured_owner
        if configured_owner
        else owner_pattern.fullmatch(sweeper[1]) is not None
    )
    created_at = _parse_timestamp(sweeper[4], "sweeper created_at")
    updated_at = _parse_timestamp(sweeper[5], "sweeper updated_at")
    expires_at = _parse_timestamp(sweeper[2], "sweeper expires_at")
    if (
        sweeper[0] != expected["sweeper_name"]
        or not owner_valid
        or sweeper[3] != "0"
        or updated_at != created_at
        or expires_at <= created_at
    ):
        raise ContractError(
            "bootstrap table sweeper_leases differs from its exact runtime baseline"
        )
    if len(cursor_rows) != 1 or len(cursor_rows[0]) != 7:
        raise ContractError(
            "bootstrap table maintenance_sweep_cursors differs from its exact runtime baseline"
        )
    cursor = cursor_rows[0]
    cursor_created_at = _parse_timestamp(cursor[5], "maintenance cursor created_at")
    cursor_updated_at = _parse_timestamp(cursor[6], "maintenance cursor updated_at")
    if (
        cursor[:5]
        != [
            expected["maintenance_cursor_name"],
            expected["maintenance_cursor_epoch"],
            "",
            "",
            "0",
        ]
        or cursor_updated_at < cursor_created_at
    ):
        raise ContractError(
            "bootstrap table maintenance_sweep_cursors differs from its exact runtime baseline"
        )
    # Lease ownership and timestamps are deliberately *not* proof identity.
    # A healthy seeded server refreshes its TTL while immutable verifier images
    # are being pre-warmed, and an HPA handover may legitimately replace the
    # random pod id.  Each observation above still validates those values, but
    # treating their physical representation as immutable made a clean launch
    # fail before the candidate server could start.  The sealed identity must
    # cover the source-owned logical baseline, while runtime counts retain the
    # fail-closed guard against actual user/runtime state.
    serialized = json.dumps(
        {
            "contract": expected,
            "maintenance_sweep_cursor": cursor[:5],
            "sweeper_lease": [sweeper[0], sweeper[3]],
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return {
        "maintenance_sweep_cursors_count": 1,
        "sweeper_leases_count": 1,
        "runtime_system_baseline_sha256": hashlib.sha256(serialized).hexdigest(),
    }


def _validate_boot_metadata(database: str, repo: Path, counts: dict[str, int]) -> dict:
    version = _core_schema_contract_version(repo)
    contracts = _mysql_rows(
        "SELECT component, contract_version FROM astra_schema_contracts ORDER BY component",
        database,
    )
    if contracts != [["astra-core", version]]:
        raise ContractError(
            "astra_schema_contracts does not match the exact source contract"
        )
    leases = _mysql_rows(
        "SELECT component, holder_id FROM astra_schema_bootstrap_leases ORDER BY component",
        database,
    )
    if leases:
        raise ContractError("schema bootstrap lease remains active after seeding")
    table_contracts = _mysql_rows(
        "SELECT table_name, component, owner, contract_version, ddl_sha256 "
        "FROM astra_schema_table_contracts ORDER BY table_name",
        database,
    )
    if len(table_contracts) != counts["astra_schema_table_contracts"]:
        raise ContractError("schema table contract row count changed during inspection")
    manifest = _canonical_schema_manifest(repo)
    canonical = set(manifest["tables"])
    conditional_tables = set(manifest["conditional_tables"])
    seen: set[str] = set()
    for row in table_contracts:
        if len(row) != 5:
            raise ContractError("schema table contract row has an invalid shape")
        table, component, owner, row_version, ddl_sha256 = row
        if (
            table not in canonical
            or table in seen
            or component != "astra-core"
            or not owner
            or row_version != version
            or re.fullmatch(r"[0-9a-f]{64}", ddl_sha256) is None
        ):
            raise ContractError(f"schema table contract is not canonical: {table!r}")
        seen.add(table)
    required_contracts = (
        canonical
        - conditional_tables
        - {
            "agent_mailbox_directory",
            "agent_message_broadcast_delivery",
            "agent_message_queue",
            "resource_limits",
            "resource_usage",
        }
    )
    if seen != required_contracts:
        raise ContractError(
            "schema table contracts differ from the exact startup-owned inventory"
        )
    digest = hashlib.sha256(
        json.dumps(table_contracts, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    return {
        "core_component": "astra-core",
        "core_contract_version": version,
        "table_contract_count": len(table_contracts),
        "table_contract_sha256": digest,
        **_validate_baseline_registry_rows(database, repo),
        **_validate_runtime_system_baseline(database, repo),
    }


def _runtime_state(database: str, repo: Path | None = None) -> dict[str, object]:
    repo = Path(__file__).resolve().parents[2] if repo is None else repo
    manifest = _canonical_schema_manifest(repo)
    canonical = set(manifest["tables"])
    conditional_tables = set(manifest["conditional_tables"])
    present_rows = _mysql_rows(
        "SELECT table_name FROM information_schema.tables "
        f"WHERE table_schema='{database}' AND table_type='BASE TABLE' ORDER BY table_name"
    )
    actual = {row[0] for row in present_rows if len(row) == 1}
    _validate_closed_schema_presence(canonical, actual, conditional_tables)
    union = " UNION ALL ".join(
        f"SELECT '{table}', COUNT(*) FROM `{table}`" for table in sorted(actual)
    )
    rows = _mysql_rows(union, database)
    try:
        counts = {row[0]: int(row[1]) for row in rows if len(row) == 2}
    except ValueError as error:
        raise ContractError(
            "database count inventory returned a non-integer"
        ) from error
    _validate_closed_schema_counts(
        canonical, actual, counts, optional_absent=conditional_tables
    )
    boot_metadata = _validate_boot_metadata(database, repo, counts)
    return {
        "counts": counts,
        "boot_metadata": boot_metadata,
        "schema_inventory_sha256": manifest["sha256"],
    }


def _runtime_counts(database: str, repo: Path | None = None) -> dict[str, int]:
    """Compatibility-free internal convenience for diagnostic callers."""
    state = _runtime_state(database, repo)
    return dict(state["counts"])


def _model_state(database: str) -> dict[str, str | int | None]:
    if EXPECTED_THINKING_MODE not in {"none", "high"}:
        raise ContractError(
            "ASTRA_HARNESS_MODEL_THINKING must be exactly 'none' or 'high'"
        )
    rows = _mysql_rows(
        "SELECT model_name, is_active, COALESCE(thinking_capability,''), "
        "COALESCE(thinking_probe_error,''), CAST(updated_at AS CHAR) "
        "FROM infra_llm_models",
        database,
    )
    if len(rows) != 1 or len(rows[0]) != 5:
        raise ContractError(f"expected exactly one model offering, found {len(rows)}")
    name, active, capability, probe_error, updated_at = rows[0]
    if name != EXPECTED_MODEL or active != "1":
        raise ContractError("the exact selected model offering is not uniquely active")
    if (
        EXPECTED_THINKING_MODE == "high"
        and capability not in {"both", "effort_only"}
    ):
        raise ContractError("the selected offering cannot honor thinking:high")
    if EXPECTED_THINKING_MODE == "high" and probe_error:
        raise ContractError("the selected offering has a thinking/provider probe error")
    return {
        "model_name": name,
        "is_active": 1,
        "thinking_capability": capability,
        "thinking_probe_error": probe_error or None,
        "requested_thinking_mode": EXPECTED_THINKING_MODE,
        "checked_updated_at": updated_at,
    }


def _parse_timestamp(value: str, label: str) -> datetime:
    try:
        parsed = datetime.fromisoformat(value)
    except (TypeError, ValueError) as error:
        raise ContractError(
            f"database proof has an invalid {label} timestamp"
        ) from error
    return (
        parsed.replace(tzinfo=UTC) if parsed.tzinfo is None else parsed.astimezone(UTC)
    )


def _write_json_atomic(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=path.parent, delete=False
    ) as stream:
        json.dump(payload, stream, indent=2, sort_keys=True)
        stream.write("\n")
        temporary = Path(stream.name)
    os.chmod(temporary, 0o600)
    temporary.replace(path)


def begin(repo: Path, database: str, proof: Path) -> None:
    if proof.exists():
        raise ContractError(
            "proof path already exists; every round requires a new proof"
        )
    if _database_exists(database):
        raise ContractError(
            "database already exists; refusing a contaminated benchmark round"
        )
    payload = {
        "schema_version": SCHEMA_VERSION,
        "phase": "absent_before_seed",
        "database": database,
        "database_identity_sha256": _database_identity_sha256(database),
        "source_revision": _source_revision(repo),
        "nonce": secrets.token_hex(32),
        "absent_before_seed": True,
        "begun_at": datetime.now(UTC).isoformat(),
    }
    _write_json_atomic(proof, payload)


def _load_proof(path: Path) -> dict:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ContractError(f"cannot read database proof: {error}") from error
    if not isinstance(payload, dict) or payload.get("schema_version") != SCHEMA_VERSION:
        raise ContractError("database proof has an unsupported schema")
    return payload


def _validate_identity(
    payload: dict,
    repo: Path,
    database: str,
    expected_source_revision: str | None = None,
) -> None:
    if payload.get("database") != database:
        raise ContractError(
            "database proof identity does not match the requested database"
        )
    source_revision = expected_source_revision or _source_revision(repo)
    if payload.get("source_revision") != source_revision:
        raise ContractError("database proof source revision does not match HEAD")
    if payload.get("database_identity_sha256") != _database_identity_sha256(database):
        raise ContractError(
            "database proof MatrixOne endpoint/user/database identity changed"
        )
    if payload.get("absent_before_seed") is not True:
        raise ContractError("database proof lacks the absent-before-seed fact")
    nonce = payload.get("nonce")
    if not isinstance(nonce, str) or re.fullmatch(r"[0-9a-f]{64}", nonce) is None:
        raise ContractError("database proof nonce is malformed")


def _sealed_contract_sha256(payload: dict) -> str:
    claimed_hash = payload.get("contract_sha256")
    if (
        not isinstance(claimed_hash, str)
        or re.fullmatch(r"[0-9a-f]{64}", claimed_hash) is None
    ):
        raise ContractError("database proof content hash is malformed")
    unsigned = dict(payload)
    unsigned.pop("contract_sha256", None)
    canonical = json.dumps(unsigned, sort_keys=True, separators=(",", ":")).encode()
    if claimed_hash != hashlib.sha256(canonical).hexdigest():
        raise ContractError("database proof content hash is invalid")
    return claimed_hash


def _validate_sealed_contract(
    repo: Path,
    database: str,
    proof: Path,
    expected_source_revision: str | None = None,
    *,
    require_launch_freshness: bool = True,
) -> tuple[dict, str, str]:
    payload = _load_proof(proof)
    _validate_identity(payload, repo, database, expected_source_revision)
    if payload.get("phase") != "sealed_ready":
        raise ContractError("database proof is not sealed and ready")
    if require_launch_freshness:
        sealed_at = _parse_timestamp(payload.get("sealed_at"), "sealed_at")
        now = datetime.now(UTC)
        if sealed_at > now + timedelta(minutes=1) or now - sealed_at > timedelta(
            minutes=15
        ):
            raise ContractError(
                "database proof is not from the current benchmark launch window"
            )
    claimed_hash = _sealed_contract_sha256(payload)
    database_identity = payload["database_identity_sha256"]
    return payload, database_identity, claimed_hash


def sealed_contract_identity(
    repo: Path,
    database: str,
    proof: Path,
    expected_source_revision: str | None = None,
) -> dict[str, str]:
    _, database_identity, contract_sha256 = _validate_sealed_contract(
        repo, database, proof, expected_source_revision
    )
    return {
        "database_identity_sha256": database_identity,
        "contract_sha256": contract_sha256,
        "lifecycle_schema": "astra.harness.lifecycle.v1",
    }


def launch_identity(
    repo: Path,
    database: str,
    proof: Path,
    expected_source_revision: str | None = None,
) -> dict[str, str]:
    """Return the immutable identity needed to acquire the lifecycle lease.

    A new database cannot be seeded or sealed until its runner-owned server is
    started.  Bind that startup to the exact absent-before-seed proof, then
    replace the admission hash with the sealed contract hash before Harbor.
    """
    payload = _load_proof(proof)
    _validate_identity(payload, repo, database, expected_source_revision)
    phase = payload.get("phase")
    if phase == "sealed_ready":
        identity = sealed_contract_identity(
            repo, database, proof, expected_source_revision
        )
        return {**identity, "phase": phase}
    if phase != "absent_before_seed":
        raise ContractError("database proof is not admissible for a benchmark launch")
    begun_at = _parse_timestamp(payload.get("begun_at"), "begun_at")
    now = datetime.now(UTC)
    if begun_at > now + timedelta(minutes=1) or now - begun_at > timedelta(minutes=15):
        raise ContractError(
            "database absence proof is not from the current benchmark launch window"
        )
    canonical = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    return {
        "database_identity_sha256": payload["database_identity_sha256"],
        "admission_sha256": hashlib.sha256(canonical).hexdigest(),
        "lifecycle_schema": "astra.harness.lifecycle.v1",
        "phase": phase,
    }


def _validated_consumption_directory(directory: Path) -> Path:
    if not directory.is_absolute():
        raise ContractError("database consumption directory must be absolute")
    try:
        status = directory.lstat()
        mode = status.st_mode
    except OSError as error:
        raise ContractError(
            f"database lifecycle directory is unavailable: {directory}: {error}"
        ) from error
    if not stat.S_ISDIR(mode) or directory.is_symlink():
        raise ContractError(
            f"database lifecycle path is not a real directory: {directory}"
        )
    if not os.access(directory, os.W_OK | os.X_OK):
        raise ContractError(
            f"database lifecycle directory is not writable/searchable: {directory}"
        )
    shared_writes = mode & (stat.S_IWGRP | stat.S_IWOTH)
    if shared_writes and not mode & stat.S_ISVTX:
        raise ContractError(
            f"shared database lifecycle directory lacks sticky-bit protection: {directory}"
        )
    return directory


def _reserve_one_use_verification(
    consumption_directory: Path,
    database_identity_sha256: str,
    contract_sha256: str,
) -> Path:
    directory = _validated_consumption_directory(consumption_directory)
    marker = directory / (f"{DATABASE_LOCK_PREFIX}{database_identity_sha256}.consumed")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(marker, flags, 0o600)
    except FileExistsError as error:
        raise ContractError(
            "database identity was already consumed by a benchmark launch"
        ) from error
    except OSError as error:
        raise ContractError(
            f"cannot reserve one-use database proof: {error}"
        ) from error
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(f"{contract_sha256}\n".encode())
        stream.flush()
        os.fsync(stream.fileno())
    directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
    return marker


def _assert_lifecycle_brokers(
    guardian_pid: int,
    witness_pid: int,
    database_identity_sha256: str,
    gateway_port: int,
) -> None:
    try:
        import lifecycle_broker

        observed: dict[str, str] = {}
        for row in Path("/proc/net/unix").read_text(encoding="ascii").splitlines()[1:]:
            fields = row.split()
            if len(fields) >= 8:
                observed[fields[-1]] = fields[6]
    except (ImportError, OSError, ValueError) as error:
        raise ContractError(
            "lifecycle broker ownership proof is unavailable"
        ) from error
    try:
        for pid, scope in (
            (guardian_pid, "primary"),
            (guardian_pid, "runtime"),
            (witness_pid, "witness"),
        ):
            expected = {
                "@"
                + lifecycle_broker.database_address(database_identity_sha256, scope)[
                    1:
                ],
                "@" + lifecycle_broker.gateway_address(gateway_port, scope)[1:],
            }
            fd_directory = Path(f"/proc/{pid}/fd")
            owned_inodes = {
                target[8:-1]
                for entry in fd_directory.iterdir()
                if (target := os.readlink(entry)).startswith("socket:[")
            }
            if not expected <= set(observed) or any(
                observed[address] not in owned_inodes for address in expected
            ):
                raise ContractError(
                    f"lifecycle {scope} custodian does not own the exact database and gateway identities"
                )
    except OSError as error:
        raise ContractError(
            "lifecycle custodian descriptor proof is unavailable"
        ) from error


def seal(
    repo: Path,
    database: str,
    proof: Path,
    expected_admission_sha256: str | None = None,
) -> None:
    payload = _load_proof(proof)
    _validate_identity(payload, repo, database)
    if payload.get("phase") != "absent_before_seed":
        raise ContractError("only a new absent-before-seed proof can be sealed")
    canonical = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    admission_sha256 = hashlib.sha256(canonical).hexdigest()
    if (
        expected_admission_sha256 is not None
        and admission_sha256 != expected_admission_sha256
    ):
        raise ContractError("database absence proof changed after lifecycle admission")
    if not _database_exists(database):
        raise ContractError("seeded database does not exist")
    runtime_state = _runtime_state(database, repo)
    counts = runtime_state["counts"]
    nonzero = {
        name: value
        for name, value in counts.items()
        if name not in BOOT_METADATA_TABLES and value != 0
    }
    if nonzero:
        raise ContractError(
            f"seeded database already contains runtime state: {nonzero}"
        )
    model_state = _model_state(database)
    begun_at = _parse_timestamp(payload.get("begun_at"), "begun_at")
    checked_at = _parse_timestamp(model_state["checked_updated_at"], "model check")
    if checked_at < begun_at:
        raise ContractError(
            "selected model was not checked after the database absence proof"
        )
    payload.update(
        {
            "phase": "sealed_ready",
            "runtime_counts": counts,
            "boot_metadata": runtime_state["boot_metadata"],
            "schema_inventory_sha256": runtime_state["schema_inventory_sha256"],
            "selected_model": model_state,
            "sealed_at": datetime.now(UTC).isoformat(),
        }
    )
    canonical = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    payload["contract_sha256"] = hashlib.sha256(canonical).hexdigest()
    _write_json_atomic(proof, payload)


def verify(
    repo: Path,
    database: str,
    proof: Path,
    consumption_directory: Path,
    *,
    expected_database_identity_sha256: str,
    expected_contract_sha256: str,
    expected_source_revision: str | None = None,
) -> None:
    payload, database_identity, claimed_hash = _validate_sealed_contract(
        repo,
        database,
        proof,
        expected_source_revision,
        # Freshness was admitted before the launcher acquired and continuously
        # held the exact database lifecycle lease.  Reapplying a wall-clock
        # limit after bounded verifier preflight makes a valid launch expire
        # inside its own critical section.  Integrity, state drift, one-use
        # consumption and lifecycle ownership remain fail-closed below.
        require_launch_freshness=False,
    )
    if database_identity != expected_database_identity_sha256:
        raise ContractError(
            "database proof identity changed after lifecycle lease selection"
        )
    if claimed_hash != expected_contract_sha256:
        raise ContractError(
            "database proof contract changed after lifecycle lease selection"
        )
    _reserve_one_use_verification(
        consumption_directory, database_identity, claimed_hash
    )
    runtime_state = _runtime_state(database, repo)
    counts = runtime_state["counts"]
    if counts != payload.get("runtime_counts"):
        raise ContractError(
            "database acquired runtime state after the proof was sealed"
        )
    if runtime_state["boot_metadata"] != payload.get("boot_metadata"):
        raise ContractError("database boot metadata changed after the proof was sealed")
    inventory_sha256 = _canonical_schema_manifest(Path(__file__).resolve().parents[2])[
        "sha256"
    ]
    if payload.get("schema_inventory_sha256") != inventory_sha256:
        raise ContractError("database proof canonical schema inventory changed")
    if _model_state(database) != payload.get("selected_model"):
        raise ContractError("selected model state changed after the proof was sealed")
    current_payload, current_identity, current_hash = _validate_sealed_contract(
        repo,
        database,
        proof,
        expected_source_revision,
        require_launch_freshness=False,
    )
    if current_identity != database_identity or current_hash != claimed_hash:
        raise ContractError("database proof changed while verification was in progress")
    payload = current_payload
    payload.pop("contract_sha256", None)
    payload.update(
        {
            "phase": "consumed",
            "consumed_at": datetime.now(UTC).isoformat(),
            "sealed_contract_sha256": claimed_hash,
        }
    )
    _write_json_atomic(proof, payload)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("begin", "seal", "identity", "verify"))
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--database", required=True)
    parser.add_argument("--proof", type=Path, required=True)
    parser.add_argument("--expected-database-identity-sha256")
    parser.add_argument("--expected-contract-sha256")
    parser.add_argument("--expected-admission-sha256")
    parser.add_argument("--expected-source-revision")
    parser.add_argument("--lifecycle-guardian-pid", type=int)
    parser.add_argument("--lifecycle-witness-pid", type=int)
    parser.add_argument("--gateway-port", type=int)
    parser.add_argument("--consumption-directory", type=Path)
    args = parser.parse_args()
    try:
        repo = args.repo.expanduser().resolve(strict=True)
        database = _database_name(args.database)
        proof = args.proof.expanduser().resolve()
        if args.phase == "begin":
            begin(repo, database, proof)
        elif args.phase == "seal":
            if (
                args.expected_admission_sha256 is None
                or re.fullmatch(r"[0-9a-f]{64}", args.expected_admission_sha256)
                is None
            ):
                raise ContractError(
                    "seal requires the 64-hex lifecycle admission hash"
                )
            seal(repo, database, proof, args.expected_admission_sha256)
        elif args.phase == "identity":
            if (
                args.expected_source_revision is not None
                and re.fullmatch(r"[0-9a-f]{40}", args.expected_source_revision) is None
            ):
                raise ContractError("expected source revision must be 40 lowercase hex")
            identity = launch_identity(
                repo, database, proof, args.expected_source_revision
            )
        else:
            if (
                args.expected_database_identity_sha256 is None
                or args.expected_contract_sha256 is None
                or args.lifecycle_guardian_pid is None
                or args.lifecycle_witness_pid is None
                or args.gateway_port is None
                or args.consumption_directory is None
            ):
                raise ContractError(
                    "verify requires expected proof identity/hash and an active lifecycle broker"
                )
            if (
                re.fullmatch(r"[0-9a-f]{64}", args.expected_database_identity_sha256)
                is None
                or re.fullmatch(r"[0-9a-f]{64}", args.expected_contract_sha256) is None
            ):
                raise ContractError(
                    "expected database identity/hash must be 64 lowercase hex"
                )
            lifecycle_directory = _validated_consumption_directory(
                args.consumption_directory.expanduser()
            )
            _assert_lifecycle_brokers(
                args.lifecycle_guardian_pid,
                args.lifecycle_witness_pid,
                args.expected_database_identity_sha256,
                args.gateway_port,
            )
            verify(
                repo,
                database,
                proof,
                lifecycle_directory,
                expected_database_identity_sha256=args.expected_database_identity_sha256,
                expected_contract_sha256=args.expected_contract_sha256,
                expected_source_revision=args.expected_source_revision,
            )
    except (ContractError, OSError) as error:
        print(
            f"astra harness: fresh database contract failed: {error}",
            file=os.sys.stderr,
        )
        return 78
    result = {
        "ok": True,
        "phase": args.phase,
        "database": database,
        "proof": str(proof),
    }
    if args.phase == "identity":
        result.update(identity)
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
