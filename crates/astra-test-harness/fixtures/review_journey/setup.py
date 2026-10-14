#!/usr/bin/env python3
"""Create a small review repository only in an explicitly empty harness cwd."""

from pathlib import Path
import os
import shutil
import subprocess


root = Path.cwd()
if any(root.iterdir()):
    raise SystemExit("review fixture requires an empty --working-dir")
source = Path(__file__).resolve().parents[4]
skill = root / ".agent/skills/review_changes/SKILL.md"
skill.parent.mkdir(parents=True)
shutil.copyfile(source / ".agent/skills/review_changes/SKILL.md", skill)


def git(*args: str) -> None:
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull, GIT_TERMINAL_PROMPT="0")
    subprocess.run(
        ["git", "-c", "core.hooksPath=" + os.devnull, "-c", "commit.gpgsign=false",
         "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", *args],
        check=True, timeout=10, stdout=subprocess.DEVNULL, env=env,
    )


git("init", "-q", "--template=")
module = root / "counter.py"
module.write_text("def increment(value):\n    return value + 1\n\ndef is_even(value):\n    return value % 2 == 0\n")
git("add", ".")
git("commit", "-qm", "Baseline counter helpers")
module.write_text(module.read_text().replace("value + 1", "value - 1"))
git("commit", "-qam", "Adjust increment arithmetic")
module.write_text(module.read_text().replace("value % 2 == 0", "value % 2 == 1"))
git("commit", "-qam", "Adjust parity classification")
