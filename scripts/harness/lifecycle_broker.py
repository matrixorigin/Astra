#!/usr/bin/env python3
"""Kernel-lifetime ownership leases for local Terminal-Bench launches."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import select
import signal
import socket
import sys
import time


LEASE_SCHEMA = "astra.harness.lifecycle.v1"
IDENTITY_RE = re.compile(r"[0-9a-f]{64}")


class LifecycleLeaseError(RuntimeError):
    pass


class LifecycleLeaseBusy(LifecycleLeaseError):
    pass


def _canonical_gateway_identity(api_port: int) -> str:
    if not 1 <= api_port <= 65535:
        raise LifecycleLeaseError("gateway port must be between 1 and 65535")
    encoded = json.dumps(
        {
            "schema": f"{LEASE_SCHEMA}.gateway",
            "host": "0.0.0.0",
            "port": api_port,
        },
        sort_keys=True,
        separators=(",", ":"),
    ).encode()
    return hashlib.sha256(encoded).hexdigest()


def _scope_prefix(scope: str) -> str:
    if scope not in {"primary", "witness", "runtime"}:
        raise LifecycleLeaseError(
            "lifecycle lease scope must be primary, witness, or runtime"
        )
    return {"primary": "", "witness": ".w", "runtime": ".r"}[scope]


def database_address(database_identity: str, scope: str = "primary") -> str:
    if IDENTITY_RE.fullmatch(database_identity) is None:
        raise LifecycleLeaseError(
            "database identity must be 64 lowercase hexadecimal characters"
        )
    label = "database" if scope == "primary" else "db"
    return f"\0{LEASE_SCHEMA}{_scope_prefix(scope)}.{label}.{database_identity}"


def gateway_address(api_port: int, scope: str = "primary") -> str:
    return (
        f"\0{LEASE_SCHEMA}{_scope_prefix(scope)}.{'gateway' if scope == 'primary' else 'gw'}."
        f"{_canonical_gateway_identity(api_port)}"
    )


def _bind(address: str, label: str) -> socket.socket:
    kind = socket.SOCK_DGRAM
    if hasattr(socket, "SOCK_CLOEXEC"):
        kind |= socket.SOCK_CLOEXEC
    lease = socket.socket(socket.AF_UNIX, kind)
    os.set_inheritable(lease.fileno(), False)
    try:
        lease.bind(address)
    except OSError as error:
        lease.close()
        if error.errno in {98, 48}:
            raise LifecycleLeaseBusy(
                f"another launcher owns the canonical {label} lifecycle identity"
            ) from error
        raise LifecycleLeaseError(
            f"cannot bind the host-global abstract {label} lifecycle lease: {error}"
        ) from error
    return lease


class LifecycleLease:
    def __init__(
        self,
        database_socket: socket.socket,
        gateway_socket: socket.socket,
        database_address: str,
        gateway_address: str,
    ) -> None:
        self.database_socket = database_socket
        self.gateway_socket = gateway_socket
        self.database_address = database_address
        self.gateway_address = gateway_address

    @classmethod
    def acquire(
        cls, database_identity: str, api_port: int, scope: str = "primary"
    ) -> "LifecycleLease":
        db_address = database_address(database_identity, scope)
        port_address = gateway_address(api_port, scope)
        database_socket = _bind(db_address, "database")
        try:
            gateway_socket = _bind(port_address, "gateway")
        except BaseException:
            database_socket.close()
            raise
        return cls(database_socket, gateway_socket, db_address, port_address)

    @property
    def descriptors(self) -> tuple[int, int]:
        return self.database_socket.fileno(), self.gateway_socket.fileno()

    def close(self) -> None:
        self.gateway_socket.close()
        self.database_socket.close()

    def __enter__(self) -> "LifecycleLease":
        return self

    def __exit__(self, *_args: object) -> None:
        self.close()


def lifecycle_names_are_bound(
    database_identity: str, api_port: int, scope: str = "primary"
) -> bool:
    expected = {
        "@" + database_address(database_identity, scope)[1:],
        "@" + gateway_address(api_port, scope)[1:],
    }
    try:
        with open("/proc/net/unix", encoding="ascii") as stream:
            rows = stream.read().splitlines()
    except OSError:
        return False
    observed = {row.rsplit(" ", 1)[-1] for row in rows if " " in row}
    return expected <= observed


def _process_starttime(pid: int) -> str:
    try:
        raw = open(f"/proc/{pid}/stat", encoding="ascii").read()
    except OSError as error:
        raise LifecycleLeaseError(
            f"lifecycle owner PID {pid} is unavailable"
        ) from error
    tail = raw.rsplit(")", 1)
    if len(tail) != 2:
        raise LifecycleLeaseError(f"lifecycle owner PID {pid} has malformed proc state")
    fields = tail[1].strip().split()
    if len(fields) < 20:
        raise LifecycleLeaseError(
            f"lifecycle owner PID {pid} has incomplete proc state"
        )
    return fields[19]


def _wait_for_owner_exit(pid: int, starttime: str) -> None:
    if hasattr(os, "pidfd_open"):
        try:
            descriptor = os.pidfd_open(pid)
        except OSError:
            descriptor = -1
        if descriptor >= 0:
            try:
                poller = select.poll()
                poller.register(
                    descriptor, select.POLLIN | select.POLLHUP | select.POLLERR
                )
                while not poller.poll(1000):
                    if _process_starttime(pid) != starttime:
                        return
                return
            except LifecycleLeaseError:
                return
            finally:
                os.close(descriptor)
    while True:
        time.sleep(0.1)
        try:
            if _process_starttime(pid) != starttime:
                return
        except LifecycleLeaseError:
            return


def _hold(database_identity: str, api_port: int, owner_pid: int) -> int:
    owner_starttime = _process_starttime(owner_pid)
    lease = LifecycleLease.acquire(database_identity, api_port)
    stopping = False

    def stop(_signal: int, _frame: object) -> None:
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    print(
        json.dumps(
            {
                "ok": True,
                "schema": LEASE_SCHEMA,
                "database_address_sha256": hashlib.sha256(
                    lease.database_address.encode()
                ).hexdigest(),
                "gateway_address_sha256": hashlib.sha256(
                    lease.gateway_address.encode()
                ).hexdigest(),
                "owner_pid": owner_pid,
            },
            sort_keys=True,
        ),
        flush=True,
    )
    try:
        while not stopping:
            try:
                if _process_starttime(owner_pid) != owner_starttime:
                    break
            except LifecycleLeaseError:
                break
            time.sleep(0.1)
    finally:
        lease.close()
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    hold = subparsers.add_parser("hold")
    hold.add_argument("--database-identity", required=True)
    hold.add_argument("--gateway-port", required=True, type=int)
    hold.add_argument("--owner-pid", required=True, type=int)
    args = parser.parse_args()
    try:
        if args.owner_pid <= 1:
            raise LifecycleLeaseError("lifecycle owner PID must be greater than 1")
        return _hold(args.database_identity, args.gateway_port, args.owner_pid)
    except (LifecycleLeaseError, LifecycleLeaseBusy) as error:
        print(f"astra harness: lifecycle ownership failed: {error}", file=sys.stderr)
        return 78


if __name__ == "__main__":
    raise SystemExit(main())
