# Configuration Reference

## Source of Truth

Use these files as the canonical configuration references:

- `.env.example`
- `deployment/all-in-one/.env.example`
- `rust/crates/core/src/config.rs` (`AppSettings`, `MATRIXONE_*`, `JWT_*`, embeddings, Memoria, bridge)

## Core Variables

### Security

- `TOKEN_ENCRYPTION_KEY`
- `JWT_SECRET_KEY`
- `JWT_ALGORITHM`
- `JWT_ACCESS_TOKEN_EXPIRE_MINUTES`
- `JWT_REFRESH_TOKEN_EXPIRE_DAYS`
- `BCRYPT_ROUNDS`

### MatrixOne

- `MATRIXONE_HOST`
- `MATRIXONE_PORT`
- `MATRIXONE_USER`
- `MATRIXONE_PASSWORD`
- `ASTRA_DATABASE`
- `ASTRA_DATABASE_PREFIX` (optional): when set and non-empty, the runtime uses
  `{ASTRA_DATABASE_PREFIX}{ASTRA_DATABASE}` as the logical database name (same rule as
  `astra_core::resolve_database_name` in `rust/crates/core/src/config.rs`). Use this to
  isolate dev/CI from production on one MatrixOne server.
- `ASTRA_AUTO_CREATE_DATABASE` (optional): when `1`, `astra_services::storage::ensure_core_schema` connects to `MATRIXONE_BOOTSTRAP_CATALOG` (default `mysql`) and runs `CREATE DATABASE IF NOT EXISTS` for the effective database before `CREATE TABLE` DDL. **Default is off** so production never implicitly creates databases.
- `MATRIXONE_BOOTSTRAP_CATALOG` (optional): catalog used only for the auto-create step (default `mysql`).

### Redis

- `REDIS_HOST`
- `REDIS_PORT`
- `REDIS_PASSWORD`

### Memoria / Bridge

- `MEMORIA_BASE_URL`
- `MEMORIA_MASTER_KEY`
- `CHAT_TURN_BRIDGE_URL`
- `CHAT_TURN_BRIDGE_SECRET`

## Validation

```bash
make dev-init
make check
```
