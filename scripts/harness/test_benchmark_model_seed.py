#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))
MODULE_PATH = SCRIPT_DIR / "benchmark_model_seed.py"
SPEC = importlib.util.spec_from_file_location("benchmark_model_seed", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
seed = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(seed)


class _Response:
    def __init__(self, status: int, value: dict):
        self.status = status
        self.body = json.dumps(value).encode()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return False

    def read(self, _limit: int) -> bytes:
        return self.body


class _Opener:
    def __init__(self, responses: list[_Response]):
        self.responses = responses
        self.requests = []

    def open(self, request, timeout):
        self.requests.append((request, timeout))
        return self.responses.pop(0)


class BenchmarkModelSeedTests(unittest.TestCase):
    def fixture(self, root: Path, selector: str = "selected(thinking:high)"):
        config = root / "config.json"
        config.write_text(
            json.dumps(
                {
                    "agents": [
                        {"name": "harbor_adapter:Astra", "model_name": selector}
                    ]
                }
            )
        )
        models = root / ".models.yaml"
        models.write_text(
            """
- name: selected
  provider: openai
  api_key: provider-secret-sentinel
  base_url: https://provider.invalid/v1
  context_window: 100000
  max_completion_tokens: 20000
  supported_parameters: [tools]
  wire_model_name: upstream-selected
"""
        )
        return config, models

    def test_registers_only_selected_model_then_checks_exact_route(self):
        with tempfile.TemporaryDirectory() as directory:
            config, models = self.fixture(Path(directory))
            opener = _Opener(
                [
                    _Response(201, {"name": "selected", "is_active": False}),
                    _Response(
                        200,
                        {
                            "name": "selected",
                            "is_active": True,
                            "thinking_capability": "effort_only",
                        },
                    ),
                ]
            )
            result = seed.register_selected_model(
                "http://127.0.0.1:17012", config, models, "access-secret", opener
            )
        self.assertTrue(result["checked"])
        self.assertEqual(result["thinking_mode"], "high")
        self.assertEqual(len(opener.requests), 2)
        create, check = [request for request, _ in opener.requests]
        self.assertEqual(create.full_url, "http://127.0.0.1:17012/models")
        self.assertEqual(
            check.full_url, "http://127.0.0.1:17012/models/selected/check"
        )
        self.assertEqual(create.get_header("Authorization"), "Bearer access-secret")
        payload = json.loads(create.data)
        self.assertEqual(payload["name"], "selected")
        self.assertEqual(payload["quirks"]["wire_model_name"], "upstream-selected")
        self.assertNotIn("thinking:high", json.dumps(payload))

    def test_missing_duplicate_or_empty_credentials_fail_before_api(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config, models = self.fixture(root)
            opener = _Opener([])
            models.write_text("- name: another\n  provider: openai\n  api_key: x\n  context_window: 10\n")
            with self.assertRaisesRegex(seed.SeedError, "exactly one"):
                seed.register_selected_model("http://localhost", config, models, "token", opener)
            models.write_text(
                "- name: selected\n  provider: openai\n  api_key: ''\n  context_window: 10\n"
            )
            with self.assertRaisesRegex(seed.SeedError, "api_key"):
                seed.register_selected_model("http://localhost", config, models, "token", opener)
            self.assertEqual(opener.requests, [])

    def test_check_must_activate_exact_high_thinking_model(self):
        with tempfile.TemporaryDirectory() as directory:
            config, models = self.fixture(Path(directory))
            for checked in (
                {"name": "other", "is_active": True, "thinking_capability": "both"},
                {"name": "selected", "is_active": False, "thinking_capability": "both"},
                {"name": "selected", "is_active": True, "thinking_capability": "none"},
            ):
                opener = _Opener(
                    [
                        _Response(201, {"name": "selected"}),
                        _Response(200, checked),
                    ]
                )
                with self.assertRaises(seed.SeedError):
                    seed.register_selected_model(
                        "http://localhost", config, models, "token", opener
                    )

    def test_main_requires_token_without_printing_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            config, models = self.fixture(Path(directory))
            with (
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        str(MODULE_PATH),
                        "--api-url",
                        "http://localhost",
                        "--config",
                        str(config),
                        "--models-file",
                        str(models),
                    ],
                ),
                mock.patch("builtins.print") as output,
            ):
                self.assertEqual(seed.main(), 78)
            rendered = " ".join(str(call) for call in output.call_args_list)
            self.assertNotIn("provider-secret-sentinel", rendered)


if __name__ == "__main__":
    unittest.main()
