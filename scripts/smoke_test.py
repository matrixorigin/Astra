#!/usr/bin/env python3
"""Smoke test: run mo-agent chat end-to-end and check for issues.

Usage:
    source activate agent-engine
    python scripts/smoke_test.py
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from click.testing import CliRunner
from unittest.mock import patch


def run_smoke_test():
    inputs = iter(["hi", "帮我看看这段代码: def f(x): return x+1", "exit"])

    with patch("click.prompt", side_effect=inputs):
        from cli.mo_agent import chat

        runner = CliRunner()
        result = runner.invoke(chat, ["--user-id", "smoke_test"])

    output = result.output
    lines = output.split("\n")

    # Check for issues
    issue_keywords = [
        "not found", "fallback", "failed", "error", "warning",
        "mock", "traceback", "exception",
    ]
    # Exclude false positives (agent response may contain these words)
    agent_response_started = False
    issues = []
    for line in lines:
        if "Agent>" in line:
            agent_response_started = True
        if line.startswith(("Session:", "✅", "🤖", "==", "Type")):
            agent_response_started = False
            continue
        if agent_response_started:
            continue  # Skip agent response content
        lower = line.lower()
        for kw in issue_keywords:
            if kw in lower:
                issues.append(line.strip())
                break

    # Results
    print("=" * 60)
    print("SMOKE TEST RESULTS")
    print("=" * 60)

    if result.exception:
        print(f"❌ EXCEPTION: {result.exception}")
        import traceback
        traceback.print_exception(type(result.exception), result.exception, result.exception.__traceback__)
        return 1

    if "Session closed" not in output:
        print("❌ Session was not properly closed")
        return 1

    if issues:
        print(f"⚠️  {len(issues)} issue(s) found:")
        for issue in issues:
            print(f"  - {issue}")
        return 1

    print("✅ All clean — no warnings, no errors")
    print(f"   Output length: {len(output)} chars")

    # Show agent responses
    for line in lines:
        if "Agent>" in line:
            preview = line[:100] + "..." if len(line) > 100 else line
            print(f"   {preview}")

    return 0


if __name__ == "__main__":
    sys.exit(run_smoke_test())
