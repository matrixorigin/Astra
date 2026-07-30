#!/usr/bin/env bash
set -euo pipefail

# Hermetic Phase-0 production baseline.
#
# Required caller environment:
#   ASTRA_PHASE0_MODELS_FILE   exact models YAML path
#   ASTRA_PHASE0_SERVER_PORT   unused loopback port
#   MatrixOne connection/auth variables required by AppSettings
#
# Optional:
#   ASTRA_PHASE0_SOURCE_MODEL  defaults to deepseek-v4-flash
#   ASTRA_PHASE0_WORK_ROOT     repository-external build/output parent

phase0_workspace="$(realpath "$(git rev-parse --show-toplevel)")"
cd "$phase0_workspace"

if [[ -n "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
    echo "phase0 baseline requires a clean Git worktree" >&2
    exit 2
fi

: "${ASTRA_PHASE0_MODELS_FILE:?set ASTRA_PHASE0_MODELS_FILE to the exact models YAML}"
: "${ASTRA_PHASE0_SERVER_PORT:?set ASTRA_PHASE0_SERVER_PORT to an unused loopback port}"

phase0_models_file="$(realpath "$ASTRA_PHASE0_MODELS_FILE")"
if [[ ! -f "$phase0_models_file" ]]; then
    echo "phase0 models file does not exist" >&2
    exit 2
fi

if [[ -n "${ASTRA_PHASE0_WORK_ROOT:-}" ]]; then
    phase0_work_root="$ASTRA_PHASE0_WORK_ROOT"
elif [[ -n "${XDG_CACHE_HOME:-}" ]]; then
    phase0_work_root="$XDG_CACHE_HOME/astra/phase0"
elif [[ -n "${HOME:-}" ]]; then
    phase0_work_root="$HOME/.cache/astra/phase0"
else
    phase0_work_root="/var/tmp/astra-phase0"
fi
phase0_work_root="$(realpath -m "$phase0_work_root")"
case "$phase0_work_root/" in
    "$phase0_workspace/"*)
        echo "phase0 work root must be outside the Git worktree" >&2
        exit 2
        ;;
esac
phase0_existing_parent="$phase0_work_root"
while [[ ! -d "$phase0_existing_parent" ]]; do
    phase0_parent="$(dirname "$phase0_existing_parent")"
    if [[ "$phase0_parent" == "$phase0_existing_parent" ]]; then
        echo "could not resolve an existing parent for phase0 work root" >&2
        exit 2
    fi
    phase0_existing_parent="$phase0_parent"
done
if git -C "$phase0_existing_parent" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "phase0 work root must not be nested in any Git worktree" >&2
    exit 2
fi
mkdir -p -- "$phase0_work_root"
phase0_work_root="$(realpath "$phase0_work_root")"
case "$phase0_work_root/" in
    "$phase0_workspace/"*)
        echo "phase0 work root must be outside the Git worktree" >&2
        exit 2
        ;;
esac
if git -C "$phase0_work_root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "phase0 work root must not be nested in any Git worktree" >&2
    exit 2
fi

phase0_run_id="$(
    od -An -N32 -tx1 /dev/urandom | tr -d ' \n'
)"
if [[ ! "$phase0_run_id" =~ ^[0-9a-f]{64}$ ]]; then
    echo "could not generate a fresh 64-hex build epoch" >&2
    exit 2
fi

phase0_target_dir="$(mktemp -d "$phase0_work_root/target.XXXXXXXX")"
phase0_output_dir="$(mktemp -d "$phase0_work_root/output.XXXXXXXX")"

cleanup_phase0_target() {
    case "$phase0_target_dir" in
        "$phase0_work_root"/target.*)
            rm -rf -- "$phase0_target_dir"
            ;;
        *)
            echo "refusing to clean unexpected target path" >&2
            ;;
    esac
}
trap cleanup_phase0_target EXIT

export CARGO_TARGET_DIR="$phase0_target_dir"
export CARGO_INCREMENTAL=0
export ASTRA_BUILD_ATTESTATION_NONCE="$phase0_run_id"
export ASTRA_HISTORY_WORK_BASELINE_RUN_ID="$phase0_run_id"
export ASTRA_HISTORY_WORK_BASELINE_GIT_SHA
ASTRA_HISTORY_WORK_BASELINE_GIT_SHA="$(git rev-parse --verify 'HEAD^{commit}')"
export ASTRA_HISTORY_WORK_TRACE=1
export ASTRA_PHASE0_BASELINE_EXCLUSIVE=1
export ASTRA_PHASE0_BASELINE_DIR="$phase0_output_dir"
export ASTRA_CONFIG_SOURCE=explicit-env
export ASTRA_PHASE0_MODELS_FILE="$phase0_models_file"
export ASTRA_PHASE0_SOURCE_MODEL="${ASTRA_PHASE0_SOURCE_MODEL:-deepseek-v4-flash}"
export ASTRA_AUTO_CREATE_DATABASE=1

cargo build \
    -p astra-runtime \
    -p astra-cli \
    -p astra-edge \
    -p astra-core \
    --bin astra-server \
    --bin astra \
    --bin astra-edge \
    --bin history-work-production-baseline \
    --bin history-work-production-baseline-verify

export ASTRA_PHASE0_SERVER_BIN="$phase0_target_dir/debug/astra-server"
export ASTRA_PHASE0_CLI_BIN="$phase0_target_dir/debug/astra"
export ASTRA_PHASE0_EDGE_BIN="$phase0_target_dir/debug/astra-edge"

cargo test \
    -p astra-runtime \
    --test system_matrix_http_e2e \
    --features bridge-e2e-hooks \
    e2e_matrix_phase0_external_production_topologies \
    -- \
    --ignored \
    --exact \
    --nocapture \
    --test-threads=1

mapfile -d '' phase0_scenarios < <(
    find "$phase0_output_dir" -maxdepth 1 -type f \
        -name '*.production_scenario.json' -print0 | sort -z
)
mapfile -d '' phase0_captures < <(
    find "$phase0_output_dir" -maxdepth 1 -type f \
        -name '*.production_process_capture.json' -print0 | sort -z
)

if [[ "${#phase0_scenarios[@]}" -ne 9 || "${#phase0_captures[@]}" -ne 19 ]]; then
    echo "phase0 runner did not emit the exact 9-scenario/19-capture matrix" >&2
    exit 1
fi

phase0_artifact="$phase0_output_dir/production_baseline.json"
phase0_assemble_args=(
    --cli-executable "$ASTRA_PHASE0_CLI_BIN"
    --server-executable "$ASTRA_PHASE0_SERVER_BIN"
    --edge-executable "$ASTRA_PHASE0_EDGE_BIN"
    --output "$phase0_artifact"
)
for phase0_scenario in "${phase0_scenarios[@]}"; do
    phase0_assemble_args+=(--scenario "$phase0_scenario")
done
for phase0_capture in "${phase0_captures[@]}"; do
    phase0_assemble_args+=(--capture "$phase0_capture")
done

"$phase0_target_dir/debug/history-work-production-baseline" \
    "${phase0_assemble_args[@]}"
"$phase0_target_dir/debug/history-work-production-baseline-verify" \
    --input "$phase0_artifact"

echo "phase0 baseline run_id=$phase0_run_id"
echo "phase0 baseline artifact=$phase0_artifact"
