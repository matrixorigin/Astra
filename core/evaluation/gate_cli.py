"""CLI tool for running regression gates in CI/CD pipelines.

Usage:
    python -m core.evaluation.gate_cli run --sessions 50 --threshold 0.95
    python -m core.evaluation.gate_cli report --format github-comment
"""

import argparse
import json
import sys
from typing import Optional, Any

from sqlalchemy.orm import Session
from api.database import get_db_session
from core.evaluation.regression_gate import RegressionGate, ChangeType
from core.logging_config import get_logger

logger = get_logger(__name__)


def run_gate(
    change_type: str,
    change_id: str,
    change_file: str,
    sessions: int,
    error_threshold: float,
    score_threshold: float,
) -> dict[str, Any]:
    """Run regression gate validation.
    
    Args:
        change_type: Type of change (prompt/skill/config)
        change_id: Change identifier
        change_file: Path to file containing change content (JSON)
        sessions: Number of golden sessions to test
        error_threshold: Max allowed error rate
        score_threshold: Max allowed score regression
        
    Returns:
        Gate result dict
    """
    # Load change content
    with open(change_file, 'r') as f:
        change_content = json.load(f)
    
    # Run gate
    db = next(get_db_session())
    try:
        gate = RegressionGate(db=db)
        result = gate.validate_change(
            change_type=ChangeType(change_type),
            change_id=change_id,
            change_content=change_content,
            golden_session_count=sessions,
            error_rate_threshold=error_threshold,
            score_regression_threshold=score_threshold,
        )
        return result
    finally:
        db.close()


def format_github_comment(result: dict[str, Any]) -> str:
    """Format gate result as GitHub PR comment.
    
    Args:
        result: Gate result dict
        
    Returns:
        Markdown formatted comment
    """
    if result["verdict"] == "skip":
        return f"""## ⚠️ Regression Gate: SKIPPED

**Change**: `{result['change_type']}` - `{result['change_id']}`
**Reason**: {result['reason']}

No golden sessions available for testing. The gate will activate once the system accumulates high-quality conversation data.

**Gate ID**: `{result['gate_id']}`
**Created**: {result['created_at']}
"""
    
    verdict_emoji = "✅" if result["verdict"] == "pass" else "❌"
    
    comment = f"""## {verdict_emoji} Regression Gate: {result['verdict'].upper()}

**Change**: `{result['change_type']}` - `{result['change_id']}`
**Sessions Tested**: {result['sessions_tested']}
**Verdict**: {result['verdict']} ({result['reason']})

### Metrics

| Metric | Value |
|--------|-------|
| Error Rate | {result['metrics'].get('error_rate', 0):.2%} |
| Score Delta | {result['metrics'].get('score_delta', 0):.3f} |
| Avg Original Score | {result['metrics'].get('avg_original_score', 0):.2f} |
| Avg Replay Score | {result['metrics'].get('avg_replay_score', 0):.2f} |
| Failed Sessions | {result['metrics'].get('failed_sessions', 0)} / {result['metrics'].get('total_sessions', 0)} |

### Details

- **Gate ID**: `{result['gate_id']}`
- **Snapshot**: `{result.get('snapshot_id', 'N/A')}`
- **Created**: {result['created_at']}

"""
    
    if result["verdict"] == "fail":
        comment += """
### ⚠️ Action Required

This change caused regression on golden sessions. Please:
1. Review the failed sessions
2. Adjust the change to fix regressions
3. Re-run the gate validation

"""
    
    return comment


def format_json(result: dict[str, Any]) -> str:
    """Format gate result as JSON."""
    return json.dumps(result, indent=2)


def main():
    parser = argparse.ArgumentParser(description="Regression gate CLI for CI/CD")
    subparsers = parser.add_subparsers(dest="command", help="Command to run")
    
    # Run command
    run_parser = subparsers.add_parser("run", help="Run regression gate")
    run_parser.add_argument("--change-type", required=True, choices=["prompt", "skill", "config", "selector"])
    run_parser.add_argument("--change-id", required=True, help="Change identifier")
    run_parser.add_argument("--change-file", required=True, help="Path to change content JSON file")
    run_parser.add_argument("--sessions", type=int, default=50, help="Number of golden sessions to test")
    run_parser.add_argument("--error-threshold", type=float, default=0.05, help="Max error rate (default 5%%)")
    run_parser.add_argument("--score-threshold", type=float, default=-0.1, help="Max score regression (default -0.1)")
    run_parser.add_argument("--output", default="result.json", help="Output file for results")
    
    # Report command
    report_parser = subparsers.add_parser("report", help="Generate gate report")
    report_parser.add_argument("--input", default="result.json", help="Input file with gate results")
    report_parser.add_argument("--format", choices=["github-comment", "json"], default="github-comment")
    report_parser.add_argument("--output", help="Output file (default: stdout)")
    
    args = parser.parse_args()
    
    if args.command == "run":
        # Run gate
        result = run_gate(
            change_type=args.change_type,
            change_id=args.change_id,
            change_file=args.change_file,
            sessions=args.sessions,
            error_threshold=args.error_threshold,
            score_threshold=args.score_threshold,
        )
        
        # Save result
        with open(args.output, 'w') as f:
            json.dump(result, f, indent=2)
        
        logger.info(f"Gate result saved to {args.output}")
        
        # Exit with error code if gate failed
        if result["verdict"] == "fail":
            sys.exit(1)
    
    elif args.command == "report":
        # Load result
        with open(args.input, 'r') as f:
            result = json.load(f)
        
        # Format report
        if args.format == "github-comment":
            report = format_github_comment(result)
        else:
            report = format_json(result)
        
        # Output report
        if args.output:
            with open(args.output, 'w') as f:
                f.write(report)
            logger.info(f"Report saved to {args.output}")
        else:
            print(report)
    
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
