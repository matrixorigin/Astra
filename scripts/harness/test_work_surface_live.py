#!/usr/bin/env python3
"""Offline contracts for the opt-in Web + TUI Work journey runner."""

from __future__ import annotations

import importlib.util
import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("work_surface_live.py")
SPEC = importlib.util.spec_from_file_location("astra_work_surface_live", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)


class WorkSurfaceLiveContracts(unittest.TestCase):
    def test_screen_is_current_viewport_and_handles_clear(self) -> None:
        screen = live.Screen(rows=4, columns=20)
        screen.feed(b"old output")
        self.assertIn("old output", screen.text())
        screen.feed(b"\x1b[2Jnew output")
        self.assertIn("new output", screen.text())
        self.assertNotIn("old output", screen.text())

    def test_screen_cursor_and_line_erase(self) -> None:
        screen = live.Screen(rows=3, columns=20)
        screen.feed(b"first\nsecond")
        screen.feed(b"\x1b[2K\rreplacement")
        self.assertIn("replacement", screen.text())
        self.assertNotIn("second", screen.text())

    def test_atomic_state_is_private_and_complete(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            live.atomic_write_json(path, {"schema_version": 1, "phase": "started"})
            self.assertEqual(json.loads(path.read_text()), {"schema_version": 1, "phase": "started"})
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            self.assertEqual(list(Path(directory).glob("*.tmp")), [])

    def test_control_requires_a_new_id(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "control.json"
            live.atomic_write_json(
                path,
                {"schema_version": 1, "command": "web_observed", "control_id": "one"},
            )
            first = live.wait_control(path, "web_observed", live.utc_seconds() + 0.2)
            self.assertEqual(first["control_id"], "one")
            # A consumed command is not replayed as a second browser event.
            with self.assertRaises(live.HarnessError):
                live.wait_control(
                    path,
                    "web_observed",
                    live.utc_seconds() + 0.2,
                    after_control_id="one",
                )

    def test_control_fails_when_browser_exits(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(live.HarnessError):
                live.wait_control(
                    Path(directory) / "control.json",
                    "web_observed",
                    live.utc_seconds() + 1,
                    is_alive=lambda: False,
                )

    def test_milestones_survive_a_fast_phase_transition(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            live.write_phase(path, phase="started", work_id="work-1", branch_id="branch-1")
            live.write_phase(path, phase="tui_exited", run_id="run-1", events_at_tui_exit=3)
            live.write_phase(path, phase="run_settled", run_id="run-1", post_exit_run={"events_count": 4})
            state = live.read_json(path)
            self.assertIsNotNone(state)
            assert state is not None
            self.assertEqual(state["phase"], "run_settled")
            self.assertEqual(state["milestones"]["tui_exited"]["events_at_tui_exit"], 3)
            self.assertEqual(state["milestones"]["run_settled"]["post_exit_run"]["events_count"], 4)

    def test_real_pty_reads_split_utf8_sets_window_and_closes_idempotently(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stub = root / "stub.py"
            stub.write_text(
                """#!/usr/bin/env python3
import fcntl
import os
import struct
import sys
import signal
import termios
import time

rows, columns, _, _ = struct.unpack(
    'HHHH', fcntl.ioctl(0, termios.TIOCGWINSZ, struct.pack('HHHH', 0, 0, 0, 0))
)
signal.signal(signal.SIGHUP, lambda _signum, _frame: sys.exit(0))
os.write(1, f'SIZE {rows}x{columns}\\nWork started '.encode())
os.write(1, b'\\xc2')
time.sleep(0.02)
os.write(1, b'\\xb7 work-1\\n')
sys.stdout.flush()
time.sleep(30)
""",
                encoding="utf-8",
            )
            stub.chmod(0o755)
            workspace = root / "workspace"
            home = root / "home"
            workspace.mkdir()
            home.mkdir()
            tui = live.PtyTui(
                binary=stub,
                api_url="http://127.0.0.1:1",
                model="test-model",
                token="test-token",
                profile="test-profile",
                home=home,
                workspace=workspace,
                log=root / "tui.log",
            )
            try:
                tui.start()
                tui.wait_for_text("SIZE 30x100", live.utc_seconds() + 3)
                match = tui.wait_for_regex(r"Work started · (work-1)", live.utc_seconds() + 3)
                self.assertEqual(match.group(1), "work-1")
                self.assertTrue(tui.is_alive())
                self.assertTrue(tui.is_alive())
                self.assertEqual(tui.stop(live.utc_seconds() + 3), 0)
                self.assertFalse(tui.is_alive())
                self.assertFalse(tui.is_alive())
                tui.close()
                tui.close()
            finally:
                tui.close()

    def test_work_identity_and_delivery_branch_are_pinned(self) -> None:
        overview = {
            "overview": {
                "work_id": "work-1",
                "delivery_branch": {"branch_id": "branch-1"},
            }
        }
        self.assertEqual(live.branch_from_overview(overview, "work-1"), "branch-1")
        with self.assertRaises(live.HarnessError):
            live.branch_from_overview(overview, "work-2")

    def test_live_lane_rejects_tracked_source_drift_before_starting_processes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "--quiet", str(root)], check=True)
            subprocess.run(
                ["git", "-C", str(root), "config", "user.email", "harness@example.invalid"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(root), "config", "user.name", "Harness"],
                check=True,
            )
            tracked = root / "tracked.txt"
            tracked.write_text("clean\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(root), "add", "tracked.txt"], check=True)
            subprocess.run(
                ["git", "-C", str(root), "commit", "--quiet", "-m", "fixture"], check=True
            )
            tracked.write_text("edited\n", encoding="utf-8")
            with self.assertRaises(live.NotTestedError):
                live.ensure_source_checkout_is_clean(root)

    def test_live_lane_ignores_untracked_run_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "--quiet", str(root)], check=True)
            subprocess.run(
                ["git", "-C", str(root), "config", "user.email", "harness@example.invalid"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(root), "config", "user.name", "Harness"],
                check=True,
            )
            tracked = root / "tracked.txt"
            tracked.write_text("clean\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(root), "add", "tracked.txt"], check=True)
            subprocess.run(
                ["git", "-C", str(root), "commit", "--quiet", "-m", "fixture"], check=True
            )
            (root / "run-artifact.log").write_text("generated\n", encoding="utf-8")
            live.ensure_source_checkout_is_clean(root)

    @unittest.skipUnless(sys.platform.startswith("linux"), "requires Linux /proc and prctl")
    def test_supervised_leader_exit_reaps_owned_child(self) -> None:
        """A crashed Web/Playwright leader must not leave its child alive."""

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child_pid_path = root / "child.pid"
            leader = root / "leader.py"
            leader.write_text(
                """import os
import pathlib
import time

# File existence means a complete PID; leader exit means the child is ready.
ready_read, ready_write = os.pipe()
if os.fork() == 0:
    os.close(ready_read)
    child_pid = pathlib.Path(os.environ['CHILD_PID'])
    staged_pid = child_pid.with_suffix('.tmp')
    staged_pid.write_text(str(os.getpid()), encoding='utf-8')
    staged_pid.replace(child_pid)
    os.write(ready_write, b'1')
    os.close(ready_write)
    time.sleep(30)
    os._exit(0)
os.close(ready_write)
if os.read(ready_read, 1) != b'1':
    os._exit(1)
os.close(ready_read)
os._exit(0)
""",
                encoding="utf-8",
            )
            output = (root / "supervisor.log").open("w", encoding="utf-8")
            process = live.start_supervised_process(
                [sys.executable, str(leader)],
                cwd=root,
                env={**os.environ, "CHILD_PID": str(child_pid_path)},
                stdin=subprocess.DEVNULL,
                stdout=output,
                stderr=subprocess.STDOUT,
                identity="work-live-leader-exit",
            )
            try:
                deadline = time.monotonic() + 5
                while not child_pid_path.exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(child_pid_path.exists())
                child_pid = int(child_pid_path.read_text(encoding="utf-8"))
                self.assertEqual(process.wait(timeout=5), 0)
                deadline = time.monotonic() + 5
                while Path(f"/proc/{child_pid}").exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertFalse(Path(f"/proc/{child_pid}").exists())
            finally:
                live.terminate_process(process)
                output.close()

    def test_source_contract_keeps_live_lane_out_of_offline(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("pty.fork()", source)
        self.assertIn("termios.TIOCSWINSZ", source)
        self.assertIn("signal.SIGHUP", source)
        self.assertIn("start_supervised_process", source)
        self.assertIn("PROCESS_SUPERVISOR", source)
        self.assertIn('current["events_count"] > tui_exit_events', source)
        self.assertIn("SUCCESS_RUN_STATUSES", source)
        self.assertIn("same root Run to settle after TUI exit", source)
        self.assertNotIn("dev-seed", source)
        makefile = Path(__file__).parents[2] / "Makefile"
        make = makefile.read_text(encoding="utf-8")
        self.assertIn("test-work-live", make)
        self.assertNotIn("test-work-live", make.split(".PHONY: test-offline", 1)[1].split(".PHONY:", 1)[0])


if __name__ == "__main__":
    unittest.main()
