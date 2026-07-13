#!/usr/bin/env bash
#
# Run the configured MySQL-protocol client with portable TLS negotiation.
#
# MySQL, MariaDB, and distro-provided compatibility clients do not share a
# portable "disable TLS" argument. `ASTRA_MYSQL_TLS_MODE` describes the
# endpoint policy instead of exposing one client's command-line syntax:
#
#   auto (default) - probe the endpoint with a harmless authenticated request,
#                    prefer TLS when it is usable, otherwise use plaintext.
#                    This is appropriate for local MatrixOne and does not let
#                    ambient client defaults silently force TLS.
#   disabled       - require plaintext. Use this only for a known non-TLS
#                    development endpoint.
#   required       - require TLS.
#
# The adapter selects the best supported option for the installed client. An
# endpoint policy that the client cannot represent is an explicit setup error;
# silently changing its security mode would be unsafe.

set -euo pipefail

mysql_client="${ASTRA_MYSQL_CLIENT:-mysql}"
tls_mode="${ASTRA_MYSQL_TLS_MODE:-auto}"

case "$tls_mode" in
    auto|disabled|required) ;;
    *)
        echo "ASTRA_MYSQL_TLS_MODE must be one of: auto, disabled, required." >&2
        exit 2
        ;;
esac

client_help="$("$mysql_client" --help 2>&1 || true)"
supports_ssl_mode=false
supports_skip_ssl=false
supports_ssl=false
if grep -Eq '(^|[[:space:]])--ssl-mode([=[:space:]]|$)' <<<"$client_help"; then
    supports_ssl_mode=true
fi
if grep -Eq '(^|[[:space:]])--skip-ssl([[:space:]]|$)' <<<"$client_help"; then
    supports_skip_ssl=true
fi
if grep -Eq '(^|[[:space:]])--ssl([[:space:]]|$)' <<<"$client_help"; then
    supports_ssl=true
fi

args=(
    --protocol=TCP
    "-h${MATRIXONE_HOST:-127.0.0.1}"
    "-P${MATRIXONE_PORT:-6001}"
    "-u${MATRIXONE_USER:-root}"
    "-p${MATRIXONE_PASSWORD:-111}"
)

tls_arg=""
case "$tls_mode" in
    disabled)
        if [[ "$supports_ssl_mode" == true ]]; then
            tls_arg='--ssl-mode=DISABLED'
        elif [[ "$supports_skip_ssl" == true ]]; then
            tls_arg='--skip-ssl'
        else
            echo "The configured MySQL client cannot enforce ASTRA_MYSQL_TLS_MODE=disabled." >&2
            echo "Install a client with --ssl-mode or --skip-ssl support, or choose a compatible client explicitly." >&2
            exit 2
        fi
        ;;
    required)
        if [[ "$supports_ssl_mode" == true ]]; then
            tls_arg='--ssl-mode=REQUIRED'
        elif [[ "$supports_ssl" == true ]]; then
            tls_arg='--ssl'
        else
            echo "The configured MySQL client cannot enforce ASTRA_MYSQL_TLS_MODE=required." >&2
            echo "Install a client with --ssl-mode or --ssl support, or choose a compatible client explicitly." >&2
            exit 2
        fi
        ;;
    auto)
        # A client feature only tells us which switch it understands; it says
        # nothing about the endpoint. Select its connection policy before the
        # caller's command runs, so an SQL mutation is never retried merely to
        # work around a TLS handshake. Prefer encrypted transport when it is
        # available, then make a deliberate plaintext fallback.
        if [[ "$supports_ssl_mode" == true ]]; then
            candidates=(--ssl-mode=PREFERRED --ssl-mode=DISABLED)
        elif [[ "$supports_skip_ssl" == true ]]; then
            candidates=("" --skip-ssl)
        else
            candidates=("")
        fi
        for candidate in "${candidates[@]}"; do
            probe_args=("${args[@]}")
            [[ -n "$candidate" ]] && probe_args+=("$candidate")
            if "$mysql_client" "${probe_args[@]}" -e 'SELECT 1' >/dev/null 2>&1; then
                tls_arg="$candidate"
                break
            fi
        done

        # Preserve the native diagnostic when neither harmless probe can
        # connect. It contains the endpoint/authentication detail a synthetic
        # wrapper error would otherwise hide.
        if [[ -z "$tls_arg" && "${candidates[0]}" != "" ]]; then
            tls_arg="${candidates[0]}"
        fi
        ;;
esac

[[ -n "$tls_arg" ]] && args+=("$tls_arg")
exec "$mysql_client" "${args[@]}" "$@"
