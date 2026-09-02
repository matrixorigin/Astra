#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.harness import verifier_readiness as readiness


MODULE_PATH = Path(__file__).with_name("preflight.py")
RUNNER_PATH = Path(__file__).with_name("run_terminal_bench_current.sh")
SPEC = importlib.util.spec_from_file_location("astra_harness_preflight", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
preflight = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(preflight)


def catalog_item(
    name: str, *, active: bool = True, capability: str | None = "both"
) -> dict:
    return {
        "name": name,
        "offering_id": "offer-" + name,
        "is_active": active,
        "thinking_capability": capability,
    }


def catalog_page(items: list[dict], *, total: int | None = None, cursor=None) -> dict:
    return {
        "items": items,
        "next_cursor": cursor,
        "limit": 50,
        "total": len(items) if total is None else total,
        "catalog_revision": "sha256:test",
    }


class FakeResponse:
    def __init__(self, payload: dict):
        self.payload = json.dumps(payload).encode()

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def read(self, _limit: int) -> bytes:
        return self.payload


class FakeOpener:
    def __init__(self, payloads: list[dict]):
        self.payloads = iter(payloads)
        self.requests = []

    def open(self, request, timeout: float):
        self.requests.append((request, timeout))
        return FakeResponse(next(self.payloads))


class PreflightTests(unittest.TestCase):
    @staticmethod
    def _verifier_readiness_config(
        root: Path, builds: list[float], verifier_timeouts: list[float]
    ) -> Path:
        if len(builds) != len(verifier_timeouts):
            raise ValueError("test budgets must have equal lengths")
        paths = []
        for index, (build, verifier_timeout) in enumerate(
            zip(builds, verifier_timeouts, strict=True)
        ):
            task = root / f"task-{index}"
            task.mkdir()
            (task / "task.toml").write_text(
                "[environment]\n"
                f"build_timeout_sec={build}\n"
                "[verifier]\n"
                f"timeout_sec={verifier_timeout}\n"
            )
            paths.append(task)
        config = root / "config.json"
        config.write_text(
            json.dumps({"tasks": [{"path": str(path)} for path in paths]})
        )
        return config

    def benchmark_plan(self, paths: list[Path]) -> dict:
        return {
            "jobs_dir": str(paths[0].parent / "jobs"),
            "n_attempts": 1,
            "install_only": False,
            "timeout_multiplier": 1.0,
            "agent_timeout_multiplier": None,
            "verifier_timeout_multiplier": None,
            "agent_setup_timeout_multiplier": None,
            "environment_build_timeout_multiplier": None,
            "debug": False,
            "quiet": False,
            "n_concurrent_trials": 1,
            "retry": {"max_retries": 0},
            "environment": {"type": "docker", "force_build": False, "delete": True},
            "verifier": {"disable": False},
            "metrics": [],
            "agents": [
                {
                    "name": "harbor_adapter:Astra",
                    "model_name": "deepseek-v4-flash(thinking:high)",
                    "env": {
                        "ASTRA_EXPECTED_BUILD_GIT_SHA": "a" * 40,
                        "ASTRA_HARNESS_BINARY_SHA256": "b" * 64,
                        "ASTRA_HARNESS_BUILD_PROFILE": "debug",
                        "ASTRA_HARNESS_TASK_SET_SHA256": preflight.benchmark_task_set_sha256(
                            paths
                        )[0],
                        "ASTRA_HARBOR_HTTP_PROXY": "${ASTRA_HARBOR_HTTP_PROXY}",
                        "ASTRA_HARBOR_HTTPS_PROXY": "${ASTRA_HARBOR_HTTPS_PROXY}",
                    },
                }
            ],
            "datasets": [],
            "tasks": [{"path": str(path)} for path in paths],
            "artifacts": [],
            "extra_instruction_paths": [],
            "source_jobs": [],
        }

    def fetch(self, payloads: list[dict], requirement=("deepseek-v4-flash", "high")):
        opener = FakeOpener(payloads)
        with mock.patch.object(
            preflight.urllib.request, "build_opener", return_value=opener
        ):
            result = preflight.fetch_model_catalog(
                "http://127.0.0.1:17016/models", "secret-sentinel", [requirement]
            )
        self.assertNotIn("secret-sentinel", result[1])
        for request, _ in opener.requests:
            self.assertNotIn("secret-sentinel", request.full_url)
        return result, opener

    def test_held_fd_paths_are_not_collapsed_to_replaceable_pathnames(self):
        with tempfile.TemporaryDirectory() as directory:
            descriptor = os.open(directory, os.O_RDONLY)
            try:
                path = Path(f"/proc/{os.getpid()}/fd/{descriptor}")
                self.assertEqual(preflight.lexical_absolute(path), path)
                self.assertNotEqual(preflight.lexical_absolute(path), path.resolve())
            finally:
                os.close(descriptor)

    def test_thinking_suffix_is_removed_only_when_valid(self):
        self.assertEqual(
            preflight.resolve_model_selector("deepseek-v4-flash(thinking:high)"),
            ("deepseek-v4-flash", "high"),
        )
        self.assertEqual(
            preflight.resolve_model_selector("model(thinking:low)"),
            ("model", "low"),
        )
        self.assertEqual(
            preflight.resolve_model_selector("model(thinking:budget:4096)"),
            ("model", "budget:4096"),
        )
        self.assertEqual(
            preflight.resolve_model_selector("literal(thinking:invalid)"),
            ("literal(thinking:invalid)", None),
        )

    def test_model_plan_requires_one_astra_high_thinking_agent(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config.json"

            def write(agents):
                config.write_text(json.dumps({"agents": agents}))
                return preflight.configured_model_requirements(config)

            exact = {
                "name": "harbor_adapter:Astra",
                "model_name": "deepseek-v4-flash(thinking:high)",
            }
            ok, requirements, detail = write([exact])
            self.assertTrue(ok, detail)
            self.assertEqual(requirements, [("deepseek-v4-flash", "high")])
            qwen = {**exact, "model_name": "qwen3.7-max(thinking:high)"}
            ok, requirements, detail = write([qwen])
            self.assertTrue(ok, detail)
            self.assertEqual(requirements, [("qwen3.7-max", "high")])
            for selector in (
                "deepseek-v4-flash(thinking:low)",
                "deepseek-v4-flash(thinking:medium)",
                "deepseek-v4-flash(thinking:max)",
                "deepseek-v4-flash(thinking)",
            ):
                ok, _, _ = write([{**exact, "model_name": selector}])
                self.assertFalse(ok, selector)
            ok, _, _ = write([exact, exact])
            self.assertFalse(ok)
            ok, _, _ = write([{**exact, "name": "nop"}])
            self.assertFalse(ok)

    def test_empty_or_other_active_catalog_fails_exact_selection(self):
        (ok, _), _ = self.fetch([catalog_page([])])
        self.assertFalse(ok)
        (ok, detail), _ = self.fetch([catalog_page([catalog_item("another-model")])])
        self.assertFalse(ok)
        self.assertIn("expected one active exact Offering", detail)

    def test_inactive_or_noncontrollable_thinking_selection_fails(self):
        (ok, _), _ = self.fetch(
            [catalog_page([catalog_item("deepseek-v4-flash", active=False)])]
        )
        self.assertFalse(ok)
        (ok, detail), _ = self.fetch(
            [catalog_page([catalog_item("deepseek-v4-flash", capability="none")])]
        )
        self.assertFalse(ok)
        self.assertIn("requested controllable thinking", detail)
        (ok, _), _ = self.fetch(
            [
                catalog_page(
                    [catalog_item("deepseek-v4-flash", capability="native_only")]
                )
            ]
        )
        self.assertFalse(ok)

    def test_catalog_is_drained_before_exact_selection(self):
        cursor = {
            "provider": "openai",
            "model_name": "another-model",
            "model_id": "offer-another-model",
        }
        pages = [
            catalog_page([catalog_item("another-model")], total=2, cursor=cursor),
            catalog_page([catalog_item("deepseek-v4-flash")], total=2),
        ]
        (ok, detail), opener = self.fetch(pages)
        self.assertTrue(ok, detail)
        self.assertEqual(len(opener.requests), 2)
        self.assertIn(
            "after_offering_id=offer-another-model", opener.requests[1][0].full_url
        )

    def test_repeated_catalog_cursor_fails_closed(self):
        cursor = {
            "provider": "openai",
            "model_name": "another-model",
            "model_id": "offer-another-model",
        }
        page = catalog_page([catalog_item("another-model")], total=2, cursor=cursor)
        (ok, detail), _ = self.fetch([page, page])
        self.assertFalse(ok)
        self.assertIn("cursor repeated", detail)

    def test_task_gate_requires_three_unique_equal_budget_tasks(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for index in range(3):
                path = root / f"task-{index}"
                path.mkdir()
                (path / "task.toml").write_text("[agent]\ntimeout_sec = 900\n")
                paths.append(path)
            config = root / "config.json"
            plan = self.benchmark_plan(paths)
            config.write_text(json.dumps(plan))
            shape_ok, shape_detail = preflight.validate_benchmark_source_config(config)
            self.assertTrue(shape_ok, shape_detail)
            ok, detail = preflight.validate_benchmark_tasks(config)
            self.assertTrue(ok, detail)
            (paths[0] / "task.toml").write_text("[agent]\ntimeout_sec = 901\n")
            ok, _ = preflight.validate_benchmark_tasks(config)
            self.assertFalse(ok, "task-tree tampering must invalidate provenance")
            (paths[0] / "task.toml").write_text("[agent]\ntimeout_sec = 900\n")
            config.write_text(
                json.dumps({**plan, "tasks": [{"path": str(paths[0])}] * 3})
            )
            ok, _ = preflight.validate_benchmark_tasks(config)
            self.assertFalse(ok)
            for invalid in (
                {"n_attempts": 2},
                {"datasets": [{"path": str(root)}]},
                {"source_jobs": [{"type": "local", "path": str(root)}]},
                {"install_only": True},
                {"verifier": {"disable": True}},
                {"verifier": {}},
            ):
                config.write_text(json.dumps({**plan, **invalid}))
                ok, _ = preflight.validate_benchmark_tasks(config)
                self.assertFalse(ok, invalid)

    def test_source_config_is_a_closed_scored_run_schema(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for index in range(3):
                path = root / f"task-{index}"
                path.mkdir()
                (path / "task.toml").write_text("[agent]\ntimeout_sec = 900\n")
                paths.append(path)
            config = root / "config.json"
            plan = self.benchmark_plan(paths)

            config.write_text(json.dumps(plan))
            ok, detail = preflight.validate_benchmark_source_config(config)
            self.assertTrue(ok, detail)

            sneaks = (
                {"artifacts": ["/workspace/answer"]},
                {"extra_instruction_paths": ["prompt.md"]},
                {"timeout_multiplier": 2.0},
                {"agent_timeout_multiplier": 2.0},
                {"n_concurrent_trials": 2},
                {"retry": {"max_retries": 1}},
                {"plugins": ["hidden:Plugin"]},
                {"environment": {"type": "docker", "env": {"TOKEN": "x"}}},
                {"verifier": {"disable": False, "kwargs": {"reward": 1}}},
            )
            for sneak in sneaks:
                config.write_text(json.dumps({**plan, **sneak}))
                ok, _ = preflight.validate_benchmark_source_config(config)
                self.assertFalse(ok, sneak)

            agent_sneaks = (
                {"skills": ["/tmp/skill"]},
                {"resume_trajectory": True},
                {"load_trajectory": "prior.jsonl"},
                {"mcp_servers": [{"name": "hidden"}]},
                {"kwargs": {"system_prompt": "easier"}},
                {"override_timeout_sec": 3600},
                {"extra_allowed_hosts": ["example.com"]},
                {"env": {**plan["agents"][0]["env"], "INJECTED": "1"}},
            )
            for sneak in agent_sneaks:
                agent = {**plan["agents"][0], **sneak}
                config.write_text(json.dumps({**plan, "agents": [agent]}))
                ok, _ = preflight.validate_benchmark_source_config(config)
                self.assertFalse(ok, sneak)

            task = {**plan["tasks"][0], "git_url": "https://example.invalid/task"}
            config.write_text(json.dumps({**plan, "tasks": [task, *plan["tasks"][1:]]}))
            ok, _ = preflight.validate_benchmark_source_config(config)
            self.assertFalse(ok)

    def test_finalized_config_accepts_only_exact_verifier_network_projection(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for index in range(3):
                path = root / f"task-{index}"
                path.mkdir()
                (path / "task.toml").write_text("[agent]\ntimeout_sec = 900\n")
                paths.append(path)
            config = root / "config.json"
            plan = self.benchmark_plan(paths)
            projection = {
                "HTTP_PROXY": "http://proxy.example:8080",
                "HTTPS_PROXY": "http://proxy.example:8080",
                "http_proxy": "http://proxy.example:8080",
                "https_proxy": "http://proxy.example:8080",
                "NO_PROXY": "localhost,127.0.0.1,::1,172.17.0.1",
                "no_proxy": "localhost,127.0.0.1,::1,172.17.0.1",
            }

            config.write_text(json.dumps(plan))
            ok, detail = preflight.validate_benchmark_source_config(config)
            self.assertTrue(ok, detail)

            finalized = {
                **plan,
                "verifier": {"disable": False, "env": projection},
            }
            config.write_text(json.dumps(finalized))
            ok, _ = preflight.validate_benchmark_source_config(config)
            self.assertFalse(ok, "the source contract must remain closed")
            ok, detail = preflight.validate_benchmark_finalized_config(
                config, projection
            )
            self.assertTrue(ok, detail)

            for invalid in (
                {**projection, "OPENAI_API_KEY": "provider-secret"},
                {**projection, "MATRIXONE_PASSWORD": "database-secret"},
                {**projection, "NO_PROXY": "*"},
                {**projection, "HTTPS_PROXY": "http://other.example:8080"},
            ):
                config.write_text(
                    json.dumps({**plan, "verifier": {"disable": False, "env": invalid}})
                )
                ok, _ = preflight.validate_benchmark_finalized_config(
                    config, projection
                )
                self.assertFalse(ok, invalid)

    def test_verifier_network_projection_is_allowlisted(self):
        environment = {
            "http_proxy": "http://proxy.example:8080",
            "https_proxy": "http://proxy.example:8080",
            "NO_PROXY": "*,internal.example",
            "OPENAI_API_KEY": "provider-secret",
            "MATRIXONE_PASSWORD": "database-secret",
        }
        projection = preflight.verifier_network_projection(environment)
        self.assertEqual(set(projection), set(preflight.VERIFIER_NETWORK_ENV_KEYS))
        self.assertEqual(projection["NO_PROXY"], preflight.VERIFIER_NO_PROXY)
        serialized = json.dumps(projection)
        self.assertNotIn("provider-secret", serialized)
        self.assertNotIn("database-secret", serialized)
        self.assertNotIn("internal.example", serialized)

        direct = preflight.verifier_network_projection(
            {
                "HTTP_PROXY": "",
                "HTTPS_PROXY": "",
                "http_proxy": "",
                "https_proxy": "",
            }
        )
        self.assertEqual(direct["HTTP_PROXY"], "")
        self.assertEqual(direct["HTTPS_PROXY"], "")
        self.assertEqual(direct["NO_PROXY"], preflight.VERIFIER_NO_PROXY)

        for invalid in (
            {
                "HTTP_PROXY": "http://first.example:8080",
                "http_proxy": "http://second.example:8080",
                "HTTPS_PROXY": "http://proxy.example:8080",
            },
            {
                "http_proxy": "http://user:password@proxy.example:8080",
                "https_proxy": "http://proxy.example:8080",
            },
        ):
            with self.assertRaises(ValueError):
                preflight.verifier_network_projection(invalid)

    def test_verifier_readiness_record_binds_exact_task_image_namespace_and_env(self):
        projection = {
            "HTTP_PROXY": "http://proxy.example:8080",
            "HTTPS_PROXY": "http://proxy.example:8080",
            "http_proxy": "http://proxy.example:8080",
            "https_proxy": "http://proxy.example:8080",
            "NO_PROXY": preflight.VERIFIER_NO_PROXY,
            "no_proxy": preflight.VERIFIER_NO_PROXY,
        }
        setup_plan = {
            "policy": preflight.DEPENDENCY_SETUP_POLICY,
            "shell": "bash",
            "runner_family": "pytest",
            "rendered_command_sha256": None,
            "scoring_command_sha256": "4" * 64,
            "fixtures": [],
            "steps": [],
        }
        record = {
            "schema": "astra.harness.verifier_readiness.v6",
            "task_sha256": "a" * 64,
            "environment_id": "b" * 64,
            "image_id": "sha256:" + "c" * 64,
            "repo_digests": ["example/task@sha256:" + "d" * 64],
            "image_source": "pulled",
            "environment_lifecycle": "started_deleted",
            "verifier_env_sha256": preflight.canonical_json_sha256(projection),
            "verifier_env_keys": sorted(projection),
            "official_verifier": {
                "test_sha256": "e" * 64,
                "execution_mode": "container_lifecycle_non_scoring",
                "terminal_boundary_reached": True,
                "score_eligible": False,
                "reward_disposition": "scored_trial_only",
                "environment_deleted": True,
            },
            "dependency_setup_probe": {
                "mode": "no_setup",
                "plan": setup_plan,
                "plan_sha256": preflight.canonical_json_sha256(setup_plan),
                "budget_seconds": 900.0,
                "invocations": [
                    {
                        "sequence": 0,
                        "kind": "readability_probe",
                        "command_sha256": "0" * 64,
                        "exit_code": 0,
                    }
                ],
                "batches": [],
                "batches_sha256": preflight.canonical_json_sha256([]),
                "sources": [],
                "sources_sha256": preflight.canonical_json_sha256([]),
                "fixtures": [],
                "fixtures_sha256": preflight.canonical_json_sha256([]),
                "executions": [],
                "scoring_invoked": False,
            },
        }
        ok, detail = preflight.validate_verifier_readiness_record(
            record,
            expected_task_sha256="a" * 64,
            expected_test_sha256="e" * 64,
            expected_projection=projection,
            expected_dependency_setup_seconds=900.0,
        )
        self.assertTrue(ok, detail)
        for expected in (
            {
                "expected_task_sha256": "f" * 64,
                "expected_test_sha256": "e" * 64,
                "expected_projection": projection,
            },
            {
                "expected_task_sha256": "a" * 64,
                "expected_test_sha256": "f" * 64,
                "expected_projection": projection,
            },
            {
                "expected_task_sha256": "a" * 64,
                "expected_test_sha256": "e" * 64,
                "expected_projection": {
                    **projection,
                    "HTTP_PROXY": "http://different.example:8080",
                    "http_proxy": "http://different.example:8080",
                },
            },
        ):
            ok, _ = preflight.validate_verifier_readiness_record(
                record,
                **expected,
                expected_dependency_setup_seconds=900.0,
            )
            self.assertFalse(ok, expected)
        ok, _ = preflight.validate_verifier_readiness_record(
            record,
            expected_task_sha256="a" * 64,
            expected_test_sha256="e" * 64,
            expected_projection=projection,
            expected_dependency_setup_seconds=901.0,
        )
        self.assertFalse(ok)
        for mutation in (
            {"schema": "astra.harness.verifier_readiness.v4"},
            {"image_id": "latest"},
            {"repo_digests": ["example/task:latest"]},
            {"environment_lifecycle": "scored_trial"},
            {"verifier_env_keys": [*sorted(projection), "OPENAI_API_KEY"]},
            {
                "official_verifier": {
                    **record["official_verifier"],
                    "execution_mode": "scored_trial",
                }
            },
            {
                "official_verifier": {
                    **record["official_verifier"],
                    "reward_disposition": "discarded",
                }
            },
            {
                "official_verifier": {
                    **record["official_verifier"],
                    "score_eligible": True,
                }
            },
            {
                "official_verifier": {
                    **record["official_verifier"],
                    "environment_deleted": False,
                }
            },
            {
                "dependency_setup_probe": {
                    **record["dependency_setup_probe"],
                    "plan_sha256": "f" * 64,
                }
            },
            {
                "dependency_setup_probe": {
                    **record["dependency_setup_probe"],
                    "scoring_invoked": True,
                }
            },
        ):
            ok, _ = preflight.validate_verifier_readiness_record(
                {**record, **mutation},
                expected_task_sha256="a" * 64,
                expected_test_sha256="e" * 64,
                expected_projection=projection,
                expected_dependency_setup_seconds=900.0,
            )
            self.assertFalse(ok, mutation)

        runtime_plan = readiness.build_dependency_setup_plan(
            "cp /tests/preflight.py .\nnpm ci\nnpm test\n",
            Path(__file__).with_suffix(".sh"),
            tests_source_dir=Path(__file__).parent,
        )
        executed_plan = runtime_plan.receipt_plan()
        self.assertEqual(executed_plan["runner_family"], "npm_test")
        self.assertEqual(len(executed_plan["fixtures"]), 1)
        self.assertEqual(executed_plan["steps"][0]["kind"], "fixture_stage")
        batch_command = readiness._render_dependency_setup_batch(
            runtime_plan,
            1,
            len(runtime_plan.steps),
            readiness.EnvironmentDelta(),
        )
        batch_sha256 = hashlib.sha256(batch_command.encode()).hexdigest()
        fixture = {
            "sequence": executed_plan["fixtures"][0]["sequence"],
            "step_index": executed_plan["fixtures"][0]["step_index"],
            "cwd_sha256": hashlib.sha256(b"/app").hexdigest(),
            "source_sha256": executed_plan["fixtures"][0]["source_path_sha256"],
            "destination_sha256": hashlib.sha256(b"/app/preflight.py").hexdigest(),
            "content_sha256": executed_plan["fixtures"][0]["source_sha256"],
            "content_bytes": (Path(__file__).parent / "preflight.py").stat().st_size,
            "destination_probe_command_sha256": "5" * 64,
            "stat_command_sha256": "6" * 64,
            "digest_command_sha256": "7" * 64,
        }
        executed_probe = {
            "mode": "executed",
            "plan": executed_plan,
            "plan_sha256": preflight.canonical_json_sha256(executed_plan),
            "budget_seconds": 900.0,
            "invocations": [
                {
                    "sequence": 0,
                    "kind": "readability_probe",
                    "command_sha256": "0" * 64,
                    "exit_code": 0,
                },
                {
                    "sequence": 1,
                    "kind": "fixture_workdir_probe",
                    "command_sha256": hashlib.sha256(b"pwd -P").hexdigest(),
                    "exit_code": 0,
                },
                {
                    "sequence": 2,
                    "kind": "fixture_destination_probe",
                    "command_sha256": fixture["destination_probe_command_sha256"],
                    "exit_code": 0,
                },
                {
                    "sequence": 3,
                    "kind": "fixture_stat",
                    "command_sha256": fixture["stat_command_sha256"],
                    "exit_code": 0,
                },
                {
                    "sequence": 4,
                    "kind": "fixture_digest",
                    "command_sha256": fixture["digest_command_sha256"],
                    "exit_code": 0,
                },
                {
                    "sequence": 5,
                    "kind": "dependency_setup",
                    "command_sha256": batch_sha256,
                    "exit_code": 0,
                },
            ],
            "batches": [
                {
                    "start": 1,
                    "end": len(executed_plan["steps"]),
                    "command_sha256": batch_sha256,
                    "step_exit_codes": [0] * (len(executed_plan["steps"]) - 1),
                }
            ],
            "batches_sha256": preflight.canonical_json_sha256(
                [
                    {
                        "start": 1,
                        "end": len(executed_plan["steps"]),
                        "command_sha256": batch_sha256,
                        "step_exit_codes": [0]
                        * (len(executed_plan["steps"]) - 1),
                    }
                ]
            ),
            "sources": [],
            "sources_sha256": preflight.canonical_json_sha256([]),
            "fixtures": [fixture],
            "fixtures_sha256": preflight.canonical_json_sha256([fixture]),
            "executions": [
                {
                    "index": index,
                    "kind": step["kind"],
                    "command_sha256": step["command_sha256"],
                    "exit_code": 0,
                }
                for index, step in enumerate(executed_plan["steps"])
            ],
            "scoring_invoked": False,
        }
        executed_record = {**record, "dependency_setup_probe": executed_probe}
        ok, detail = preflight.validate_verifier_readiness_record(
            executed_record,
            expected_task_sha256="a" * 64,
            expected_test_sha256="e" * 64,
            expected_projection=projection,
            expected_dependency_setup_seconds=900.0,
        )
        self.assertTrue(ok, detail)
        failed_batch = {
            **executed_probe["batches"][0],
            "step_exit_codes": [
                17,
                *executed_probe["batches"][0]["step_exit_codes"][1:],
            ],
        }
        strict_failure_probe = {
            **executed_probe,
            "batches": [failed_batch],
            "batches_sha256": preflight.canonical_json_sha256([failed_batch]),
        }
        ok, _ = preflight.validate_verifier_readiness_record(
            {**record, "dependency_setup_probe": strict_failure_probe},
            expected_task_sha256="a" * 64,
            expected_test_sha256="e" * 64,
            expected_projection=projection,
            expected_dependency_setup_seconds=900.0,
        )
        self.assertFalse(ok)
        for invalid_exit in (17, False, 0.0):
            failed_execution = {
                **executed_probe,
                "executions": [
                    executed_probe["executions"][0],
                    {
                        **executed_probe["executions"][1],
                        "exit_code": invalid_exit,
                    },
                ],
            }
            ok, _ = preflight.validate_verifier_readiness_record(
                {**record, "dependency_setup_probe": failed_execution},
                expected_task_sha256="a" * 64,
                expected_test_sha256="e" * 64,
                expected_projection=projection,
                expected_dependency_setup_seconds=900.0,
            )
            self.assertFalse(ok, invalid_exit)

        scoring_invocation = {
            "sequence": 2,
            "kind": "scoring",
            "command_sha256": "3" * 64,
            "exit_code": 0,
        }
        for claimed_scoring in (False, True):
            observed_scoring = {
                **executed_probe,
                "invocations": [
                    *executed_probe["invocations"],
                    scoring_invocation,
                ],
                "scoring_invoked": claimed_scoring,
            }
            ok, _ = preflight.validate_verifier_readiness_record(
                {**record, "dependency_setup_probe": observed_scoring},
                expected_task_sha256="a" * 64,
                expected_test_sha256="e" * 64,
                expected_projection=projection,
                expected_dependency_setup_seconds=900.0,
            )
            self.assertFalse(ok, claimed_scoring)

        disguised_scoring = {
            **executed_probe,
            "invocations": [
                executed_probe["invocations"][0],
                {
                    **executed_probe["invocations"][1],
                    "command_sha256": executed_plan["scoring_command_sha256"],
                },
            ],
            "scoring_invoked": False,
        }
        ok, _ = preflight.validate_verifier_readiness_record(
            {**record, "dependency_setup_probe": disguised_scoring},
            expected_task_sha256="a" * 64,
            expected_test_sha256="e" * 64,
            expected_projection=projection,
            expected_dependency_setup_seconds=900.0,
        )
        self.assertFalse(ok)

        wrong_entrypoint_plan = {
            **executed_plan,
            "steps": [
                executed_plan["steps"][0],
                {
                    **executed_plan["steps"][1],
                    "command_sha256": "9" * 64,
                },
            ],
        }
        wrong_entrypoint_probe = {
            **executed_probe,
            "plan": wrong_entrypoint_plan,
            "plan_sha256": preflight.canonical_json_sha256(wrong_entrypoint_plan),
            "executions": [
                {
                    "index": index,
                    "kind": step["kind"],
                    "command_sha256": step["command_sha256"],
                    "exit_code": 0,
                }
                for index, step in enumerate(wrong_entrypoint_plan["steps"])
            ],
        }
        ok, _ = preflight.validate_verifier_readiness_record(
            {**record, "dependency_setup_probe": wrong_entrypoint_probe},
            expected_task_sha256="a" * 64,
            expected_test_sha256="e" * 64,
            expected_projection=projection,
            expected_dependency_setup_seconds=900.0,
        )
        self.assertFalse(ok)

        source_runtime_plan = readiness.build_dependency_setup_plan(
            "source $HOME/.local/bin/env\npytest /tests/score.py\n",
            Path("test.sh"),
        )
        source_plan = source_runtime_plan.receipt_plan()
        source_binding = {
            "step_index": 0,
            "canonical_path": "/root/.local/bin/env",
            "device": 2049,
            "inode": 1701,
            "content_sha256": "5" * 64,
            "content_bytes": 100,
            "environment_delta_sha256": "6" * 64,
            "resolve_command_sha256": "7" * 64,
            "stat_command_sha256": "8" * 64,
            "digest_command_sha256": "9" * 64,
        }
        source_batch = {
            "start": 1,
            "end": 2,
            "command_sha256": "a" * 64,
            "step_exit_codes": [0],
        }
        source_probe = {
            "mode": "executed",
            "plan": source_plan,
            "plan_sha256": preflight.canonical_json_sha256(source_plan),
            "budget_seconds": 900.0,
            "invocations": [
                {
                    "sequence": 0,
                    "kind": "readability_probe",
                    "command_sha256": "0" * 64,
                    "exit_code": 0,
                },
                {
                    "sequence": 1,
                    "kind": "source_resolve",
                    "command_sha256": source_binding["resolve_command_sha256"],
                    "exit_code": 0,
                },
                {
                    "sequence": 2,
                    "kind": "source_stat_before",
                    "command_sha256": source_binding["stat_command_sha256"],
                    "exit_code": 0,
                },
                {
                    "sequence": 3,
                    "kind": "source_digest_before",
                    "command_sha256": source_binding["digest_command_sha256"],
                    "exit_code": 0,
                },
                {
                    "sequence": 4,
                    "kind": "source_stat_after",
                    "command_sha256": source_binding["stat_command_sha256"],
                    "exit_code": 0,
                },
                {
                    "sequence": 5,
                    "kind": "source_digest_after",
                    "command_sha256": source_binding["digest_command_sha256"],
                    "exit_code": 0,
                },
                {
                    "sequence": 6,
                    "kind": "dependency_setup",
                    "command_sha256": source_batch["command_sha256"],
                    "exit_code": 0,
                },
            ],
            "batches": [source_batch],
            "batches_sha256": preflight.canonical_json_sha256([source_batch]),
            "sources": [source_binding],
            "sources_sha256": preflight.canonical_json_sha256([source_binding]),
            "fixtures": [],
            "fixtures_sha256": preflight.canonical_json_sha256([]),
            "executions": [
                {
                    "index": index,
                    "kind": step["kind"],
                    "command_sha256": step["command_sha256"],
                    "exit_code": 0,
                }
                for index, step in enumerate(source_plan["steps"])
            ],
            "scoring_invoked": False,
        }
        ok, detail = preflight.validate_verifier_readiness_record(
            {**record, "dependency_setup_probe": source_probe},
            expected_task_sha256="a" * 64,
            expected_test_sha256="e" * 64,
            expected_projection=projection,
            expected_dependency_setup_seconds=900.0,
        )
        self.assertTrue(ok, detail)
        tampered_source = {
            **source_binding,
            "content_sha256": "b" * 64,
        }
        ok, _ = preflight.validate_verifier_readiness_record(
            {
                **record,
                "dependency_setup_probe": {
                    **source_probe,
                    "sources": [tampered_source],
                },
            },
            expected_task_sha256="a" * 64,
            expected_test_sha256="e" * 64,
            expected_projection=projection,
            expected_dependency_setup_seconds=900.0,
        )
        self.assertFalse(ok)

    def test_readiness_probe_avoids_scoring_and_uses_generic_container_probe(self):
        source = MODULE_PATH.with_name("verifier_readiness.py").read_text()
        self.assertIn("_probe_verifier_container", source)
        self.assertNotIn("verifier.verify()", source)
        self.assertNotIn("parse_bootstrap_contract", source)
        self.assertNotIn("_bootstrap_command", source)
        non_template = (
            MODULE_PATH.parents[2]
            / "target"
            / "tbench21-dataset"
            / "terminal-bench-2-1"
            / "kv-store-grpc"
            / "tests"
            / "test.sh"
        )
        if non_template.is_file():
            self.assertNotIn("uvx", non_template.read_text())

    def test_verifier_readiness_ledger_preserves_config_task_order(self):
        projection = {
            "HTTP_PROXY": "http://proxy.example:8080",
            "HTTPS_PROXY": "http://proxy.example:8080",
            "http_proxy": "http://proxy.example:8080",
            "https_proxy": "http://proxy.example:8080",
            "NO_PROXY": preflight.VERIFIER_NO_PROXY,
            "no_proxy": preflight.VERIFIER_NO_PROXY,
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = []
            for name in ("z-task", "a-task", "m-task"):
                path = root / name
                path.mkdir()
                (path / "task.toml").write_text(
                    "[agent]\ntimeout_sec=900\n[verifier]\ntimeout_sec=900\n"
                )
                (path / "identity.txt").write_text(name)
                (path / "tests").mkdir()
                (path / "tests" / "test.sh").write_text("#!/bin/sh\npytest -q\n")
                paths.append(path)
            config = root / "config.json"
            config.write_text(
                json.dumps(
                    {
                        "tasks": [{"path": str(path)} for path in paths],
                        "verifier": {"disable": False, "env": projection},
                    }
                )
            )
            records = []
            for index, path in enumerate(paths):
                setup_plan = {
                    "policy": preflight.DEPENDENCY_SETUP_POLICY,
                    "shell": "bash",
                    "runner_family": "pytest",
                    "rendered_command_sha256": None,
                    "scoring_command_sha256": "4" * 64,
                    "fixtures": [],
                    "steps": [],
                }
                records.append(
                    {
                        "schema": "astra.harness.verifier_readiness.v6",
                        "task_sha256": preflight.benchmark_task_tree_sha256(path),
                        "environment_id": f"{index + 1:064x}",
                        "image_id": "sha256:" + f"{index + 4:064x}",
                        "repo_digests": [
                            f"example/task{index}@sha256:" + f"{index + 30:064x}"
                        ],
                        "image_source": "pulled",
                        "environment_lifecycle": "started_deleted",
                        "verifier_env_sha256": preflight.canonical_json_sha256(
                            projection
                        ),
                        "verifier_env_keys": sorted(projection),
                        "official_verifier": {
                            "test_sha256": preflight.sha256(path / "tests" / "test.sh"),
                            "execution_mode": "container_lifecycle_non_scoring",
                            "terminal_boundary_reached": True,
                            "score_eligible": False,
                            "reward_disposition": "scored_trial_only",
                            "environment_deleted": True,
                        },
                        "dependency_setup_probe": {
                            "mode": "no_setup",
                            "plan": setup_plan,
                            "plan_sha256": preflight.canonical_json_sha256(setup_plan),
                            "budget_seconds": 900,
                            "invocations": [
                                {
                                    "sequence": 0,
                                    "kind": "readability_probe",
                                    "command_sha256": "0" * 64,
                                    "exit_code": 0,
                                }
                            ],
                            "batches": [],
                            "batches_sha256": preflight.canonical_json_sha256([]),
                            "sources": [],
                            "sources_sha256": preflight.canonical_json_sha256([]),
                            "fixtures": [],
                            "fixtures_sha256": preflight.canonical_json_sha256([]),
                            "executions": [],
                            "scoring_invoked": False,
                        },
                    }
                )
            ledger = root / "ledger.json"
            ledger.write_text(
                json.dumps(
                    {
                        "schema": "astra.harness.verifier_readiness_ledger.v1",
                        "config_sha256": preflight.sha256(config),
                        "projection_sha256": preflight.canonical_json_sha256(
                            projection
                        ),
                        "records": records,
                    }
                )
            )
            ok, detail = preflight.validate_verifier_readiness_ledger(
                ledger, config, projection
            )
            self.assertTrue(ok, detail)
            payload = json.loads(ledger.read_text())
            payload["records"] = list(reversed(records))
            ledger.write_text(json.dumps(payload))
            ok, _ = preflight.validate_verifier_readiness_ledger(
                ledger, config, projection
            )
            self.assertFalse(ok)

    def test_verifier_readiness_timeout_models_serial_images_and_parallel_tails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = self._verifier_readiness_config(root, [10] * 3, [20] * 3)
            self.assertEqual(
                preflight.verifier_readiness_timeout(config),
                540 + 1 / 3,
            )

    def test_verifier_readiness_timeout_covers_skewed_pull_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = self._verifier_readiness_config(
                root, [2000, 1, 1], [1, 1, 1]
            )
            expected = 2149 + 2569 / 3 + (1 - 1 / 3) * 2189
            timeout = preflight.verifier_readiness_timeout(config)
            self.assertAlmostEqual(timeout, expected)
            old_fixed_image_budget = 3087
            self.assertGreater(timeout, old_fixed_image_budget)

    def test_verifier_readiness_timeout_bounds_multiple_worker_waves(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = self._verifier_readiness_config(
                root, [600] * 8, [1800] * 8
            )
            # Eight serialized image budgets plus the four-worker
            # list-scheduling bound for eight equal tails.
            self.assertEqual(
                preflight.verifier_readiness_timeout(config),
                8 * 649 + 8 * 2588 / 4 + (1 - 1 / 4) * 2588,
            )

    def test_verifier_readiness_timeout_handles_one_task(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = self._verifier_readiness_config(root, [10], [20])
            self.assertEqual(preflight.verifier_readiness_timeout(config), 277)

    def test_verifier_readiness_uses_complete_separate_verifier_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = root / "task"
            task.mkdir()
            (task / "task.toml").write_text(
                "[environment]\n"
                "build_timeout_sec=10\n"
                "[environment.healthcheck]\n"
                "command='agent-health'\n"
                "start_period_sec=100000\n"
                "[verifier]\n"
                "timeout_sec=20\n"
                "environment_mode='separate'\n"
                "[verifier.environment]\n"
                "build_timeout_sec=123\n"
                "[verifier.environment.healthcheck]\n"
                "command='not-run-for-separate-verifier'\n"
                "start_period_sec=200000\n"
            )
            config = root / "config.json"
            config.write_text(json.dumps({"tasks": [{"path": str(task)}]}))
            # Separate verifier environments use their complete override and
            # Harbor does not run an environment healthcheck for that clone.
            self.assertEqual(preflight.verifier_readiness_timeout(config), 503)

    def test_verifier_readiness_bounds_huge_shared_healthcheck(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = root / "task"
            task.mkdir()
            (task / "task.toml").write_text(
                "[environment]\n"
                "build_timeout_sec=10\n"
                "[environment.healthcheck]\n"
                "command='health'\n"
                "start_period_sec=100000\n"
                "timeout_sec=30\n"
                "start_interval_sec=5\n"
                "interval_sec=5\n"
                "retries=3\n"
                "[verifier]\n"
                "timeout_sec=20\n"
            )
            config = root / "config.json"
            config.write_text(json.dumps({"tasks": [{"path": str(task)}]}))
            # Harbor bound: 100000 + 30 + 5 + 3*30 + 2*5 = 100135.
            self.assertEqual(
                preflight.verifier_readiness_timeout(config),
                100412,
            )

    def test_verifier_readiness_watchdog_contract_matches_probe_constants(self):
        self.assertEqual(
            preflight.VERIFIER_READINESS_TASK_CONCURRENCY,
            readiness.DEFAULT_MAX_CONCURRENCY,
        )
        self.assertEqual(
            preflight.VERIFIER_READINESS_IMAGE_MATERIALIZATION_CONCURRENCY,
            readiness.IMAGE_MATERIALIZATION_CONCURRENCY,
        )
        self.assertEqual(
            preflight.VERIFIER_READINESS_IMAGE_INSPECT_TIMEOUT_SECONDS,
            readiness.IMAGE_INSPECT_TIMEOUT_SECONDS,
        )
        self.assertEqual(
            preflight.VERIFIER_READINESS_MAX_IMAGE_INSPECTIONS_PER_MATERIALIZATION,
            readiness.MAX_IMAGE_INSPECTIONS_PER_MATERIALIZATION,
        )
        self.assertEqual(
            preflight.VERIFIER_READINESS_PROCESS_TERMINATION_SECONDS,
            readiness.PROCESS_TERMINATION_TIMEOUT_SECONDS,
        )
        self.assertEqual(
            preflight.VERIFIER_READINESS_NETWORK_TRANSITION_SECONDS,
            readiness.NETWORK_TRANSITION_TIMEOUT_SECONDS,
        )
        self.assertEqual(
            preflight.VERIFIER_READINESS_NETWORK_TRANSITIONS_PER_PROBE,
            readiness.NETWORK_TRANSITIONS_PER_PROBE,
        )
        self.assertEqual(
            preflight.VERIFIER_READINESS_TAIL_PROCESS_TERMINATION_SECONDS,
            readiness.TAIL_PROCESS_TERMINATION_SECONDS,
        )
        self.assertEqual(
            preflight.VERIFIER_READINESS_CLEANUP_GRACE_SECONDS,
            readiness.CLEANUP_GRACE_SECONDS,
        )

    def test_preflight_runs_all_snapshot_tasks_before_server_or_harbor(self):
        source = RUNNER_PATH.read_text()
        prewarm = source.index("--probe-verifier-readiness")
        server = source.index('python3 "$process_supervisor_script" run', prewarm)
        model_seed = source.index('python3 "$model_seed_script"')
        database_seal = source.index('python3 "$database_contract_script" seal')
        database_verify = source.index('python3 "$database_contract_script" verify')
        harbor = source.index('harbor "${harbor_args[@]}"')
        self.assertLess(prewarm, server)
        self.assertLess(server, model_seed)
        self.assertLess(model_seed, database_seal)
        self.assertLess(server, database_seal)
        self.assertLess(database_seal, database_verify)
        self.assertLess(prewarm, harbor)
        self.assertIn("verifier-readiness-ledger", source)
        self.assertTrue(source.startswith("#!/usr/bin/env bash\nset -euo pipefail\n"))

    def test_explicit_entry_generates_a_complete_closed_scored_config(self):
        spec = importlib.util.spec_from_file_location(
            "astra_harness_scored_config", MODULE_PATH.with_name("scored_config.py")
        )
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        with mock.patch.dict(sys.modules, {"preflight": preflight}):
            spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            dataset = repo / "target" / "tbench21-dataset" / "terminal-bench-2-1"
            tasks = []
            for name in ("first", "second", "third"):
                task = dataset / name
                task.mkdir(parents=True)
                (task / "task.toml").write_text("[agent]\ntimeout_sec=900\n")
                tasks.append(task)
            agent = repo / "astra"
            agent.write_bytes(b"official-agent")
            payload = module.canonical_payload(
                repo=repo,
                revision="a" * 40,
                agent=agent,
                tasks=tasks,
            )
            config = repo / "config.json"
            config.write_text(json.dumps(payload))
            ok, detail = preflight.validate_benchmark_source_config(
                config, repo / "target" / "harbor-jobs"
            )
            self.assertTrue(ok, detail)
            ok, detail = preflight.validate_benchmark_tasks(config)
            self.assertTrue(ok, detail)
            qwen_payload = module.canonical_payload(
                repo=repo,
                revision="a" * 40,
                agent=agent,
                model="qwen3.7-max",
                tasks=tasks,
            )
            qwen_config = repo / "qwen-config.json"
            qwen_config.write_text(json.dumps(qwen_payload))
            ok, detail = preflight.validate_benchmark_source_config(
                qwen_config, repo / "target" / "harbor-jobs"
            )
            self.assertTrue(ok, detail)
            self.assertEqual(qwen_payload["agents"][0]["model_name"], "qwen3.7-max")
            self.assertEqual(set(payload), preflight.SCORED_SOURCE_CONFIG_KEYS)
            with self.assertRaisesRegex(module.ConfigError, "explicitly selected"):
                module.canonical_payload(
                    repo=repo,
                    revision="a" * 40,
                    agent=agent,
                )
        runner = RUNNER_PATH.read_text()
        self.assertNotIn("target/harbor-configs/current.json", runner)
        self.assertIn(
            "must explicitly select a new batch of at least three tasks", runner
        )

    def test_runner_database_lease_wraps_verify_server_and_harbor(self):
        source = RUNNER_PATH.read_text()
        broker = source.index('python3 "$lifecycle_domain_script"')
        fresh_database_bootstrap = source.index("ASTRA_AUTO_CREATE_DATABASE=1")
        server = source.index('python3 "$process_supervisor_script" run')
        model_seed = source.index('python3 "$model_seed_script"')
        database_seal = source.index('python3 "$database_contract_script" seal')
        database_verify = source.index('python3 "$database_contract_script" verify')
        harbor = source.index('harbor "${harbor_args[@]}"')
        self.assertLess(broker, server)
        self.assertLess(broker, fresh_database_bootstrap)
        self.assertLess(fresh_database_bootstrap, server)
        self.assertLess(server, model_seed)
        self.assertLess(model_seed, database_seal)
        self.assertLess(server, database_seal)
        self.assertLess(database_seal, database_verify)
        self.assertLess(server, harbor)
        self.assertEqual(source.count("ASTRA_AUTO_CREATE_DATABASE=1"), 1)
        self.assertIn(
            'ASTRA_AUTO_CREATE_DATABASE=1 \\\n    ASTRA_API_HOST="$api_host" ASTRA_API_PORT="$api_port" \\\n    ASTRA_SERVER_LIFECYCLE_OWNER="harness-$$" \\\n    python3 "$process_supervisor_script" run',
            source,
        )
        self.assertNotIn("export ASTRA_AUTO_CREATE_DATABASE", source)
        self.assertIn("--expected-admission-sha256", source)
        self.assertIn("--lifecycle-guardian-pid", source)
        self.assertIn("--lifecycle-witness-pid", source)
        self.assertIn("ASTRA_HARNESS_DOMAIN_STATE", source)
        self.assertNotIn("lifecycle-lock-fd", source)

    def test_runner_bootstraps_admin_and_seeds_via_api_before_seal(self):
        source = RUNNER_PATH.read_text()
        auth = source.index('base_url + "/admin/register"')
        model_seed = source.index('python3 "$model_seed_script"')
        database_seal = source.index('python3 "$database_contract_script" seal')
        self.assertLess(auth, model_seed)
        self.assertLess(model_seed, database_seal)
        self.assertIn("scripts/harness/benchmark_model_seed.py", source)
        self.assertIn('export ASTRA_HARNESS_MODEL_BASE="$selected_model_base"', source)
        self.assertIn(
            'export ASTRA_HARNESS_MODEL_THINKING="$selected_model_thinking"', source
        )
        self.assertNotIn('base_url + "/auth/register"', source)

    def test_runner_projects_ambient_proxy_for_harbor_placeholders(self):
        source = RUNNER_PATH.read_text()
        self.assertIn("export ASTRA_HARBOR_HTTP_PROXY=", source)
        self.assertIn("export ASTRA_HARBOR_HTTPS_PROXY=", source)
        self.assertIn('network_mode="${ASTRA_HARNESS_NETWORK_MODE:-proxy}"', source)
        self.assertIn("proxy mode requires both HTTP and HTTPS proxy endpoints", source)
        self.assertIn("direct mode requires all proxy endpoints to be empty", source)
        self.assertIn('export HTTP_PROXY="$ASTRA_HARBOR_HTTP_PROXY"', source)
        self.assertIn('export HTTPS_PROXY="$ASTRA_HARBOR_HTTPS_PROXY"', source)
        self.assertIn('ASTRA_HARBOR_ALL_PROXY="" ALL_PROXY="" all_proxy=""', source)
        self.assertLess(
            source.index("export ASTRA_HARBOR_HTTP_PROXY="),
            source.index("gateway_contract="),
        )

    def test_build_info_requires_clean_exact_commit_release_and_target(self):
        expected = {
            "schema": "astra.build_info.v1",
            "git_sha": "a" * 40,
            "git_dirty": False,
            "target": "x86_64-unknown-linux-musl",
            "profile": "debug",
        }
        with mock.patch.object(
            preflight,
            "run_text",
            return_value=(0, json.dumps(expected), ""),
        ):
            ok, detail = preflight.probe_build_info(
                Path("/candidate/astra"),
                expected_git_sha="a" * 40,
                expected_target="x86_64-unknown-linux-musl",
                expected_profile="debug",
            )
        self.assertTrue(ok, detail)

        for changed in (
            {"git_sha": "c" * 40},
            {"git_dirty": True},
            {"target": "x86_64-unknown-linux-gnu"},
            {"profile": "release"},
            {"unexpected": "ignored-by-old-probes"},
        ):
            payload = {**expected, **changed}
            with mock.patch.object(
                preflight,
                "run_text",
                return_value=(0, json.dumps(payload), ""),
            ):
                ok, _ = preflight.probe_build_info(
                    Path("/candidate/astra"),
                    expected_git_sha="a" * 40,
                    expected_target="x86_64-unknown-linux-musl",
                    expected_profile="debug",
                )
            self.assertFalse(ok, changed)

    def test_subprocess_probe_does_not_inherit_credentials(self):
        completed = mock.Mock(returncode=0, stdout="ok\n", stderr="")
        secret_keys = {
            "ASTRA_ACCESS_TOKEN": "access-secret",
            "OPENAI_API_KEY": "provider-secret",
            "DOCKER_AUTH_CONFIG": "docker-secret",
            "HTTPS_PROXY": "http://proxy-secret@example.invalid:8080",
        }
        with (
            mock.patch.dict(
                preflight.os.environ,
                {"PATH": "/usr/bin", "PYTHONPATH": "/safe", **secret_keys},
                clear=True,
            ),
            mock.patch.object(
                preflight.subprocess, "run", return_value=completed
            ) as run,
        ):
            self.assertEqual(preflight.run_text(["probe"]), (0, "ok", ""))
        environment = run.call_args.kwargs["env"]
        self.assertEqual(environment["PATH"], "/usr/bin")
        self.assertEqual(environment["PYTHONPATH"], "/safe")
        for key, value in secret_keys.items():
            self.assertNotIn(key, environment)
            self.assertNotIn(value, json.dumps(environment))

    def test_custom_agent_probe_can_bind_to_the_sealed_adapter_path(self):
        adapter_path = Path("/sealed/control/repo/crates/astra-test-harness")
        with mock.patch.object(
            preflight,
            "run_text_with_env",
            return_value=(0, "/harbor/python", ""),
        ) as probe:
            ok, detail = preflight.probe_custom_agent_import(
                Path("/harbor/python"), "harbor_adapter", "Astra", adapter_path
            )
        self.assertTrue(ok, detail)
        self.assertEqual(probe.call_args.args[1], {"PYTHONPATH": str(adapter_path)})
        self.assertNotIn("/home/", detail)

    def test_runner_rejects_semantic_harbor_overrides_before_preflight(self):
        for arguments in (
            ["--n-attempts=2"],
            ["--agent", "nop"],
            ["--model", "another-model"],
            ["--agent-env", "ASTRA_HARNESS_BINARY_SHA256=forged"],
            ["--disable-verification"],
            ["--n-concurrent=3"],
        ):
            completed = subprocess.run(
                ["bash", str(RUNNER_PATH), *arguments],
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertEqual(completed.returncode, 78, arguments)
            self.assertIn("refusing semantic Harbor passthrough", completed.stderr)

    def test_runner_rejects_every_server_reuse_switch(self):
        for legacy_environment in (
            {"ASTRA_HARNESS_REUSE_SERVER": "0"},
            {"ASTRA_HARNESS_REUSE_SERVER": "1"},
            {"ASTRA_HARNESS_EXTERNAL_SERVER_PID": "12345"},
        ):
            completed = subprocess.run(
                ["bash", str(RUNNER_PATH)],
                capture_output=True,
                text=True,
                timeout=5,
                env={**preflight.os.environ, **legacy_environment},
            )
            self.assertEqual(completed.returncode, 78, legacy_environment)
            self.assertIn("server reuse is unsupported", completed.stderr)


if __name__ == "__main__":
    unittest.main()
