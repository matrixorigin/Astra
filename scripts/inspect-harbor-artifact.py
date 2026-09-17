#!/usr/bin/env python3
"""Resolve one Harbor artifact through its structured API response."""

from __future__ import annotations

import base64
import json
import os
import re
import sys
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlparse
from urllib.request import Request, urlopen


NOT_FOUND = 44


def main() -> int:
    if len(sys.argv) != 4:
        print(
            f"usage: {sys.argv[0]} <harbor-api-base-url> <repository> <reference>",
            file=sys.stderr,
        )
        return 2

    base_url, repository, reference = sys.argv[1:]
    parsed = urlparse(base_url)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc or parsed.path not in {"", "/"}:
        print("Harbor API base URL must contain only an HTTP(S) scheme and authority", file=sys.stderr)
        return 2

    project, separator, repository_name = repository.partition("/")
    if not separator or not project or not repository_name or not reference:
        print("Harbor repository must include a project and repository name", file=sys.stderr)
        return 2

    username = os.environ.get("IDC_REGISTRY_USERNAME", "")
    password = os.environ.get("IDC_REGISTRY_PASSWORD", "")
    if not username or not password:
        print("Harbor registry credentials are required", file=sys.stderr)
        return 2

    credentials = base64.b64encode(f"{username}:{password}".encode()).decode("ascii")
    encoded_repository_name = quote(quote(repository_name, safe=""), safe="")
    url = (
        f"{base_url.rstrip('/')}/api/v2.0/projects/{quote(project, safe='')}"
        f"/repositories/{encoded_repository_name}/artifacts/{quote(reference, safe='')}"
    )
    request = Request(
        url,
        headers={
            "Accept": "application/json",
            "Authorization": f"Basic {credentials}",
        },
    )

    try:
        with urlopen(request, timeout=30) as response:
            document = json.load(response)
    except HTTPError as error:
        if error.code == 404:
            try:
                document = json.load(error)
            except (UnicodeDecodeError, json.JSONDecodeError):
                document = None
            errors = document.get("errors") if isinstance(document, dict) else None
            if isinstance(errors, list) and any(
                isinstance(item, dict) and item.get("code") == "NOT_FOUND"
                for item in errors
            ):
                return NOT_FOUND
        print(
            f"Harbor artifact lookup failed with HTTP {error.code} {error.reason}",
            file=sys.stderr,
        )
        return 1
    except (URLError, TimeoutError, OSError) as error:
        print(f"Harbor artifact lookup failed: {error}", file=sys.stderr)
        return 1
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        print(f"Harbor artifact lookup returned invalid JSON: {error}", file=sys.stderr)
        return 1

    digest = document.get("digest") if isinstance(document, dict) else None
    if not isinstance(digest, str) or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None:
        print("Harbor artifact lookup returned an invalid digest", file=sys.stderr)
        return 1
    print(digest)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
