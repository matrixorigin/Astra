#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import http.server
import threading
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
    def test_owned_api_never_redirects_authorization(self):
        received = []
        class Target(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                received.append(self.headers.get("Authorization"))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"{}")
            do_POST = do_GET
            def log_message(self, *_):
                pass
        target = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Target)
        class Source(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(302)
                self.send_header("Location", f"http://127.0.0.1:{target.server_port}/target")
                self.end_headers()
            do_POST = do_GET
            def log_message(self, *_):
                pass
        source = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Source)
        threads = [threading.Thread(target=server.serve_forever, daemon=True) for server in (target, source)]
        for thread in threads:
            thread.start()
        try:
            for method in ("GET", "POST"):
                with self.subTest(method=method), self.assertRaisesRegex(seed.SeedError, "HTTP 302"):
                    seed._request_json(seed.owned_api_opener(), f"http://127.0.0.1:{source.server_port}/models/exact",
                                       "test-secret-sentinel", None, 200, "model state", method=method, timeout=2)
            self.assertEqual(received, [], "redirect target must receive no request or token")
        finally:
            for server, thread in zip((target, source), threads):
                server.shutdown()
                server.server_close()
                thread.join(timeout=2)

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
  input_modalities: [text, image]
  output_modalities: [text]
  fixed_temperature: 0.7
  request_headers: {X-Provider-Option: enabled}
  wire_model_name: upstream-selected
  pricing_currency: USD
  pricing_unit: per_token
  pricing_prompt: 0.000001
  pricing_completion: 0
  judgment_default: false
  request_body_overrides: {custom_provider_option: {enabled: true}}
"""
        )
        return config, models

    def test_registers_only_selected_model_then_checks_exact_route(self):
        with tempfile.TemporaryDirectory() as directory:
            config, models = self.fixture(Path(directory))
            with models.open("a") as output:
                output.write("- name: unselected\n  unrelated_field: private-value-sentinel\n")
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
        self.assertEqual(payload["input_modalities"], ["text", "image"])
        self.assertEqual(payload["output_modalities"], ["text"])
        self.assertEqual(payload["quirks"]["fixed_temperature"], 0.7)
        self.assertEqual(payload["quirks"]["request_headers"], {"X-Provider-Option": "enabled"})
        self.assertEqual(payload["quirks"]["wire_model_name"], "upstream-selected")
        self.assertEqual(payload["quirks"]["request_body_overrides"], {
            "custom_provider_option": {"enabled": True},
        })
        self.assertNotIn("judgment_default", payload)
        self.assertNotIn("thinking:high", json.dumps(payload))
        self.assertEqual(payload["pricing"], {
            "currency": "USD", "unit": "per_token", "prompt": 0.000001, "completion": 0,
        })

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

    def test_invalid_pricing_fails_before_api(self):
        with tempfile.TemporaryDirectory() as directory:
            config, models = self.fixture(Path(directory))
            valid = models.read_text()
            invalid_prices = (
                valid.replace("pricing_currency: USD", "pricing_currency: EUR"),
                valid.replace("pricing_unit: per_token", "pricing_unit: per_million_tokens"),
                valid.replace("  pricing_unit: per_token\n", ""),
                valid.replace("  pricing_prompt: 0.000001\n", ""),
                valid.replace("  pricing_completion: 0\n", ""),
                valid.replace("pricing_prompt: 0.000001", "pricing_prompt: -1"),
                valid.replace("pricing_prompt: 0.000001", "pricing_prompt: .nan"),
                valid.replace("pricing_prompt: 0.000001", "pricing_prompt: .inf"),
                valid.replace("pricing_completion: 0", "pricing_completion: true"),
                valid.replace("pricing_prompt: 0.000001", f"pricing_prompt: {10 ** 400}"),
            )
            for index, document in enumerate(invalid_prices):
                with self.subTest(case=index):
                    models.write_text(document)
                    opener = _Opener([])
                    with self.assertRaises(seed.SeedError):
                        seed.register_selected_model(
                            "http://localhost", config, models, "token", opener
                        )
                    self.assertEqual(opener.requests, [])

    def test_unknown_selected_fields_fail_before_api_without_values(self):
        with tempfile.TemporaryDirectory() as directory:
            config, models = self.fixture(Path(directory))
            valid = models.read_text()
            for field in ("fallback_chain", "context_widnow"):
                with self.subTest(field=field):
                    models.write_text(valid + f"  {field}: private-value-sentinel\n")
                    opener = _Opener([])
                    with self.assertRaisesRegex(seed.SeedError, "unknown fields") as error:
                        seed.register_selected_model(
                            "http://localhost", config, models, "token", opener
                        )
                    self.assertIn(field, str(error.exception))
                    self.assertNotIn("private-value-sentinel", str(error.exception))
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
