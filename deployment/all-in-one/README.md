# All-in-One Deployment

This compose stack is now Rust-first.

## Profiles

- default: infrastructure only (`matrixone` + `redis`)
- `app`: adds `init` and `api`

## Start Infrastructure Only

```bash
cd deployment/all-in-one
docker compose up -d
```

## Start Full App Stack

```bash
cd deployment/all-in-one
docker compose --profile app up -d --build
```

## Services

| Service | Port | Profile | Description |
| --- | --- | --- | --- |
| matrixone | 6001 | default | primary database |
| redis | 6379 | default | cache and queue support |
| init | - | app | one-shot initialization via `mo-admin init` |
| api | 8000 | app | Rust API shell |

## Notes

- The old Python `skill-worker` and `model-server` services were removed because they no longer exist in this repository.
- For day-to-day development from the repo root, `make dev-start-docker` wraps the app profile for you.
