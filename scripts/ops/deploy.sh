#!/usr/bin/env bash
# Deploy the production Compose profile from the repository's canonical assets.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
COMPOSE_FILE="${REPO_ROOT}/deployment/all-in-one/docker-compose.prod.yml"
ENV_FILE="${ASTRA_PRODUCTION_ENV_FILE:-${REPO_ROOT}/.env.production}"
API_REPLICAS="${1:-${ASTRA_API_REPLICAS:-1}}"

if ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1; then
    echo "Error: Docker with the Compose plugin is required." >&2
    exit 1
fi

if [ ! -f "${ENV_FILE}" ]; then
    echo "Error: production environment file not found: ${ENV_FILE}" >&2
    echo "Create it with: cp ${REPO_ROOT}/.env.production.example ${REPO_ROOT}/.env.production" >&2
    exit 1
fi

case "${API_REPLICAS}" in
    ''|*[!0-9]*|0)
        echo "Error: API replica count must be a positive integer (got: ${API_REPLICAS})." >&2
        exit 1
        ;;
esac

compose() {
    docker compose --env-file "${ENV_FILE}" -f "${COMPOSE_FILE}" "$@"
}

echo "Validating production configuration..."
compose config --quiet

echo "Starting Astra Server behind Nginx (${API_REPLICAS} API replica(s))..."
compose up -d --scale api="${API_REPLICAS}"

echo "Deployment started. Inspect it with:"
echo "  docker compose --env-file ${ENV_FILE} -f ${COMPOSE_FILE} ps"
