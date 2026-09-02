#!/usr/bin/env python3
import json
import multiprocessing
import os
import tempfile
import threading
import unittest
from contextlib import contextmanager
from unittest import mock
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).parent))
import case_history


def _contended_admission(
    jobs, root, history_read, contender_lock_acquired, continue_admission, result
):
    original = case_history._recorded_tasks
    original_lock = case_history._allocator_lock

    def paused_history(path):
        value = original(path)
        history_read.set()
        if not continue_admission.wait(5):
            raise RuntimeError("test did not resume contender")
        return value

    @contextmanager
    def observed_lock(path):
        with original_lock(path):
            contender_lock_acquired.set()
            yield

    arguments = [
        "case_history.py", "--jobs-dir", jobs, "--reservation-dir", root,
        "--reservation-owner-pid", str(os.getpid()), "--task", "/dataset/qemu-startup",
        "--task", "/dataset/new-a", "--task", "/dataset/new-b",
    ]
    with mock.patch.object(case_history, "_recorded_tasks", paused_history), \
         mock.patch.object(case_history, "_allocator_lock", observed_lock), \
         mock.patch("sys.argv", arguments):
        result.put(case_history.main())


class CaseHistoryTest(unittest.TestCase):
    def test_ignores_a_configured_task_without_an_official_result(self):
        with tempfile.TemporaryDirectory() as temporary:
            jobs = Path(temporary)
            job = jobs / "old"
            job.mkdir()
            (job / "config.json").write_text(
                json.dumps({"tasks": [{"path": "/dataset/qemu-startup"}]}),
                encoding="utf-8",
            )
            self.assertEqual(case_history._recorded_tasks(jobs), set())

    def test_rejects_an_officially_completed_task_without_regression_marker(self):
        with tempfile.TemporaryDirectory() as temporary:
            jobs = Path(temporary)
            job = jobs / "old"
            job.mkdir()
            (job / "config.json").write_text(
                json.dumps({"tasks": [{"path": "/dataset/qemu-startup"}]}),
                encoding="utf-8",
            )
            (job / "result.json").write_text(
                json.dumps(
                    {
                        "stats": {
                            "evals": {
                                "astra": {
                                    "reward_stats": {
                                        "reward": {"0.0": ["qemu-startup__trial"]}
                                    }
                                }
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            trial = job / "qemu-startup__trial"
            trial.mkdir()
            (trial / "result.json").write_text(
                json.dumps(
                    {
                        "finished_at": "2026-01-01T00:00:00Z",
                        "task_name": "terminal-bench/qemu-startup",
                        "task_id": {"path": "/dataset/qemu-startup"},
                        "verifier_result": {"rewards": {"reward": 0.0}},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(case_history.HistoryError, "qemu-startup"):
                case_history.validate_selection(
                    ["/dataset/qemu-startup", "/dataset/new-a", "/dataset/new-b"],
                    case_history._recorded_tasks(jobs),
                    set(),
                )

    def test_records_sealed_trial_identity_from_its_official_result(self):
        with tempfile.TemporaryDirectory() as temporary:
            jobs = Path(temporary)
            sealed = "a" * 64
            job = jobs / "old"
            trial = job / (sealed[:32] + "__trial")
            trial.mkdir(parents=True)
            task_path = f"/proc/123/fd/198/tasks/{sealed}"
            (job / "config.json").write_text(
                json.dumps({"tasks": [{"path": task_path}]}), encoding="utf-8"
            )
            (trial / "result.json").write_text(
                json.dumps(
                    {
                        "finished_at": "2026-01-01T00:00:00Z",
                        "task_name": "terminal-bench/qemu-startup",
                        "task_id": {"path": task_path},
                        "verifier_result": {"rewards": {"reward": 1.0}},
                    }
                ), encoding="utf-8"
            )
            self.assertEqual(case_history._recorded_tasks(jobs), {"qemu-startup"})

    def test_live_reservation_rejects_a_second_owner_and_releases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "reservations"
            task = "/dataset/qemu-startup"
            case_history.reserve_selection([task], root, os.getpid())
            with self.assertRaisesRegex(case_history.HistoryError, "already reserved"):
                case_history.reserve_selection([task], root, os.getpid())
            case_history.release_selection([task], root, os.getpid())

    def test_reservation_manifest_releases_after_an_exec_boundary(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "reservations"
            manifest = Path(temporary) / "state" / "reservation.json"
            manifest.parent.mkdir()
            task = "/dataset/qemu-startup"
            case_history.reserve_selection([task], root, os.getpid(), manifest)
            case_history.release_reservation_manifest(manifest)
            case_history.reserve_selection([task], root, os.getpid())
            case_history.release_selection([task], root, os.getpid())

    def test_reclaims_a_stale_reservation_under_the_allocator_lock(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "reservations"
            root.mkdir()
            task = "/dataset/qemu-startup"
            stale = case_history._reservation_path(root, "qemu-startup")
            stale.mkdir()
            (stale / "lease.json").write_text(
                json.dumps({"pid": 999999, "pid_start_time": "never"}), encoding="utf-8"
            )
            case_history.reserve_selection([task], root, os.getpid())
            with self.assertRaisesRegex(case_history.HistoryError, "already reserved"):
                case_history.reserve_selection([task], root, os.getpid())
            case_history.release_selection([task], root, os.getpid())

    def test_completed_handoff_rechecks_history_before_admitting_a_contender(self):
        with tempfile.TemporaryDirectory() as temporary:
            jobs = Path(temporary) / "jobs"
            job = jobs / "owner"
            job.mkdir(parents=True)
            task_path = "/dataset/qemu-startup"
            (job / "config.json").write_text(
                json.dumps({"tasks": [{"path": task_path}]}), encoding="utf-8"
            )
            root = Path(temporary) / "reservations"
            case_history.reserve_selection([task_path], root, os.getpid())
            context = multiprocessing.get_context("fork")
            history_read = context.Event()
            contender_lock_acquired = context.Event()
            continue_admission = context.Event()
            result = context.Queue()
            contender = context.Process(
                target=_contended_admission,
                args=(
                    str(jobs), str(root), history_read, contender_lock_acquired,
                    continue_admission, result,
                ),
            )
            contender.start()
            releaser = None
            try:
                self.assertTrue(history_read.wait(5))
                # On the fixed path, history is read while holding this lock.
                # The old split transaction reaches the read before this event.
                self.assertTrue(contender_lock_acquired.is_set())
                trial = job / "qemu-startup__trial"
                trial.mkdir()
                (trial / "result.json").write_text(
                    json.dumps(
                        {
                            "finished_at": "2026-01-01T00:00:00Z",
                            "task_name": "terminal-bench/qemu-startup",
                            "task_id": {"path": task_path},
                            "verifier_result": {"rewards": {"reward": 1.0}},
                        }
                    ), encoding="utf-8"
                )
                releaser = threading.Thread(
                    target=case_history.release_selection,
                    args=([task_path], root, os.getpid()),
                )
                releaser.start()
                continue_admission.set()
                contender.join(5)
                releaser.join(5)
                self.assertFalse(contender.is_alive())
                self.assertFalse(releaser.is_alive())
                self.assertEqual(result.get(timeout=1), 78)
            finally:
                continue_admission.set()
                if contender.is_alive():
                    contender.terminate()
                contender.join(5)
                if releaser is not None:
                    releaser.join(5)

    def test_records_explicit_regression_separately_from_new_tasks(self):
        result = case_history.validate_selection(
            ["/dataset/qemu-startup", "/dataset/new-a", "/dataset/new-b"],
            {"qemu-startup"},
            {"qemu-startup"},
        )
        self.assertEqual(result["regression_tasks"], ["qemu-startup"])
        self.assertEqual(result["new_tasks"], ["new-a", "new-b"])

    def test_rejects_an_allowlist_entry_not_selected(self):
        with self.assertRaisesRegex(case_history.HistoryError, "outside this round"):
            case_history.validate_selection(
                ["/dataset/new-a", "/dataset/new-b", "/dataset/new-c"],
                set(),
                {"old"},
            )

    def test_uses_cached_package_parent_as_task_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            version = (
                Path(temporary)
                / "packages"
                / "terminal-bench"
                / "qemu-startup"
                / ("a" * 64)
            )
            version.mkdir(parents=True)
            (version / "task.toml").write_text("[task]\n", encoding="utf-8")
            self.assertEqual(case_history._task_name(str(version)), "qemu-startup")

            jobs = Path(temporary) / "jobs"
            job = jobs / "old"
            job.mkdir(parents=True)
            (job / "config.json").write_text(
                json.dumps({"tasks": [{"path": str(version)}]}),
                encoding="utf-8",
            )
            (job / "result.json").write_text(
                json.dumps(
                    {
                        "stats": {
                            "evals": {
                                "astra": {
                                    "reward_stats": {
                                        "reward": {"1.0": ["qemu-startup__trial"]}
                                    }
                                }
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            trial = job / "qemu-startup__trial"
            trial.mkdir()
            (trial / "result.json").write_text(
                json.dumps(
                    {
                        "finished_at": "2026-01-01T00:00:00Z",
                        "task_name": "terminal-bench/qemu-startup",
                        "task_id": {"path": str(version)},
                        "verifier_result": {"rewards": {"reward": 1.0}},
                    }
                ),
                encoding="utf-8",
            )
            result = case_history.validate_selection(
                [str(version), "/dataset/new-a", "/dataset/new-b"],
                case_history._recorded_tasks(jobs),
                {"qemu-startup"},
            )
            self.assertEqual(result["regression_tasks"], ["qemu-startup"])
