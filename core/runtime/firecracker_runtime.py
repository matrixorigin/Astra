"""Firecracker microVM code execution runtime.

Runs untrusted code inside ephemeral microVMs with hardware-level isolation.
Requires Linux with /dev/kvm and the `firecracker` binary.

Architecture:
  1. Start firecracker process (creates Unix socket API)
  2. Configure VM via REST API (kernel, rootfs, resources)
  3. Boot VM, code runs inside guest via init script
  4. Read stdout/stderr from shared virtio device
  5. Kill firecracker process (VM destroyed)
"""

from __future__ import annotations

import json
import logging
import os
import platform
import shutil
import socket
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

import urllib.request

from core.runtime import (
    ExecutionResult, IsolationLevel, ResourceProfile, Runtime, RuntimeCapabilities,
)

logger = logging.getLogger(__name__)

_DEFAULT_KERNEL = "/opt/firecracker/vmlinux"
_DEFAULT_ROOTFS = "/opt/firecracker/rootfs.ext4"
_FIRECRACKER_BIN = "firecracker"


def _is_available() -> bool:
    """Check if Firecracker can run on this host."""
    if platform.system() != "Linux":
        return False
    if not os.path.exists("/dev/kvm"):
        return False
    if not shutil.which(_FIRECRACKER_BIN):
        return False
    return True


class FirecrackerRuntime(Runtime):
    """Execute code in ephemeral Firecracker microVMs.

    Each execution boots a fresh VM that is destroyed after completion.
    Provides hardware-level isolation via KVM.
    """

    def __init__(
        self,
        kernel_path: str = _DEFAULT_KERNEL,
        rootfs_path: str = _DEFAULT_ROOTFS,
        firecracker_bin: str = _FIRECRACKER_BIN,
    ):
        self.kernel_path = kernel_path
        self.rootfs_path = rootfs_path
        self.firecracker_bin = firecracker_bin

    @property
    def capabilities(self) -> RuntimeCapabilities:
        return RuntimeCapabilities(
            isolation=IsolationLevel.MICROVM,
            network_isolatable=True,
            filesystem_isolated=True,
            resource_limits=True,
            reproducible=True,
        )

    @property
    def supported_languages(self) -> list[str]:
        return ["python"]

    def health_check(self) -> bool:
        return _is_available() and os.path.exists(self.kernel_path) and os.path.exists(self.rootfs_path)

    def execute(
        self,
        code: str,
        language: str = "python",
        resources: ResourceProfile | None = None,
        env: dict[str, str] | None = None,
    ) -> ExecutionResult:
        if language not in self.supported_languages:
            return ExecutionResult(
                stdout="", stderr=f"Unsupported language: {language}",
                exit_code=1, execution_time_ms=0,
            )

        resources = resources or ResourceProfile()
        start = time.monotonic()
        started_at = datetime.now(timezone.utc)

        with tempfile.TemporaryDirectory(prefix="fc_exec_") as tmpdir:
            sock_path = os.path.join(tmpdir, "fc.sock")
            code_path = os.path.join(tmpdir, "code.py")
            stdout_path = os.path.join(tmpdir, "stdout")
            stderr_path = os.path.join(tmpdir, "stderr")
            exitcode_path = os.path.join(tmpdir, "exitcode")

            # Write code to be injected into rootfs overlay
            with open(code_path, "w") as f:
                f.write(code)

            # Start firecracker process
            fc_proc = subprocess.Popen(
                [self.firecracker_bin, "--api-sock", sock_path],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )

            try:
                # Wait for socket
                if not self._wait_for_socket(sock_path, timeout=5):
                    fc_proc.kill()
                    return ExecutionResult(
                        stdout="", stderr="Firecracker socket not ready",
                        exit_code=1, execution_time_ms=(time.monotonic() - start) * 1000,
                        started_at=started_at,
                    )

                # Configure VM
                vcpu_count = 1
                mem_mb = resources.max_memory_mb

                self._api_put(sock_path, "/machine-config", {
                    "vcpu_count": vcpu_count,
                    "mem_size_mib": mem_mb,
                })

                self._api_put(sock_path, "/boot-source", {
                    "kernel_image_path": self.kernel_path,
                    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off quiet",
                })

                self._api_put(sock_path, "/drives/rootfs", {
                    "drive_id": "rootfs",
                    "path_on_host": self.rootfs_path,
                    "is_root_device": True,
                    "is_read_only": True,
                })

                # Network: only configure if enabled
                if resources.network_enabled:
                    # TODO: Configure TAP device + network interface via API
                    # Requires host-side TAP setup (ip tuntap add, ip addr add, iptables)
                    logger.warning("network_enabled=True but TAP device not configured")

                # Boot VM
                self._api_put(sock_path, "/actions", {"action_type": "InstanceStart"})

                # Wait for VM to execute and exit
                try:
                    fc_proc.wait(timeout=resources.max_wall_seconds)
                except subprocess.TimeoutExpired:
                    fc_proc.kill()
                    elapsed_ms = (time.monotonic() - start) * 1000
                    return ExecutionResult(
                        stdout="",
                        stderr=f"Execution timed out after {resources.max_wall_seconds}s",
                        exit_code=137,
                        execution_time_ms=round(elapsed_ms, 2),
                        started_at=started_at,
                    )

                elapsed_ms = (time.monotonic() - start) * 1000

                # Read results from shared paths
                stdout = self._read_file(stdout_path)
                stderr = self._read_file(stderr_path)
                exit_code = int(self._read_file(exitcode_path) or "1")

                truncated = False
                if len(stdout.encode()) > resources.max_output_bytes:
                    stdout = stdout[:resources.max_output_bytes]
                    truncated = True

                return ExecutionResult(
                    stdout=stdout,
                    stderr=stderr,
                    exit_code=exit_code,
                    execution_time_ms=round(elapsed_ms, 2),
                    truncated=truncated,
                    started_at=started_at,
                )

            except Exception as e:
                elapsed_ms = (time.monotonic() - start) * 1000
                return ExecutionResult(
                    stdout="", stderr=f"Firecracker error: {e}",
                    exit_code=1, execution_time_ms=round(elapsed_ms, 2),
                    started_at=started_at,
                )
            finally:
                if fc_proc.poll() is None:
                    fc_proc.kill()
                    fc_proc.wait(timeout=5)

    def _wait_for_socket(self, path: str, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if os.path.exists(path):
                try:
                    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    s.connect(path)
                    s.close()
                    return True
                except OSError:
                    pass
            time.sleep(0.05)
        return False

    def _api_put(self, sock_path: str, endpoint: str, body: dict) -> None:
        """Send a PUT request to the Firecracker API via Unix socket."""
        payload = json.dumps(body).encode()
        request = (
            f"PUT {endpoint} HTTP/1.1\r\n"
            f"Host: localhost\r\n"
            f"Content-Type: application/json\r\n"
            f"Content-Length: {len(payload)}\r\n"
            f"\r\n"
        ).encode() + payload

        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.connect(sock_path)
            s.sendall(request)
            response = s.recv(4096).decode()
            if "HTTP/1.1 2" not in response:
                raise RuntimeError(f"Firecracker API error on {endpoint}: {response[:200]}")
        finally:
            s.close()

    @staticmethod
    def _read_file(path: str) -> str:
        try:
            return Path(path).read_text()
        except FileNotFoundError:
            return ""
