#!/usr/bin/env python3
"""Probe the configured OpenAI-compatible embedding endpoint without logging secrets."""

from __future__ import annotations

import argparse
import ipaddress
import json
import math
import pathlib
import re
import sys
from typing import Optional
import urllib.error
import urllib.parse
import urllib.request


def url_origin(url: str) -> tuple[str, str, int]:
    parsed = urllib.parse.urlparse(url)
    default_port = 443 if parsed.scheme.lower() == "https" else 80
    return parsed.scheme.lower(), (parsed.hostname or "").lower(), parsed.port or default_port


class SameOriginRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Keep embedding credentials on the endpoint origin selected by the user."""

    def redirect_request(  # type: ignore[no-untyped-def]
        self, request, file_pointer, code, message, headers, new_url
    ):
        resolved_url = urllib.parse.urljoin(request.full_url, new_url)
        if url_origin(request.full_url) != url_origin(resolved_url):
            raise RuntimeError(
                "embedding endpoint attempted a cross-origin redirect; "
                "configure the final endpoint URL directly"
            )
        return super().redirect_request(
            request, file_pointer, code, message, headers, resolved_url
        )


def read_env(path: pathlib.Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        value = value.strip()
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key):
            continue
        if len(value) >= 2 and value[0] in {'"', "'"} and value[-1] == value[0]:
            value = value[1:-1]
        elif " #" in value:
            value = value.split(" #", 1)[0].rstrip()
        values[key] = value
    return values


def api_error_message(payload: bytes, secrets: tuple[str, ...] = ()) -> str:
    try:
        value = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return "provider returned a non-JSON error"
    if isinstance(value, dict):
        error = value.get("error", value.get("detail", value.get("message")))
        if isinstance(error, dict):
            error = error.get("message", error.get("code"))
        if isinstance(error, str) and error.strip():
            message = error.strip()[:300]
            for secret in secrets:
                if secret:
                    message = message.replace(secret, "[redacted]")
            return message
    return "provider rejected the request"


def is_loopback(hostname: Optional[str]) -> bool:
    if not hostname:
        return False
    if hostname.lower() == "localhost":
        return True
    try:
        return ipaddress.ip_address(hostname).is_loopback
    except ValueError:
        return False


def proxy_hint(hostname: Optional[str]) -> str:
    if not hostname or urllib.request.proxy_bypass(hostname):
        return ""
    proxies = urllib.request.getproxies()
    if not any(key in proxies for key in ("http", "https")):
        return ""
    return f"; a configured HTTP proxy was used (set NO_PROXY={hostname} for a direct connection)"


def probe(env_file: pathlib.Path, timeout: float) -> tuple[str, int]:
    values = read_env(env_file)
    provider = values.get("MEMORIA_EMBEDDING_PROVIDER", "openai").strip().lower()
    if provider == "mock":
        return "mock", 0
    if provider != "openai":
        raise ValueError(f"unsupported embedding provider for setup probe: {provider}")

    base_url = values.get("MEMORIA_EMBEDDING_BASE_URL", "").strip().rstrip("/")
    model = values.get("MEMORIA_EMBEDDING_MODEL", "").strip()
    dimension_text = values.get("MEMORIA_EMBEDDING_DIM", "").strip()
    api_key = values.get("MEMORIA_EMBEDDING_API_KEY", "").strip()
    parsed = urllib.parse.urlparse(base_url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        raise ValueError("embedding base URL must be an absolute http:// or https:// URL")
    if parsed.username or parsed.password:
        raise ValueError("embedding credentials belong in the API key field, not the base URL")
    if parsed.query or parsed.fragment:
        raise ValueError("embedding base URL cannot contain a query string or fragment")
    if not model:
        raise ValueError("embedding model cannot be empty")
    try:
        expected_dimension = int(dimension_text)
    except ValueError as error:
        raise ValueError("embedding dimension must be a positive whole number") from error
    if expected_dimension <= 0:
        raise ValueError("embedding dimension must be a positive whole number")
    if parsed.hostname == "api.openai.com" and not api_key:
        raise ValueError("api.openai.com requires an embedding API key")

    body = json.dumps({"model": model, "input": "Astra setup connectivity check"}).encode()
    headers = {
        "Content-Type": "application/json",
        "User-Agent": "astra-stack-setup/1",
    }
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"
    request = urllib.request.Request(
        f"{base_url}/embeddings", data=body, headers=headers, method="POST"
    )
    handlers: list[urllib.request.BaseHandler] = [SameOriginRedirectHandler()]
    if is_loopback(parsed.hostname):
        handlers.insert(0, urllib.request.ProxyHandler({}))
    open_request = urllib.request.build_opener(*handlers).open
    try:
        with open_request(request, timeout=timeout) as response:
            payload = response.read(4 * 1024 * 1024)
    except urllib.error.HTTPError as error:
        payload = error.read(64 * 1024)
        raise RuntimeError(
            f"embedding endpoint returned HTTP {error.code}: "
            f"{api_error_message(payload, (api_key,))}{proxy_hint(parsed.hostname)}"
        ) from error
    except urllib.error.URLError as error:
        reason = getattr(error, "reason", error)
        raise RuntimeError(
            f"cannot reach embedding endpoint: {reason}{proxy_hint(parsed.hostname)}"
        ) from error

    try:
        value = json.loads(payload)
        vector = value["data"][0]["embedding"]
    except (json.JSONDecodeError, KeyError, IndexError, TypeError) as error:
        raise RuntimeError("embedding endpoint returned an invalid response shape") from error
    if not isinstance(vector, list) or not vector:
        raise RuntimeError("embedding endpoint returned an empty vector")
    if any(
        not isinstance(item, (int, float))
        or isinstance(item, bool)
        or not math.isfinite(item)
        for item in vector
    ):
        raise RuntimeError("embedding endpoint returned a non-finite or non-numeric vector")
    actual_dimension = len(vector)
    if actual_dimension != expected_dimension:
        raise RuntimeError(
            "embedding dimension mismatch: "
            f"configured {expected_dimension}, endpoint returned {actual_dimension}"
        )
    return model, actual_dimension


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("env_file", type=pathlib.Path)
    parser.add_argument("--timeout", type=float, default=30.0)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be greater than zero")
    try:
        model, dimension = probe(args.env_file, args.timeout)
    except (OSError, ValueError, RuntimeError) as error:
        print(f"embedding preflight failed: {error}", file=sys.stderr)
        return 1
    if model == "mock":
        print("mock embeddings selected; external connectivity check skipped")
    else:
        print(f"embedding endpoint ready: model={model}, dimensions={dimension}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
