# Tool discovery at scale: astra vs Claude Code (`ToolSearchTool`)

## Comparison row

| 能力 | astra | Claude Code | 对比 |
|------|----------|-------------|------|
| **ToolSearchTool**（模型主动「搜工具 / 拉 deferred schema」） | ✅ `tool_search` 是 tool surface 的激活入口；可见工具面保持小集合，deferred 目录通过短描述暴露，模型按需 `select:NAME` 激活下一轮 schema | ✅ 提供 **`ToolSearchTool`** 一类能力：大集合下由 **LLM 按需发现**并 **deferred 加载**工具定义 | 两者方向一致；差异主要在目录来源、MCP 远程拉取缓存、以及跨 compact 状态恢复粒度 |

## What astra has today (nuance)

- **Runtime 基建**：`ToolRegistry` 构建一个小而稳定的可见 tool surface；默认 always-load 工具直接进入 `tools[]`，deferred 工具只通过 `<deferred_tools>` 目录暴露短描述。
- **激活入口**：`tool_search` 是唯一的工具发现/激活接口。模型主动请求 `select:NAME` 后，下一轮才物化对应完整 schema。
- **CLI / `stream_chat_sse` 主路径**：最终 payload 只携带当前可见 tool surface。runtime 只负责过滤、注入必需工具、应用 activation 状态；不再维护主动 ranking 或两阶段选择路径。

## Gap / direction (if we want parity)

1. **持久化 activation state** 到 session / journal 或与 cloud compaction 元数据对齐，保证 compact 后仍记得已激活的 deferred 工具。
2. **MCP 大目录**：优先让 `tool_search` 目录按 provider 分页/缓存，避免一次注册上千完整 schema。

## Related docs

- [context-compact.md](./context-compact.md) — 上下文与 compact 对比（与本节正交）。  
- [skills-and-tools.md](./skills-and-tools.md) — 技能与工具总览。  
- Runtime 实现：`tool_registry/registry.rs`、`tool_registry/surface.rs`、`tool_registry/declaration.rs`。
