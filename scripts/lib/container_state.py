#!/usr/bin/env python3
"""Credential-free projections of native container inspection and dry-run output."""

import json
import os
import re
import signal
import subprocess
import sys
from collections import deque


def stop_command(process, grace=5):
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=grace)
    except (subprocess.TimeoutExpired, ProcessLookupError):
        pass
    finally:
        # The parent can exit while descendants still hold our output pipes.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def interrupt_command(signum, _frame):
    raise InterruptedError(signum)


def state(container):
    value = container.get("State", {})
    status = value.get("Status", "unknown")
    if value.get("Running"):
        health = value.get("Health") or value.get("Healthcheck") or {}
        config_health = (container.get("Config") or {}).get("Healthcheck") or {}
        has_check = config_health.get("Test") and config_health["Test"] != ["NONE"]
        return health.get("Status") or ("starting" if has_check else "running")
    if status == "exited" and value.get("ExitCode") == 0:
        labels = (container.get("Config") or {}).get("Labels") or {}
        if labels.get("io.astra.lifecycle") == "init":
            return "completed"
    return status


def plan_keeps_container(engine, name, output):
    if engine == "docker":
        if re.search(r" (Create|Created|Recreate|Recreated|Remove|Removed)( |$)", output):
            return False
        return bool(re.search(r" Container .* Running( |$)", output))
    # Podman Compose's dry run describes native commands, not Docker's plan
    # table. Positive evidence is required: an empty or unfamiliar trace must
    # never authorize reuse. Never include this credential-bearing input in a
    # diagnostic or a temporary file.
    if re.search(r"\bpodman\b.*\b(create|rm|remove)\b", output):
        return False
    return any(
        line.rstrip().endswith("** skipping create: " + name)
        for line in output.splitlines()
    )


def removal_order(containers):
    dependencies = {item["Id"]: item.get("Dependencies", []) for item in containers}
    if any(not isinstance(key, str) or not isinstance(value, list)
           or any(not isinstance(dependency, str) for dependency in value)
           for key, value in dependencies.items()):
        raise ValueError("Invalid dependency metadata")
    dependents = dict.fromkeys(dependencies, 0)
    for values in dependencies.values():
        for dependency in values:
            if dependency in dependents:
                dependents[dependency] += 1
    ready = deque(key for key, count in dependents.items() if count == 0)
    result = []
    while ready:
        key = ready.popleft()
        result.append(key)
        for dependency in dependencies[key]:
            if dependency in dependents:
                dependents[dependency] -= 1
                if dependents[dependency] == 0:
                    ready.append(dependency)
    if len(result) != len(dependencies):
        raise ValueError("Cyclic dependency metadata")
    return result


def main():
    action = sys.argv[1]
    if action == "run-timeout":
        signal.signal(signal.SIGTERM, interrupt_command)
        process = subprocess.Popen(sys.argv[3:], start_new_session=True)
        try:
            return process.wait(timeout=int(sys.argv[2]))
        except subprocess.TimeoutExpired:
            print("Container startup timed out; inspect the retained services and logs.", file=sys.stderr)
            stop_command(process, grace=0.25)
            return 124
        except KeyboardInterrupt:
            stop_command(process)
            return 130
        except InterruptedError as error:
            stop_command(process)
            return 128 + error.args[0]
    if action == "plan":
        return 0 if plan_keeps_container(sys.argv[2], sys.argv[3], sys.stdin.read()) else 1
    try:
        containers = json.load(sys.stdin)
        if not isinstance(containers, list) or not containers:
            return 1
        container = containers[0]
        if action == "removal-order":
            print("\n".join(removal_order(containers)))
        elif action == "state":
            print(state(container))
        elif action == "name":
            print(container["Name"].lstrip("/"))
        elif action == "networks":
            print(len((container.get("NetworkSettings") or {}).get("Networks") or {}))
        elif action == "service":
            labels = (container.get("Config") or {}).get("Labels") or {}
            print(labels.get(sys.argv[2], ""))
        else:
            return 1
    except (AttributeError, KeyError, TypeError, ValueError):
        # Inspection contains environment variables. Do not echo its payload.
        print("Cannot read container metadata", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
