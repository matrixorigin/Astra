"""Standalone skill runner — entry point for subprocess/K8s/Ray execution.

Usage: python -m core.skills.runner --skill feedback_classifier --inputs '{"text": "..."}'

Outputs JSON to stdout. Exit code 0 = success, non-zero = failure.
"""

import argparse
import json
import sys


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

    try:
        from api.database import get_db_session
        from core.skills.registry import SkillRegistry
        from core.skills.builtin import register_builtin_skills
        from core.code_executor import CodeExecutor
        from core.runtime import create_runtime, IsolationLevel

        db_gen = get_db_session()
        db = next(db_gen)
        try:
            registry = SkillRegistry(db)
            code_executor = CodeExecutor(
                runtime=create_runtime(min_isolation=IsolationLevel.PROCESS), db=db,
            )
            register_builtin_skills(registry, db, code_executor=code_executor)

            skill = registry.get(args.skill)
            if not skill:
                print(json.dumps({"error": f"Skill '{args.skill}' not found"}), file=sys.stderr)
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
        finally:
            db_gen.close()
    except Exception as e:
        print(json.dumps({"error": str(e)}), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
