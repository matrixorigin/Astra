#!/usr/bin/env python3
"""Offline contracts for deployment through the two native container CLIs."""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import unittest
import uuid


ROOT = Path(__file__).resolve().parents[2]
RUNTIME = ROOT / "scripts/lib/container_runtime.sh"
SPEC = importlib.util.spec_from_file_location("container_state", ROOT / "scripts/lib/container_state.py")
STATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STATE)

STUB = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys
import time
import signal
name = Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["TEST_CALLS"], "a") as log:
    log.write(json.dumps({"tool": name, "args": args, "userns": os.getenv("ASTRA_DEPENDENCY_USERNS"),
        "image": os.getenv("ASTRA_IMAGE"), "logs": os.getenv("MATRIXONE_LOG_DIR"),
        "stack_env_file": os.getenv("ASTRA_STACK_ENV_FILE"), "compose_file": os.getenv("COMPOSE_FILE"), "uid": os.getenv("UID"),
        "env_hash": os.getenv("ASTRA_STACK_ENV_HASH"), "has_mysql_password": bool(os.getenv("MYSQL_PWD")), "pid": os.getpid()}) + "\n")
is_compose = name == "podman-compose" or args[:1] == ["compose"]
if name in ("mysql", "mysqldump") and args == ["--help"]:
    print("--ssl-mode")
elif args[:1] == ["--version"]:
    print("podman version " + os.getenv("TEST_PODMAN_VERSION", "6.1.4") if name == "podman" or os.getenv("TEST_DOCKER_WRAPPER") else "Docker version 29.0.0")
elif is_compose and "version" in args:
    print(os.getenv("TEST_COMPOSE_VERSION", "1.6.0") if name == "podman-compose" else "Docker Compose version v2.39.4")
elif is_compose and "--help" in args:
    print("--dry-run")
elif args[:1] == ["info"]:
    if os.getenv("TEST_ENGINE_DOWN"):
        sys.exit(1)
    print("true" if "--format" in args else "{}")
elif not is_compose and args[:1] == ["ps"]:
    time.sleep(float(os.getenv("TEST_PS_DELAY", "0")))
    if os.getenv("TEST_MISSING"):
        sys.exit(0)
    service = "api"
    for arg in args:
        if ".service=" in arg:
            service = arg.split(".service=", 1)[1]
    replicas = int(os.getenv("TEST_REPLICAS", "1"))
    print("\n".join(service + str(i) for i in range(replicas)))
elif not is_compose and args[:1] == ["inspect"]:
    if os.getenv("TEST_INSPECT_CHILD_IGNORE_TERM"):
        import subprocess
        subprocess.Popen([sys.executable, "-c", """
import json, os, signal, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
with open(os.environ["TEST_CALLS"], "a") as log:
    log.write(json.dumps({"tool": "inspect-child", "pid": os.getpid()}) + "\\n")
time.sleep(30)
"""])
    if os.getenv("TEST_INSPECT_IGNORE_TERM"):
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    time.sleep(float(os.getenv("TEST_INSPECT_DELAY", "0")))
    if os.getenv("TEST_BAD_INSPECT"):
        print("not JSON: do-not-print-this-secret")
        sys.exit(0)
    containers = []
    dependencies = json.loads(os.getenv("TEST_DEPENDENCIES", "{}"))
    for cid in args[1:]:
        status = os.getenv("TEST_STATE", "healthy")
        init = "init" in cid
        state = {"Running": status not in ("exited", "dead"), "Status": "running", "ExitCode": 0}
        if init:
            state.update(Running=False, Status="exited")
        elif status in ("exited", "dead"):
            state["Status"] = status
        else:
            state["Healthcheck" if name == "podman" else "Health"] = {"Status": status}
        containers.append({"Id": cid, "Dependencies": dependencies.get(cid, []), "Name": "/project-" + cid, "State": state,
            "Config": {"Labels": {"io.astra.lifecycle": "init"} if init else {}, "Env": ["HIDDEN=do-not-print-this-secret"]},
            "NetworkSettings": {"Networks": {"project_default": {}}}})
    print(json.dumps(containers))
elif is_compose and "--dry-run" in args:
    plan = os.getenv("TEST_PLAN", "reuse")
    if plan == "error":
        print("do-not-print-this-secret", file=sys.stderr)
        sys.exit(1)
    if plan == "empty":
        sys.exit(0)
    if name == "podman-compose":
        print("DEBUG:podman_compose:credential=do-not-print-this-secret")
        if plan == "reuse":
            print("DEBUG:podman_compose:** skipping create: project-api0")
        else:
            print("INFO:podman_compose:podman create --name=project-api0")
    else:
        print(" Container project-api0 " + ("Running" if plan == "reuse" else "Recreate"))
elif is_compose and "config" in args and "--services" in args:
    print("matrixone-init\nmatrixone\nmemoria\napi")
elif is_compose and "up" in args and os.getenv("TEST_DEPENDENCY_HANG"):
    time.sleep(30)
elif not is_compose and args[:1] == ["exec"]:
    if "sh" in args and os.getenv("TEST_NATIVE_CLIENT_MISSING"):
        sys.exit(1)
    print("CREATE TABLE smoke (id INT);" if "mysqldump" in args else "ok")
elif not is_compose and args[:1] == ["build"]:
    sys.exit(int(os.getenv("TEST_BUILD_EXIT", "0")))
elif name == "mysqldump":
    print("CREATE TABLE smoke (id INT);")
sys.exit(0)
'''


class RuntimeContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="astra-container-contract-")
        self.addCleanup(self.directory.cleanup)
        self.work = Path(self.directory.name)
        self.bin = self.work / "bin"
        self.bin.mkdir()
        for name in ("docker", "podman", "podman-compose"):
            tool = self.bin / name
            tool.write_text(STUB)
            tool.chmod(0o755)
        self.env_file = self.work / "stack.env"
        self.env_file.write_text("ASTRA_STACK_NAME=project\nASTRA_IMAGE=matrixorigin/astra:0.2.11\n")
        self.calls_file = self.work / "calls.jsonl"
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("ASTRA_", "TEST_", "PODMAN_"))}
        self.env.update(PATH=str(self.bin) + os.pathsep + os.environ["PATH"], TEST_CALLS=str(self.calls_file))
        self.compose = ROOT / "deployment/all-in-one/docker-compose.yml"

    def calls(self):
        return [json.loads(line) for line in self.calls_file.read_text().splitlines()] if self.calls_file.exists() else []

    def run_runtime(self, *args, engine=None, timeout=None, **changes):
        env = {**self.env, **changes}
        if engine is not None:
            env["ASTRA_CONTAINER_ENGINE"] = engine
        return subprocess.run(
            [str(RUNTIME), "--env-file", str(self.env_file), "--file", str(self.compose), *args],
            env=env, text=True, capture_output=True, timeout=timeout,
        )

    def test_default_and_file_and_explicit_selection(self):
        self.assertEqual(self.run_runtime("engine").stdout.strip(), "docker")
        self.env_file.write_text("ASTRA_CONTAINER_ENGINE=podman\n")
        self.assertEqual(self.run_runtime("engine").stdout.strip(), "podman")
        self.assertEqual(self.run_runtime("engine", engine="docker").stdout.strip(), "docker")
        for engine in ("", "other", "podman; echo injected"):
            result = self.run_runtime("compose", "up", engine=engine)
            self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_native_commands_and_explicit_project_isolation(self):
        for engine in ("docker", "podman"):
            with self.subTest(engine=engine):
                self.calls_file.unlink(missing_ok=True)
                result = self.run_runtime("compose", "config", "--quiet", engine=engine,
                    COMPOSE_PROJECT_NAME="wrong", COMPOSE_FILE="wrong", ASTRA_STACK_NAME="wrong", DOCKER_HOST="unix:///does-not-exist")
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = self.calls()
                self.assertTrue(all(c["tool"] in (("podman", "podman-compose") if engine == "podman" else ("docker",)) for c in calls))
                config = next(c for c in calls if "config" in c["args"])
                self.assertIn(str(self.env_file), config["args"])
                self.assertIn(str(self.compose), config["args"])
                self.assertEqual(config["args"][config["args"].index("--project-name") + 1], "project")
                self.assertIsNone(config["compose_file"])
                if engine == "podman":
                    self.assertIn("--podman-run-args=--http-proxy=false", config["args"])
                    self.assertEqual(config["userns"], "keep-id")
                    self.assertEqual(config["image"], "docker.io/matrixorigin/astra:0.2.11")
                    self.assertIn("/podman/project/", config["logs"])

    def test_relative_descriptor_is_projected_as_absolute_env_file(self):
        logical = self.work / "logical"
        logical.symlink_to(self.work, target_is_directory=True)
        self.env_file = logical / self.env_file.name
        env = {**self.env, "ASTRA_CONTAINER_ENGINE": "podman"}
        result = subprocess.run([str(RUNTIME), "--env-file", self.env_file.name, "--file", str(self.compose),
            "compose", "config", "--quiet"], cwd=self.work, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        config = next(c for c in self.calls() if "config" in c["args"])
        projected = Path(config["stack_env_file"])
        self.assertTrue(projected.is_absolute())
        self.assertEqual(projected.resolve(), self.env_file.resolve())

    def test_env_file_contents_participate_in_configuration(self):
        self.run_runtime("compose", "config", "--quiet", engine="podman")
        first = next(c["env_hash"] for c in self.calls() if "config" in c["args"])
        self.calls_file.unlink()
        with self.env_file.open("a") as file:
            file.write("ASTRA_LLM_PROVIDER_ADMISSION_RPM=12\n")
        self.run_runtime("compose", "config", "--quiet", engine="podman")
        second = next(c["env_hash"] for c in self.calls() if "config" in c["args"])
        self.assertNotEqual(first, second)
        self.assertEqual(len(first), 64)

    def test_wrapper_and_old_native_provider_rejected_before_start(self):
        for engine, changes in (("docker", {"TEST_DOCKER_WRAPPER": "1"}), ("podman", {"TEST_COMPOSE_VERSION": "1.3.0"}), ("podman", {"TEST_PODMAN_VERSION": "4.5.1"})):
            result = self.run_runtime("compose", "up", "-d", engine=engine, **changes)
            self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any("up" in c["args"] and "--help" not in c["args"] for c in self.calls()))

    def test_missing_native_tools(self):
        for name in ("bash", "python3", "dirname", "awk"):
            (self.bin / name).symlink_to(shutil.which(name))
        (self.bin / "podman-compose").unlink()
        result = self.run_runtime("compose", "up", "-d", engine="podman", PATH=str(self.bin))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires podman-compose", result.stderr)

    def test_readiness_for_both_engines_and_completed_init(self):
        for engine in ("docker", "podman"):
            result = self.run_runtime("compose", "up", "-d", "--wait", "--wait-timeout", "2", engine=engine)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("--wait" in c["args"] for c in self.calls()))

    def test_unhealthy_and_exited_services_fail(self):
        for state in ("unhealthy", "exited", "dead"):
            result = self.run_runtime("compose", "up", "--wait", "api", engine="podman", TEST_STATE=state)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("failed readiness", result.stderr)

    def test_missing_and_starting_services_time_out(self):
        self.compose = ROOT / "deployment/all-in-one/docker-compose.prod.yml"
        for changes in ({"TEST_MISSING": "1"}, {"TEST_STATE": "starting"}):
            started = time.monotonic()
            result = self.run_runtime("compose", "up", "--wait", "--wait-timeout", "1", "api", engine="podman", **changes)
            self.assertNotEqual(result.returncode, 0)
            self.assertRegex(result.stderr, r"within 1s|readiness timeout|startup timed out")
            self.assertLess(time.monotonic() - started, 5)

    def test_provider_dependency_wait_is_bounded(self):
        self.compose = ROOT / "deployment/all-in-one/docker-compose.prod.yml"
        result = self.run_runtime("compose", "up", "--wait", "--wait-timeout", "1", "api", engine="podman", TEST_DEPENDENCY_HANG="1")
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("startup timed out", result.stderr)

    def test_readiness_deadline_bounds_ps_and_healthy_inspect(self):
        self.compose = ROOT / "deployment/all-in-one/docker-compose.prod.yml"
        for engine in ("docker", "podman"):
            for delay in ("TEST_PS_DELAY", "TEST_INSPECT_DELAY"):
                started = time.monotonic()
                result = self.run_runtime("compose", "up", "--wait", "--wait-timeout", "1", "api",
                    engine=engine, **{delay: "3", "TEST_INSPECT_IGNORE_TERM": "1"})
                self.assertEqual(result.returncode, 124, result.stderr)
                self.assertLess(time.monotonic() - started, 2.8)

    def test_readiness_timeout_kills_descendant_after_parent_exits(self):
        self.compose = ROOT / "deployment/all-in-one/docker-compose.prod.yml"
        for engine in ("docker", "podman"):
            with self.subTest(engine=engine):
                self.calls_file.unlink(missing_ok=True)
                try:
                    started = time.monotonic()
                    result = self.run_runtime("compose", "up", "--wait", "--wait-timeout", "1", "api",
                        engine=engine, timeout=5, TEST_INSPECT_DELAY="30", TEST_INSPECT_CHILD_IGNORE_TERM="1")
                    self.assertEqual(result.returncode, 124, result.stderr)
                    self.assertLess(time.monotonic() - started, 2.8)
                    self.assertTrue(any(call["tool"] == "inspect-child" for call in self.calls()))
                finally:
                    # Clean up the isolated inspect group if the regression fails.
                    for call in self.calls():
                        if call["tool"] == engine and call["args"][:1] == ["inspect"]:
                            try:
                                os.killpg(call["pid"], signal.SIGKILL)
                            except ProcessLookupError:
                                pass

    def test_cancel_reaps_native_provider_command(self):
        env = {**self.env, "ASTRA_CONTAINER_ENGINE": "podman", "TEST_DEPENDENCY_HANG": "1"}
        process = subprocess.Popen([str(RUNTIME), "--env-file", str(self.env_file), "--file", str(self.compose),
            "compose", "up", "--wait", "--wait-timeout", "30", "api"], env=env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, start_new_session=True)
        child = None
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                for call in self.calls():
                    if call["tool"] == "podman-compose" and "up" in call["args"]:
                        child = call["pid"]
                if child:
                    break
                time.sleep(0.02)
            self.assertIsNotNone(child, "native provider did not start")
        finally:
            os.killpg(process.pid, signal.SIGTERM)
            process.communicate(timeout=7)
        self.assertNotEqual(process.returncode, 0)
        with self.assertRaises(ProcessLookupError):
            os.kill(child, 0)

    def test_scale_waits_for_all_replicas(self):
        result = self.run_runtime("compose", "up", "--wait", "--wait-timeout", "2", "--scale", "api=3", "api", engine="podman", TEST_REPLICAS="3")
        self.assertEqual(result.returncode, 0, result.stderr)
        inspected = [c["args"][-1] for c in self.calls() if c["args"][:1] == ["inspect"]]
        self.assertEqual(inspected, ["api0", "api1", "api2"])

    def test_invalid_scale_fails_before_start(self):
        result = self.run_runtime("compose", "up", "--wait", "--scale", "api=bogus", "api", engine="podman")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any("up" in c["args"] for c in self.calls()))

    def test_plan_reuse_requires_positive_evidence_without_leaking_secrets(self):
        for engine in ("docker", "podman"):
            for plan in ("reuse", "change", "empty", "error"):
                result = self.run_runtime("plan", "api", engine=engine, TEST_PLAN=plan)
                self.assertEqual(result.returncode == 0, plan == "reuse", result.stderr)
                self.assertNotIn("do-not-print-this-secret", result.stdout + result.stderr)

    def test_bad_inspection_does_not_expose_metadata(self):
        result = self.run_runtime("compose", "up", "--wait", "api", engine="podman", TEST_BAD_INSPECT="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("do-not-print-this-secret", result.stdout + result.stderr)

    def test_initialization_and_health_projection(self):
        container = {"State": {"Status": "exited", "ExitCode": 0}, "Config": {"Labels": {"io.astra.lifecycle": "init"}}}
        self.assertEqual(STATE.state(container), "completed")
        container["State"]["ExitCode"] = 1
        self.assertEqual(STATE.state(container), "exited")
        container = {"State": {"Running": True}, "Config": {"Healthcheck": {"Test": ["CMD", "true"]}}}
        self.assertEqual(STATE.state(container), "starting")
        for key in ("Health", "Healthcheck"):
            container["State"] = {"Running": True, key: {"Status": "healthy"}}
            self.assertEqual(STATE.state(container), "healthy")

    def test_status_filters_service_using_native_labels(self):
        result = self.run_runtime("compose", "ps", "-a", "-q", "api", engine="podman")
        self.assertEqual(result.returncode, 0, result.stderr)
        command = self.calls()[-1]
        self.assertEqual(command["tool"], "podman")
        self.assertIn("label=io.podman.compose.service=api", command["args"])
        self.assertIn("label=io.podman.compose.project=project", command["args"])

    def test_native_down_removes_all_selected_service_replicas(self):
        self.compose = ROOT / "deployment/all-in-one/docker-compose.prod.yml"
        result = self.run_runtime("compose", "down", "--timeout", "2", "api", engine="podman", TEST_REPLICAS="3")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        removed = [c["args"][-1] for c in calls if c["tool"] == "podman" and c["args"][:1] == ["rm"]]
        self.assertEqual(removed, ["api0", "api1", "api2"])
        stopped = [c for c in calls if c["tool"] == "podman" and c["args"][:1] == ["stop"]]
        self.assertTrue(all("--time" in c["args"] and "2" in c["args"] for c in stopped))
        self.assertFalse(any("volume" in c["args"] for c in calls))

    def test_invalid_down_selection_changes_no_services(self):
        result = self.run_runtime("compose", "down", "api", "unknown-service", engine="podman")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c["args"][:1] in (["stop"], ["rm"]) for c in self.calls()))

    def test_down_removes_native_dependents_before_dependencies(self):
        dependencies = {"memoria0": ["matrixone0"], "api0": ["matrixone0", "memoria0"]}
        result = self.run_runtime("compose", "down", engine="podman", TEST_DEPENDENCIES=json.dumps(dependencies))
        self.assertEqual(result.returncode, 0, result.stderr)
        removed = [c["args"][-1] for c in self.calls() if c["args"][:1] == ["rm"]]
        self.assertLess(removed.index("api0"), removed.index("memoria0"))
        self.assertLess(removed.index("memoria0"), removed.index("matrixone0"))

    def test_down_rejects_cyclic_native_metadata_before_stopping_services(self):
        dependencies = {"memoria0": ["api0"], "api0": ["memoria0"]}
        result = self.run_runtime("compose", "down", engine="podman", TEST_DEPENDENCIES=json.dumps(dependencies))
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c["args"][:1] in (["stop"], ["rm"]) for c in self.calls()))

    def test_stack_env_persists_choice_and_recursive_make_uses_it(self):
        path = self.work / "generated.env"
        env = {**self.env, "ASTRA_CONTAINER_ENGINE": "podman"}
        result = subprocess.run(["make", "--no-print-directory", "stack-env", f"STACK_ENV={path}"], cwd=ROOT, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ASTRA_CONTAINER_ENGINE=podman", path.read_text())
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        env.pop("ASTRA_CONTAINER_ENGINE")
        env["MEMORIA_EMBEDDING_PROVIDER"] = "mock"
        result = subprocess.run(["make", "--no-print-directory", "stack-status", f"STACK_ENV={path}"], cwd=ROOT, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(all(c["tool"] != "docker" for c in self.calls()))

    def test_ops_native_backup_and_ambiguous_discovery(self):
        backup = self.work / "backups"
        for replicas, expected in (("1", 0), ("2", 1)):
            env = {**self.env, "ASTRA_CONTAINER_ENGINE": "podman", "ASTRA_OPS_ENV_FILE": str(self.env_file),
                "MATRIXONE_PASSWORD": "test-password", "BACKUP_DIR": str(backup), "TEST_REPLICAS": replicas}
            # The engine stub uses the api count for generic ps discovery.
            result = subprocess.run([str(ROOT / "scripts/ops/backup.sh")], env=env, cwd=self.work, capture_output=True, text=True)
            self.assertEqual(result.returncode, expected, result.stderr)
        self.assertTrue(list(backup.glob("*.sql.gz")))
        self.assertTrue(any(c["tool"] == "podman" and c["args"][:1] == ["exec"] for c in self.calls()))

    def test_local_image_build_uses_native_engine_and_explicit_tag(self):
        for engine in ("docker", "podman"):
            self.calls_file.unlink(missing_ok=True)
            env = {**self.env, "ASTRA_CONTAINER_ENGINE": engine}
            result = subprocess.run(["make", "--no-print-directory", "dev-api-docker-build", "IMAGE_NAME=localhost/astra"],
                cwd=ROOT, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            build = next(c for c in self.calls() if c["args"][:1] == ["build"])
            self.assertEqual(build["tool"], engine)
            self.assertIn("localhost/astra:latest", build["args"])

    def test_head_build_selects_checkout_image_and_preserves_selection_on_failure(self):
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        for engine in ("docker", "podman"):
            self.calls_file.unlink(missing_ok=True)
            self.env_file.write_text(f"ASTRA_CONTAINER_ENGINE={engine}\nASTRA_IMAGE=example.test/astra:previous\n")
            command = ["make", "--no-print-directory", "stack-build-head", f"STACK_ENV={self.env_file}"]
            result = subprocess.run(command, cwd=ROOT, env=self.env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            build = next(c for c in self.calls() if c["args"][:1] == ["build"])
            image = build["args"][build["args"].index("-t") + 1]
            self.assertEqual(build["tool"], engine)
            self.assertRegex(image, rf"^localhost/astra:head-{revision}-[0-9a-f]{{12}}$")
            self.assertIn(f"IMAGE_REVISION={revision}", build["args"])
            self.assertEqual(self.env_file.read_text().count("ASTRA_IMAGE="), 1)
            self.assertIn(f"ASTRA_IMAGE={image}\n", self.env_file.read_text())
            self.assertEqual(self.env_file.stat().st_mode & 0o777, 0o600)
            self.assertFalse(any("up" in c["args"] for c in self.calls()))

            self.calls_file.unlink(missing_ok=True)
            result = subprocess.run(command, cwd=ROOT, env=self.env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            rebuilt = next(c for c in self.calls() if c["args"][:1] == ["build"])
            replacement = rebuilt["args"][rebuilt["args"].index("-t") + 1]
            self.assertNotEqual(image, replacement, "same-HEAD rebuilds must change the selected image")
            self.assertIn(f"ASTRA_IMAGE={replacement}\n", self.env_file.read_text())

            selected = self.env_file.read_bytes()
            start = ["make", "--no-print-directory", "stack-start-head", f"STACK_ENV={self.env_file}"]
            result = subprocess.run(start, cwd=ROOT, env={**self.env, "TEST_BUILD_EXIT": "1"}, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(self.env_file.read_bytes(), selected)
            self.assertFalse(any("up" in c["args"] for c in self.calls()))

    def test_restore_retains_confirmation_with_each_engine(self):
        backup = self.work / "smoke.sql"
        backup.write_text("CREATE TABLE smoke (id INT);\n")
        for engine in ("docker", "podman"):
            env = {**self.env, "ASTRA_CONTAINER_ENGINE": engine, "ASTRA_OPS_ENV_FILE": str(self.env_file), "MATRIXONE_PASSWORD": "test-password"}
            for answer in ("no\n", "yes\n"):
                self.calls_file.unlink(missing_ok=True)
                result = subprocess.run([str(ROOT / "scripts/ops/restore.sh"), str(backup)], env=env, cwd=self.work,
                    input=answer, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                executions = [c for c in self.calls() if c["args"][:1] == ["exec"]]
                self.assertEqual(bool(executions), answer == "yes\n")
                if executions:
                    self.assertEqual(executions[0]["tool"], engine)

    def test_ops_host_fallback_uses_selected_endpoint_and_hidden_password(self):
        for name in ("mysql", "mysqldump"):
            tool = self.bin / name
            tool.write_text(STUB)
            tool.chmod(0o755)
        with self.env_file.open("a") as file:
            file.write("MATRIXONE_PORT=26009\n")
        env = {**self.env, "ASTRA_CONTAINER_ENGINE": "podman", "ASTRA_OPS_ENV_FILE": str(self.env_file),
            "MATRIXONE_PASSWORD": "do-not-print-this-secret", "BACKUP_DIR": str(self.work / "backups"),
            "TEST_NATIVE_CLIENT_MISSING": "1"}
        result = subprocess.run([str(ROOT / "scripts/ops/backup.sh")], env=env, cwd=self.work, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        dump = next(c for c in self.calls() if c["tool"] == "mysqldump" and "--help" not in c["args"])
        self.assertIn("-P26009", dump["args"])
        self.assertIn("--protocol=TCP", dump["args"])
        self.assertTrue(dump["has_mysql_password"])
        self.assertNotIn("do-not-print-this-secret", json.dumps(self.calls()) + result.stdout + result.stderr)

    def test_ops_host_endpoint_does_not_require_an_accessible_engine(self):
        for name in ("mysql", "mysqldump", "curl"):
            tool = self.bin / name
            tool.write_text(STUB)
            tool.chmod(0o755)
        with self.env_file.open("a") as file:
            file.write("MATRIXONE_HOST=127.0.0.1\nMATRIXONE_PORT=26009\n")
        backup = self.work / "restore.sql"
        backup.write_text("CREATE TABLE smoke (id INT);\n")
        for engine in ("docker", "podman"):
            env = {**self.env, "ASTRA_CONTAINER_ENGINE": engine, "ASTRA_OPS_ENV_FILE": str(self.env_file),
                "MATRIXONE_PASSWORD": "do-not-print-this-secret", "BACKUP_DIR": str(self.work / "backups" / engine),
                "TEST_ENGINE_DOWN": "1"}
            for script,args in (("backup.sh",[]),("restore.sh",[str(backup)]),("health_check.sh",[])):
                self.calls_file.unlink(missing_ok=True)
                result = subprocess.run([str(ROOT / "scripts/ops" / script),*args],env=env,cwd=self.work,
                    input="yes\n",capture_output=True,text=True)
                self.assertEqual(result.returncode,0,result.stderr+result.stdout)
                clients=[c for c in self.calls() if c["tool"] in ("mysql","mysqldump") and "--help" not in c["args"]]
                self.assertTrue(clients)
                self.assertTrue(all("-P26009" in c["args"] and "--protocol=TCP" in c["args"] for c in clients))
                self.assertFalse(any(c["args"][:1]==["exec"] for c in self.calls()))
                self.assertNotIn("do-not-print-this-secret",json.dumps(self.calls())+result.stdout+result.stderr)
        invalid={**env,"ASTRA_CONTAINER_ENGINE":"docker","TEST_DOCKER_WRAPPER":"1"}
        result=subprocess.run([str(ROOT / "scripts/ops/backup.sh")],env=invalid,cwd=self.work,capture_output=True,text=True)
        self.assertEqual(result.returncode,2)
        self.assertIn("docker resolves to Podman",result.stderr)

    @unittest.skipUnless(shutil.which("podman-compose"),"requires the native Compose provider for interpolation coverage")
    def test_native_compose_rejects_missing_and_empty_required_image_pin(self):
        real=shutil.which("podman-compose")
        (self.bin / "podman-compose").unlink()
        (self.bin / "podman-compose").symlink_to(real)
        for value in ("","MATRIXONE_IMAGE=\n"):
            self.env_file.write_text("ASTRA_STACK_NAME=project\nASTRA_IMAGE=example.test/astra:test\n"+value)
            result=self.run_runtime("compose","config","--quiet",engine="podman")
            self.assertNotEqual(result.returncode,0)
            self.assertIn("MATRIXONE_IMAGE",result.stderr)

    @unittest.skipUnless(os.getenv("ASTRA_TEST_NATIVE_PODMAN") == "1", "opt-in native image/Compose regression")
    def test_same_head_rebuild_updates_the_running_native_image(self):
        base=os.environ["ASTRA_TEST_PODMAN_BASE_IMAGE"]
        project="astra-review-head-"+uuid.uuid4().hex[:10]
        env={**self.env,"PATH":os.environ["PATH"],"ASTRA_CONTAINER_ENGINE":"podman",
            "ASTRA_STACK_NAME":project,"MEMORIA_EMBEDDING_PROVIDER":"mock"}
        spec=self.work / "native.yml"
        spec.write_text('services:\n  api:\n    image: "${ASTRA_IMAGE:?select test image}"\n    entrypoint: ["/bin/sh", "-c", "exec sleep 3600"]\n')
        dockerfile=self.work / "Dockerfile"
        native=[str(RUNTIME),"--env-file",str(self.env_file),"--file",str(spec),"--project-name",project]
        selector=["--filter",f"label=io.podman.compose.project={project}","--filter","label=io.podman.compose.service=api"]
        revision=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
        images=[]
        try:
            for marker in ("first","second"):
                dockerfile.write_text(f"ARG BASE_IMAGE\nFROM ${{BASE_IMAGE}}\nUSER 0\nRUN printf '%s' '{marker}' > /astra-review-marker\n")
                args=f"--pull=never --file {dockerfile} --build-arg BASE_IMAGE={base} --label io.astra.review-test={project}"
                built=subprocess.run(["make","--no-print-directory","stack-build-head",f"STACK_ENV={self.env_file}",f"DOCKER_BUILD_ARGS={args}"],
                    cwd=ROOT,env=env,text=True,capture_output=True,timeout=90)
                self.assertEqual(built.returncode,0,built.stderr+built.stdout)
                started=subprocess.run([*native,"compose","up","-d","--wait","--wait-timeout","30","api"],env=env,capture_output=True,text=True,timeout=40)
                self.assertEqual(started.returncode,0,started.stderr+started.stdout)
                cid=subprocess.check_output(["podman","ps","-q",*selector],env=env,text=True).strip()
                self.assertTrue(cid and "\n" not in cid)
                images.append(subprocess.check_output(["podman","inspect","--format","{{.Image}}",cid],env=env,text=True).strip())
                self.assertEqual(subprocess.check_output(["podman","exec",cid,"cat","/astra-review-marker"],env=env,text=True),marker)
                self.assertEqual(subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip(),revision)
            self.assertNotEqual(images[0],images[1],"running container retained the earlier same-HEAD image")
        finally:
            ids=subprocess.check_output(["podman","ps","-aq",*selector],env=env,text=True).split()
            if ids:subprocess.run(["podman","rm","--force","--volumes",*ids],env=env,check=True,capture_output=True)
            subprocess.run([*native,"compose","down","--volumes"],env=env,capture_output=True,timeout=20)
            owned=subprocess.check_output(["podman","images","-q","--filter",f"label=io.astra.review-test={project}"],env=env,text=True).split()
            if owned:subprocess.run(["podman","rmi",*dict.fromkeys(owned)],env=env,check=True,capture_output=True)

    def test_production_deploy_through_each_native_backend(self):
        config = self.work / "production.env"
        values = dict(ASTRA_IMAGE="matrixorigin/astra:0.2.11", MATRIXONE_HOST="db.example.test", MATRIXONE_USER="astra",
            MATRIXONE_PASSWORD="test-password", ASTRA_DATABASE="astra_runtime", ASTRA_CORS_ORIGINS="https://app.example.test",
            ASTRA_JWT_SECRET="1"*64, ASTRA_TOKEN_ENCRYPTION_KEY="2"*64, ASTRA_RUNTIME_ROOT_SECRET="3"*64,
            MEMORIA_BASE_URL="https://memory.example.test", MEMORIA_MASTER_KEY="4"*64)
        config.write_text("\n".join(k+"="+v for k,v in values.items())+"\n")
        for engine in ("docker", "podman"):
            self.calls_file.unlink(missing_ok=True)
            env = {**self.env, "ASTRA_CONTAINER_ENGINE": engine, "ASTRA_PRODUCTION_ENV_FILE": str(config), "TEST_REPLICAS": "3"}
            result = subprocess.run([str(ROOT / "scripts/ops/deploy.sh"), "3"], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            starts = [c for c in self.calls() if "up" in c["args"] and "--help" not in c["args"]]
            self.assertEqual(len(starts), 1)
            self.assertEqual(starts[0]["tool"], "podman-compose" if engine == "podman" else "docker")
            self.assertIn("api=3", starts[0]["args"])
            self.assertTrue(any(c["args"][:1] == ["inspect"] for c in self.calls()))


if __name__ == "__main__":
    unittest.main()
