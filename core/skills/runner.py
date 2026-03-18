"""Standalone skill runner — entry point for subprocess/K8s/Ray execution.

Usage: python -m core.skills.runner --skill feedback_classifier --inputs '{"text": "..."}'

Outputs JSON to stdout. Exit code 0 = success, non-zero = failure.
"""

import argparse
import json
import sys

from core.skills.runner_index import runner_skill_exists

DIAGNOSE_HINT = "Run 'diagnose_skills' to check skill health"


def main() -> None:
    parser = argparse.ArgumentParser(description="Run a skill in isolation")
    parser.add_argument("--skill", required=True, help="Skill name")
    parser.add_argument("--inputs", required=True, help="JSON inputs")
    args = parser.parse_args()

    try:
        inputs = json.loads(args.inputs)
    except (json.JSONDecodeError, ValueError) as e:
        print(json.dumps({"error": f"Invalid JSON inputs: {e}"}), file=sys.stderr)
        sys.exit(1)

    if not runner_skill_exists(args.skill):
        print(
            json.dumps(
                {
                    "error": f"Skill '{args.skill}' not found",
                    "hint": DIAGNOSE_HINT,
                }
            ),
            file=sys.stderr,
        )
        sys.exit(1)

    try:
        from api.database import SessionLocal
        from core.code_executor import CodeExecutor
        from core.runtime import IsolationLevel, create_runtime
        from core.skills.builtin import register_builtin_skills
        from core.skills.catalog import SkillCatalog

        registry = SkillCatalog(SessionLocal)
        code_executor = CodeExecutor(
            runtime=create_runtime(min_isolation=IsolationLevel.PROCESS),
            db_factory=SessionLocal,
        )
        register_builtin_skills(registry, SessionLocal, code_executor=code_executor)

        skill = registry.get(args.skill)
        if not skill:
            print(
                json.dumps(
                    {
                        "error": f"Skill '{args.skill}' not found",
                        "hint": DIAGNOSE_HINT,
                    }
                ),
                file=sys.stderr,
            )
            sys.exit(1)

        import asyncio

        validated = skill.validate_input(inputs)
        result = asyncio.run(skill.execute(validated))

        if hasattr(result, "model_dump"):
            output = result.model_dump()
        elif isinstance(result, dict):
            output = result
        else:
            output = {"output": str(result)}

        json.dump(output, sys.stdout)
    except ImportError as e:
        print(
            json.dumps(
                {
                    "error": f"Failed to load skill: {e}",
                    "hint": DIAGNOSE_HINT,
                }
            ),
            file=sys.stderr,
        )
        sys.exit(1)
    except Exception as e:
        err_type = type(e).__name__
        # Skill-related errors get the hint
        if "skill" in str(e).lower() or "load" in str(e).lower():
            print(
                json.dumps(
                    {
                        "error": f"{err_type}: {e}",
                        "hint": DIAGNOSE_HINT,
                    }
                ),
                file=sys.stderr,
            )
        else:
            print(json.dumps({"error": f"{err_type}: {e}"}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
