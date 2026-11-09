#!/usr/bin/env python3
"""Keep the rmcp advisory exception tied to the disabled upstream backends."""

import json
import subprocess
import sys


def check(metadata):
    packages = [package for package in metadata["packages"] if package["name"] == "rmcp"]
    if len(packages) != 1:
        raise ValueError("expected exactly one rmcp package")
    package = packages[0]
    if package["version"] != "1.6.0" or package["source"] != "registry+https://github.com/rust-lang/crates.io-index":
        raise ValueError("rmcp version/source changed; re-evaluate the RustSec exception")
    nodes = [node for node in metadata["resolve"]["nodes"] if node["id"] == package["id"]]
    if len(nodes) != 1:
        raise ValueError("rmcp resolved features are missing")
    unsafe = {
        "transport-streamable-http-client-reqwest",
        "transport-streamable-http-client-unix-socket",
    }.intersection(nodes[0]["features"])
    if unsafe:
        raise ValueError(f"vulnerable rmcp transport features enabled: {', '.join(sorted(unsafe))}")


def main():
    try:
        result = subprocess.run(
            ["cargo", "metadata", "--locked", "--all-features", "--format-version", "1"],
            check=True, capture_output=True, text=True,
        )
        check(json.loads(result.stdout))
    except (subprocess.CalledProcessError, ValueError, KeyError, TypeError) as error:
        print(f"rmcp audit guard failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
