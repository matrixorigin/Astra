#!/usr/bin/env bash
set -euo pipefail

test_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/harness/local_gateway_contract.sh
source "${test_dir}/local_gateway_contract.sh"

assert_rejected_port() {
  local value="$1"
  if astra_harness_normalize_tcp_port "$value" >/dev/null; then
    echo "expected port to be rejected: ${value@Q}" >&2
    return 1
  fi
}

[ "$(astra_harness_normalize_tcp_port 00080)" = "80" ]
[ "$(astra_harness_normalize_tcp_port 65535)" = "65535" ]
assert_rejected_port ""
assert_rejected_port "0"
assert_rejected_port "65536"
assert_rejected_port "-1"
assert_rejected_port "17012/path"

astra_harness_is_owned_gateway_url "http://172.17.0.1:17012" "17012"
! astra_harness_is_owned_gateway_url "http://127.0.0.1:17012" "17012"
! astra_harness_is_owned_gateway_url "http://172.17.0.1:17012/path" "17012"
! astra_harness_is_owned_gateway_url "https://172.17.0.1:17012" "17012"

astra_harness_can_auto_bootstrap_auth 1 1 1 1
! astra_harness_can_auto_bootstrap_auth 0 1 1 1
! astra_harness_can_auto_bootstrap_auth 1 0 1 1
! astra_harness_can_auto_bootstrap_auth 1 1 0 1
! astra_harness_can_auto_bootstrap_auth 1 1 1 0

astra_harness_process_is_alive "$$"
! astra_harness_process_is_alive ""
! astra_harness_process_is_alive "0"
! astra_harness_process_is_alive "not-a-pid"
! astra_harness_process_is_alive "999999999"

# Cleanup is bounded even when a child ignores TERM; KILL and wait must leave
# no zombie or live process behind.
bash -c 'trap "" TERM; while :; do sleep 60; done' &
stubborn_pid=$!
astra_harness_terminate_and_reap "$stubborn_pid" 2
! astra_harness_process_is_alive "$stubborn_pid"

# Listener authority is stronger than PID liveness and is bound to an explicit
# lifecycle owner marker. A live but unrelated process must not authorize
# token bootstrap or shared-server reuse.
listener_port_file="$(mktemp)"
ASTRA_SERVER_LIFECYCLE_OWNER=external python3 - "$listener_port_file" <<'PY' &
import pathlib
import socket
import sys
import time

sock = socket.socket()
sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
sock.bind(("127.0.0.1", 0))
sock.listen()
pathlib.Path(sys.argv[1]).write_text(str(sock.getsockname()[1]), encoding="ascii")
while True:
    time.sleep(60)
PY
listener_pid=$!
for _ in $(seq 1 100); do
  [ -s "$listener_port_file" ] && break
  sleep 0.01
done
listener_port="$(<"$listener_port_file")"
astra_harness_process_owns_tcp_listener "$listener_pid" "$listener_port"
! astra_harness_process_owns_tcp_listener "$$" "$listener_port"
astra_harness_process_has_lifecycle_owner "$listener_pid" external
! astra_harness_process_has_lifecycle_owner "$listener_pid" harness-test
astra_harness_process_tree_owns_tcp_listener "$listener_pid" "$listener_port"
astra_harness_process_tree_has_lifecycle_owner "$listener_pid" external
kill -TERM "$listener_pid"
wait "$listener_pid" 2>/dev/null || true
! astra_harness_process_owns_tcp_listener "$listener_pid" "$listener_port"
rm -f "$listener_port_file"

# Reuse authorization is intentionally absent from the helper. A process not
# started by this harness may never gain registration authority from a reuse
# flag or address equality alone.
reused_server_started_by_harness=0
! astra_harness_can_auto_bootstrap_auth 1 "$reused_server_started_by_harness" 1 1
