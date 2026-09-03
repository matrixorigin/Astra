#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
configure="${script_dir}/configure-sccache.sh"
fixture_dir="$(mktemp -d)"
trap 'rm -rf -- "${fixture_dir}"' EXIT

mkdir -p "${fixture_dir}/bin"

cat > "${fixture_dir}/bin/sccache" <<'EOF'
#!/usr/bin/env bash
printf 'called\n' >> "${FAKE_SCCACHE_CALLS}"
if [[ "${FAKE_SCCACHE_STATUS}" != "0" ]]; then
  exit "${FAKE_SCCACHE_STATUS}"
fi
exec "$@"
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

assert_env_line() {
  local expected="$1"
  if ! grep -Fxq -- "${expected}" "${GITHUB_ENV}"; then
    echo "missing environment line: ${expected}" >&2
    exit 1
  fi
}

export PATH="${fixture_dir}/bin:${PATH}"
export SCCACHE_PATH="${fixture_dir}/bin/sccache"
export FAKE_SCCACHE_CALLS="${fixture_dir}/sccache-calls"
export FAKE_RUSTC_CALLS="${fixture_dir}/rustc-calls"

# A healthy backend enables the installed sccache binary and native I/O
# fallback without invoking rustc outside the cache probe.
export GITHUB_ENV="${fixture_dir}/healthy.env"
export FAKE_SCCACHE_STATUS=0
export FAKE_RUSTC_STATUS=0
"${configure}"
assert_env_line "SCCACHE_GHA_ENABLED=true"
assert_env_line "SCCACHE_CACHE_SIZE=5G"
assert_env_line "SCCACHE_IGNORE_SERVER_IO_ERROR=1"
assert_env_line "RUSTC_WRAPPER=${SCCACHE_PATH}"
assert_count 1 "${FAKE_SCCACHE_CALLS}"
assert_count 1 "${FAKE_RUSTC_CALLS}"

# A backend startup failure is confirmed against direct rustc and then leaves
# the rest of the job uncached.
export GITHUB_ENV="${fixture_dir}/cache-failure.env"
export FAKE_SCCACHE_STATUS=2
"${configure}"
assert_env_line "RUSTC_WRAPPER="
assert_count 2 "${FAKE_SCCACHE_CALLS}"
assert_count 2 "${FAKE_RUSTC_CALLS}"

# A broken compiler remains fatal rather than being mislabeled as a cache-only
# failure.
export GITHUB_ENV="${fixture_dir}/compiler-failure.env"
export FAKE_SCCACHE_STATUS=0
export FAKE_RUSTC_STATUS=42
if "${configure}"; then
  echo "expected compiler failure to remain fatal" >&2
  exit 1
else
  status=$?
fi
if [[ "${status}" != "42" ]]; then
  echo "expected compiler status 42, found ${status}" >&2
  exit 1
fi
assert_count 3 "${FAKE_SCCACHE_CALLS}"
assert_count 4 "${FAKE_RUSTC_CALLS}"
if grep -Fq "RUSTC_WRAPPER=" "${GITHUB_ENV}"; then
  echo "compiler failure must not emit a successful cache configuration" >&2
  exit 1
fi

echo "sccache fallback contract: ok"
