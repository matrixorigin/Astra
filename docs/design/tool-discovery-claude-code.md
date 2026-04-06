# Tool discovery at scale: astra vs Claude Code (`ToolSearchTool`)

## Comparison row

| 能力 | astra | Claude Code | 对比 |
|------|----------|-------------|------|
| **ToolSearchTool**（模型主动「搜工具 / 拉 deferred schema」） | ❌ **CLI 路径未暴露**同名工具；每轮由 **selector（TF-IDF / LLM 等）在边侧先裁剪**，再把 **子集 schema** 交给模型 | ✅ 提供 **`ToolSearchTool`** 一类能力：大集合下由 **LLM 按需发现**并 **deferred 加载**工具定义 | **CC 优势**：超大规模 tool / MCP 池时，模型可自主「翻目录」，不必一次性依赖本轮静态子集 |

## What astra has today (nuance)

- **Runtime 基建（CC `ToolSearch` 的类比物）** 已存在：`rust/crates/runtime/src/tool_registry/tool_pool.rs`  
  - `ToolPool` + `SearchableToolMeta`：大索引、**按需物化**完整 JSON schema。  
  - `ToolSearchState` / `restore_state_from_messages`：跨 turn、与 compact 边界上的 **discovered tools** 恢复（见 `compact_metadata.discovered_tools`）。  
  - 两阶段选择：`select_two_phase` / `select_two_phase_with_state`（单测与 bench 覆盖，如 `runtime/tests/tool_search_quality.rs`）。
- **CLI / `stream_chat_sse` 主路径**：当前仍以 **`edge_tools::all_tool_schemas()`** + **`tool_selector`** 产出的 **本轮可用工具列表** 为主，**没有把「tool_search」做成模型可调用的 OpenAI tool**。  
  因此与 CC 的 **「模型自己调 ToolSearchTool 再扩表」** 在 **产品形态上不等价**。

## Gap / direction (if we want parity)

1. **Expose a `tool_search` (or similar) tool** to the LLM on the edge loop：query → 返回候选名 + 短描述 → 再由 pool **物化 schema** 注入下一轮（对齐 `ToolPool`）。  
2. **Persist `ToolSearchState`** in session / journal 或与 cloud compaction 元数据对齐，保证 compact 后仍记得已 discover 的工具。  
3. **MCP 大目录**：优先在两阶段路径上接 `ToolSchemaStore` 的远程拉取缓存，避免一次注册上千 schema。

## Related docs

- [context-compact.md](./context-compact.md) — 上下文与 compact 对比（与本节正交）。  
- [skills-and-tools.md](./skills-and-tools.md) — 技能与工具总览。  
- Runtime 实现：`tool_registry/tool_pool.rs`、`tool_selector.rs`。
