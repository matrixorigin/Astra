#!/usr/bin/env python3
"""Create and verify private immutable snapshots for a scored Harbor run."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
from pathlib import Path, PurePosixPath


LEDGER_SCHEMA = "astra.harness.sealed_run_snapshot.v1"
SHA40_RE = re.compile(r"[0-9a-f]{40}")
SHA64_RE = re.compile(r"[0-9a-f]{64}")


class SnapshotError(RuntimeError):
    pass


def _sha256_fd(descriptor: int) -> str:
    digest = hashlib.sha256()
    offset = 0
    while True:
        block = os.pread(descriptor, 1024 * 1024, offset)
        if not block:
            return digest.hexdigest()
        digest.update(block)
        offset += len(block)


def _sha256_path_at(root_fd: int, relative: str) -> str:
    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    descriptor = os.open(relative, flags, dir_fd=root_fd)
    try:
        return _sha256_fd(descriptor)
    finally:
        os.close(descriptor)


def _open_source(path: Path) -> tuple[int, os.stat_result]:
    source_status = path.lstat()
    if not stat.S_ISREG(source_status.st_mode) or path.is_symlink():
        raise SnapshotError(f"snapshot source must be a regular non-symlink: {path}")
    flags = os.O_RDONLY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    descriptor = os.open(path, flags)
    opened = os.fstat(descriptor)
    if (opened.st_dev, opened.st_ino) != (source_status.st_dev, source_status.st_ino):
        os.close(descriptor)
        raise SnapshotError(f"snapshot source changed while opening: {path}")
    return descriptor, opened


def _copy_regular(source: Path, destination: Path, mode: int) -> None:
    source_fd, initial = _open_source(source)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    try:
        target_fd = os.open(destination, flags, mode)
        try:
            while True:
                block = os.read(source_fd, 1024 * 1024)
                if not block:
                    break
                view = memoryview(block)
                while view:
                    written = os.write(target_fd, view)
                    view = view[written:]
            os.fsync(target_fd)
        finally:
            os.close(target_fd)
        final = os.fstat(source_fd)
        if (final.st_dev, final.st_ino, final.st_size, final.st_mtime_ns) != (
            initial.st_dev,
            initial.st_ino,
            initial.st_size,
            initial.st_mtime_ns,
        ):
            raise SnapshotError(f"snapshot source changed while copying: {source}")
        os.chmod(destination, mode)
    finally:
        os.close(source_fd)


def _safe_symlink_target(task_root: Path, link: Path) -> str:
    target = os.readlink(link)
    if os.path.isabs(target):
        raise SnapshotError(f"task symlink escape is forbidden: {link} -> {target}")
    resolved = (link.parent / target).resolve(strict=False)
    try:
        resolved.relative_to(task_root)
    except ValueError as error:
        raise SnapshotError(
            f"task symlink escape is forbidden: {link} -> {target}"
        ) from error
    return target


def _copy_task_tree(source: Path, destination: Path) -> None:
    source = source.resolve(strict=True)
    source_mode = stat.S_IMODE(source.stat().st_mode)
    os.mkdir(destination, source_mode)
    # mkdir is filtered through the process umask.  The sealed task identity
    # includes directory modes, so restore the exact official mode explicitly
    # just as _copy_regular does for files.
    os.chmod(destination, source_mode)
    for entry in sorted(
        source.rglob("*"), key=lambda path: path.relative_to(source).as_posix()
    ):
        relative = entry.relative_to(source)
        target = destination / relative
        status = entry.lstat()
        if stat.S_ISLNK(status.st_mode):
            os.symlink(_safe_symlink_target(source, entry), target)
        elif stat.S_ISDIR(status.st_mode):
            mode = stat.S_IMODE(status.st_mode)
            os.mkdir(target, mode)
            os.chmod(target, mode)
        elif stat.S_ISREG(status.st_mode):
            _copy_regular(entry, target, stat.S_IMODE(status.st_mode))
        else:
            raise SnapshotError(f"unsupported task entry type: {entry}")


def _copy_control_files(base: Path, paths: list[Path], destination: Path) -> list[str]:
    base = base.resolve(strict=True)
    copied: list[str] = []
    for source in paths:
        source = source.resolve(strict=True)
        try:
            relative = source.relative_to(base)
        except ValueError as error:
            raise SnapshotError(
                f"control-plane source escapes its repository: {source}"
            ) from error
        if source.is_symlink() or not source.is_file():
            raise SnapshotError(f"control-plane source is not a regular file: {source}")
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        _copy_regular(source, target, stat.S_IMODE(source.stat().st_mode))
        copied.append(relative.as_posix())
    if not copied or len(copied) != len(set(copied)):
        raise SnapshotError("control-plane manifest must be non-empty and unique")
    return sorted(copied)


def _task_set_sha256(paths: list[Path]) -> str:
    """Mirror preflight's byte-and-mode task identity after sealing."""
    combined = hashlib.sha256()
    # A sealed copy has a different parent from the official cache.  Its
    # identity must therefore use the stable, unique task-root name rather
    # than the location-dependent absolute path.
    for path in sorted(paths, key=lambda value: value.name):
        task_digest = hashlib.sha256()
        for entry in sorted(
            path.rglob("*"), key=lambda value: value.relative_to(path).as_posix()
        ):
            relative = entry.relative_to(path).as_posix()
            status = entry.lstat()
            if stat.S_ISLNK(status.st_mode):
                kind = b"symlink"
                content = os.readlink(entry).encode("utf-8", errors="surrogateescape")
            elif stat.S_ISREG(status.st_mode):
                kind = b"file"
                content = entry.read_bytes()
            elif stat.S_ISDIR(status.st_mode):
                kind = b"directory"
                content = b""
            else:
                raise SnapshotError(f"unsupported task tree entry: {entry}")
            task_digest.update(
                kind
                + b"\0"
                + relative.encode()
                + b"\0"
                + f"{stat.S_IMODE(status.st_mode):o}".encode()
                + b"\0"
                + content
            )
        digest = task_digest.hexdigest()
        combined.update(path.name.encode() + b"\0" + digest.encode() + b"\0")
    return combined.hexdigest()


def _read_json_regular(path: Path) -> dict:
    descriptor, initial = _open_source(path)
    try:
        raw = bytearray()
        while True:
            block = os.read(descriptor, 1024 * 1024)
            if not block:
                break
            raw.extend(block)
        final = os.fstat(descriptor)
        if (final.st_dev, final.st_ino, final.st_size, final.st_mtime_ns) != (
            initial.st_dev,
            initial.st_ino,
            initial.st_size,
            initial.st_mtime_ns,
        ):
            raise SnapshotError(f"snapshot source changed while reading: {path}")
        value = json.loads(raw.decode("utf-8"))
    finally:
        os.close(descriptor)
    if not isinstance(value, dict):
        raise SnapshotError("snapshot config must be a JSON object")
    return value


def _probe_build_info(binary: Path, source_revision: str, target: str | None) -> dict:
    try:
        completed = subprocess.run(
            [str(binary), "--build-info-json"],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
            env={"PATH": os.environ.get("PATH", "/usr/bin:/bin")},
        )
        value = json.loads(completed.stdout)
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        raise SnapshotError(
            f"cannot read embedded build info from {binary.name}: {error}"
        ) from error
    expected_keys = {"schema", "git_sha", "git_dirty", "target", "profile"}
    if completed.returncode != 0 or set(value) != expected_keys:
        raise SnapshotError(f"{binary.name} returned invalid embedded build info")
    if value.get("schema") != "astra.build_info.v1":
        raise SnapshotError(f"{binary.name} build-info schema is not canonical")
    if value.get("git_sha") != source_revision or value.get("git_dirty") is not False:
        raise SnapshotError(f"{binary.name} build-info source identity is not exact")
    if value.get("profile") != "debug":
        raise SnapshotError(f"{binary.name} build-info profile is not debug")
    actual_target = value.get("target")
    if not isinstance(actual_target, str) or not actual_target:
        raise SnapshotError(f"{binary.name} build-info target is missing")
    if target is not None and actual_target != target:
        raise SnapshotError(
            f"{binary.name} build-info target={actual_target!r}, expected {target!r}"
        )
    return value


def _entry_record(root: Path, path: Path) -> dict:
    status = path.lstat()
    relative = path.relative_to(root).as_posix()
    common = {
        "path": relative,
        "device": status.st_dev,
        "inode": status.st_ino,
        "mode": stat.S_IMODE(status.st_mode),
    }
    if stat.S_ISLNK(status.st_mode):
        return {**common, "kind": "symlink", "target": os.readlink(path)}
    if stat.S_ISDIR(status.st_mode):
        return {**common, "kind": "directory"}
    if stat.S_ISREG(status.st_mode):
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try:
            digest = _sha256_fd(descriptor)
        finally:
            os.close(descriptor)
        return {
            **common,
            "kind": "file",
            "size": status.st_size,
            "sha256": digest,
        }
    raise SnapshotError(f"unsupported snapshot entry: {path}")


def _remove_failed_snapshot(root: Path) -> None:
    if not root.exists():
        return
    for directory in [root, *(path for path in root.rglob("*") if path.is_dir())]:
        try:
            directory.chmod(0o700)
        except OSError:
            pass
    shutil.rmtree(root, ignore_errors=True)


class SealedRunSnapshot:
    def __init__(self, root: Path, root_fd: int, ledger_fd: int, ledger: dict):
        self.root = root
        self.root_fd = root_fd
        self.ledger_fd = ledger_fd
        self.ledger = ledger
        self._ledger_status = os.fstat(ledger_fd)
        self._ledger_sha256 = _sha256_fd(ledger_fd)

    @property
    def agent(self) -> Path:
        return self.root / "agent" / "astra"

    @property
    def server(self) -> Path:
        return self.root / "server" / "astra-server"

    @property
    def config(self) -> Path:
        return self.root / "config" / "final.json"

    @property
    def tasks(self) -> list[Path]:
        return [self.root / value for value in self.ledger["task_paths"]]

    @property
    def ledger_path(self) -> Path:
        return self.root / "ledger.json"

    def verify_open_ledger(self) -> None:
        ledger_status = os.fstat(self.ledger_fd)
        if (
            ledger_status.st_dev,
            ledger_status.st_ino,
            ledger_status.st_size,
            stat.S_IMODE(ledger_status.st_mode),
            _sha256_fd(self.ledger_fd),
        ) != (
            self._ledger_status.st_dev,
            self._ledger_status.st_ino,
            self._ledger_status.st_size,
            stat.S_IMODE(self._ledger_status.st_mode),
            self._ledger_sha256,
        ):
            raise SnapshotError("open snapshot ledger identity changed")
        root_status = os.fstat(self.root_fd)
        expected_root = self.ledger["root"]
        if (
            root_status.st_dev,
            root_status.st_ino,
            stat.S_IMODE(root_status.st_mode),
        ) != (
            expected_root["device"],
            expected_root["inode"],
            expected_root["mode"],
        ):
            raise SnapshotError("open snapshot root identity or mode changed")
        for entry in self.ledger["entries"]:
            relative = entry["path"]
            try:
                status = os.stat(relative, dir_fd=self.root_fd, follow_symlinks=False)
            except OSError as error:
                raise SnapshotError(
                    f"snapshot entry is unavailable: {relative}: {error}"
                ) from error
            if (status.st_dev, status.st_ino) != (entry["device"], entry["inode"]):
                raise SnapshotError(f"snapshot entry inode changed: {relative}")
            if stat.S_IMODE(status.st_mode) != entry["mode"]:
                raise SnapshotError(f"snapshot entry mode changed: {relative}")
            if entry["kind"] == "file":
                if (
                    status.st_size != entry["size"]
                    or _sha256_path_at(self.root_fd, relative) != entry["sha256"]
                ):
                    raise SnapshotError(f"snapshot entry content changed: {relative}")
            elif entry["kind"] == "symlink":
                if os.readlink(relative, dir_fd=self.root_fd) != entry["target"]:
                    raise SnapshotError(f"snapshot symlink changed: {relative}")

    def close(self) -> None:
        os.close(self.ledger_fd)
        os.close(self.root_fd)


def _validate_ledger_shape(ledger: object) -> dict:
    if not isinstance(ledger, dict) or set(ledger) != {
        "schema",
        "source_revision",
        "root",
        "task_paths",
        "task_set_sha256",
        "control_manifest",
        "build_info",
        "entries",
    }:
        raise SnapshotError("open snapshot ledger fields are not canonical")
    if (
        ledger.get("schema") != LEDGER_SCHEMA
        or SHA40_RE.fullmatch(str(ledger.get("source_revision"))) is None
    ):
        raise SnapshotError("open snapshot ledger identity is invalid")
    root = ledger.get("root")
    if (
        not isinstance(root, dict)
        or set(root) != {"device", "inode", "mode"}
        or not all(isinstance(root[key], int) and root[key] >= 0 for key in root)
        or root["mode"] != 0o500
    ):
        raise SnapshotError("open snapshot root identity is invalid")
    task_paths = ledger.get("task_paths")
    if not isinstance(task_paths, list) or len(task_paths) != len(set(task_paths)):
        raise SnapshotError("open snapshot task path inventory is invalid")
    for value in task_paths:
        path = PurePosixPath(value) if isinstance(value, str) else PurePosixPath("/")
        if path.is_absolute() or ".." in path.parts or path.parts[:1] != ("tasks",):
            raise SnapshotError("open snapshot task path escapes its root")
    if SHA64_RE.fullmatch(str(ledger.get("task_set_sha256"))) is None:
        raise SnapshotError("open snapshot task identity is invalid")
    control_manifest = ledger.get("control_manifest")
    if (
        not isinstance(control_manifest, list)
        or not control_manifest
        or control_manifest != sorted(set(control_manifest))
    ):
        raise SnapshotError("open snapshot control-plane manifest is invalid")
    for value in control_manifest:
        path = PurePosixPath(value) if isinstance(value, str) else PurePosixPath("/")
        if path.is_absolute() or ".." in path.parts or not path.parts:
            raise SnapshotError("open snapshot control-plane path escapes its root")
    if not isinstance(ledger.get("build_info"), dict):
        raise SnapshotError("open snapshot build info is invalid")
    entries = ledger.get("entries")
    if not isinstance(entries, list) or not entries:
        raise SnapshotError("open snapshot entry inventory is empty")
    observed: set[str] = set()
    for entry in entries:
        if not isinstance(entry, dict):
            raise SnapshotError("open snapshot entry is invalid")
        kind = entry.get("kind")
        common = {"path", "device", "inode", "mode", "kind"}
        expected = {
            "directory": common,
            "symlink": common | {"target"},
            "file": common | {"size", "sha256"},
        }.get(kind)
        path_value = entry.get("path")
        path = (
            PurePosixPath(path_value)
            if isinstance(path_value, str)
            else PurePosixPath("/")
        )
        if (
            expected is None
            or set(entry) != expected
            or path.is_absolute()
            or ".." in path.parts
            or not path.parts
            or path_value in observed
            or not all(
                isinstance(entry[key], int) and entry[key] >= 0
                for key in ("device", "inode", "mode")
            )
        ):
            raise SnapshotError("open snapshot entry inventory is not canonical")
        if kind == "file" and (
            not isinstance(entry["size"], int)
            or entry["size"] < 0
            or SHA64_RE.fullmatch(str(entry["sha256"])) is None
        ):
            raise SnapshotError("open snapshot file identity is invalid")
        if kind == "symlink" and not isinstance(entry["target"], str):
            raise SnapshotError("open snapshot symlink identity is invalid")
        observed.add(path_value)
    return ledger


def create_snapshot(
    *,
    parent: Path,
    snapshot_id: str,
    agent: Path,
    server: Path,
    config: Path,
    tasks: list[Path],
    source_revision: str,
    probe_build_info: bool = False,
    agent_target: str | None = None,
    server_target: str | None = None,
    consumer_root: Path | None = None,
    control_base: Path | None = None,
    control_paths: list[Path] | None = None,
) -> SealedRunSnapshot:
    if SHA40_RE.fullmatch(source_revision) is None:
        raise SnapshotError(
            "source revision must be 40 lowercase hexadecimal characters"
        )
    if not snapshot_id or PurePosixPath(snapshot_id).name != snapshot_id:
        raise SnapshotError("snapshot identity must be one safe path component")
    parent = parent.resolve(strict=True)
    root = parent / snapshot_id
    task_consumer_root = consumer_root if consumer_root is not None else root
    os.mkdir(root, 0o700)
    try:
        for relative in ("agent", "server", "config", "tasks", "control"):
            os.mkdir(root / relative, 0o700)
        os.mkdir(root / "control" / "repo", 0o700)
        _copy_regular(agent.resolve(strict=True), root / "agent" / "astra", 0o500)
        _copy_regular(
            server.resolve(strict=True), root / "server" / "astra-server", 0o500
        )
        copied_tasks: list[Path] = []
        task_paths: list[str] = []
        source_tasks = [task.resolve(strict=True) for task in tasks]
        source_task_set_before = _task_set_sha256(source_tasks)
        if len({task.name for task in source_tasks}) != len(source_tasks):
            raise SnapshotError("task root basenames must be unique")
        for task in source_tasks:
            relative = f"tasks/{task.name}"
            destination = root / relative
            _copy_task_tree(task, destination)
            copied_tasks.append(destination)
            task_paths.append(relative)
        source_config = _read_json_regular(config.resolve(strict=True))
        configured = source_config.get("tasks")
        if not isinstance(configured, list) or len(configured) != len(source_tasks):
            raise SnapshotError(
                "config tasks do not match the selected task snapshot set"
            )
        for index, task_entry in enumerate(configured):
            if not isinstance(task_entry, dict) or set(task_entry) != {"path"}:
                raise SnapshotError(f"config task {index} is not a closed local path")
            configured_path = (
                Path(str(task_entry["path"])).expanduser().resolve(strict=True)
            )
            if configured_path != source_tasks[index]:
                raise SnapshotError(f"config task {index} changed before snapshot")
            task_entry["path"] = str(task_consumer_root / task_paths[index])
        agents = source_config.get("agents")
        environment = (
            agents[0].get("env")
            if isinstance(agents, list)
            and len(agents) == 1
            and isinstance(agents[0], dict)
            else None
        )
        if _task_set_sha256(source_tasks) != source_task_set_before:
            raise SnapshotError("source task tree changed while snapshotting")
        if (
            isinstance(environment, dict)
            and "ASTRA_HARNESS_TASK_SET_SHA256" in environment
        ):
            if environment["ASTRA_HARNESS_TASK_SET_SHA256"] != source_task_set_before:
                raise SnapshotError(
                    "source task bytes do not match the configured task provenance"
                )
        if _task_set_sha256(copied_tasks) != source_task_set_before:
            raise SnapshotError(
                "sealed task identity differs from official source identity"
            )
        control_manifest = _copy_control_files(
            control_base if control_base is not None else Path.cwd(),
            control_paths or [],
            root / "control" / "repo",
        )
        serialized_config = (
            json.dumps(source_config, indent=2, sort_keys=True) + "\n"
        ).encode()
        config_path = root / "config" / "final.json"
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(config_path, flags, 0o400)
        try:
            os.write(descriptor, serialized_config)
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        build_info = {}
        if probe_build_info:
            build_info = {
                "agent": _probe_build_info(
                    root / "agent" / "astra", source_revision, agent_target
                ),
                "server": _probe_build_info(
                    root / "server" / "astra-server", source_revision, server_target
                ),
            }
        for directory in (
            root / "agent",
            root / "server",
            root / "config",
            root / "tasks",
            root / "control",
        ):
            os.chmod(directory, 0o500)
        entries = [
            _entry_record(root, path)
            for path in sorted(
                root.rglob("*"), key=lambda value: value.relative_to(root).as_posix()
            )
            if path.name != "ledger.json"
        ]
        root_status = root.stat()
        ledger = {
            "schema": LEDGER_SCHEMA,
            "source_revision": source_revision,
            "root": {
                "device": root_status.st_dev,
                "inode": root_status.st_ino,
                "mode": 0o500,
            },
            "task_paths": task_paths,
            "task_set_sha256": source_task_set_before,
            "control_manifest": control_manifest,
            "build_info": build_info,
            "entries": entries,
        }
        ledger_bytes = (json.dumps(ledger, indent=2, sort_keys=True) + "\n").encode()
        ledger_path = root / "ledger.json"
        ledger_writer_fd = os.open(
            ledger_path,
            os.O_RDWR
            | os.O_CREAT
            | os.O_EXCL
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_CLOEXEC", 0),
            0o400,
        )
        os.write(ledger_writer_fd, ledger_bytes)
        os.fsync(ledger_writer_fd)
        os.chmod(ledger_path, 0o400)
        ledger_fd = os.open(
            ledger_path,
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
        )
        if (
            os.fstat(ledger_fd).st_dev,
            os.fstat(ledger_fd).st_ino,
        ) != (
            os.fstat(ledger_writer_fd).st_dev,
            os.fstat(ledger_writer_fd).st_ino,
        ):
            os.close(ledger_fd)
            os.close(ledger_writer_fd)
            raise SnapshotError("snapshot ledger changed during descriptor handoff")
        os.close(ledger_writer_fd)
        root_fd = os.open(
            root, os.O_RDONLY | os.O_DIRECTORY | getattr(os, "O_CLOEXEC", 0)
        )
        os.chmod(root, 0o500)
        snapshot = SealedRunSnapshot(root, root_fd, ledger_fd, ledger)
        snapshot.verify_open_ledger()
        return snapshot
    except BaseException:
        _remove_failed_snapshot(root)
        raise


def open_snapshot(root_fd: int, ledger_fd: int) -> SealedRunSnapshot:
    try:
        raw = os.pread(ledger_fd, 16 * 1024 * 1024, 0)
        ledger = json.loads(raw.decode("utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SnapshotError(f"cannot read open snapshot ledger: {error}") from error
    ledger = _validate_ledger_shape(ledger)
    root = Path(f"/proc/self/fd/{root_fd}")
    return SealedRunSnapshot(root, root_fd, ledger_fd, ledger)


def _hold_read_leases(snapshot: SealedRunSnapshot) -> list[int]:
    if not hasattr(fcntl, "F_SETLEASE") or not hasattr(fcntl, "F_RDLCK"):
        raise SnapshotError("Linux read leases are required for immutable snapshot use")
    descriptors: list[int] = []
    try:
        file_paths = [
            entry["path"]
            for entry in snapshot.ledger["entries"]
            if entry["kind"] == "file"
        ]
        file_paths.append("ledger.json")
        for relative in file_paths:
            descriptor = os.open(
                relative,
                os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0),
                dir_fd=snapshot.root_fd,
            )
            os.set_inheritable(descriptor, True)
            try:
                fcntl.fcntl(descriptor, fcntl.F_SETLEASE, fcntl.F_RDLCK)
            except OSError as error:
                os.close(descriptor)
                raise SnapshotError(
                    f"cannot establish immutable read lease for {relative}: {error}"
                ) from error
            descriptors.append(descriptor)
        return descriptors
    except BaseException:
        for descriptor in descriptors:
            os.close(descriptor)
        raise


def _exec_snapshot(
    snapshot: SealedRunSnapshot,
    root_fd: int,
    ledger_fd: int,
    command: list[str],
) -> None:
    if not command:
        raise SnapshotError("snapshot exec command is required")
    snapshot.verify_open_ledger()
    leases = _hold_read_leases(snapshot)
    os.dup2(snapshot.root_fd, root_fd, inheritable=True)
    os.dup2(snapshot.ledger_fd, ledger_fd, inheritable=True)
    environment = dict(os.environ)
    environment.update(
        {
            "ASTRA_HARNESS_SNAPSHOT_ACTIVE": "1",
            "ASTRA_HARNESS_SNAPSHOT_ROOT_FD": str(root_fd),
            "ASTRA_HARNESS_SNAPSHOT_LEDGER_FD": str(ledger_fd),
            "ASTRA_HARNESS_CONTROL_REPO": f"/proc/self/fd/{root_fd}/control/repo",
        }
    )
    # Keep the lease descriptors referenced until exec; they deliberately
    # remain inherited by the launcher for its whole lifecycle.
    if not leases:
        raise SnapshotError("immutable snapshot lease inventory is empty")
    signal.signal(signal.SIGIO, signal.SIG_DFL)
    os.execvpe(command[0], command, environment)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="operation", required=True)
    create = subparsers.add_parser("create")
    create.add_argument("--parent", type=Path, required=True)
    create.add_argument("--snapshot-id", required=True)
    create.add_argument("--agent", type=Path, required=True)
    create.add_argument("--server", type=Path, required=True)
    create.add_argument("--config", type=Path, required=True)
    create.add_argument("--task", action="append", type=Path, required=True)
    create.add_argument("--source-revision", required=True)
    create.add_argument("--probe-build-info", action="store_true")
    create.add_argument("--agent-target")
    create.add_argument("--server-target")
    create.add_argument(
        "--consumer-root",
        type=Path,
        help="stable held-FD root consumed by the launcher and Harbor",
    )
    create.add_argument("--control-base", type=Path, required=True)
    create.add_argument("--control", action="append", type=Path, required=True)
    create_exec = subparsers.add_parser("create-exec")
    for action in create._actions[1:]:
        if action.dest in {"help"}:
            continue
        create_exec._add_action(action)
    create_exec.add_argument("--root-fd", type=int, default=198)
    create_exec.add_argument("--ledger-fd", type=int, default=197)
    create_exec.add_argument("exec_argv", nargs=argparse.REMAINDER)
    verify = subparsers.add_parser("verify")
    verify.add_argument("--root-fd", type=int, required=True)
    verify.add_argument("--ledger-fd", type=int, required=True)
    args = parser.parse_args()
    snapshot: SealedRunSnapshot | None = None
    try:
        if args.operation in {"create", "create-exec"}:
            snapshot = create_snapshot(
                parent=args.parent,
                snapshot_id=args.snapshot_id,
                agent=args.agent,
                server=args.server,
                config=args.config,
                tasks=args.task,
                source_revision=args.source_revision,
                probe_build_info=args.probe_build_info,
                agent_target=args.agent_target,
                server_target=args.server_target,
                consumer_root=args.consumer_root,
                control_base=args.control_base,
                control_paths=args.control,
            )
            if args.operation == "create-exec":
                command = (
                    args.exec_argv[1:]
                    if args.exec_argv[:1] == ["--"]
                    else args.exec_argv
                )
                _exec_snapshot(snapshot, args.root_fd, args.ledger_fd, command)
            print(
                json.dumps(
                    {
                        "ok": True,
                        "root": str(snapshot.root),
                        "ledger": str(snapshot.ledger_path),
                        "agent": str(snapshot.agent),
                        "server": str(snapshot.server),
                        "config": str(snapshot.config),
                        "tasks": [str(path) for path in snapshot.tasks],
                        "ledger_sha256": snapshot._ledger_sha256,
                    },
                    sort_keys=True,
                )
            )
        else:
            snapshot = open_snapshot(args.root_fd, args.ledger_fd)
            snapshot.verify_open_ledger()
            print(json.dumps({"ok": True, "schema": LEDGER_SCHEMA}, sort_keys=True))
        return 0
    except (
        OSError,
        ValueError,
        TypeError,
        SnapshotError,
        json.JSONDecodeError,
    ) as error:
        print(f"astra harness: sealed run snapshot failed: {error}", file=sys.stderr)
        return 78
    finally:
        if snapshot is not None:
            snapshot.close()


if __name__ == "__main__":
    raise SystemExit(main())
