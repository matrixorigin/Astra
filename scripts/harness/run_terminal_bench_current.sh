#!/usr/bin/env bash
set -euo pipefail

# One supported local Terminal-Bench entrypoint.  The harness owns the
# ephemeral Astra server for the duration of a run; MatrixOne and Memoria are
# external dependencies and are never started or stopped here.
entry_repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
repo_root="${ASTRA_HARNESS_RUNTIME_REPO:-$entry_repo_root}"
control_repo_root="${ASTRA_HARNESS_CONTROL_REPO:-$repo_root}"
export ASTRA_HARNESS_RUNTIME_REPO="$repo_root"
cd "$repo_root"
# Install the local bypass before any Python/urllib or curl client is created.
# The verifier snapshot uses the same canonical list; ambient internal routes
# are never forwarded into scored task containers.
harness_no_proxy="localhost,127.0.0.1,::1,172.17.0.1"
export NO_PROXY="$harness_no_proxy"
export no_proxy="$harness_no_proxy"
# Harbor resolves the deliberately literal proxy placeholders in the scored
# config before the adapter is constructed.  Project the launcher environment
# once, so a normal shell that supplies lowercase proxy variables cannot pass
# preflight and then fail only after all trial environments are prepared.
network_mode="${ASTRA_HARNESS_NETWORK_MODE:-proxy}"
case "$network_mode" in
  proxy)
    export ASTRA_HARBOR_HTTP_PROXY="${ASTRA_HARBOR_HTTP_PROXY:-${HTTP_PROXY:-${http_proxy:-}}}"
    export ASTRA_HARBOR_HTTPS_PROXY="${ASTRA_HARBOR_HTTPS_PROXY:-${HTTPS_PROXY:-${https_proxy:-}}}"
    if [ -z "$ASTRA_HARBOR_HTTP_PROXY" ] || [ -z "$ASTRA_HARBOR_HTTPS_PROXY" ]; then
      echo "astra harness: proxy mode requires both HTTP and HTTPS proxy endpoints" >&2
      exit 78
    fi
    export HTTP_PROXY="$ASTRA_HARBOR_HTTP_PROXY"
    export http_proxy="$ASTRA_HARBOR_HTTP_PROXY"
    export HTTPS_PROXY="$ASTRA_HARBOR_HTTPS_PROXY"
    export https_proxy="$ASTRA_HARBOR_HTTPS_PROXY"
    export ASTRA_HARBOR_ALL_PROXY="" ALL_PROXY="" all_proxy=""
    ;;
  direct)
    if [ -n "${ASTRA_HARBOR_HTTP_PROXY:-}" ] \
      || [ -n "${ASTRA_HARBOR_HTTPS_PROXY:-}" ] \
      || [ -n "${ASTRA_HARBOR_ALL_PROXY:-}" ] \
      || [ -n "${HTTP_PROXY:-}" ] || [ -n "${HTTPS_PROXY:-}" ] \
      || [ -n "${http_proxy:-}" ] || [ -n "${https_proxy:-}" ] \
      || [ -n "${ALL_PROXY:-}" ] || [ -n "${all_proxy:-}" ]; then
      echo "astra harness: direct mode requires all proxy endpoints to be empty" >&2
      exit 78
    fi
    export ASTRA_HARBOR_HTTP_PROXY=""
    export ASTRA_HARBOR_HTTPS_PROXY=""
    export HTTP_PROXY="" HTTPS_PROXY="" http_proxy="" https_proxy=""
    export ASTRA_HARBOR_ALL_PROXY="" ALL_PROXY="" all_proxy=""
    ;;
  *)
    echo "astra harness: ASTRA_HARNESS_NETWORK_MODE must be proxy or direct" >&2
    exit 78
    ;;
esac
gateway_contract="${control_repo_root}/scripts/harness/local_gateway_contract.sh"
if [ ! -f "$gateway_contract" ]; then
  echo "astra harness: local gateway contract is missing: $gateway_contract" >&2
  exit 78
fi
# shellcheck source=scripts/harness/local_gateway_contract.sh
source "$gateway_contract"

# The config and launcher-owned flags form the scored-run contract. Only the
# user-visible durable job name may be customized; accepting arbitrary Harbor
# flags here would let a caller change agents, models, attempts, verification,
# timeouts, or injected provenance after preflight validated the JSON source.
passthrough_args=()
has_job_name=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --job-name)
      if [ "$#" -lt 2 ] || [ -z "$2" ] || [[ "$2" == -* ]]; then
        echo "astra harness: --job-name requires a non-option value" >&2
        exit 78
      fi
      passthrough_args+=("$1" "$2")
      has_job_name=true
      shift 2
      ;;
    --job-name=*)
      if [ -z "${1#--job-name=}" ]; then
        echo "astra harness: --job-name requires a non-empty value" >&2
        exit 78
      fi
      passthrough_args+=("$1")
      has_job_name=true
      shift
      ;;
    *)
      echo "astra harness: refusing semantic Harbor passthrough argument: $1" >&2
      exit 78
      ;;
  esac
done

# Scored runs always own a newly started server.  Keep the removed reuse
# surface fail-closed so an old operator environment cannot silently weaken
# freshness before the tracked-worktree and artifact gates run.
if [ "${ASTRA_HARNESS_REUSE_SERVER+x}" = "x" ] \
  || [ "${ASTRA_HARNESS_EXTERNAL_SERVER_PID+x}" = "x" ]; then
  echo "astra harness: server reuse is unsupported for scored runs" >&2
  exit 78
fi

# A benchmark run needs an authenticated server-side model catalogue and a
# private token for the task container. Prefer an explicitly supplied token;
# an exact local server process started by this harness gets a short-lived
# identity automatically after ownership and health are proven. Never register
# users against a reused or remote deployment.
configured_access_token="${ASTRA_ACCESS_TOKEN:-}"

# A portable agent binary embeds the Git revision at compile time.  Refuse to
# run a dirty checkout: otherwise a fresh-looking Harbor trial can silently
# exercise a binary that predates local edits while the server health check
# still matches HEAD.  This is a harness invariant, not a task policy.
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  echo "astra harness: checkout has uncommitted tracked changes; commit before running" >&2
  exit 78
fi

expected_sha="$(git rev-parse --verify HEAD^{commit})"
api_host="${ASTRA_API_HOST:-0.0.0.0}"
api_port="${ASTRA_API_PORT-17012}"
if ! normalized_api_port="$(astra_harness_normalize_tcp_port "$api_port")"; then
  echo "astra harness: ASTRA_API_PORT must be a decimal TCP port (1-65535)" >&2
  exit 78
fi
api_port="$normalized_api_port"
api_url="${ASTRA_API_URL:-http://172.17.0.1:${api_port}}"
# Auto-bootstrap is bound to the exact gateway the harness owns.  A hostname
# allowlist is insufficient: task-container loopback, another host port, or a
# URL with a path can all receive a token minted by the host server.
api_url_is_owned_gateway=0
if astra_harness_is_owned_gateway_url "$api_url" "$api_port"; then
  api_url_is_owned_gateway=1
fi
if [ "$api_host" != "0.0.0.0" ] || [ "$api_url_is_owned_gateway" != "1" ]; then
  echo "astra harness: scored runs require the owned Docker gateway for the newly started server" >&2
  exit 78
fi
server_log="${ASTRA_HARNESS_SERVER_LOG:-${repo_root}/target/harness-runtime/astra-server.log}"
server_bin="${ASTRA_HARNESS_SERVER_BIN:-${repo_root}/target/debug/astra-server}"
agent_bin="${ASTRA_HARNESS_BIN:-${repo_root}/target/x86_64-unknown-linux-musl/debug/astra}"
source_config="${ASTRA_HARNESS_CONFIG:-}"
target_triple="x86_64-unknown-linux-musl"
probe_image="ghcr.io/cross-rs/x86_64-unknown-linux-musl:0.2.5"
if { [ -n "${ASTRA_HARNESS_TARGET:-}" ] && [ "$ASTRA_HARNESS_TARGET" != "$target_triple" ]; } \
  || { [ -n "${ASTRA_HARNESS_PROBE_IMAGE:-}" ] && [ "$ASTRA_HARNESS_PROBE_IMAGE" != "$probe_image" ]; }; then
  echo "astra harness: scored runs require the pinned musl target and probe image" >&2
  exit 78
fi
harness_pythonpath="${control_repo_root}/crates/astra-test-harness"
preflight_script="${control_repo_root}/scripts/harness/preflight.py"
case_history_script="${control_repo_root}/scripts/harness/case_history.py"
database_contract_script="${control_repo_root}/scripts/harness/fresh_database_contract.py"
model_seed_script="${control_repo_root}/scripts/harness/benchmark_model_seed.py"
database_proof="${ASTRA_HARNESS_DATABASE_PROOF:-}"
expected_database="${ASTRA_HARNESS_EXPECTED_DATABASE:-}"
database_base="${ASTRA_DATABASE:-}"
database_prefix="${ASTRA_DATABASE_PREFIX:-}"
models_file="${ASTRA_HARNESS_MODELS_FILE:-${repo_root}/.models.yaml}"

if [ ! -f "$preflight_script" ]; then
  echo "astra harness: preflight script is missing: $preflight_script" >&2
  exit 78
fi
if [ ! -f "$case_history_script" ]; then
  echo "astra harness: case history validator is missing: $case_history_script" >&2
  exit 78
fi
if [ ! -f "$database_contract_script" ]; then
  echo "astra harness: fresh database contract script is missing: $database_contract_script" >&2
  exit 78
fi
if [ ! -f "$model_seed_script" ]; then
  echo "astra harness: benchmark model seed script is missing: $model_seed_script" >&2
  exit 78
fi
if [ ! -f "$models_file" ]; then
  echo "astra harness: benchmark model credential file is missing" >&2
  exit 78
fi
if [ -z "$database_proof" ] || [ -z "$expected_database" ] || [ -z "$database_base" ]; then
  echo "astra harness: ASTRA_HARNESS_DATABASE_PROOF, ASTRA_HARNESS_EXPECTED_DATABASE, and ASTRA_DATABASE are required" >&2
  exit 78
fi
effective_database="${database_prefix}${database_base}"
if [ "$effective_database" != "$expected_database" ]; then
  echo "astra harness: effective database ${effective_database} does not match ASTRA_HARNESS_EXPECTED_DATABASE=${expected_database}" >&2
  exit 78
fi

mkdir -p "$(dirname "$server_log")" "${repo_root}/target/harbor-jobs"
if [ -n "$source_config" ] && [ ! -f "$source_config" ]; then
  echo "astra harness: benchmark config is missing: $source_config" >&2
  exit 78
fi

# Resolve the exact absent-before-seed proof before starting anything.  Its
# admission hash fences server/model seeding; the sealed contract hash is
# resolved only after the runner-owned server has performed that seed.
if ! database_identity_json="$(python3 "$database_contract_script" identity \
  --repo "$control_repo_root" \
  --database "$effective_database" \
  --proof "$database_proof" \
  --expected-source-revision "$expected_sha")"; then
  exit 78
fi
if ! read -r database_identity_sha256 database_admission_sha256 lifecycle_schema proof_phase < <(
  printf '%s' "$database_identity_json" | python3 -c '
import json
import sys

value = json.load(sys.stdin)
fields = (
    value.get("database_identity_sha256"),
    value.get("admission_sha256"),
    value.get("lifecycle_schema"),
    value.get("phase"),
)
if not all(isinstance(item, str) and item for item in fields):
    raise SystemExit(78)
print(*fields)
'
); then
  echo "astra harness: database proof identity output is malformed" >&2
  exit 78
fi
if ! [[ "$database_identity_sha256" =~ ^[0-9a-f]{64}$ ]] \
  || ! [[ "$database_admission_sha256" =~ ^[0-9a-f]{64}$ ]] \
  || [ "$lifecycle_schema" != "astra.harness.lifecycle.v1" ] \
  || [ "$proof_phase" != "absent_before_seed" ]; then
  echo "astra harness: database proof must be a fresh absent-before-seed admission" >&2
  exit 78
fi

# Abstract UDS leases cannot be replaced through the filesystem and live only
# as long as the broker process. The database identity is independent of the
# API port, so copied proofs on different ports still have one winner.
lifecycle_broker_script="${control_repo_root}/scripts/harness/lifecycle_broker.py"
lifecycle_domain_script="${control_repo_root}/scripts/harness/lifecycle_domain.py"
process_supervisor_script="${control_repo_root}/scripts/harness/process_supervisor.py"
snapshot_script="${control_repo_root}/scripts/harness/sealed_run_snapshot.py"
for helper in "$lifecycle_broker_script" "$lifecycle_domain_script" "$process_supervisor_script" "$snapshot_script"; do
  if [ ! -f "$helper" ]; then
    echo "astra harness: required lifecycle helper is missing: $helper" >&2
    exit 78
  fi
done

if [ -z "${ASTRA_HARNESS_DOMAIN_ACTIVE:-}" ]; then
  if [ -n "${ASTRA_HARNESS_SNAPSHOT_ACTIVE:-}" ]; then
    echo "astra harness: snapshot activation is valid only inside the owned lifecycle domain" >&2
    exit 78
  fi
  exec env PYTHONPATH="${control_repo_root}/scripts/harness" \
    python3 "$lifecycle_domain_script" \
      --database-identity "$database_identity_sha256" \
      --gateway-port "$api_port" \
      --state-parent "${repo_root}/target/harness-domains" -- \
      "$0" "${passthrough_args[@]}"
fi
if [ -z "${ASTRA_HARNESS_DOMAIN_STATE:-}" ] \
  || [ -z "${ASTRA_HARNESS_LIFECYCLE_GUARDIAN_PID:-}" ] \
  || [ -z "${ASTRA_HARNESS_LIFECYCLE_WITNESS_PID:-}" ]; then
  echo "astra harness: lifecycle domain did not provide its closed ownership identity" >&2
  exit 78
fi

consumption_directory="/run/lock"
if [ ! -d "$consumption_directory" ] || [ -L "$consumption_directory" ] \
  || [ ! -w "$consumption_directory" ] || [ ! -x "$consumption_directory" ] \
  || [ ! -k "$consumption_directory" ]; then
  echo "astra harness: /run/lock must be a real writable/searchable sticky directory for one-use database consumption" >&2
  exit 78
fi

server_pid=""
harbor_pid=""
snapshot_root_fd=""
snapshot_ledger_fd=""
server_started_by_harness=0
case_reservation_manifest=""
cleanup() {
  trap - EXIT INT TERM
  if astra_harness_process_is_alive "$harbor_pid"; then
    astra_harness_terminate_and_reap "$harbor_pid" 100
  fi
  if [ "$server_started_by_harness" = "1" ] && astra_harness_process_is_alive "$server_pid"; then
    astra_harness_terminate_and_reap "$server_pid" 100
  fi
  if [ -n "$case_reservation_manifest" ] && [ -f "$case_reservation_manifest" ]; then
    python3 "$case_history_script" --jobs-dir "${repo_root}/target/harbor-jobs" \
      --reservation-manifest "$case_reservation_manifest" --release-reservation >/dev/null 2>&1 || true
  fi
  if [ -n "$snapshot_ledger_fd" ]; then
    eval "exec ${snapshot_ledger_fd}<&-"
    snapshot_ledger_fd=""
  fi
  if [ -n "$snapshot_root_fd" ]; then
    eval "exec ${snapshot_root_fd}<&-"
    snapshot_root_fd=""
  fi
}
trap 'status=$?; cleanup; exit "$status"' EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

if [ ! -x "$server_bin" ]; then
  echo "astra harness: server binary is missing or not executable: $server_bin" >&2
  exit 78
fi
if [ ! -x "$agent_bin" ]; then
  echo "astra harness: agent binary is missing or not executable: $agent_bin" >&2
  exit 78
fi

# Finalize once inside the lifecycle domain, then let the snapshot creator
# exec the held-FD consumer in the same process.  There is no path reopen gap.
staging_directory="${ASTRA_HARNESS_DOMAIN_STATE}/staging"
if [ "${ASTRA_HARNESS_SNAPSHOT_ACTIVE:-}" != "1" ]; then
  mkdir "$staging_directory"
  chmod 0700 "$staging_directory"
  if [ -z "$source_config" ]; then
    echo "astra harness: ASTRA_HARNESS_CONFIG must explicitly select a new batch of at least three tasks" >&2
    exit 78
  fi
  finalized_config="${staging_directory}/finalized.json"
  PYTHONPATH="${control_repo_root}/scripts/harness" python3 - "$source_config" "$finalized_config" <<'PY'
import json
import os
import sys
from pathlib import Path

import preflight

source = Path(sys.argv[1]).expanduser()
target = Path(sys.argv[2]).resolve()
flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
try:
    source_fd = os.open(source, flags)
except OSError as error:
    print(f"astra harness: cannot open source config without following links: {error}", file=sys.stderr)
    raise SystemExit(78)
initial = os.fstat(source_fd)
opened_source = Path(f"/proc/self/fd/{source_fd}")
ok, detail = preflight.validate_benchmark_source_config(opened_source)
if not ok:
    print(f"astra harness: refusing to finalize invalid source config: {detail}", file=sys.stderr)
    raise SystemExit(78)
projection = preflight.verifier_network_projection()
payload = json.loads(opened_source.read_text(encoding="utf-8"))
final = os.fstat(source_fd)
if (initial.st_dev, initial.st_ino, initial.st_size, initial.st_mtime_ns) != (
    final.st_dev, final.st_ino, final.st_size, final.st_mtime_ns
):
    print("astra harness: source config changed during finalization", file=sys.stderr)
    raise SystemExit(78)
os.close(source_fd)
payload["verifier"] = {"disable": False, "env": projection}
serialized = (json.dumps(payload, indent=2, sort_keys=True) + "\n").encode()
flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
if hasattr(os, "O_NOFOLLOW"):
    flags |= os.O_NOFOLLOW
try:
    descriptor = os.open(target, flags, 0o400)
except OSError as error:
    print(f"astra harness: cannot create finalized config: {error}", file=sys.stderr)
    raise SystemExit(78)
with os.fdopen(descriptor, "wb") as stream:
    stream.write(serialized)
    stream.flush()
    os.fsync(stream.fileno())
PY

  mapfile -t source_tasks < <(python3 - "$finalized_config" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
for entry in payload.get("tasks", []):
    path = entry.get("path") if isinstance(entry, dict) else None
    if not isinstance(path, str) or "\n" in path:
        raise SystemExit(78)
    print(str(Path(path).expanduser().resolve(strict=True)))
PY
)
  if [ "${#source_tasks[@]}" -lt 3 ]; then
    echo "astra harness: finalized config must resolve at least three tasks" >&2
    exit 78
  fi
  # Normal scored rounds must expand coverage. Repeating a task is allowed
  # only as an explicitly named regression and is recorded separately from
  # new coverage; job names never authorize reuse.
  regression_args=()
  if [ -n "${ASTRA_HARNESS_REGRESSION_TASKS:-}" ]; then
    IFS=',' read -r -a regression_tasks <<<"${ASTRA_HARNESS_REGRESSION_TASKS}"
    for task in "${regression_tasks[@]}"; do
      if [ -z "$task" ]; then
        echo "astra harness: ASTRA_HARNESS_REGRESSION_TASKS contains an empty task" >&2
        exit 78
      fi
      regression_args+=(--allow-regression-task "$task")
    done
  fi
  history_ledger="${staging_directory}/case-history.json"
  case_reservation_manifest="${staging_directory}/case-reservation.json"
  history_task_args=()
  for task in "${source_tasks[@]}"; do
    history_task_args+=(--task "$task")
  done
  python3 "$case_history_script" --jobs-dir "${repo_root}/target/harbor-jobs" \
    "${regression_args[@]}" \
    --reservation-dir "${repo_root}/target/harness-case-reservations" \
    --reservation-owner-pid "$$" \
    --reservation-manifest "$case_reservation_manifest" \
    "${history_task_args[@]}" >"$history_ledger"
  snapshot_id="${expected_sha:0:12}-$$-$(date -u +%Y%m%dT%H%M%S%N)"
  export ASTRA_HARNESS_SNAPSHOT_ID="$snapshot_id"
  export ASTRA_HARNESS_STAGING_DIRECTORY="$staging_directory"
  snapshot_consumer_root="/proc/$$/fd/198"
  snapshot_args=()
  for task in "${source_tasks[@]}"; do
    snapshot_args+=(--task "$task")
  done
  control_relative_paths=(
    scripts/harness/run_terminal_bench_current.sh
    scripts/harness/local_gateway_contract.sh
    scripts/harness/preflight.py
    scripts/harness/case_history.py
    scripts/harness/benchmark_model_seed.py
    scripts/harness/fresh_database_contract.py
    scripts/harness/lifecycle_broker.py
    scripts/harness/lifecycle_domain.py
    scripts/harness/process_supervisor.py
    scripts/harness/recovery_environment.py
    scripts/harness/sealed_run_snapshot.py
    scripts/harness/verifier_readiness.py
    scripts/schema/schema_inventory.py
    crates/astra-test-harness/harbor_adapter.py
    crates/astra-test-harness/harbor_adapter_env.py
    crates/services/src/storage.rs
    crates/services/src/work.rs
    crates/services/src/config_version_cloud.rs
    crates/services/src/resource_governor.rs
    crates/services/src/workspace_records.rs
    crates/services/src/context_manifest.rs
    crates/astra-messaging/src/db_transport.rs
    crates/runtime/src/llm_provider_admission.rs
    crates/runtime/src/server/sweeper_lease.rs
    crates/runtime/src/server/tool_invocation_compactor.rs
  )
  control_args=()
  for relative in "${control_relative_paths[@]}"; do
    control_args+=(--control "${repo_root}/${relative}")
  done
  snapshot_parent="${ASTRA_HARNESS_DOMAIN_STATE}/snapshot"
  mkdir "$snapshot_parent"
  exec python3 "$snapshot_script" create-exec \
    --parent "$snapshot_parent" \
    --snapshot-id "$snapshot_id" \
    --agent "$agent_bin" \
    --server "$server_bin" \
    --config "$finalized_config" \
    "${snapshot_args[@]}" \
    --source-revision "$expected_sha" \
    --probe-build-info \
    --agent-target "$target_triple" \
    --consumer-root "$snapshot_consumer_root" \
    --control-base "$repo_root" \
    "${control_args[@]}" -- \
    "$snapshot_consumer_root/control/repo/scripts/harness/run_terminal_bench_current.sh" \
    "${passthrough_args[@]}"
fi
snapshot_root_fd="${ASTRA_HARNESS_SNAPSHOT_ROOT_FD:-}"
snapshot_ledger_fd="${ASTRA_HARNESS_SNAPSHOT_LEDGER_FD:-}"
if [ "$snapshot_root_fd" != "198" ] || [ "$snapshot_ledger_fd" != "197" ]; then
  echo "astra harness: sealed launcher did not inherit the canonical snapshot descriptors" >&2
  exit 78
fi
snapshot_id="${ASTRA_HARNESS_SNAPSHOT_ID:-}"
staging_directory="${ASTRA_HARNESS_STAGING_DIRECTORY:-}"
if [ -z "$snapshot_id" ] || [ "$staging_directory" != "${ASTRA_HARNESS_DOMAIN_STATE}/staging" ]; then
  echo "astra harness: sealed launcher snapshot identity is incomplete" >&2
  exit 78
fi
case_reservation_manifest="${staging_directory}/case-reservation.json"
snapshot_fd_root="/proc/$$/fd/198"
agent_bin="${snapshot_fd_root}/agent/astra"
server_bin="${snapshot_fd_root}/server/astra-server"
config="${snapshot_fd_root}/config/final.json"
verifier_readiness_ledger="${staging_directory}/verifier-readiness-ledger.json"
# A readiness failure occurs before the server, credentials, or Harbor can run.
# Keep its bounded, non-secret stdout/stderr outside the lifecycle domain (the
# domain is deliberately removed during cleanup) so a fail-closed gate is
# diagnosable afterwards. In particular, do not turn an unknown preflight
# failure into a retry or a scored model/Astra result.
preflight_evidence_directory="${repo_root}/target/harness-evidence/${snapshot_id}"
mkdir -p "$preflight_evidence_directory"
chmod 0700 "$preflight_evidence_directory"
preflight_stdout_log="${preflight_evidence_directory}/preflight.stdout.log"
preflight_stderr_log="${preflight_evidence_directory}/preflight.stderr.log"
# Harbor constructs each trial after the initial preflight process has exited.
# Give that long-lived process the physical sealed adapter directory explicitly:
# a temporary PYTHONPATH on the supervisor's Python invocation is not an
# authority for Harbor's own interpreter/process tree.
sealed_harness_pythonpath="$(readlink -f "${snapshot_fd_root}/control/repo/crates/astra-test-harness")"
if [ ! -f "${sealed_harness_pythonpath}/harbor_adapter.py" ] \
  || [ ! -f "${sealed_harness_pythonpath}/harbor_adapter_env.py" ]; then
  echo "astra harness: sealed Harbor adapter is unavailable" >&2
  exit 78
fi

# The exact snapshot, Harbor-resolved images, network namespaces and verifier
# bootstraps must all pass before database consumption, server start or spend.
PYTHONPATH="${control_repo_root}/scripts/harness:${harness_pythonpath}" python3 "$preflight_script" \
  --repo "$repo_root" \
  --agent "$agent_bin" \
  --server "$server_bin" \
  --config "$config" \
  --target "$target_triple" \
  --portable-probe-image "$probe_image" \
  --expect-verifier-network-projection \
  --snapshot-root-fd "$snapshot_root_fd" \
  --snapshot-ledger-fd "$snapshot_ledger_fd" \
  --probe-verifier-readiness \
  --verifier-readiness-ledger "$verifier_readiness_ledger" \
  --domain-state "$ASTRA_HARNESS_DOMAIN_STATE" \
  > >(tee "$preflight_stdout_log") \
  2> >(tee "$preflight_stderr_log" >&2)

python3 "$snapshot_script" verify \
  --root-fd "$snapshot_root_fd" --ledger-fd "$snapshot_ledger_fd"
validate_health() {
  local payload="$1"
  printf '%s' "$payload" | EXPECTED_BUILD_GIT_SHA="$expected_sha" python3 -c '
import json
import os
import sys

try:
    value = json.load(sys.stdin)
except Exception as error:
    print(f"invalid health JSON: {error}", file=sys.stderr)
    raise SystemExit(1)

expected = os.environ["EXPECTED_BUILD_GIT_SHA"]
actual = value.get("build_git_sha")
if actual != expected:
    print(f"health build_git_sha={actual!r}, expected current HEAD {expected}", file=sys.stderr)
    raise SystemExit(1)
database = value.get("database")
if database != "connected":
    print("health database is not connected: {!r}".format(database), file=sys.stderr)
    raise SystemExit(1)
status = value.get("status")
if status not in {"healthy", "degraded"}:
    print("health status is not ready: {!r}".format(status), file=sys.stderr)
    raise SystemExit(1)
print("health ok: status={} build_git_sha={}".format(status, actual), file=sys.stderr)
'
}

health_url="http://127.0.0.1:${api_port}/health"
models_url="http://127.0.0.1:${api_port}/models"
existing_health=""
server_started_by_harness=0
if existing_health="$(curl --noproxy '*' --connect-timeout 1 --max-time 2 -fsS "$health_url" 2>/dev/null)"; then
  echo "astra harness: a server already owns port ${api_port}; scored runs prohibit reuse" >&2
  exit 78
else
  python3 "$snapshot_script" verify \
    --root-fd "$snapshot_root_fd" --ledger-fd "$snapshot_ledger_fd"
  # A fresh-run proof intentionally starts with an absent database.  The
  # runner-owned server is its sole materializer, under the database lifecycle
  # lease it already holds; inherited operator settings must not decide this.
  ASTRA_AUTO_CREATE_DATABASE=1 \
    ASTRA_API_HOST="$api_host" ASTRA_API_PORT="$api_port" \
    ASTRA_SERVER_LIFECYCLE_OWNER="harness-$$" \
    python3 "$process_supervisor_script" run \
      --owner-pid "$$" --identity "astra-server-${snapshot_id}" -- \
      "$server_bin" >"$server_log" 2>&1 &
  server_pid=$!
  server_started_by_harness=1

  ready_health=""
  for _ in $(seq 1 60); do
    if ready_health="$(curl --noproxy '*' --connect-timeout 1 --max-time 2 -fsS "$health_url" 2>/dev/null)"; then
      break
    fi
    sleep 1
  done
  if [ -z "$ready_health" ]; then
    echo "astra harness: candidate server did not become healthy on port ${api_port}" >&2
    exit 78
  fi
  validate_health "$ready_health"
fi

server_process_is_alive=0
if [ "$server_started_by_harness" = "1" ]; then
  if ! astra_harness_process_tree_owns_tcp_listener "${server_pid:-}" "$api_port" \
    || ! astra_harness_process_tree_has_lifecycle_owner "${server_pid:-}" "harness-$$"; then
    echo "astra harness: the candidate process does not own the expected listener on port ${api_port}" >&2
    exit 78
  fi
  server_process_is_alive=1
fi

bootstrap_local_access_token() {
  python3 - "$health_url" <<'PY'
import json
import secrets
import string
import sys
import urllib.error
import urllib.request

health_url = sys.argv[1]
base_url = health_url.rsplit("/", 1)[0]
alphabet = string.ascii_lowercase + string.digits
username = "harness-" + "".join(secrets.choice(alphabet) for _ in range(20))
password = secrets.token_urlsafe(32)
payload = json.dumps(
    {
        "username": username,
        "email": username + "@local.invalid",
        "password": password,
        "display_name": username,
    },
    separators=(",", ":"),
).encode("utf-8")
request = urllib.request.Request(
    base_url + "/admin/register",
    data=payload,
    headers={"content-type": "application/json"},
    method="POST",
)
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
try:
    with opener.open(request, timeout=10) as response:
        body = response.read()
except (OSError, urllib.error.URLError) as error:
    print(f"local auth bootstrap failed: {error}", file=sys.stderr)
    raise SystemExit(78)
try:
    value = json.loads(body.decode("utf-8"))
except (UnicodeDecodeError, json.JSONDecodeError):
    print("local auth bootstrap returned invalid JSON", file=sys.stderr)
    raise SystemExit(78)
token = value.get("access_token")
if not isinstance(token, str) or not token.strip():
    print("local auth bootstrap response omitted access_token", file=sys.stderr)
    raise SystemExit(78)
print(token.strip())
PY
}

if [ -z "$configured_access_token" ]; then
  auto_bootstrap="${ASTRA_HARNESS_AUTO_BOOTSTRAP_AUTH:-1}"
  # Process ownership, not address equality or an operator reuse switch, is
  # the authority to create an identity. A same-build server already listening
  # on this port can still belong to another run/user and share a durable DB.
  if astra_harness_can_auto_bootstrap_auth \
    "$auto_bootstrap" \
    "$server_started_by_harness" \
    "$api_url_is_owned_gateway" \
    "$server_process_is_alive"; then
    configured_access_token="$(bootstrap_local_access_token)"
    export ASTRA_ACCESS_TOKEN="$configured_access_token"
    echo "astra harness: bootstrapped an ephemeral local benchmark identity" >&2
  else
    echo "astra harness: ASTRA_ACCESS_TOKEN is required; for a harness-owned local server set ASTRA_HARNESS_AUTO_BOOTSTRAP_AUTH=1" >&2
    exit 78
  fi
else
  export ASTRA_ACCESS_TOKEN="$configured_access_token"
fi

# Refuse to seed or seal against a server that died or lost the exact owned
# listener.  The first admin and selected model go through Astra's normal
# authenticated control-plane APIs; the harness never writes model rows.
if [ "$server_started_by_harness" != "1" ] \
  || ! astra_harness_process_tree_owns_tcp_listener "${server_pid:-}" "$api_port"; then
  echo "astra harness: the owned server exited during authentication/bootstrap" >&2
  exit 78
fi

if ! model_seed_json="$(PYTHONPATH="${control_repo_root}/scripts/harness" \
  python3 "$model_seed_script" \
    --api-url "http://127.0.0.1:${api_port}" \
    --config "$config" \
    --models-file "$models_file")"; then
  exit 78
fi
if ! read -r selected_model_base selected_model_thinking < <(
  printf '%s' "$model_seed_json" | python3 -c '
import json, sys
value = json.load(sys.stdin)
name = value.get("model_name")
thinking = value.get("thinking_mode")
if not isinstance(name, str) or not name or thinking not in {"none", "high"}:
    raise SystemExit(78)
print(name, thinking)
'
); then
  echo "astra harness: model seed result is malformed" >&2
  exit 78
fi
export ASTRA_HARNESS_MODEL_BASE="$selected_model_base"
export ASTRA_HARNESS_MODEL_THINKING="$selected_model_thinking"

PYTHONPATH="${control_repo_root}/scripts/harness" python3 "$database_contract_script" seal \
  --repo "$control_repo_root" \
  --database "$effective_database" \
  --proof "$database_proof" \
  --expected-admission-sha256 "$database_admission_sha256"
if ! sealed_identity_json="$(python3 "$database_contract_script" identity \
  --repo "$control_repo_root" \
  --database "$effective_database" \
  --proof "$database_proof" \
  --expected-source-revision "$expected_sha")"; then
  exit 78
fi
if ! read -r sealed_database_identity database_contract_sha256 sealed_lifecycle_schema sealed_phase < <(
  printf '%s' "$sealed_identity_json" | python3 -c '
import json, sys
value = json.load(sys.stdin)
print(value.get("database_identity_sha256", ""), value.get("contract_sha256", ""), value.get("lifecycle_schema", ""), value.get("phase", ""))
'
); then
  exit 78
fi
if [ "$sealed_database_identity" != "$database_identity_sha256" ] \
  || ! [[ "$database_contract_sha256" =~ ^[0-9a-f]{64}$ ]] \
  || [ "$sealed_lifecycle_schema" != "$lifecycle_schema" ] \
  || [ "$sealed_phase" != "sealed_ready" ]; then
  echo "astra harness: sealed database contract does not match lifecycle admission" >&2
  exit 78
fi
PYTHONPATH="${control_repo_root}/scripts/harness" python3 "$database_contract_script" verify \
  --repo "$control_repo_root" \
  --database "$effective_database" \
  --proof "$database_proof" \
  --expected-database-identity-sha256 "$database_identity_sha256" \
  --expected-contract-sha256 "$database_contract_sha256" \
  --expected-source-revision "$expected_sha" \
  --lifecycle-guardian-pid "$ASTRA_HARNESS_LIFECYCLE_GUARDIAN_PID" \
  --lifecycle-witness-pid "$ASTRA_HARNESS_LIFECYCLE_WITNESS_PID" \
  --gateway-port "$api_port" \
  --consumption-directory "$consumption_directory"

# Re-check the runtime gate against the server that is actually listening.
PYTHONPATH="${control_repo_root}/scripts/harness:${harness_pythonpath}" python3 "$preflight_script" \
  --repo "$repo_root" \
  --agent "$agent_bin" \
  --server "$server_bin" \
  --config "$config" \
  --target "$target_triple" \
  --portable-probe-image "$probe_image" \
  --health-url "$health_url" \
  --expected-build-git-sha "$expected_sha" \
  --models-url "$models_url" \
  --expect-verifier-network-projection \
  --snapshot-root-fd "$snapshot_root_fd" \
  --snapshot-ledger-fd "$snapshot_ledger_fd"

agent_sha256="$(sha256sum "$agent_bin" | awk '{print $1}')"

if [ -n "${ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS:-}" ] \
  && [ "$ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS" != "60" ]; then
  echo "astra harness: scored runs require ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS=60" >&2
  exit 78
fi
harbor_cleanup_margin=60

harbor_args=(run -c "$config")
if [ "$has_job_name" = false ]; then
  # Harbor job names are user-visible durable identities.  Make every default
  # invocation unique so an interrupted/stale job cannot collide with a new
  # trial and be mistaken for a resumed run.
  harbor_args+=(--job-name "astra-tbench-$(git rev-parse --short HEAD)-$(date -u +%Y%m%dT%H%M%SZ)-$$")
fi

python3 "$snapshot_script" verify \
  --root-fd "$snapshot_root_fd" --ledger-fd "$snapshot_ledger_fd"
verifier_readiness_sha256="$(sha256sum "$verifier_readiness_ledger" | awk '{print $1}')"
python3 "$process_supervisor_script" run \
  --owner-pid "$$" --identity "harbor-${snapshot_id}" -- \
  env "PYTHONPATH=${sealed_harness_pythonpath}" harbor "${harbor_args[@]}" \
  --agent-env "ASTRA_HARBOR_BIN=${agent_bin}" \
  --agent-env "ASTRA_API_URL=${api_url}" \
  --agent-env "ASTRA_EXPECTED_BUILD_GIT_SHA=${expected_sha}" \
  --agent-env "ASTRA_HARNESS_BINARY_SHA256=${agent_sha256}" \
  --agent-env "ASTRA_HARNESS_BUILD_PROFILE=debug" \
  --agent-env "ASTRA_HARNESS_VERIFIER_READINESS_SHA256=${verifier_readiness_sha256}" \
  --agent-env "ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS=${harbor_cleanup_margin}" \
  --agent-env "ASTRA_ALLOW_ENVIRONMENT_BACKGROUND_TASKS=1" \
  --allow-agent-host 10.222.1.10 \
  --allow-environment-host 10.222.1.10 \
  "${passthrough_args[@]}" &
harbor_pid=$!
if wait "$harbor_pid"; then
  harbor_status=0
else
  harbor_status=$?
fi
harbor_pid=""
exit "$harbor_status"
