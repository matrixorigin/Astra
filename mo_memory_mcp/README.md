# TrustMem Lite

Persistent memory for AI coding tools — local single-user mode.

Works with **Kiro**, **Cursor**, and **Claude Code**. Stores memories in [MatrixOne](https://github.com/matrixorigin/matrixone) with vector search for semantic retrieval.

## Quick Start

```bash
# Install
pip install trust-mem-lite

# For local embeddings (recommended, +80MB model download on first use)
pip install trust-mem-lite[local-embedding]

# Initialize (creates database, tables, MCP config, steering rules)
trustmem init --db-url 'mysql+pymysql://root:111@localhost:6001/trustmem'

# Or with default local MatrixOne (localhost:6001, database: trustmem)
trustmem init

# Restart your AI tool — done!
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
trustmem init           # Configure everything (tables + MCP + rules)
trustmem status         # Show configuration and rule versions
trustmem update-rules   # Update steering rules to latest version
trustmem migrate        # Create/update database tables only
trustmem health         # Check remote memory service health
trustmem governance     # Run memory cleanup and maintenance
```

## Embedding Options

```bash
# Local (default) — free, private, ~80MB model
trustmem init --embedding-provider local

# OpenAI — better quality, requires API key
trustmem init --embedding-provider openai --embedding-api-key sk-...

# Custom endpoint (Ollama, etc.)
trustmem init --embedding-provider openai --embedding-base-url http://localhost:11434/v1
```
