#!/usr/bin/env bash
# Enable compiler caching only after its backend is usable. Cargo and rustc are
# correctness dependencies; the GitHub Actions cache service is not.
set -euo pipefail

if [[ -z "${GITHUB_ENV:-}" ]]; then
  echo "GITHUB_ENV is required to configure sccache" >&2
  exit 2
fi

export SCCACHE_GHA_ENABLED=true
export SCCACHE_CACHE_SIZE=5G
# This is sccache's native fail-open path for server communication failures
# after a successful startup. The explicit probe below covers startup failures.
export SCCACHE_IGNORE_SERVER_IO_ERROR=1

{
  echo "SCCACHE_GHA_ENABLED=${SCCACHE_GHA_ENABLED}"
  echo "SCCACHE_CACHE_SIZE=${SCCACHE_CACHE_SIZE}"
  echo "SCCACHE_IGNORE_SERVER_IO_ERROR=${SCCACHE_IGNORE_SERVER_IO_ERROR}"
} >> "${GITHUB_ENV}"

rustc_path="$(command -v rustc)"
sccache_path="${SCCACHE_PATH:-sccache}"

if "${sccache_path}" "${rustc_path}" -vV >/dev/null; then
  echo "RUSTC_WRAPPER=${sccache_path}" >> "${GITHUB_ENV}"
  exit 0
else
  cache_status=$?
fi

# Separate an optional cache failure from a broken compiler/toolchain. The
# latter must remain fatal rather than being reported as a successful fallback.
if "${rustc_path}" -vV >/dev/null; then
  :
else
  compiler_status=$?
  echo "rustc failed after sccache exited with status ${cache_status}" >&2
  exit "${compiler_status}"
fi

echo "::warning title=Rust cache disabled::sccache health check failed with status ${cache_status}; continuing with uncached Rust compilation" >&2
echo "RUSTC_WRAPPER=" >> "${GITHUB_ENV}"
