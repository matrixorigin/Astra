"""mo-memory MCP server — expose memory tools to Kiro, Cursor, Claude Code.

Two modes:
    # Local (embedded, stdio) — talks directly to DB
    python -m mo_memory_mcp

    # Remote (HTTP) — proxies to memory service API
    python -m mo_memory_mcp --api-url http://localhost:8100
"""
