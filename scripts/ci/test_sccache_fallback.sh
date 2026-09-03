#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
wrapper="${script_dir}/rustc-sccache-fallback.sh"
fixture_dir="$(mktemp -d)"
trap 'rm -rf -- "${fixture_dir}"' EXIT

mkdir -p "${fixture_dir}/bin"

cat > "${fixture_dir}/bin/sccache" <<'EOF'
#!/usr/bin/env bash
printf 'called\n' >> "${FAKE_SCCACHE_CALLS}"
exit "${FAKE_SCCACHE_STATUS}"
EOF

cat > "${fixture_dir}/bin/rustc" <<'EOF'
#!/usr/bin/env bash
printf 'called\n' >> "${FAKE_RUSTC_CALLS}"
exit "${FAKE_RUSTC_STATUS}"
EOF

chmod +x "${fixture_dir}/bin/sccache" "${fixture_dir}/bin/rustc"

call_count() {
  local path="$1"
  if [[ -f "${path}" ]]; then
    wc -l < "${path}" | tr -d ' '
  else
    printf '0\n'
  fi
}

assert_count() {
  local expected="$1"
  local path="$2"
  local actual
  actual="$(call_count "${path}")"
  if [[ "${actual}" != "${expected}" ]]; then
    echo "expected ${expected} calls in ${path}, found ${actual}" >&2
    exit 1
  fi
}

export PATH="${fixture_dir}/bin:${PATH}"
export FAKE_SCCACHE_CALLS="${fixture_dir}/sccache-calls"
export FAKE_RUSTC_CALLS="${fixture_dir}/rustc-calls"

# A cache backend failure is retried directly, then disables caching for all
# later compiler invocations in the same job.
export RUNNER_TEMP="${fixture_dir}/cache-failure"
export FAKE_SCCACHE_STATUS=2
export FAKE_RUSTC_STATUS=0
mkdir -p "${RUNNER_TEMP}"
"${wrapper}" "${fixture_dir}/bin/rustc" --version
[[ -f "${RUNNER_TEMP}/astra-sccache-disabled" ]]
assert_count 1 "${FAKE_SCCACHE_CALLS}"
assert_count 1 "${FAKE_RUSTC_CALLS}"

"${wrapper}" "${fixture_dir}/bin/rustc" --version
assert_count 1 "${FAKE_SCCACHE_CALLS}"
assert_count 2 "${FAKE_RUSTC_CALLS}"

# A genuine compiler failure remains fatal and must not disable the cache.
export RUNNER_TEMP="${fixture_dir}/compiler-failure"
export FAKE_RUSTC_STATUS=1
mkdir -p "${RUNNER_TEMP}"
if "${wrapper}" "${fixture_dir}/bin/rustc" --version; then
  echo "expected direct compiler failure to remain fatal" >&2
  exit 1
fi
[[ ! -e "${RUNNER_TEMP}/astra-sccache-disabled" ]]
assert_count 2 "${FAKE_SCCACHE_CALLS}"
assert_count 3 "${FAKE_RUSTC_CALLS}"

# Healthy cache invocations do not call rustc directly or create fallback
# state.
export RUNNER_TEMP="${fixture_dir}/cache-success"
export FAKE_SCCACHE_STATUS=0
export FAKE_RUSTC_STATUS=0
mkdir -p "${RUNNER_TEMP}"
"${wrapper}" "${fixture_dir}/bin/rustc" --version
[[ ! -e "${RUNNER_TEMP}/astra-sccache-disabled" ]]
assert_count 3 "${FAKE_SCCACHE_CALLS}"
assert_count 3 "${FAKE_RUSTC_CALLS}"

echo "sccache fallback contract: ok"
