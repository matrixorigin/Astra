#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import json
import multiprocessing
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock


MODULE_PATH = Path(__file__).with_name("fresh_database_contract.py")
SPEC = importlib.util.spec_from_file_location(
    "astra_fresh_database_contract", MODULE_PATH
)
assert SPEC is not None and SPEC.loader is not None
contract = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(contract)
BROKER_SPEC = importlib.util.spec_from_file_location(
    "astra_harness_lifecycle_broker", MODULE_PATH.with_name("lifecycle_broker.py")
)
assert BROKER_SPEC is not None and BROKER_SPEC.loader is not None
broker_module = importlib.util.module_from_spec(BROKER_SPEC)
BROKER_SPEC.loader.exec_module(broker_module)


def runtime_state(counts: dict[str, int]) -> dict[str, object]:
    return {
        "counts": counts,
        "boot_metadata": {
            "core_component": "astra-core",
            "core_contract_version": "test",
            "table_contract_count": 1,
            "table_contract_sha256": "b" * 64,
        },
        "schema_inventory_sha256": contract._canonical_schema_manifest(
            Path(__file__).resolve().parents[2]
        )["sha256"],
    }


class FreshDatabaseContractTests(unittest.TestCase):
    def setUp(self):
        self.database_environment = mock.patch.dict(
            contract.os.environ,
            {
                "MATRIXONE_HOST": "127.0.0.1",
                "MATRIXONE_PORT": "6001",
                "MATRIXONE_USER": "sys:root",
            },
        )
        self.database_environment.start()

    def tearDown(self):
        self.database_environment.stop()

    def test_model_state_allows_an_explicit_non_thinking_selection(self):
        row = [["provider-neutral-model", "1", "none", "unsupported", "2026-08-28 00:00:00.000000"]]
        with (
            mock.patch.object(contract, "EXPECTED_MODEL", "provider-neutral-model"),
            mock.patch.object(contract, "EXPECTED_THINKING_MODE", "none"),
            mock.patch.object(contract, "_mysql_rows", return_value=row),
        ):
            self.assertEqual(
                contract._model_state("astra_tb_round5_0123456789abcdef"),
                {
                    "model_name": "provider-neutral-model",
                    "is_active": 1,
                    "thinking_capability": "none",
                    "thinking_probe_error": "unsupported",
                    "requested_thinking_mode": "none",
                    "checked_updated_at": "2026-08-28 00:00:00.000000",
                },
            )

    def test_model_state_keeps_high_thinking_fail_closed(self):
        row = [["deepseek-v4-flash", "1", "none", "", "2026-08-28 00:00:00.000000"]]
        with mock.patch.object(contract, "_mysql_rows", return_value=row):
            with self.assertRaisesRegex(contract.ContractError, "thinking:high"):
                contract._model_state("astra_tb_round5_0123456789abcdef")

    def test_mysql_password_is_never_put_in_argv(self):
        completed = mock.Mock(returncode=0, stdout="1\n", stderr="")
        environment = {
            "MATRIXONE_HOST": "127.0.0.1",
            "MATRIXONE_PORT": "6001",
            "MATRIXONE_USER": "root",
            "MATRIXONE_PASSWORD": "secret-sentinel",
            "PATH": "/usr/bin",
        }
        with (
            mock.patch.dict(contract.os.environ, environment, clear=True),
            mock.patch.object(
                contract.subprocess, "run", return_value=completed
            ) as run,
        ):
            self.assertEqual(contract._mysql_rows("SELECT 1"), [["1"]])
        argv = run.call_args.args[0]
        self.assertNotIn("secret-sentinel", json.dumps(argv))
        self.assertEqual(run.call_args.kwargs["env"]["MYSQL_PWD"], "secret-sentinel")

    def test_contract_is_one_use_and_fails_on_runtime_or_model_drift(self):
        database = "astra_tb_round5_0123456789abcdef"
        revision = "a" * 40
        empty = {table: 0 for table in contract.TEST_FIXTURE_TABLES}
        model = {
            "model_name": "deepseek-v4-flash",
            "is_active": 1,
            "thinking_capability": "both",
            "thinking_probe_error": None,
            "checked_updated_at": (
                contract.datetime.now(contract.UTC) + contract.timedelta(seconds=1)
            ).strftime("%Y-%m-%d %H:%M:%S.%f"),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proof = root / "proof.json"
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(contract, "_database_exists", return_value=False),
            ):
                contract.begin(root, database, proof)
            self.assertEqual(
                json.loads(proof.read_text())["phase"], "absent_before_seed"
            )
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(contract, "_database_exists", return_value=True),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(empty)
                ),
                mock.patch.object(contract, "_model_state", return_value=model),
            ):
                contract.seal(root, database, proof)
                sealed_payload = proof.read_text()
                identity = contract.sealed_contract_identity(root, database, proof)
                consumption = root / "consumption"
                consumption.mkdir(mode=0o700)
                contract.verify(
                    root,
                    database,
                    proof,
                    consumption,
                    expected_database_identity_sha256=identity[
                        "database_identity_sha256"
                    ],
                    expected_contract_sha256=identity["contract_sha256"],
                )
            self.assertEqual(json.loads(proof.read_text())["phase"], "consumed")
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(empty)
                ),
                mock.patch.object(contract, "_model_state", return_value=model),
            ):
                with self.assertRaises(contract.ContractError):
                    contract.verify(
                        root,
                        database,
                        proof,
                        consumption,
                        expected_database_identity_sha256=identity[
                            "database_identity_sha256"
                        ],
                        expected_contract_sha256=identity["contract_sha256"],
                    )

            dirty = {**empty, "agent_runs": 1}
            dirty_proof = root / "dirty-proof.json"
            dirty_proof.write_text(sealed_payload)
            dirty_consumption = root / "dirty-consumption"
            dirty_consumption.mkdir(mode=0o700)
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(dirty)
                ),
            ):
                with self.assertRaises(contract.ContractError):
                    contract.verify(
                        root,
                        database,
                        dirty_proof,
                        dirty_consumption,
                        expected_database_identity_sha256=identity[
                            "database_identity_sha256"
                        ],
                        expected_contract_sha256=identity["contract_sha256"],
                    )

            changed_model = {**model, "thinking_capability": "none"}
            model_proof = root / "model-proof.json"
            model_proof.write_text(sealed_payload)
            model_consumption = root / "model-consumption"
            model_consumption.mkdir(mode=0o700)
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(empty)
                ),
                mock.patch.object(contract, "_model_state", return_value=changed_model),
            ):
                with self.assertRaises(contract.ContractError):
                    contract.verify(
                        root,
                        database,
                        model_proof,
                        model_consumption,
                        expected_database_identity_sha256=identity[
                            "database_identity_sha256"
                        ],
                        expected_contract_sha256=identity["contract_sha256"],
                    )

            metadata_proof = root / "metadata-proof.json"
            metadata_proof.write_text(sealed_payload)
            metadata_consumption = root / "metadata-consumption"
            metadata_consumption.mkdir(mode=0o700)
            changed_state = runtime_state(empty)
            changed_state["boot_metadata"] = {
                **changed_state["boot_metadata"],
                "table_contract_sha256": "c" * 64,
            }
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(
                    contract, "_runtime_state", return_value=changed_state
                ),
            ):
                with self.assertRaisesRegex(contract.ContractError, "boot metadata"):
                    contract.verify(
                        root,
                        database,
                        metadata_proof,
                        metadata_consumption,
                        expected_database_identity_sha256=identity[
                            "database_identity_sha256"
                        ],
                        expected_contract_sha256=identity["contract_sha256"],
                    )

    def test_lifecycle_admission_stays_valid_across_bounded_preflight(self):
        database = "astra_tb_round5_0123456789abcdef"
        revision = "a" * 40
        empty = {table: 0 for table in contract.TEST_FIXTURE_TABLES}
        model = {
            "model_name": "deepseek-v4-flash",
            "is_active": 1,
            "thinking_capability": "both",
            "thinking_probe_error": None,
            "checked_updated_at": (
                contract.datetime.now(contract.UTC) + contract.timedelta(seconds=1)
            ).strftime("%Y-%m-%d %H:%M:%S.%f"),
        }
        real_datetime = contract.datetime

        class AfterPreflight(real_datetime):
            @classmethod
            def now(cls, tz=None):
                return real_datetime.now(tz) + contract.timedelta(minutes=16)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proof = root / "proof.json"
            consumption = root / "consumption"
            consumption.mkdir(mode=0o700)
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(
                    contract, "_database_exists", side_effect=[False, True]
                ),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(empty)
                ),
                mock.patch.object(contract, "_model_state", return_value=model),
            ):
                contract.begin(root, database, proof)
                contract.seal(root, database, proof)
                selected = contract.sealed_contract_identity(root, database, proof)
                with mock.patch.object(contract, "datetime", AfterPreflight):
                    with self.assertRaisesRegex(
                        contract.ContractError, "current benchmark launch window"
                    ):
                        contract.sealed_contract_identity(root, database, proof)
                    contract.verify(
                        root,
                        database,
                        proof,
                        consumption,
                        expected_database_identity_sha256=selected[
                            "database_identity_sha256"
                        ],
                        expected_contract_sha256=selected["contract_sha256"],
                    )
            self.assertEqual(json.loads(proof.read_text())["phase"], "consumed")

    def test_begin_refuses_existing_database_and_existing_proof(self):
        database = "astra_tb_round5_0123456789abcdef"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proof = root / "proof.json"
            with mock.patch.object(contract, "_database_exists", return_value=True):
                with self.assertRaises(contract.ContractError):
                    contract.begin(root, database, proof)
            proof.write_text("{}")
            with self.assertRaises(contract.ContractError):
                contract.begin(root, database, proof)

    def test_absence_admission_is_fenced_until_owned_server_seals_it(self):
        database = "astra_tb_round5_0123456789abcdef"
        revision = "a" * 40
        empty = {table: 0 for table in contract.TEST_FIXTURE_TABLES}
        model = {
            "model_name": "deepseek-v4-flash",
            "is_active": 1,
            "thinking_capability": "both",
            "thinking_probe_error": None,
            "checked_updated_at": (
                contract.datetime.now(contract.UTC) + contract.timedelta(seconds=1)
            ).strftime("%Y-%m-%d %H:%M:%S.%f"),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proof = root / "proof.json"
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(contract, "_database_exists", return_value=False),
            ):
                contract.begin(root, database, proof)
                admitted = contract.launch_identity(root, database, proof)
            self.assertEqual(admitted["phase"], "absent_before_seed")
            self.assertRegex(admitted["admission_sha256"], r"^[0-9a-f]{64}$")

            changed = json.loads(proof.read_text())
            changed["nonce"] = "f" * 64
            proof.write_text(json.dumps(changed))
            with mock.patch.object(contract, "_source_revision", return_value=revision):
                with self.assertRaisesRegex(
                    contract.ContractError, "changed after lifecycle admission"
                ):
                    contract.seal(
                        root, database, proof, admitted["admission_sha256"]
                    )

            # Restore the exact admitted proof and prove the owned seed can be
            # sealed only against that admission hash.
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(contract, "_database_exists", return_value=False),
            ):
                proof.unlink()
                contract.begin(root, database, proof)
                admitted = contract.launch_identity(root, database, proof)
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(contract, "_database_exists", return_value=True),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(empty)
                ),
                mock.patch.object(contract, "_model_state", return_value=model),
            ):
                contract.seal(root, database, proof, admitted["admission_sha256"])
                sealed = contract.launch_identity(root, database, proof)
            self.assertEqual(sealed["phase"], "sealed_ready")
            self.assertNotIn("admission_sha256", sealed)
            self.assertRegex(sealed["contract_sha256"], r"^[0-9a-f]{64}$")

    def test_copied_sealed_proofs_cannot_consume_one_database_twice(self):
        database = "astra_tb_round5_0123456789abcdef"
        revision = "a" * 40
        empty = {table: 0 for table in contract.TEST_FIXTURE_TABLES}
        model = {
            "model_name": "deepseek-v4-flash",
            "is_active": 1,
            "thinking_capability": "both",
            "thinking_probe_error": None,
            "checked_updated_at": (
                contract.datetime.now(contract.UTC) + contract.timedelta(seconds=1)
            ).strftime("%Y-%m-%d %H:%M:%S.%f"),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = root / "original-proof.json"
            first = root / "first-copy.json"
            second = root / "second-copy.json"
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(
                    contract, "_database_exists", side_effect=[False, True]
                ),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(empty)
                ),
                mock.patch.object(contract, "_model_state", return_value=model),
            ):
                contract.begin(root, database, original)
                contract.seal(root, database, original)
                sealed = original.read_text()
                first.write_text(sealed)
                second.write_text(sealed)
                identity = contract.sealed_contract_identity(root, database, original)
                consumption = root / "consumption"
                consumption.mkdir(mode=0o700)
                outcomes = []
                for proof in (first, second):
                    try:
                        contract.verify(
                            root,
                            database,
                            proof,
                            consumption,
                            expected_database_identity_sha256=identity[
                                "database_identity_sha256"
                            ],
                            expected_contract_sha256=identity["contract_sha256"],
                        )
                        outcomes.append("consumed")
                    except contract.ContractError:
                        outcomes.append("rejected")
            self.assertEqual(outcomes.count("consumed"), 1)
            self.assertEqual(outcomes.count("rejected"), 1)

    def test_two_processes_two_proof_paths_and_ports_have_one_lifecycle_winner(self):
        database = "astra_tb_round5_0123456789abcdef"
        revision = "a" * 40
        empty = {table: 0 for table in contract.TEST_FIXTURE_TABLES}
        model = {
            "model_name": "deepseek-v4-flash",
            "is_active": 1,
            "thinking_capability": "both",
            "thinking_probe_error": None,
            "checked_updated_at": (
                contract.datetime.now(contract.UTC) + contract.timedelta(seconds=1)
            ).strftime("%Y-%m-%d %H:%M:%S.%f"),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = root / "original.json"
            first = root / "proof-for-port-17012.json"
            second = root / "proof-for-port-17013.json"
            lifecycle = root / "lifecycle"
            lifecycle.mkdir(mode=0o700)
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(
                    contract, "_database_exists", side_effect=[False, True]
                ),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(empty)
                ),
                mock.patch.object(contract, "_model_state", return_value=model),
            ):
                contract.begin(root, database, original)
                contract.seal(root, database, original)
                sealed = original.read_text()
                first.write_text(sealed)
                second.write_text(sealed)
                context = multiprocessing.get_context("fork")
                start = context.Event()
                release = context.Event()
                outcomes = context.Queue()
                broker = broker_module

                def compete(proof: Path, api_port: int) -> None:
                    selected = contract.sealed_contract_identity(root, database, proof)
                    start.wait()
                    try:
                        lease = broker.LifecycleLease.acquire(
                            selected["database_identity_sha256"], api_port
                        )
                    except broker.LifecycleLeaseBusy:
                        outcomes.put((api_port, "locked_out"))
                        return
                    with lease:
                        contract._reserve_one_use_verification(
                            lifecycle,
                            selected["database_identity_sha256"],
                            selected["contract_sha256"],
                        )
                        outcomes.put((api_port, "reserved"))
                        release.wait()

                processes = [
                    context.Process(target=compete, args=(first, 17012)),
                    context.Process(target=compete, args=(second, 17013)),
                ]
                for process in processes:
                    process.start()
                start.set()
                observed = [outcomes.get(timeout=5), outcomes.get(timeout=5)]
                release.set()
                for process in processes:
                    process.join(timeout=5)
                    self.assertEqual(process.exitcode, 0)
            self.assertEqual({port for port, _ in observed}, {17012, 17013})
            self.assertEqual(
                sorted(outcome for _, outcome in observed),
                ["locked_out", "reserved"],
            )

    def test_identity_fences_proof_swap_and_unavailable_lifecycle_directory(self):
        database = "astra_tb_round5_0123456789abcdef"
        revision = "a" * 40
        empty = {table: 0 for table in contract.TEST_FIXTURE_TABLES}
        model = {
            "model_name": "deepseek-v4-flash",
            "is_active": 1,
            "thinking_capability": "both",
            "thinking_probe_error": None,
            "checked_updated_at": (
                contract.datetime.now(contract.UTC) + contract.timedelta(seconds=1)
            ).strftime("%Y-%m-%d %H:%M:%S.%f"),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proof = root / "proof.json"
            with (
                mock.patch.object(contract, "_source_revision", return_value=revision),
                mock.patch.object(
                    contract, "_database_exists", side_effect=[False, True]
                ),
                mock.patch.object(
                    contract, "_runtime_state", return_value=runtime_state(empty)
                ),
                mock.patch.object(contract, "_model_state", return_value=model),
            ):
                contract.begin(root, database, proof)
                contract.seal(root, database, proof)
                selected = contract.sealed_contract_identity(root, database, proof)
                swapped = json.loads(proof.read_text())
                swapped["nonce"] = "f" * 64
                swapped.pop("contract_sha256")
                canonical = json.dumps(
                    swapped, sort_keys=True, separators=(",", ":")
                ).encode()
                swapped["contract_sha256"] = contract.hashlib.sha256(
                    canonical
                ).hexdigest()
                proof.write_text(json.dumps(swapped))
                lifecycle = root / "lifecycle"
                lifecycle.mkdir(mode=0o700)
                with self.assertRaisesRegex(
                    contract.ContractError, "contract changed after lifecycle lease"
                ):
                    contract.verify(
                        root,
                        database,
                        proof,
                        lifecycle,
                        expected_database_identity_sha256=selected[
                            "database_identity_sha256"
                        ],
                        expected_contract_sha256=selected["contract_sha256"],
                    )
            with self.assertRaisesRegex(
                contract.ContractError, "lifecycle directory is unavailable"
            ):
                contract._validated_consumption_directory(root / "missing")
            unsafe = root / "shared-without-sticky-bit"
            unsafe.mkdir(mode=0o777)
            unsafe.chmod(0o777)
            with self.assertRaisesRegex(
                contract.ContractError, "lacks sticky-bit protection"
            ):
                contract._validated_consumption_directory(unsafe)

    def test_abstract_lifecycle_lease_is_versioned_cloexec_and_path_independent(self):
        broker = broker_module
        identity = "a" * 64
        with tempfile.TemporaryDirectory() as directory:
            replaced_path = Path(directory) / "obsolete-pathname.lock"
            replaced_path.write_text("first")
            with broker.LifecycleLease.acquire(identity, 17012) as lease:
                self.assertTrue(
                    lease.database_address.startswith("\0astra.harness.lifecycle.v1.")
                )
                self.assertTrue(
                    lease.gateway_address.startswith("\0astra.harness.lifecycle.v1.")
                )
                for descriptor in lease.descriptors:
                    self.assertFalse(os.get_inheritable(descriptor))
                replaced_path.unlink()
                replaced_path.write_text("replacement inode")
                with self.assertRaises(broker.LifecycleLeaseBusy):
                    broker.LifecycleLease.acquire(identity, 17013)

    def test_schema_inventory_is_closed_and_all_non_boot_tables_are_empty(self):
        canonical = contract._canonical_schema_inventory(MODULE_PATH.parents[2])
        self.assertIn("plans", canonical)
        self.assertIn("workspace_records", canonical)
        self.assertIn("session_checkpoints", canonical)
        self.assertIn("work_runtime_event_outbox", canonical)
        boot = contract.BOOT_METADATA_TABLES
        counts = {table: (1 if table in boot else 0) for table in canonical}
        # The sealed seed has exactly one production-admin identity: its
        # default user role plus the one bootstrap admin role.
        counts["auth_roles"] = 2
        counts["auth_user_roles"] = 2
        counts["raw_ref_scheme_registry"] = 9
        counts["preview_template_registry"] = 37
        contract._validate_closed_schema_counts(canonical, canonical, counts)
        changed_bootstrap_auth = {**counts, "auth_users": 2}
        with self.assertRaisesRegex(contract.ContractError, "auth_users"):
            contract._validate_closed_schema_counts(
                canonical, canonical, changed_bootstrap_auth
            )
        for dirty in (
            "plans",
            "workspace_records",
            "session_checkpoints",
            "work_runtime_event_outbox",
        ):
            with self.subTest(table=dirty):
                changed = {**counts, dirty: 1}
                with self.assertRaisesRegex(contract.ContractError, dirty):
                    contract._validate_closed_schema_counts(
                        canonical, canonical, changed
                    )
        with self.assertRaisesRegex(contract.ContractError, "unknown"):
            contract._validate_closed_schema_counts(
                canonical, canonical | {"unknown_runtime_table"}, counts
            )

    def test_default_server_schema_allows_only_source_classified_conditional_tables(
        self,
    ):
        repo = MODULE_PATH.parents[2]
        manifest = contract._canonical_schema_manifest(repo)
        canonical = set(manifest["tables"])
        conditional = set(manifest["conditional_tables"])
        self.assertEqual(
            conditional,
            {
                "llm_provider_admission_pacing",
                "llm_provider_admission_windows",
            },
        )
        actual = canonical - conditional
        counts = {table: 0 for table in actual}
        counts.update(
            {
                "astra_schema_contracts": 1,
                "astra_schema_table_contracts": 1,
                "infra_llm_models": 1,
                "maintenance_sweep_cursors": 1,
                "preview_template_registry": 37,
                "raw_ref_scheme_registry": 9,
                "sweeper_leases": 1,
            }
        )

        def rows(sql, _database=None):
            if "FROM information_schema.tables" in sql:
                return [[table] for table in sorted(actual)]
            if "COUNT(*)" in sql:
                for table in conditional:
                    self.assertNotIn(f"FROM `{table}`", sql)
                return [[table, str(counts[table])] for table in sorted(actual)]
            raise AssertionError(sql)

        with (
            mock.patch.object(contract, "_mysql_rows", side_effect=rows),
            mock.patch.object(
                contract,
                "_validate_boot_metadata",
                return_value={"evidence": "default-server-schema"},
            ),
        ):
            state = contract._runtime_state("fresh_default", repo)
        self.assertEqual(set(state["counts"]), actual)
        self.assertEqual(state["boot_metadata"]["evidence"], "default-server-schema")

        missing_required = actual - {"plans"}
        missing_counts = {
            table: count for table, count in counts.items() if table in missing_required
        }
        with self.assertRaisesRegex(contract.ContractError, "plans"):
            contract._validate_closed_schema_counts(
                canonical,
                missing_required,
                missing_counts,
                optional_absent=conditional,
            )
        partial_conditional = actual | {"llm_provider_admission_windows"}
        with self.assertRaisesRegex(contract.ContractError, "conditional.*pacing"):
            contract._validate_closed_schema_counts(
                canonical,
                partial_conditional,
                {**counts, "llm_provider_admission_windows": 0},
                optional_absent=conditional,
            )

    def test_production_baseline_registries_are_exact_counted_and_hashed(self):
        repo = MODULE_PATH.parents[2]
        expected = contract._canonical_baseline_registry_rows(repo)

        def rows(sql, _database=None):
            if "FROM raw_ref_scheme_registry" in sql:
                return [list(row) for row in expected["raw_ref_scheme_registry"]]
            if "FROM preview_template_registry" in sql:
                return [list(row) for row in expected["preview_template_registry"]]
            raise AssertionError(sql)

        with mock.patch.object(contract, "_mysql_rows", side_effect=rows):
            sealed = contract._validate_baseline_registry_rows("fresh", repo)
        self.assertEqual(sealed["raw_ref_scheme_registry_count"], 9)
        self.assertEqual(sealed["preview_template_registry_count"], 37)
        self.assertRegex(sealed["raw_ref_scheme_registry_sha256"], r"^[0-9a-f]{64}$")
        self.assertRegex(sealed["preview_template_registry_sha256"], r"^[0-9a-f]{64}$")

        mutated = {
            table: [list(row) for row in rows] for table, rows in expected.items()
        }
        mutated["preview_template_registry"][0][3] = "9999"

        def changed_rows(sql, _database=None):
            table = (
                "raw_ref_scheme_registry"
                if "FROM raw_ref_scheme_registry" in sql
                else "preview_template_registry"
            )
            return mutated[table]

        with mock.patch.object(contract, "_mysql_rows", side_effect=changed_rows):
            with self.assertRaisesRegex(contract.ContractError, "exact source-owned"):
                contract._validate_baseline_registry_rows("fresh", repo)

    def test_runtime_system_baseline_is_source_owned_and_exact(self):
        repo = MODULE_PATH.parents[2]
        expected = contract._canonical_runtime_system_baseline(repo)
        self.assertEqual(expected["sweeper_name"], "runtime_sweepers")
        self.assertEqual(
            expected["maintenance_cursor_name"], "tool_invocation_compaction_v1"
        )
        self.assertEqual(
            expected["maintenance_cursor_epoch"], "1970-01-01 00:00:00.000000"
        )
        sweeper = [
            "runtime_sweepers",
            "astra-runtime-4ab24073-f95c-4f7d-a811-5a8a9236f3ea",
            "2026-08-24 13:01:00.000000",
            "0",
            "2026-08-24 13:00:00.000000",
            "2026-08-24 13:00:00.000000",
        ]
        cursor = [
            "tool_invocation_compaction_v1",
            "1970-01-01 00:00:00.000000",
            "",
            "",
            "0",
            "2026-08-24 13:00:00.000000",
            "2026-08-24 13:00:00.000001",
        ]

        def rows(sql, _database=None):
            if "FROM sweeper_leases" in sql:
                return [sweeper]
            if "FROM maintenance_sweep_cursors" in sql:
                return [cursor]
            raise AssertionError(sql)

        with (
            mock.patch.dict(contract.os.environ, {}, clear=True),
            mock.patch.object(contract, "_mysql_rows", side_effect=rows),
        ):
            sealed = contract._validate_runtime_system_baseline("fresh", repo)
        self.assertEqual(sealed["sweeper_leases_count"], 1)
        self.assertEqual(sealed["maintenance_sweep_cursors_count"], 1)
        self.assertRegex(sealed["runtime_system_baseline_sha256"], r"^[0-9a-f]{64}$")

        # A running seed server legitimately refreshes its TTL (and a later
        # replica may take the lease).  Those lifecycle facts are validated,
        # but must not invalidate a sealed clean-database proof.
        refreshed_sweeper = [
            sweeper[0],
            "astra-runtime-0a0c3ce1-c544-4cae-9bf7-0eeef9a3b340",
            "2026-08-24 13:02:00.000000",
            sweeper[3],
            sweeper[4],
            sweeper[5],
        ]
        refreshed_cursor = [
            *cursor[:5],
            cursor[5],
            "2026-08-24 13:01:00.000001",
        ]
        with (
            mock.patch.dict(contract.os.environ, {}, clear=True),
            mock.patch.object(
                contract,
                "_mysql_rows",
                side_effect=lambda sql, _database=None: (
                    [refreshed_sweeper]
                    if "FROM sweeper_leases" in sql
                    else [refreshed_cursor]
                ),
            ),
        ):
            refreshed = contract._validate_runtime_system_baseline("fresh", repo)
        self.assertEqual(
            refreshed["runtime_system_baseline_sha256"],
            sealed["runtime_system_baseline_sha256"],
        )

        for changed in (
            [
                sweeper,
                [
                    "unexpected_sweeper",
                    sweeper[1],
                    *sweeper[2:],
                ],
            ],
            [[*sweeper[:1], "wrong-owner", *sweeper[2:]]],
        ):
            with self.subTest(rows=changed):
                with (
                    mock.patch.dict(contract.os.environ, {}, clear=True),
                    mock.patch.object(
                        contract,
                        "_mysql_rows",
                        side_effect=lambda sql, _database=None: (
                            changed if "FROM sweeper_leases" in sql else [cursor]
                        ),
                    ),
                    self.assertRaises(contract.ContractError),
                ):
                    contract._validate_runtime_system_baseline("fresh", repo)

        with (
            mock.patch.dict(contract.os.environ, {}, clear=True),
            mock.patch.object(
                contract,
                "_mysql_rows",
                side_effect=lambda sql, _database=None: (
                    [sweeper]
                    if "FROM sweeper_leases" in sql
                    else [[*cursor[:4], "1", *cursor[5:]]]
                ),
            ),
            self.assertRaisesRegex(contract.ContractError, "maintenance"),
        ):
            contract._validate_runtime_system_baseline("fresh", repo)


if __name__ == "__main__":
    unittest.main()
