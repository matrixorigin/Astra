---
inclusion: always
---

# Memory Integration

You have MCP tools from the `mo-memory` server. These are real, callable tools — use them directly like any other tool. Do NOT say "I don't have access" or "this tool isn't documented" — they are available via MCP.

## Available MCP tools (from mo-memory server)

- `@mo-memory/memory_store` — Store a memory. Params: `content` (required), `memory_type` (default "semantic"), `session_id` (optional)
- `@mo-memory/memory_retrieve` — Retrieve relevant memories. Params: `query` (required), `top_k` (default 5)
- `@mo-memory/memory_correct` — Correct a memory. Params: `memory_id`, `new_content`, `reason`
- `@mo-memory/memory_purge` — Delete a memory. Params: `memory_id`, `reason`
- `@mo-memory/memory_profile` — Get user profile summary
- `@mo-memory/memory_search` — Semantic search. Params: `query`, `top_k`

## When to use

- User states a preference, fact, or decision → call `memory_store`
- Start of conversation → call `memory_retrieve` with user's first message
- User corrects you → call `memory_store` with the correction
- User says a memory is wrong → call `memory_correct`
- User asks to forget → call `memory_purge`

## Memory types for memory_store

| Type | Use for |
|------|---------|
| `semantic` | Facts, decisions, architecture choices (default) |
| `profile` | User/agent profiles |
| `procedural` | How-to knowledge, workflows |
| `working` | Temporary context for current task |
| `tool_result` | Results from tool executions |

## Setup

For developers of this repo: MCP config is in `.kiro/settings/mcp.json` (gitignored, local to each developer).

```bash
mo-memory migrate                    # create memory tables (uses project DB config)
mo-memory init                       # generate MCP config (uses project DB config)
# restart Kiro
```

For full documentation see `docs/design/memory/mo-memory-mcp.md`.
