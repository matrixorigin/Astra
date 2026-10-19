#!/usr/bin/env python3
"""Run the opt-in, real Web + TUI Work surface journey.

This is deliberately separate from ``astra-test`` and the offline test suite.
It drives the shipped TUI through a real controlling terminal, starts a local
Web dev server, and lets Playwright observe the same owner-scoped Work.  The
API server is never restarted by this script: callers must point it at an
isolated candidate server whose build identity matches this checkout.

The JSON files in ``--run-dir`` are a coordination protocol only.  The final
verdict is based on authenticated API observations and the real browser/TUI
processes.  A successful exit is impossible unless the same root Run was
observed running by Web, the TUI then exited, and that Run later produced a
new event count and terminal result.
"""

from __future__ import annotations

import argparse
import codecs
import errno
import fcntl
import json
import os
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


SCHEMA_VERSION = 1
WORK_API_MAJOR = "1"
INTERACTION_API_MAJOR = "3"
DEFAULT_WEB_PORT = 3537
DEFAULT_TIMEOUT_SECONDS = 240.0
PROCESS_SUPERVISOR = Path(__file__).with_name("process_supervisor.py")
TERMINAL_RUN_STATUSES = {
    "completed",
    "failed",
    "cancelled",
    "interrupted",
    "delegated",
    "error",
}
SUCCESS_RUN_STATUSES = {"completed"}
CONTROL_COMMANDS = {
    "web_observed",
    "web_working_observed",
    "web_settled_observed",
}


class HarnessError(RuntimeError):
    """A truthful setup, product, or evidence failure."""


class NotTestedError(HarnessError):
    """The journey could not reach a meaningful cross-surface verdict."""


def ensure_source_checkout_is_clean(root: Path) -> None:
    """Refuse to mix a candidate client with a different local source tree.

    The live lane starts an API that may already be running.  A commit SHA on
    ``HEAD`` only identifies the committed tree, so a staged or unstaged
    tracked edit could otherwise compile into the TUI/Web client while the
    API still serves the clean commit.  Ignored build outputs are intentionally
    excluded; they are not source identity and are produced by this lane.
    """

    try:
        result = subprocess.run(
            ["git", "status", "--porcelain=v1", "--untracked-files=no"],
            cwd=root,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise NotTestedError(f"could not inspect candidate source checkout: {error}") from error
    if result.returncode != 0:
        detail = result.stderr.strip() or f"git status exited with {result.returncode}"
        raise NotTestedError(f"could not inspect candidate source checkout: {detail}")
    if result.stdout.strip():
        changed = len(result.stdout.splitlines())
        raise NotTestedError(
            "candidate source checkout is dirty; commit or stash tracked changes before the live "
            f"journey ({changed} tracked path{'s' if changed != 1 else ''} changed)"
        )


def utc_seconds() -> float:
    return time.time()


def atomic_write_json(path: Path, value: dict[str, Any]) -> None:
    """Publish one complete coordination record with owner-only permissions."""

    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    encoded = json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2) + "\n"
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    descriptor = os.open(temporary, flags, 0o600)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            descriptor = -1
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if descriptor != -1:
            os.close(descriptor)
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def read_json(path: Path) -> dict[str, Any] | None:
    try:
        with path.open(encoding="utf-8") as stream:
            value = json.load(stream)
    except (FileNotFoundError, json.JSONDecodeError, OSError):
        return None
    return value if isinstance(value, dict) else None


def scrub_token_from_evidence(
    run_dir: Path, token: str, *, exclude: Path | None = None
) -> None:
    """Remove an access token if a child accidentally wrote it to evidence."""

    marker = token.encode("utf-8")
    if not marker:
        return
    replacement = b"[REDACTED_ACCESS_TOKEN]"
    excluded = exclude.resolve() if exclude is not None else None
    try:
        paths = list(run_dir.rglob("*"))
    except OSError:
        return
    for path in paths:
        if not path.is_file():
            continue
        if excluded is not None and path.resolve() == excluded:
            continue
        try:
            data = path.read_bytes()
            if marker not in data:
                continue
            mode = path.stat().st_mode
            path.write_bytes(data.replace(marker, replacement))
            os.chmod(path, mode)
        except OSError:
            # Evidence redaction is best-effort; no child receives a token on
            # argv and the normal state/log writers never include it.
            continue


def open_private_log(path: Path):
    """Open an evidence log with owner-only permissions or fail clearly."""

    try:
        handle = path.open("a", encoding="utf-8")
        try:
            os.fchmod(handle.fileno(), 0o600)
        except OSError:
            handle.close()
            raise
        return handle
    except OSError as error:
        raise HarnessError(f"could not open private evidence log {path}: {error}") from error


def wait_until(
    predicate: Any,
    deadline: float,
    description: str,
    interval: float = 0.1,
    fatal_errors: tuple[type[Exception], ...] = (),
) -> Any:
    last_error: Exception | None = None
    while utc_seconds() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except Exception as error:  # noqa: BLE001 - report the bounded retry
            if fatal_errors and isinstance(error, fatal_errors):
                raise
            last_error = error
        time.sleep(interval)
    detail = f": {last_error}" if last_error else ""
    raise HarnessError(f"timed out waiting for {description}{detail}")


def safe_id(value: str, label: str) -> str:
    if not value or not re.fullmatch(r"[A-Za-z0-9._:-]{1,128}", value):
        raise HarnessError(f"{label} is not a canonical identity")
    return value


def open_url(target: str | urllib.request.Request, timeout: float):
    """Open a URL while keeping loopback traffic off ambient HTTP proxies."""

    url = target.full_url if isinstance(target, urllib.request.Request) else target
    hostname = urllib.parse.urlsplit(url).hostname
    if hostname in {"localhost", "127.0.0.1", "::1"}:
        return urllib.request.build_opener(urllib.request.ProxyHandler({})).open(
            target, timeout=timeout
        )
    return urllib.request.urlopen(target, timeout=timeout)


class Api:
    """Small authenticated reader used for evidence and preflight."""

    def __init__(self, base_url: str, token: str) -> None:
        self.base_url = base_url.rstrip("/")
        self.token = token

    def request(
        self,
        method: str,
        path: str,
        *,
        body: dict[str, Any] | None = None,
        work_contract: bool = False,
        timeout: float = 10.0,
    ) -> tuple[int, Any]:
        url = f"{self.base_url}{path}"
        headers = {
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/json",
        }
        if work_contract:
            headers["x-astra-work-api-major"] = WORK_API_MAJOR
        encoded = None
        if body is not None:
            encoded = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=encoded, method=method, headers=headers)
        try:
            with open_url(request, timeout) as response:
                raw = response.read()
                status = response.status
        except urllib.error.HTTPError as error:
            raw = error.read()
            status = error.code
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise HarnessError(f"API request {method} {path} failed: {error}") from error
        try:
            value = json.loads(raw.decode("utf-8")) if raw else None
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise HarnessError(f"API returned invalid JSON for {method} {path}") from error
        return status, value

    def get(self, path: str, *, work_contract: bool = False) -> Any:
        status, value = self.request("GET", path, work_contract=work_contract)
        if status < 200 or status >= 300:
            detail = value.get("code", "unknown") if isinstance(value, dict) else "unknown"
            raise HarnessError(f"API GET {path} returned HTTP {status} ({detail})")
        return value


class Screen:
    """Minimal VT100 screen used to inspect the current TUI viewport.

    The parser intentionally implements only the cursor/erase controls Astra
    uses for its interactive frame.  It keeps a viewport rather than searching
    cumulative ANSI output, so text that has scrolled away cannot satisfy a
    later assertion.
    """

    def __init__(self, rows: int = 30, columns: int = 100) -> None:
        self.rows = rows
        self.columns = columns
        self.cells = [[" "] * columns for _ in range(rows)]
        self.row = 0
        self.column = 0
        self._state = "normal"
        self._csi = ""
        self._decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def _newline(self) -> None:
        self.row += 1
        self.column = 0
        if self.row >= self.rows:
            self.cells.pop(0)
            self.cells.append([" "] * self.columns)
            self.row = self.rows - 1

    def _put(self, character: str) -> None:
        if self.column >= self.columns:
            self._newline()
        self.cells[self.row][self.column] = character
        self.column += 1

    def _erase_line(self, mode: int) -> None:
        if mode == 2:
            self.cells[self.row] = [" "] * self.columns
        elif mode == 1:
            self.cells[self.row][: self.column + 1] = [" "] * (self.column + 1)
        else:
            self.cells[self.row][self.column :] = [" "] * (self.columns - self.column)

    def _erase_display(self, mode: int) -> None:
        if mode == 2 or mode == 3:
            self.cells = [[" "] * self.columns for _ in range(self.rows)]
            self.row = 0
            self.column = 0
        elif mode == 1:
            for index in range(self.row):
                self.cells[index] = [" "] * self.columns
            self.cells[self.row][: self.column + 1] = [" "] * (self.column + 1)
        else:
            self._erase_line(0)
            for index in range(self.row + 1, self.rows):
                self.cells[index] = [" "] * self.columns

    def _finish_csi(self, final: str) -> None:
        raw = self._csi
        private = raw.startswith("?")
        if private:
            raw = raw[1:]
        values = []
        for item in raw.split(";") if raw else []:
            try:
                values.append(int(item or "0"))
            except ValueError:
                values.append(0)
        first = values[0] if values else 0
        if final in ("H", "f"):
            self.row = max(0, min(self.rows - 1, (values[0] if values else 1) - 1))
            self.column = max(0, min(self.columns - 1, (values[1] if len(values) > 1 else 1) - 1))
        elif final == "A":
            self.row = max(0, self.row - (first or 1))
        elif final == "B":
            self.row = min(self.rows - 1, self.row + (first or 1))
        elif final == "C":
            self.column = min(self.columns - 1, self.column + (first or 1))
        elif final == "D":
            self.column = max(0, self.column - (first or 1))
        elif final == "G":
            self.column = max(0, min(self.columns - 1, (first or 1) - 1))
        elif final == "d":
            self.row = max(0, min(self.rows - 1, (first or 1) - 1))
        elif final == "J":
            self._erase_display(first)
        elif final == "K":
            self._erase_line(first)
        # SGR, mode switches, scroll regions, and device attributes do not
        # change the textual viewport and are intentionally ignored.
        self._state = "normal"
        self._csi = ""

    def feed(self, data: bytes) -> None:
        for character in self._decoder.decode(data, final=False):
            byte = ord(character)
            if self._state == "escape":
                if character == "[":
                    self._state = "csi"
                    self._csi = ""
                else:
                    self._state = "normal"
                continue
            if self._state == "csi":
                if "@" <= character <= "~":
                    self._finish_csi(character)
                else:
                    self._csi += character
                continue
            if byte == 0x1B:
                self._state = "escape"
            elif byte in (0x0A, 0x0B, 0x0C):
                self._newline()
            elif byte == 0x0D:
                self.column = 0
            elif byte == 0x08:
                self.column = max(0, self.column - 1)
            elif byte >= 0x20 and byte != 0x7F:
                self._put(character)

    def text(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self.cells)


class PtyTui:
    """The Astra binary attached to a real controlling terminal."""

    def __init__(
        self,
        *,
        binary: Path,
        api_url: str,
        model: str,
        token: str,
        profile: str,
        home: Path,
        workspace: Path,
        log: Path,
    ) -> None:
        self.binary = binary
        self.api_url = api_url
        self.model = model
        self.token = token
        self.profile = profile
        self.home = home
        self.workspace = workspace
        self.log_path = log
        self.master_fd: int | None = None
        self.pid: int | None = None
        self.screen = Screen()
        self.raw = bytearray()
        self._log = None

    def start(self) -> None:
        import pty

        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        try:
            self._log = self.log_path.open("wb")
        except OSError as error:
            raise HarnessError(f"could not open private TUI evidence log: {error}") from error
        try:
            os.fchmod(self._log.fileno(), 0o600)
        except OSError as error:
            self._log.close()
            self._log = None
            raise HarnessError(f"TUI evidence log is not private: {error}") from error
        pid, master = pty.fork()
        if pid == 0:
            # Match the viewport consumed by Screen.  Without an explicit
            # size, forkpty inherits a (0, 0) window in headless runners and
            # the real TUI may render an empty or differently wrapped frame.
            fcntl.ioctl(
                0,
                termios.TIOCSWINSZ,
                struct.pack("HHHH", self.screen.rows, self.screen.columns, 0, 0),
            )
            os.chdir(self.workspace)
            environment = os.environ.copy()
            environment.update(
                {
                    "HOME": str(self.home),
                    "XDG_CONFIG_HOME": str(self.home / ".config"),
                    "XDG_CACHE_HOME": str(self.home / ".cache"),
                    "XDG_DATA_HOME": str(self.home / ".local/share"),
                    "ASTRA_LOCAL_STATE_ROOT": str(self.home / ".astra"),
                    "ASTRA_ACCESS_TOKEN": self.token,
                    "ASTRA_API_URL": self.api_url,
                    "TERM": "xterm-256color",
                }
            )
            if self._log is not None:
                os.set_inheritable(self._log.fileno(), False)
            for name in ("TMUX", "ZELLIJ_SESSION_NAME"):
                environment.pop(name, None)
            argv = [
                str(self.binary),
                "--api-url",
                self.api_url,
                "--profile",
                self.profile,
                "--model",
                self.model,
                "--no-instructions",
                "interactive",
            ]
            os.execvpe(argv[0], argv, environment)
        self.pid = pid
        self.master_fd = master
        flags = fcntl.fcntl(master, fcntl.F_GETFL)
        fcntl.fcntl(master, fcntl.F_SETFL, flags | os.O_NONBLOCK)

    def _write(self, data: bytes) -> None:
        if self.master_fd is None:
            raise HarnessError("TUI terminal is not running")
        try:
            os.write(self.master_fd, data)
        except OSError as error:
            raise HarnessError(f"TUI input failed: {error}") from error

    def send(self, text: str) -> None:
        # Match the actual paste path used by the PTY integration tests; this
        # avoids a bulk command's newline being mistaken for a submit gesture.
        self._write(b"\x1b[200~" + text.encode("utf-8") + b"\x1b[201~\r")

    def _answer_queries(self) -> None:
        if self.master_fd is None:
            return
        while self.raw.count(b"\x1b[6n") > getattr(self, "_cpr_replies", 0):
            self._write(b"\x1b[1;1R")
            self._cpr_replies = getattr(self, "_cpr_replies", 0) + 1
        while self.raw.count(b"\x1b[c") > getattr(self, "_da1_replies", 0):
            self._write(b"\x1b[?1;2c")
            self._da1_replies = getattr(self, "_da1_replies", 0) + 1

    def receive(self, timeout: float = 0.1) -> None:
        if self.master_fd is None:
            return
        ready, _, _ = select.select([self.master_fd], [], [], timeout)
        if not ready:
            self._check_alive()
            return
        try:
            data = os.read(self.master_fd, 64 * 1024)
        except OSError as error:
            if error.errno in (errno.EIO, errno.EBADF):
                self._check_alive()
                return
            raise
        if not data:
            self._check_alive()
            return
        self.raw.extend(data)
        self.screen.feed(data)
        if self._log is not None:
            self._log.write(data)
            self._log.flush()
        self._answer_queries()

    def _check_alive(self) -> None:
        if self.pid is None or hasattr(self, "_exit_status"):
            return
        try:
            waited, status = os.waitpid(self.pid, os.WNOHANG)
        except ChildProcessError:
            # Another owner reaped an already-dead child.  Preserve an
            # explicit terminal marker so future probes remain idempotent.
            self._exit_status = 0
            return
        if waited:
            self._exit_status = status

    def is_alive(self) -> bool:
        self._check_alive()
        return self.pid is not None and not hasattr(self, "_exit_status")

    def wait_for_text(self, text: str, deadline: float) -> None:
        def find() -> bool:
            if not self.is_alive():
                return False
            self.receive(0.05)
            return text in self.screen.text()

        wait_until(
            find,
            deadline,
            f"TUI screen text {text!r}",
            interval=0.05,
        )

    def wait_for_regex(self, pattern: str, deadline: float) -> re.Match[str]:
        compiled = re.compile(pattern)

        def find() -> re.Match[str] | None:
            if not self.is_alive():
                return None
            self.receive(0.05)
            return compiled.search(self.screen.text())

        return wait_until(find, deadline, f"TUI screen pattern {pattern!r}", interval=0.05)

    def stop(self, deadline: float) -> int:
        if self.pid is None:
            return 0
        if self.is_alive():
            try:
                os.kill(self.pid, signal.SIGHUP)
            except ProcessLookupError:
                self._check_alive()
        while self.is_alive() and utc_seconds() < deadline:
            self.receive(0.05)
        if self.is_alive():
            try:
                os.killpg(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, status = os.waitpid(self.pid, 0)
            self._exit_status = status
        status = getattr(self, "_exit_status", 0)
        if self._log is not None:
            self._log.close()
            self._log = None
        if self.master_fd is not None:
            try:
                os.close(self.master_fd)
            except OSError:
                pass
            self.master_fd = None
        return os.waitstatus_to_exitcode(status) if status >= 0 else status

    def close(self) -> None:
        if self.pid is not None and self.is_alive():
            self.stop(utc_seconds() + 10)
        elif self._log is not None:
            self._log.close()
            self._log = None
        if self.master_fd is not None:
            try:
                os.close(self.master_fd)
            except OSError:
                pass
            self.master_fd = None


def seed_workspace(home: Path, workspace: Path) -> None:
    workspace.mkdir(parents=True, exist_ok=True)
    home.mkdir(parents=True, exist_ok=True)
    (home / ".config").mkdir(parents=True, exist_ok=True)
    (home / ".cache").mkdir(parents=True, exist_ok=True)
    (home / ".local/share").mkdir(parents=True, exist_ok=True)
    git = subprocess.run(
        ["git", "init", "--quiet", str(workspace)], check=False, capture_output=True
    )
    if git.returncode != 0:
        raise HarnessError("could not create the disposable Work fixture repository")
    trusted = workspace.resolve().as_posix()
    ledger = {
        "version": 1,
        "workspaces": {trusted: {"trust": "trusted", "trusted_at": "2026-09-17T00:00:00Z"}},
    }
    astra_home = home / ".astra"
    astra_home.mkdir(parents=True, exist_ok=True)
    atomic_write_json(astra_home / "trusted_workspaces.json", ledger)


def branch_from_overview(overview: dict[str, Any], work_id: str) -> str:
    try:
        value = overview["overview"]["delivery_branch"]["branch_id"]
    except (KeyError, TypeError) as error:
        raise HarnessError("Work overview did not contain its delivery branch") from error
    branch = safe_id(str(value), "Work branch")
    if overview.get("overview", {}).get("work_id") != work_id:
        raise HarnessError("Work overview identity changed during the live journey")
    return branch


def run_status(api: Api, run_id: str) -> dict[str, Any]:
    value = api.get(f"/chat/runs/{urllib.parse.quote(run_id, safe='')}")
    if not isinstance(value, dict) or value.get("run_id") != run_id:
        raise HarnessError("run status identity disagrees with the admitted root Run")
    return value


def write_phase(
    path: Path,
    *,
    phase: str,
    work_id: str | None = None,
    branch_id: str | None = None,
    run_id: str | None = None,
    **facts: Any,
) -> None:
    current = read_json(path) or {"schema_version": SCHEMA_VERSION}
    phase_record: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "phase": phase,
        "updated_at": utc_seconds(),
    }
    for key, value in (("work_id", work_id), ("branch_id", branch_id), ("run_id", run_id)):
        if value is not None:
            phase_record[key] = value
    phase_record.update(facts)
    milestones = current.get("milestones")
    if not isinstance(milestones, dict):
        milestones = {}
    # Keep immutable milestone records so a fast provider cannot overwrite a
    # phase before the browser observes it.  The top-level projection remains
    # the latest state for simple readers.
    milestones.setdefault(phase, phase_record)
    current.update(phase_record)
    current["milestones"] = milestones
    atomic_write_json(path, current)


def wait_control(
    path: Path,
    command: str,
    deadline: float,
    *,
    after_control_id: str | None = None,
    is_alive: Any | None = None,
    pump: Any | None = None,
) -> dict[str, Any]:
    if command not in CONTROL_COMMANDS:
        raise HarnessError(f"unsupported live harness control command {command}")
    seen_id = after_control_id

    def find() -> dict[str, Any] | None:
        nonlocal seen_id
        if pump is not None:
            pump()
        if is_alive is not None and not is_alive():
            raise HarnessError("live browser exited before writing its control handshake")
        value = read_json(path)
        if not value or value.get("schema_version") != SCHEMA_VERSION:
            return None
        if value.get("command") != command:
            return None
        control_id = value.get("control_id")
        if not isinstance(control_id, str) or not control_id or control_id == seen_id:
            return None
        seen_id = control_id
        return value

    return wait_until(
        find,
        deadline,
        f"browser control {command!r}",
        interval=0.1,
        fatal_errors=(HarnessError,),
    )


def terminate_process(process: subprocess.Popen[str] | None, timeout: float = 10.0) -> None:
    """Stop a process-supervisor leader and its owned process tree.

    Web and Playwright are launched through ``process_supervisor.py``. The
    supervisor owns the actual command in a separate process group and reaps
    descendants even when that command's leader exits first. Returning for a
    completed supervisor is therefore intentional: it has already completed
    child cleanup, and blindly signalling a reused PID would be unsafe.
    """
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
        return
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            # Cleanup is best-effort after the bounded kill.  The retained log
            # and non-zero harness result still make the leak visible.
            pass


def start_supervised_process(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    stdin: Any,
    stdout: Any,
    stderr: Any,
    identity: str,
) -> subprocess.Popen[str]:
    """Launch a command behind the harness's ownership/reaping supervisor."""

    if not PROCESS_SUPERVISOR.is_file():
        raise HarnessError(f"process supervisor is missing: {PROCESS_SUPERVISOR}")
    supervisor_command = [
        sys.executable,
        str(PROCESS_SUPERVISOR),
        "run",
        "--owner-pid",
        str(os.getpid()),
        "--identity",
        identity,
        "--",
        *command,
    ]
    try:
        return subprocess.Popen(
            supervisor_command,
            cwd=cwd,
            env=env,
            stdin=stdin,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
            text=True,
        )
    except OSError as error:
        raise HarnessError(f"could not start supervised process {identity}: {error}") from error


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Opt-in real TUI + Web Work surface journey (never part of test-offline)."
    )
    parser.add_argument("--api-url", default=os.environ.get("ASTRA_API_URL", "http://127.0.0.1:17001"))
    parser.add_argument("--web-port", type=int, default=int(os.environ.get("ASTRA_WORK_LIVE_WEB_PORT", DEFAULT_WEB_PORT)))
    parser.add_argument("--web-url", default=None, help="Use an already-running Web only with --reuse-web")
    parser.add_argument("--reuse-web", action="store_true", help="Observe an already-running Web at --web-url")
    parser.add_argument("--astra-bin", type=Path, default=None)
    parser.add_argument("--model", default=os.environ.get("ASTRA_WORK_LIVE_MODEL", "deepseek-v4-flash"))
    parser.add_argument("--profile", default=os.environ.get("ASTRA_WORK_LIVE_PROFILE", "harness-auto"))
    parser.add_argument(
        "--token-file",
        type=Path,
        default=None,
        help="Read the access token from an owner-only file; prefer ASTRA_HARNESS_ACCESS_TOKEN",
    )
    parser.add_argument("--goal", default="Verify one durable Work across TUI and Web surfaces.")
    parser.add_argument("--message", default="Inspect the current Work, explain the next useful step, and report the result.")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--run-dir", type=Path, default=None)
    parser.add_argument("--keep-artifacts", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if sys.platform != "linux":
        print(json.dumps({"status": "not_tested", "error": "Work live journey requires Linux process supervision"}), file=sys.stderr)
        return 2
    root = Path(__file__).resolve().parents[2]
    binary = (args.astra_bin or root / "target/debug/astra").resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise HarnessError(f"Astra binary is missing or not executable: {binary}; build it first")
    token = os.environ.get("ASTRA_HARNESS_ACCESS_TOKEN")
    if not token and args.token_file is not None:
        try:
            token = args.token_file.read_text(encoding="utf-8").strip()
        except OSError as error:
            raise HarnessError(f"could not read --token-file: {error}") from error
    if not token or not token.strip():
        raise HarnessError("set ASTRA_HARNESS_ACCESS_TOKEN (a disposable owner token); the harness never reads or prints credentials")
    if args.web_port < 1024 or args.web_port > 65535:
        raise HarnessError("--web-port must be between 1024 and 65535")

    ensure_source_checkout_is_clean(root)
    expected_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    run_dir_owned = args.run_dir is None
    run_dir = (args.run_dir or Path(tempfile.mkdtemp(prefix="astra-work-live-"))).resolve()
    run_dir.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(run_dir, 0o700)
    except OSError as error:
        raise HarnessError(f"live harness run directory is not private: {run_dir}: {error}") from error
    state_path = run_dir / "state.json"
    control_path = run_dir / "control.json"
    # A caller-supplied directory is still owned by this invocation.  Remove
    # prior coordination records so an interrupted run cannot satisfy a new
    # phase with a stale browser command or Work identity.
    for coordination_path in (state_path, control_path):
        try:
            coordination_path.unlink()
        except FileNotFoundError:
            pass
    api = Api(args.api_url, token.strip())
    tui: PtyTui | None = None
    web: subprocess.Popen[str] | None = None
    playwright: subprocess.Popen[str] | None = None
    web_log = run_dir / "web.log"
    tui_log = run_dir / "tui.pty.log"
    workspace = run_dir / "workspace"
    home = run_dir / "home"
    web_next_dir = f".next-work-live-{os.getpid()}-{run_dir.name}"
    started_at = utc_seconds()
    deadline = started_at + args.timeout
    write_phase(state_path, phase="preflight", expected_server_sha=expected_sha, run_dir=str(run_dir))

    try:
        status, health = api.request("GET", "/health", timeout=10)
        if status != 200 or not isinstance(health, dict):
            raise NotTestedError(f"candidate API health is unavailable (HTTP {status})")
        if health.get("interaction_api_major") != INTERACTION_API_MAJOR:
            raise NotTestedError("candidate API interaction protocol is incompatible with this client")
        actual_sha = health.get("build_git_sha")
        if actual_sha != expected_sha or health.get("build_git_dirty") is not False:
            raise NotTestedError(
                "API build does not match this checkout; start the candidate Server from the same source "
                f"(expected {expected_sha}, observed {actual_sha})"
            )
        auth_status, _ = api.request("GET", "/auth/me")
        if auth_status != 200:
            raise NotTestedError(
                f"disposable access token was rejected by the candidate Server (HTTP {auth_status})"
            )
        catalog_status, catalog = api.request("GET", "/v1/works?limit=1", work_contract=True)
        if catalog_status != 200:
            raise NotTestedError(
                f"candidate Server did not expose the Work catalog (HTTP {catalog_status})"
            )
        if not isinstance(catalog, dict) or catalog.get("schema_version") != 1:
            raise NotTestedError("candidate Server did not return the Work catalog contract")
        write_phase(state_path, phase="preflight_ready", server_sha=actual_sha, api_url=args.api_url)

        seed_workspace(home, workspace)
        tui = PtyTui(
            binary=binary,
            api_url=args.api_url,
            model=args.model,
            token=token.strip(),
            profile=args.profile,
            home=home,
            workspace=workspace,
            log=tui_log,
        )
        tui.start()
        tui.wait_for_text("Message Astra", deadline)
        tui.send(f"/work start {args.goal}")
        match = tui.wait_for_regex(r"Work started · ([A-Za-z0-9._:-]+)", deadline)
        work_id = safe_id(match.group(1), "Work")
        overview = api.get(f"/v1/works/{urllib.parse.quote(work_id, safe='')}", work_contract=True)
        branch_id = branch_from_overview(overview, work_id)
        write_phase(
            state_path,
            phase="started",
            work_id=work_id,
            branch_id=branch_id,
            goal=args.goal,
            tui_pid=tui.pid,
            evidence={"tui_log": str(tui_log), "screen": tui.screen.text()},
        )

        # Web must discover this Work before TUI starts its root Run.
        if args.reuse_web:
            if not args.web_url:
                raise HarnessError("--reuse-web requires --web-url")
        else:
            web_url = f"http://127.0.0.1:{args.web_port}"
            environment = os.environ.copy()
            # Do not let a developer's ambient frontend token or demo switch
            # change the live journey.  The browser receives its disposable
            # token through its own test-process environment and cookie.
            environment.pop("ASTRA_ACCESS_TOKEN", None)
            environment.pop("ASTRA_HARNESS_ACCESS_TOKEN", None)
            environment.update(
                {
                    "ASTRA_API_URL": args.api_url,
                    "ASTRA_WEB_HOST": "127.0.0.1",
                    "ASTRA_WEB_PORT": str(args.web_port),
                    "ASTRA_WEB_DEMO": "false",
                    "ASTRA_NEXT_DIST_DIR": web_next_dir,
                }
            )
            web_log_handle = open_private_log(web_log)
            try:
                web = start_supervised_process(
                    ["npm", "run", "dev"],
                    cwd=root / "web",
                    env=environment,
                    stdin=subprocess.DEVNULL,
                    stdout=web_log_handle,
                    stderr=subprocess.STDOUT,
                    identity=f"{run_dir.name}:web",
                )
            finally:
                # Keep the file descriptor owned by the child process lifetime.
                web_log_handle.close()
            def web_ready() -> bool:
                if web is not None and web.poll() is not None:
                    raise HarnessError(
                        f"Web dev server exited before readiness (status {web.returncode})"
                    )
                try:
                    with open_url(f"{web_url}/login", timeout=2) as response:
                        return response.status < 500
                except (urllib.error.URLError, TimeoutError, OSError):
                    return False

            wait_until(
                web_ready,
                deadline,
                "Web dev server readiness",
                interval=0.2,
                fatal_errors=(HarnessError,),
            )
            args.web_url = web_url
        write_phase(state_path, phase="web_ready", work_id=work_id, branch_id=branch_id, web_url=args.web_url)

        web_test_env = os.environ.copy()
        web_test_env.update(
            {
                "ASTRA_API_URL": args.api_url,
                "ASTRA_WORK_LIVE_WEB_URL": args.web_url or "",
                "ASTRA_WORK_LIVE_STATE": str(state_path),
                "ASTRA_WORK_LIVE_CONTROL": str(control_path),
                "ASTRA_WORK_LIVE_OUTPUT_DIR": str(run_dir / "playwright-artifacts"),
                # The token is inherited only by the browser test process and
                # is never passed as a command-line argument or written to
                # the coordination state.
                "ASTRA_WORK_LIVE_ACCESS_TOKEN": token.strip(),
            }
        )
        playwright_log = run_dir / "playwright.log"
        playwright_handle = open_private_log(playwright_log)
        try:
            playwright = start_supervised_process(
                [
                    "npm",
                    "exec",
                    "--",
                    "playwright",
                    "test",
                    "--config",
                    "playwright.work-live.config.ts",
                    "--project=chromium",
                ],
                cwd=root / "web",
                env=web_test_env,
                stdin=subprocess.DEVNULL,
                stdout=playwright_handle,
                stderr=subprocess.STDOUT,
                identity=f"{run_dir.name}:playwright",
            )
        finally:
            playwright_handle.close()

        # Playwright owns the browser and writes only commands. It cannot claim
        # a passing run from the state file; each command is acknowledged here.
        wait_control(
            control_path,
            "web_observed",
            deadline,
            is_alive=lambda: playwright is not None and playwright.poll() is None,
            pump=lambda: tui.receive(0.01),
        )
        write_phase(state_path, phase="web_observed", work_id=work_id, branch_id=branch_id)
        tui.send(f"/work continue {work_id} {args.message}")
        run_match = tui.wait_for_regex(r"Work accepted · run ([A-Za-z0-9._:-]+)", deadline)
        run_id = safe_id(run_match.group(1), "root Run")
        before_exit = run_status(api, run_id)
        if before_exit.get("status") != "running":
            raise NotTestedError(
                "the provider settled the root Run before the Web handoff window; "
                f"status={before_exit.get('status')!r}. This attempt is not a cross-surface pass."
            )
        pre_exit_events = before_exit.get("events_count")
        if not isinstance(pre_exit_events, int) or pre_exit_events < 1:
            raise HarnessError("admitted root Run did not expose a valid event count")
        write_phase(
            state_path,
            phase="run_admitted",
            work_id=work_id,
            branch_id=branch_id,
            run_id=run_id,
            pre_exit_run={"status": before_exit.get("status"), "events_count": pre_exit_events},
        )

        wait_control(
            control_path,
            "web_working_observed",
            deadline,
            is_alive=lambda: playwright is not None and playwright.poll() is None,
            pump=lambda: tui.receive(0.01),
        )
        write_phase(state_path, phase="web_working_observed", work_id=work_id, branch_id=branch_id, run_id=run_id)
        # The observed root Run is now the authority. Close only the TUI; the
        # Server's admitted run must outlive the client process.
        exit_code = tui.stop(min(deadline, utc_seconds() + 20))
        at_tui_exit = run_status(api, run_id)
        if at_tui_exit.get("status") in TERMINAL_RUN_STATUSES:
            raise NotTestedError(
                "the root Run settled before the TUI exited; this attempt cannot prove post-exit continuation"
            )
        tui_exit_events = at_tui_exit.get("events_count")
        if not isinstance(tui_exit_events, int) or tui_exit_events < pre_exit_events:
            raise HarnessError("root Run event count regressed at TUI exit")
        write_phase(
            state_path,
            phase="tui_exited",
            work_id=work_id,
            branch_id=branch_id,
            run_id=run_id,
            tui_exit_code=exit_code,
            pre_exit_events_count=pre_exit_events,
            events_at_tui_exit=tui_exit_events,
        )
        after_exit: dict[str, Any] | None = None

        def settled_after_exit() -> dict[str, Any] | None:
            nonlocal after_exit
            current = run_status(api, run_id)
            if (
                current.get("status") in TERMINAL_RUN_STATUSES
                and isinstance(current.get("events_count"), int)
                and current["events_count"] > tui_exit_events
            ):
                after_exit = current
                return current
            return None

        wait_until(settled_after_exit, deadline, "the same root Run to settle after TUI exit", interval=0.2)
        write_phase(
            state_path,
            phase="run_settled",
            work_id=work_id,
            branch_id=branch_id,
            run_id=run_id,
            post_exit_run=after_exit,
            proof={
                "same_run_id": True,
                "events_increased_after_tui_exit": after_exit["events_count"] > tui_exit_events,
                "terminal_after_tui_exit": after_exit["status"] in TERMINAL_RUN_STATUSES,
            },
        )
        if after_exit["status"] not in SUCCESS_RUN_STATUSES:
            raise HarnessError(
                "the same root Run settled after TUI exit without a successful result: "
                f"status={after_exit['status']!r}"
            )
        wait_control(
            control_path,
            "web_settled_observed",
            deadline,
            is_alive=lambda: playwright is not None and playwright.poll() is None,
            pump=lambda: tui.receive(0.01),
        )
        write_phase(state_path, phase="passed", work_id=work_id, branch_id=branch_id, run_id=run_id)
        if playwright is not None:
            try:
                playwright.wait(timeout=max(1.0, deadline - utc_seconds()))
            except subprocess.TimeoutExpired as error:
                raise HarnessError(
                    "live Web Playwright did not exit after its final observation; see the retained playwright.log evidence"
                ) from error
            if playwright.returncode != 0:
                raise HarnessError(
                    "live Web Playwright failed; see the retained playwright.log evidence"
                )
        print(json.dumps({"status": "passed", "run_dir": str(run_dir), "work_id": work_id, "run_id": run_id}, ensure_ascii=False))
        return 0
    except NotTestedError as error:
        write_phase(state_path, phase="not_tested", error=str(error))
        print(json.dumps({"status": "not_tested", "run_dir": str(run_dir), "error": str(error)}, ensure_ascii=False))
        return 2
    except HarnessError as error:
        write_phase(state_path, phase="failed", error=str(error))
        print(json.dumps({"status": "failed", "run_dir": str(run_dir), "error": str(error)}, ensure_ascii=False))
        return 1
    finally:
        try:
            if tui is not None:
                tui.close()
        except Exception as error:  # noqa: BLE001 - cleanup must continue independently
            print(f"[astra-work-live] TUI cleanup failed: {error}", file=sys.stderr)
        try:
            terminate_process(playwright)
        except Exception as error:  # noqa: BLE001 - cleanup must continue independently
            print(f"[astra-work-live] Playwright cleanup failed: {error}", file=sys.stderr)
        try:
            terminate_process(web)
        except Exception as error:  # noqa: BLE001 - cleanup must continue independently
            print(f"[astra-work-live] Web cleanup failed: {error}", file=sys.stderr)
        if web is not None:
            # The dev server's Next build directory is process-scoped and
            # must not collide with a developer's normal `.next-dev` cache.
            import shutil

            shutil.rmtree(root / "web" / web_next_dir, ignore_errors=True)
        scrub_token_from_evidence(run_dir, token.strip(), exclude=args.token_file)
        # A caller-supplied --run-dir is evidence by definition. Temporary
        # directories are retained on failure; successful cleanup is optional
        # and only removes coordination/process logs, never the server Work.
        if run_dir_owned and args.keep_artifacts:
            print(f"[astra-work-live] evidence retained at {run_dir}", file=sys.stderr)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except HarnessError as error:
        print(json.dumps({"status": "unavailable", "error": str(error)}), file=sys.stderr)
        raise SystemExit(2)
