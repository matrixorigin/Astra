# mo-memory — Shared Memory for AI Coding Tools

mo-memory gives your AI coding tools (Kiro, Cursor, Claude Code) persistent memory
backed by MatrixOne. Facts, preferences, and decisions survive across sessions and
are shared between tools.

## Quick Start

```bash
pip install mo-memory

# Create memory tables
mo-memory migrate --db-url 'mysql+pymysql://root:111@localhost:6001/my_db'

# Configure for your project (auto-detects Kiro / Cursor / Claude Code)
cd your-project
mo-memory init --db-url 'mysql+pymysql://root:111@localhost:6001/my_db'

# Restart your IDE — done!
```

## What `mo-memory init` Does

1. **Detects AI tools** — scans for `.kiro/`, `.cursor/`, `CLAUDE.md`
2. **Writes MCP config** — registers the mo-memory MCP server with your IDE
3. **Writes steering rules** — tells the AI when to store/retrieve/correct memories

Does NOT create database tables — run `mo-memory migrate` first.

## Configuration

### Database (required)

```bash
mo-memory init --db-url 'mysql+pymysql://user:pass@host:6001/database'
```

### Embedding (optional)

By default, mo-memory uses a local embedding model (`all-MiniLM-L6-v2`, 384 dimensions).
No API key needed.

To use OpenAI embeddings:

```bash
mo-memory init \
  --db-url 'mysql+pymysql://user:pass@host:6001/db' \
  --embedding-provider openai \
  --embedding-api-key sk-... \
  --embedding-model text-embedding-3-small \
  --embedding-dim 1536
```

To use a local OpenAI-compatible API (Ollama, vLLM, etc.):

```bash
mo-memory init \
  --db-url 'mysql+pymysql://user:pass@host:6001/db' \
  --embedding-provider openai \
  --embedding-base-url http://localhost:11434/v1 \
  --embedding-model nomic-embed-text \
  --embedding-dim 768
```

### All Options

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--db-url` | `MO_MEMORY_DB_URL` | — | Database connection URL (required) |
| `--embedding-provider` | `MO_MEMORY_EMBEDDING_PROVIDER` | `local` | `local`, `openai`, or `mock` |
| `--embedding-model` | `MO_MEMORY_EMBEDDING_MODEL` | `all-MiniLM-L6-v2` | Model name |
| `--embedding-dim` | `MO_MEMORY_EMBEDDING_DIM` | `384` | Vector dimension |
| `--embedding-api-key` | `MO_MEMORY_EMBEDDING_API_KEY` | — | API key (openai provider) |
| `--embedding-base-url` | `MO_MEMORY_EMBEDDING_BASE_URL` | — | Custom API endpoint |
| `--mode` | — | `stdio` | `stdio` (local) or `remote` (HTTP) |

## How It Works

```
IDE (Kiro/Cursor/Claude Code)
  │
  │ stdio (JSON-RPC)
  ▼
mo-memory MCP server
  │
  ├── memory_store     → vectorize + persist
  ├── memory_retrieve  → semantic search
  ├── memory_correct   → update with audit trail
  ├── memory_purge     → delete with reason
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
| `memory_store` | Store a memory | `content`, `memory_type` (default: semantic) |
| `memory_retrieve` | Recall relevant memories | `query`, `top_k` (default: 5) |
| `memory_correct` | Fix an existing memory | `memory_id`, `new_content`, `reason` |
| `memory_purge` | Delete a memory | `memory_id`, `reason` |
| `memory_profile` | Get user profile summary | — |
| `memory_search` | Semantic search | `query`, `top_k` (default: 10) |

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

Files written by `mo-memory init`:
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
mo-memory init        # Configure MCP + steering rules
mo-memory migrate     # Create memory tables in the database
mo-memory status      # Show which tools are configured
mo-memory health      # Check memory service health (remote mode)
mo-memory governance  # Run governance cycle (quarantine, cleanup, IVF rebuild)
mo-memory consolidate --user-id <uid>  # Run graph consolidation
mo-memory reflect     --user-id <uid>  # Run reflection (requires LLM)
```

### migrate

Creates only the memory tables (`mem_memories`, `mem_edit_log`, `mem_experiments`,
`mem_user_memory_config`, `memory_graph_nodes`, `memory_graph_edges`).
Safe to run multiple times (`checkfirst=True`).

```bash
# Release: explicit DB URL
mo-memory migrate --db-url 'mysql+pymysql://user:pass@host:6001/db'

# Dev: auto-reads project .env config
mo-memory migrate
```

Priority: `--db-url` > `MO_MEMORY_DB_URL` env var > project `config/settings.py`.

## Remote Mode

For shared/team deployments, run the memory service as an HTTP API:

```bash
uvicorn api.memory_app:memory_app --port 8100

# Configure clients to use remote mode
mo-memory init --mode remote
```

## Troubleshooting

**"Table mem_memories doesn't exist"**
- Run `mo-memory migrate --db-url '...'` to create tables
- Dev mode: just `mo-memory migrate` (reads project config)

**MCP server won't connect**
- Check IDE logs for error messages
- Verify database is reachable: `mysql -h host -P 6001 -u user -p`
- Test manually: `MO_MEMORY_DB_URL='...' python -m mo_memory_mcp`

**Memories not vectorized**
- Check embedding provider is configured correctly
- For local provider: `pip install sentence-transformers`
- For openai provider: verify API key is set

**IDE doesn't see the tools**
- Restart the IDE after running `mo-memory init`
- Check that `.kiro/settings/mcp.json` (or equivalent) exists and is valid JSON

**"No such file or directory" on MCP start**
- The `command` in MCP config must be an absolute path to python
- `mo-memory init` uses `sys.executable` — re-run init from the correct conda/venv
