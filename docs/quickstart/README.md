# 5-Minute Quick Start

## Prerequisites

- Rust toolchain
- Docker
- Make
- Git

## Local Development

```bash
git clone <repo-url>
cd astra
make dev-init
make dev-start
make dev-status
```

Open `http://localhost:6789/docs`.

## Docker App Stack

```bash
make stack-env
# Edit deployment/all-in-one/.env and fill MEMORIA_EMBEDDING_API_KEY/MEMORIA_EMBEDDING_BASE_URL.
make stack-up
make stack-status
```

## First Validation Pass

```bash
make check
make test
make test-contract
```
