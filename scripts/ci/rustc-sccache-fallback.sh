#!/usr/bin/env bash
# Keep the optional compiler cache from suppressing Rust CI when its remote
# backend is unavailable. Cargo passes the real rustc path as the first
# argument to RUSTC_WRAPPER.
set -uo pipefail

state_dir="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
disabled_file="${state_dir}/astra-sccache-disabled"

if [[ -f "${disabled_file}" ]]; then
  exec "$@"
fi

sccache "$@"
cache_status=$?
if (( cache_status == 0 )); then
  exit 0
fi

# A non-zero cache invocation can mean either a backend failure or a real
# compiler failure. Running rustc directly distinguishes the two without
# matching provider-specific error messages.
"$@"
compiler_status=$?
if (( compiler_status != 0 )); then
  exit "${compiler_status}"
fi

mkdir -p "${state_dir}"
if (set -o noclobber; : > "${disabled_file}") 2>/dev/null; then
  printf '%s\n' \
    '::warning title=Rust cache disabled::sccache failed while direct rustc succeeded; using uncached compilation for the rest of this job' \
    >&2
fi

exit 0
