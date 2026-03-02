# Dependencies Reference

## Dependency Groups

All dependencies are defined in `pyproject.toml` (single source of truth).

| Group | Scope | Install command |
|-------|-------|-----------------|
| `[tool.poetry.dependencies]` | Runtime — required for API server | `poetry install` |
| `[tool.poetry.group.dev.dependencies]` | Dev + test — pytest, ruff, mypy, etc. | `poetry install --with dev` |
| `[tool.poetry.extras] local-embedding` | Optional — local embedding models | `poetry install -E local-embedding` |

## Installation

```bash
# Development (recommended) — installs everything
make install-dev-deps

# Static checks only (lint, type-check) — lighter, no sentence-transformers
make install-check-deps

# Production — runtime only
pip install .

# Production — with local embeddings
pip install ".[local-embedding]"

# Docker
docker build .                                                    # without local embeddings
docker build --build-arg INSTALL_EXTRAS="local-embedding" .       # with local embeddings
```

## Dual Declaration: sentence-transformers

`sentence-transformers` appears in two places in `pyproject.toml`:

1. **`[tool.poetry.dependencies]`** as `optional = true` — production users *may* install it via the `local-embedding` extra to use `LocalProvider` for offline/private embeddings.
2. **`[tool.poetry.group.dev.dependencies]`** as required — tests *must* exercise `LocalProvider`, so dev environments always have it.

This means:
- `make install-dev-deps` → always installed (tests need it)
- `pip install .` → **not** installed (production default uses cloud embeddings)
- `pip install ".[local-embedding]"` → installed (production opt-in)

## Troubleshooting

**`ModuleNotFoundError: No module named 'sentence_transformers'`**

You're using `LocalProvider` without the optional dependency:
```bash
pip install ".[local-embedding]"
```

**Tests fail with `ImportError`**

Dev dependencies missing — reinstall:
```bash
make install-dev-deps
```

## See Also

- [Development Setup](../quickstart/development.md)
- [Production Deployment](../quickstart/production.md)
- [Makefile Commands](makefile-commands.md)
