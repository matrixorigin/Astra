# Memoria Lite — Shared Memory for AI Coding Tools

Memoria Lite gives your AI coding tools (Kiro, Cursor, Claude Code) persistent memory
backed by MatrixOne. Facts, preferences, and decisions survive across sessions and
are shared between tools. This is the local single-user edition.

## Quick Start

```bash
pip install memoria-lite

# Create memory tables
memoria migrate --db-url 'mysql+pymysql://root:111@localhost:6001/my_db'

# Configure for your project (auto-detects Kiro / Cursor / Claude Code)
cd your-project
memoria init --db-url 'mysql+pymysql://root:111@localhost:6001/my_db'

# Restart your IDE — done!
```

## What `memoria init` Does

1. **Detects AI tools** — scans for `.kiro/`, `.cursor/`, `CLAUDE.md`
2. **Writes MCP config** — registers the Memoria Lite MCP server with your IDE (always updated)
3. **Writes steering rules** — tells the AI when to store/retrieve/correct memories (protected, see below)

Does NOT create database tables — run `memoria migrate` first.

### Steering Rule Protection

`memoria init` protects steering rules you've customized:

| Situation | Behavior |
|-----------|----------|
| File doesn't exist | Created |
| Same version, no changes | Skipped (up to date) |
| Same version, user-modified | Skipped — use `--force` to overwrite |
| Older version | Auto-updated, `.bak` backup saved first |
| `--force` flag | Always overwrites, `.bak` backup saved first |

### Selecting Tools

By default, `memoria init` auto-detects installed tools. Use `--tool` to configure specific tools only:

```bash
memoria init --tool kiro                    # Kiro only
memoria init --tool cursor                  # Cursor only
memoria init --tool kiro --tool cursor      # Both
```

If no tools are detected and `--tool` is not specified, init will prompt you to use `--tool`.

## Configuration

### Database (required)

```bash
memoria init --db-url 'mysql+pymysql://user:pass@host:6001/database'
```

### Embedding (optional)

By default, Memoria Lite uses a local embedding model (`all-MiniLM-L6-v2`, 384 dimensions).
No API key needed.

To use OpenAI embeddings:

```bash
memoria init \
  --db-url 'mysql+pymysql://user:pass@host:6001/db' \
  --embedding-provider openai \
  --embedding-api-key sk-... \
  --embedding-model text-embedding-3-small \
  --embedding-dim 1536
```

To use a local OpenAI-compatible API (Ollama, vLLM, etc.):

```bash
memoria init \
  --db-url 'mysql+pymysql://user:pass@host:6001/db' \
  --embedding-provider openai \
  --embedding-base-url http://localhost:11434/v1 \
  --embedding-model nomic-embed-text \
  --embedding-dim 768
```

### All Options

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--db-url` | `MEMORIA_DB_URL` | — | Database connection URL (required) |
| `--embedding-provider` | `EMBEDDING_PROVIDER` | `local` | `local`, `openai`, or `mock` |
| `--embedding-model` | `EMBEDDING_MODEL` | `all-MiniLM-L6-v2` | Model name |
| `--embedding-dim` | `EMBEDDING_DIM` | `384` | Vector dimension |
| `--embedding-api-key` | `EMBEDDING_API_KEY` | — | API key (openai provider) |
| `--embedding-base-url` | `EMBEDDING_BASE_URL` | — | Custom API endpoint |
| `--mode` | — | `stdio` | `stdio` (local) or `remote` (HTTP) |

## How It Works

```
IDE (Kiro/Cursor/Claude Code)
  │
  │ stdio (JSON-RPC)
  ▼
Memoria Lite MCP server
  │
  ├── memory_store     → vectorize + persist
  ├── memory_retrieve  → semantic search
  ├── memory_correct   → update with audit trail
  ├── memory_purge     → delete by ID or topic
  ├── memory_profile   → user profile summary
  └── memory_search    → full semantic search
  │
  ▼
MatrixOne (vector search + fulltext + HTAP)
```

The MCP server runs as a child process of your IDE. In `stdio` mode it connects
directly to the database — no separate service to manage.

## MCP Tools

### CRUD Tools

| Tool | Description | Key Parameters |
|------|-------------|----------------|
| `memory_store` | Store a memory. Returns `warning` field if embedding client is unavailable (memory stored without vector, retrieval falls back to keyword search) | `content`, `memory_type` (default: semantic) |
| `memory_retrieve` | Recall relevant memories; uses vector search if embedding available, falls back to keyword search otherwise. Includes ⚠️ health warnings if issues detected | `query`, `top_k` (default: 5) |
| `memory_correct` | Fix an existing memory. Returns `warning` field if embedding client is unavailable | `memory_id`, `new_content`, `reason` |
| `memory_purge` | Delete by ID or bulk-delete by topic | `memory_id` or `topic`, `reason` |
| `memory_profile` | Get user profile summary | — |
| `memory_search` | Semantic search; falls back to keyword search if embedding unavailable | `query`, `top_k` (default: 10) |

### Maintenance Tools

These tools are **expensive** and have cooldowns. Do not call proactively — only when user explicitly requests.

| Tool | Description | Cooldown | Key Parameters |
|------|-------------|----------|----------------|
| `memory_governance` | Quarantine low-confidence memories, clean stale data, auto-rebuild unhealthy IVF indexes | 1 hour | `user_id`, `force` |
| `memory_consolidate` | Detect contradicting memories, fix orphaned graph nodes, manage trust tiers | 30 min | `user_id`, `force` |
| `memory_reflect` | Analyze memory clusters and synthesize insights (scene nodes). Requires LLM | 2 hours | `user_id`, `force` |
| `memory_rebuild_index` | Manually rebuild IVF vector index with optimal centroid count | — | `table` |

**`memory_governance` auto-rebuilds** unhealthy IVF indexes during its run. Use `memory_rebuild_index` only for manual forced rebuild.

**Trigger phrases:**
- governance: "clean up memories", "run maintenance", "check memory health"
- consolidate: "check for conflicts", "consolidate memories"
- reflect: "reflect on memories", "find patterns", "summarize what you know"
- rebuild_index: "rebuild vector index" (usually not needed — governance handles it)

### Memory Types

| Type | Use For |
|------|---------|
| `profile` | User/agent profiles |
| `semantic` | Facts, decisions, architecture choices (default) |
| `procedural` | How-to knowledge, workflows |
| `working` | Temporary context for current task |
| `tool_result` | Results from tool executions |

## IDE-Specific Details

### Kiro

Files written by `memoria init`:
- `.kiro/settings/mcp.json` — MCP server config (gitignore this)
- `.kiro/steering/memory.md` — steering rule (commit this)

### Cursor

Files written:
- `.cursor/mcp.json` — MCP server config (gitignore this)
- `.cursor/rules/memory.mdc` — rule file (commit this)

### Claude Code

Files written:
- `.claude/mcp.json` — MCP server config (gitignore this)
- `CLAUDE.md` — appended with memory instructions (commit this)

## CLI Commands

```bash
memoria init        # Configure MCP + steering rules (auto-detects tools)
memoria init --tool kiro              # Configure Kiro only
memoria init --tool kiro --tool cursor  # Configure specific tools
memoria init --force                  # Overwrite steering rules even if user-customized
memoria migrate     # Create memory tables in the database
memoria status      # Show which tools are configured
memoria update-rules  # Update steering rules to latest version
memoria health      # Check memory service health (remote mode)
memoria governance  # Run governance cycle (quarantine, cleanup, IVF rebuild)
memoria consolidate --user-id <uid>  # Run graph consolidation
memoria reflect     --user-id <uid>  # Run reflection (requires LLM)
```

### migrate

Creates only the memory tables (`mem_memories`, `mem_edit_log`, `mem_experiments`,
`mem_user_memory_config`, `memory_graph_nodes`, `memory_graph_edges`).
Safe to run multiple times (`checkfirst=True`).

```bash
# Release: explicit DB URL
memoria migrate --db-url 'mysql+pymysql://user:pass@host:6001/db'

# Dev: auto-reads project .env config
memoria migrate
```

Priority: `--db-url` > `MEMORIA_DB_URL` env var > project `config/settings.py`.

## Remote Mode

For shared/team deployments, run the memory service as an HTTP API:

```bash
uvicorn api.memory_app:memory_app --port 8100

# Configure clients to use remote mode
memoria init --mode remote
```

## Troubleshooting

**"Table mem_memories doesn't exist"**
- Run `memoria migrate --db-url '...'` to create tables
- Dev mode: just `memoria migrate` (reads project config)

**MCP server won't connect**
- Check IDE logs for error messages
- Verify database is reachable: `mysql -h host -P 6001 -u user -p`
- Test manually: `MEMORIA_DB_URL='...' python -m mo_memory_mcp`

**Memories not vectorized**
- Check embedding provider is configured correctly
- For local provider: `pip install sentence-transformers`
- For openai provider: verify API key is set and `openai` package is installed (`pip install openai`)
- If `memory_store` returns a `warning` field, the embedding client failed to initialize — check the above
- Ensure all dependencies are installed: `pip install -e .` or `poetry install` in the project root

**IDE doesn't see the tools**
- Restart the IDE after running `memoria init`
- Check that `.kiro/settings/mcp.json` (or equivalent) exists and is valid JSON

**"No such file or directory" on MCP start**
- The `command` in MCP config must be an absolute path to python
- `memoria init` uses `sys.executable` — re-run init from the correct conda/venv
