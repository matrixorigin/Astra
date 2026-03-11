# TrustMem Lite

Persistent memory for AI coding tools — local single-user mode.

Works with **Kiro**, **Cursor**, and **Claude Code**. Stores memories in [MatrixOne](https://github.com/matrixorigin/matrixone) with vector search for semantic retrieval.

## Quick Start

```bash
# Install
pip install 'trust-mem-lite'

# For local embeddings (sentence-transformers, +80MB model download on first use)
pip install 'trust-mem-lite[local-embedding]'

# For OpenAI-compatible embeddings (OpenAI, SiliconFlow, Ollama, etc.)
pip install 'trust-mem-lite[openai-embedding]'

# Install from TestPyPI (pre-release testing)
pip install --index-url https://pypi.org/simple/ --extra-index-url https://test.pypi.org/simple/ 'trust-mem-lite[local-embedding]'

# Initialize — writes MCP config and steering rules (no DB connection needed)
trustmem init --db-url 'mysql+pymysql://root:111@localhost:6001/trustmem'

# Or with default local MatrixOne (localhost:6001, database: trustmem)
trustmem init

# Restart your AI tool — done!
# Database tables are created automatically when the MCP server starts.
```

## What It Does

After `trustmem init`, your AI tool will:
- **Remember** facts, preferences, and decisions across conversations
- **Retrieve** relevant memories at the start of each conversation
- **Correct** memories when you tell it something changed
- **Forget** memories on request

## Requirements

- Python 3.11+
- [MatrixOne](https://github.com/matrixorigin/matrixone) database (local or remote)

## Commands

```bash
trustmem init                          # Auto-detect tools, write MCP config + steering rules
trustmem init --tool kiro              # Configure Kiro only
trustmem init --tool kiro --tool cursor  # Configure specific tools
trustmem init --force                  # Overwrite steering rules even if user-customized
trustmem status                        # Show configuration and rule versions
trustmem update-rules                  # Update steering rules to latest version
trustmem migrate                       # Create/update database tables manually
trustmem health                        # Check remote memory service health
trustmem governance                    # Run memory cleanup and maintenance
```

### Steering Rule Protection

`trustmem init` protects rules you've customized:
- **No changes** → skipped (up to date)
- **User-modified** → skipped, use `--force` to overwrite
- **Version upgrade** → auto-updated, original saved as `.bak`
- **`--force`** → always overwrites, original saved as `.bak`

## Embedding Options

Configure embedding **before** your AI tool starts for the first time — the MCP server
creates tables using the configured dimension, so there's no mismatch.

```bash
# Local (default) — free, private, ~80MB model download on first use
trustmem init --embedding-provider local

# OpenAI
trustmem init --embedding-provider openai --embedding-api-key sk-...

# SiliconFlow (recommended for China users)
trustmem init --embedding-provider openai \
  --embedding-model BAAI/bge-m3 \
  --embedding-dim 1024 \
  --embedding-api-key sk-... \
  --embedding-base-url https://api.siliconflow.cn/v1

# Any OpenAI-compatible endpoint (Ollama, Azure, etc.)
trustmem init --embedding-provider openai \
  --embedding-base-url http://localhost:11434/v1 \
  --embedding-model nomic-embed-text \
  --embedding-dim 768
```

> **Note**: `--embedding-dim` must match the model's actual output dimension.
> Common values: `all-MiniLM-L6-v2`=384, `BAAI/bge-m3`=1024, `text-embedding-ada-002`=1536.

## Switching Embedding Provider

If you want to switch providers after tables already exist, run `migrate --force` to
ALTER the embedding column (this clears existing embeddings — memories are kept but
will need to be re-embedded manually via `trustmem governance`):

```bash
trustmem migrate --dim 1536 --force
```
