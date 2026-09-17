#!/usr/bin/env bash

# Small ownership and validation primitives for the local Terminal-Bench
# launcher. They deliberately avoid Astra, Harbor, Docker, and credentials so
# concurrency and input unhappy paths can be tested in isolation.

astra_harness_normalize_tcp_port() {
  local raw_port="${1-}"
  if ! [[ "$raw_port" =~ ^[0-9]{1,5}$ ]]; then
    return 1
  fi
  local normalized_port=$((10#$raw_port))
  if (( normalized_port < 1 || normalized_port > 65535 )); then
    return 1
  fi
  printf '%s\n' "$normalized_port"
}

astra_harness_is_owned_gateway_url() {
  local api_url="${1-}"
  local api_port="${2-}"
  [ "$api_url" = "http://172.17.0.1:${api_port}" ]
}

astra_harness_can_auto_bootstrap_auth() {
  local auto_bootstrap="${1-}"
  local server_started_by_harness="${2-}"
  local api_url_is_owned_gateway="${3-}"
  local server_process_is_alive="${4-}"
  [ "$auto_bootstrap" = "1" ] \
    && [ "$server_started_by_harness" = "1" ] \
    && [ "$api_url_is_owned_gateway" = "1" ] \
    && [ "$server_process_is_alive" = "1" ]
}

astra_harness_process_is_alive() {
  local pid="${1-}"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] && kill -0 "$pid" 2>/dev/null
}

astra_harness_terminate_and_reap() {
  local pid="${1-}"
  local grace_ticks="${2:-100}"
  local state=""
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 0
  [[ "$grace_ticks" =~ ^[1-9][0-9]*$ ]] || return 1
  if astra_harness_process_is_alive "$pid"; then
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 "$grace_ticks"); do
      if [ ! -r "/proc/${pid}/stat" ]; then
        break
      fi
      state="$(python3 - "$pid" <<'PY'
import pathlib
import sys

try:
    stat = (pathlib.Path("/proc") / sys.argv[1] / "stat").read_text(encoding="ascii")
except (FileNotFoundError, PermissionError, ProcessLookupError):
    raise SystemExit(0)
tail = stat.rsplit(")", 1)
print(tail[1].strip().split()[0] if len(tail) == 2 else "")
PY
)"
      [ "$state" = "Z" ] && break
      sleep 0.1
    done
  fi
  if astra_harness_process_is_alive "$pid" && [ "$state" != "Z" ]; then
    kill -KILL "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
}

astra_harness_process_owns_tcp_listener() {
  local pid="${1-}"
  local port="${2-}"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
  port="$(astra_harness_normalize_tcp_port "$port")" || return 1
  python3 - "$pid" "$port" <<'PY'
import os
import pathlib
import sys

pid, port = sys.argv[1], int(sys.argv[2])
fd_dir = pathlib.Path("/proc") / pid / "fd"
try:
    owned = {
        target[8:-1]
        for entry in fd_dir.iterdir()
        if (target := os.readlink(entry)).startswith("socket:[")
    }
except (FileNotFoundError, PermissionError, ProcessLookupError):
    raise SystemExit(1)

port_hex = f"{port:04X}"
for table in ("/proc/net/tcp", "/proc/net/tcp6"):
    try:
        lines = pathlib.Path(table).read_text(encoding="ascii").splitlines()[1:]
    except (FileNotFoundError, PermissionError):
        continue
    for line in lines:
        fields = line.split()
        if len(fields) >= 10 and fields[1].rsplit(":", 1)[-1] == port_hex:
            if fields[3] == "0A" and fields[9] in owned:
                raise SystemExit(0)
raise SystemExit(1)
PY
}

astra_harness_process_has_lifecycle_owner() {
  local pid="${1-}"
  local expected="${2-}"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] && [ -n "$expected" ] || return 1
  python3 - "$pid" "$expected" <<'PY'
import pathlib
import sys

pid, expected = sys.argv[1:]
try:
    entries = (pathlib.Path("/proc") / pid / "environ").read_bytes().split(b"\0")
except (FileNotFoundError, PermissionError, ProcessLookupError):
    raise SystemExit(1)
needle = ("ASTRA_SERVER_LIFECYCLE_OWNER=" + expected).encode()
raise SystemExit(0 if needle in entries else 1)
PY
}

astra_harness_process_tree_owns_tcp_listener() {
  local pid="${1-}"
  local port="${2-}"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
  port="$(astra_harness_normalize_tcp_port "$port")" || return 1
  python3 - "$pid" "$port" <<'PY'
import os
import pathlib
import sys

root, port = int(sys.argv[1]), int(sys.argv[2])

def children(pid):
    try:
        raw = pathlib.Path(f"/proc/{pid}/task/{pid}/children").read_text(encoding="ascii")
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return set()
    return {int(value) for value in raw.split() if value.isdigit()}

pids = {root}
pending = [root]
while pending:
    current = pending.pop()
    for child in children(current):
        if child not in pids:
            pids.add(child)
            pending.append(child)
owned = set()
for pid in pids:
    try:
        owned.update(
            target[8:-1]
            for entry in pathlib.Path(f"/proc/{pid}/fd").iterdir()
            if (target := os.readlink(entry)).startswith("socket:[")
        )
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        continue
port_hex = f"{port:04X}"
for table in ("/proc/net/tcp", "/proc/net/tcp6"):
    try:
        lines = pathlib.Path(table).read_text(encoding="ascii").splitlines()[1:]
    except (FileNotFoundError, PermissionError):
        continue
    for line in lines:
        fields = line.split()
        if (
            len(fields) >= 10
            and fields[1].rsplit(":", 1)[-1] == port_hex
            and fields[3] == "0A"
            and fields[9] in owned
        ):
            raise SystemExit(0)
raise SystemExit(1)
PY
}

astra_harness_process_tree_has_lifecycle_owner() {
  local pid="${1-}"
  local expected="${2-}"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] && [ -n "$expected" ] || return 1
  python3 - "$pid" "$expected" <<'PY'
import pathlib
import sys

root, expected = int(sys.argv[1]), sys.argv[2]

def children(pid):
    try:
        raw = pathlib.Path(f"/proc/{pid}/task/{pid}/children").read_text(encoding="ascii")
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return set()
    return {int(value) for value in raw.split() if value.isdigit()}

needle = ("ASTRA_SERVER_LIFECYCLE_OWNER=" + expected).encode()
pids = {root}
pending = [root]
while pending:
    current = pending.pop()
    try:
        if needle in pathlib.Path(f"/proc/{current}/environ").read_bytes().split(b"\0"):
            raise SystemExit(0)
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        pass
    for child in children(current):
        if child not in pids:
            pids.add(child)
            pending.append(child)
raise SystemExit(1)
PY
}
