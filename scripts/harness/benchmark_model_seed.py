#!/usr/bin/env python3
"""Register and verify the one model selected by a sealed benchmark config.

The provider credential is read from a local YAML file and the Astra access
token is read from the process environment.  Neither secret is accepted on the
command line or included in diagnostics.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

import preflight


class SeedError(RuntimeError):
    pass


def _load_yaml(path: Path) -> Any:
    try:
        import yaml
    except ImportError as error:
        raise SeedError("PyYAML is required to read the benchmark model credential file") from error
    try:
        return yaml.safe_load(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, yaml.YAMLError) as error:
        # A YAML parser may include the source line in its exception.  Never
        # propagate that text because the line can contain a provider key.
        raise SeedError("benchmark model credential file is unreadable or invalid") from error


def selected_model_name(config: Path) -> tuple[str, str | None]:
    ok, requirements, detail = preflight.configured_model_requirements(config)
    if not ok or len(requirements) != 1:
        raise SeedError(f"benchmark config has no unique model selection: {detail}")
    try:
        payload = json.loads(config.read_text(encoding="utf-8"))
        selector = payload["agents"][0]["model_name"].strip()
    except (OSError, UnicodeError, json.JSONDecodeError, KeyError, IndexError, TypeError, AttributeError) as error:
        raise SeedError("benchmark config has no unique model selection") from error
    name, thinking = preflight.resolve_model_selector(selector)
    if not name or (name.casefold(), thinking) != requirements[0]:
        raise SeedError("benchmark config model selection is inconsistent")
    return name, thinking


def _required_string(entry: dict[str, Any], field: str, model_name: str) -> str:
    value = entry.get(field)
    if not isinstance(value, str) or not value.strip():
        raise SeedError(f"selected model {model_name!r} requires non-empty {field}")
    return value.strip()


def _optional_string(entry: dict[str, Any], field: str, model_name: str) -> str | None:
    value = entry.get(field)
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        raise SeedError(f"selected model {model_name!r} has invalid {field}")
    return value.strip()


def _optional_string_list(
    entry: dict[str, Any], field: str, model_name: str
) -> list[str] | None:
    value = entry.get(field)
    if value is None:
        return None
    if not isinstance(value, list) or not all(
        isinstance(item, str) and item.strip() for item in value
    ):
        raise SeedError(f"selected model {model_name!r} has invalid {field}")
    return [item.strip() for item in value]


def _positive_integer(entry: dict[str, Any], field: str, model_name: str) -> int:
    value = entry.get(field)
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise SeedError(f"selected model {model_name!r} requires positive {field}")
    return value


def _optional_number(entry: dict[str, Any], field: str, model_name: str) -> float | None:
    value = entry.get(field)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value < 0:
        raise SeedError(f"selected model {model_name!r} has invalid {field}")
    return float(value)


def _selected_entry(document: Any, model_name: str) -> dict[str, Any]:
    entries = document.get("models") if isinstance(document, dict) else document
    if not isinstance(entries, list):
        raise SeedError("benchmark model credential file must contain a model list")
    matches = [
        entry
        for entry in entries
        if isinstance(entry, dict) and entry.get("name") == model_name
    ]
    if len(matches) != 1:
        raise SeedError(
            f"benchmark model credential file must contain exactly one {model_name!r} entry"
        )
    return matches[0]


def model_create_payload(entry: dict[str, Any], model_name: str) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "name": model_name,
        "provider": _required_string(entry, "provider", model_name),
        "api_key": _required_string(entry, "api_key", model_name),
        "base_url": _optional_string(entry, "base_url", model_name),
        "context_window": _positive_integer(entry, "context_window", model_name),
    }
    for field in ("description", "architecture"):
        value = _optional_string(entry, field, model_name)
        if value is not None:
            payload[field] = value
    if entry.get("max_completion_tokens") is not None:
        payload["max_completion_tokens"] = _positive_integer(
            entry, "max_completion_tokens", model_name
        )
    for field in (
        "input_modalities",
        "output_modalities",
        "supported_parameters",
        "tags",
    ):
        value = _optional_string_list(entry, field, model_name)
        if value is not None:
            payload[field] = value

    prompt_price = _optional_number(entry, "pricing_prompt", model_name)
    completion_price = _optional_number(entry, "pricing_completion", model_name)
    if prompt_price is not None or completion_price is not None:
        payload["pricing"] = {
            "prompt": prompt_price or 0.0,
            "completion": completion_price or 0.0,
        }

    quirks: dict[str, Any] = {}
    fallback_chain = _optional_string_list(entry, "fallback_chain", model_name)
    if fallback_chain is not None:
        quirks["fallback_chain"] = fallback_chain
    wire_model_name = _optional_string(entry, "wire_model_name", model_name)
    if wire_model_name is not None:
        quirks["wire_model_name"] = wire_model_name
    for field in (
        "prompt_cache_capability",
        "request_body_overrides",
        "probe_headers",
    ):
        value = entry.get(field)
        if value is not None:
            if not isinstance(value, dict):
                raise SeedError(f"selected model {model_name!r} has invalid {field}")
            quirks[field] = value
    probe_endpoint = _optional_string(entry, "probe_endpoint", model_name)
    if probe_endpoint is not None:
        quirks["probe_endpoint"] = probe_endpoint
    if quirks:
        payload["quirks"] = quirks
    return payload


def _request_json(
    opener: urllib.request.OpenerDirector,
    url: str,
    token: str,
    payload: dict[str, Any] | None,
    expected_status: int,
    operation: str,
) -> dict[str, Any]:
    body = None
    headers = {"Authorization": f"Bearer {token}"}
    if payload is not None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=body, headers=headers, method="POST")
    try:
        with opener.open(request, timeout=60) as response:
            status = response.status
            response_body = response.read(2 * 1024 * 1024)
    except urllib.error.HTTPError as error:
        raise SeedError(f"{operation} returned HTTP {error.code}") from None
    except (OSError, urllib.error.URLError):
        raise SeedError(f"{operation} could not reach the owned Astra server") from None
    if status != expected_status:
        raise SeedError(f"{operation} returned HTTP {status}")
    try:
        value = json.loads(response_body.decode("utf-8"))
    except (UnicodeError, json.JSONDecodeError) as error:
        raise SeedError(f"{operation} returned invalid JSON") from error
    if not isinstance(value, dict):
        raise SeedError(f"{operation} returned a non-object response")
    return value


def register_selected_model(
    api_url: str,
    config: Path,
    models_file: Path,
    token: str,
    opener: urllib.request.OpenerDirector | None = None,
) -> dict[str, Any]:
    model_name, thinking = selected_model_name(config)
    entry = _selected_entry(_load_yaml(models_file), model_name)
    payload = model_create_payload(entry, model_name)
    opener = opener or urllib.request.build_opener(urllib.request.ProxyHandler({}))
    base_url = api_url.rstrip("/")
    created = _request_json(
        opener,
        base_url + "/models",
        token,
        payload,
        201,
        "selected model registration",
    )
    if created.get("name") != model_name:
        raise SeedError("selected model registration returned a different model")
    checked = _request_json(
        opener,
        base_url + "/models/" + urllib.parse.quote(model_name, safe="") + "/check",
        token,
        None,
        200,
        "selected model connectivity check",
    )
    if checked.get("name") != model_name or checked.get("is_active") is not True:
        raise SeedError("selected model connectivity check did not activate the exact model")
    capability = checked.get("thinking_capability")
    if thinking == "high" and capability not in {"both", "effort_only"}:
        raise SeedError(
            "selected high-thinking model does not expose controllable thinking"
        )
    return {
        "model_name": model_name,
        "thinking_mode": thinking or "none",
        "registered": True,
        "checked": True,
        "is_active": True,
        "thinking_capability": capability,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--api-url", required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--models-file", type=Path, required=True)
    args = parser.parse_args()
    token = os.environ.get("ASTRA_ACCESS_TOKEN", "").strip()
    if not token:
        print("astra harness: ASTRA_ACCESS_TOKEN is required for model registration", file=sys.stderr)
        return 78
    try:
        result = register_selected_model(
            args.api_url, args.config, args.models_file, token
        )
    except SeedError as error:
        print(f"astra harness: model seed failed: {error}", file=sys.stderr)
        return 78
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
