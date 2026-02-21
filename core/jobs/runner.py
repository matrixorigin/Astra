"""Standalone job runner — invoked as subprocess by LocalJobBackend.

Usage:
    python -m core.jobs.runner --job-type feedback_trainer --inputs '{"epochs": 10}'

Prints JSON result to stdout. Errors go to stderr.
"""

import argparse
import importlib
import json
import sys


# Registry of job_type → module.function
JOB_REGISTRY: dict[str, str] = {
    # "feedback_trainer": "core.training.feedback_trainer:run",
    # "corpus_collector": "core.training.corpus_collector:run",
}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--job-type", required=True)
    parser.add_argument("--inputs", default="{}")
    args = parser.parse_args()

    inputs = json.loads(args.inputs)
    entry = JOB_REGISTRY.get(args.job_type)
    if not entry:
        print(f"Unknown job type: {args.job_type}", file=sys.stderr)
        sys.exit(1)

    module_path, func_name = entry.rsplit(":", 1)
    module = importlib.import_module(module_path)
    func = getattr(module, func_name)
    result = func(**inputs)

    json.dump(result if isinstance(result, dict) else {"result": result}, sys.stdout)


if __name__ == "__main__":
    main()
