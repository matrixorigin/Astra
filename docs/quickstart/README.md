# 5-Minute Quick Start

## Prerequisites

- Rust toolchain
- Docker
- Make
- Git

## Local Development

```bash
git clone <repo-url>
cd mo-dev-agent
make dev-init
make dev-start
make dev-status
```

Open `http://localhost:8000/docs`.

## Docker App Stack

```bash
cp .env.example .env
make dev-start-docker
make dev-status
```

## First Validation Pass

```bash
make check
make test
make test-contract
```
