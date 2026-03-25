# Configuration Reference

## Source of Truth

Use these files as the canonical configuration references:

- `.env.example`
- `deployment/all-in-one/.env.example`
- `rust/crates/api-shell/src/config.rs`

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
- `MATRIXONE_DATABASE`

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
