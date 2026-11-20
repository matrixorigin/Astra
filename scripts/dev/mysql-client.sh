#!/usr/bin/env bash
#
# Run the configured MySQL-protocol client with portable TLS negotiation.
#
# MySQL, MariaDB, and distro-provided compatibility clients do not share a
# portable "disable TLS" argument. `ASTRA_MYSQL_TLS_MODE` describes the
# endpoint policy instead of exposing one client's command-line syntax:
#
#   auto (default) - local endpoints may fall back to plaintext after a harmless
#                    probe; remote endpoints require TLS and never downgrade.
#   disabled       - require plaintext. Use this only for a known non-TLS
#                    development endpoint.
#   required       - require TLS.
#
# The adapter selects the best supported option for the installed client. An
# endpoint policy that the client cannot represent is an explicit setup error;
# silently changing its security mode would be unsafe.

set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../lib/mysql_protocol.sh"
mysql_connection_args
MYSQL_PWD="${MATRIXONE_PASSWORD:-111}" exec "$mysql_client" "${mysql_connection_options[@]}" "$@"
