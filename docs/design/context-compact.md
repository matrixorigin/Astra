# Context management & compaction (astra vs Claude Code)

This document compares **interactive context control** with Claude Code (CC) and records **astra** behavior after the session-memory anchor and post-compact hygiene work.

## Capability matrix

| 能力 | astra | Claude Code | 对比 |
|------|----------|-------------|------|
| Auto compact | ✅ 多级：`ContextBudget`（Normal → TrimSchemas → CompactHistory → AggressivePrune 等由 `budget_pressure` / tier 驱动） | ✅ 基于 token 预算 + circuit breaker | 都有；astra 分级更细 |
| LLM summary compact | ✅ `/compact` 与自动 compact：`COMPACT_SUMMARY_REQUEST`；Memoria `NS_EPISODE` / facts 抽取；自动路径写入 `SRC_AUTO_COMPACT` | ✅ LLM 摘要替换历史 | ✅ 对齐 |
| Micro compact | ✅ tool 结果截断 + preview stub（见 runtime / edge 注释中的 micro-compact 思路） | ✅ 清理旧 tool result，占位符 | ✅ 对齐 |
| Time-based compact | ✅ `TimeBasedCompactConfig`（离开再回来可清理旧 tool 结果） | ❌ 无独立时间触发 | astra 优势 |
| Tool schema pruning | ✅ `prune_tool_schemas` 按 compaction tier 裁剪 | ❌ 无 | astra 优势 |
| Session memory compact | ✅ **Session memory anchor**：compact 后通过 Memoria `search` 拉取少量与 **session id + 摘要种子** 相关的条目，注入到 `[Session memory anchor]` 块，再跟 `[Prior context — …]`（见 `repl_turn::fetch_compact_memory_anchor_snippet`） | ✅ compact 时保留 session memory 作锚点 | ✅ 对齐（实现路径：向量检索 + 会话 id 偏置 query） |
| Prompt cache 感知 | ✅ `estimate_tokens_cache_aware` 等（runtime） | ✅ `promptCacheBreakDetection` | 都有；CC 对「破坏 cache」的 API 通知更主动 |
| Post-compact cleanup | ✅ **compact 后 `recent_tools.clear()`**，避免已截断历史上残留的 tool 名误导下一轮选择；**plan 模式**下提示刷新 `/plan`（见 `slash_state` / `repl_turn`） | ✅ 清理 file state cache、重载 plan | astra：**每轮新建 `ToolExecutor`**，文件 `file_state` / URL cache **不跨 turn 存活**（与 CC 全局 cache 不同）；跨 turn 的清理重点是 `recent_tools` + 用户提示 |

## Implementation pointers

- **Session memory anchor**: `rust/crates/astra-cli/src/cli/repl_turn.rs` — `fetch_compact_memory_anchor_snippet`, `compact_assistant_message`；`/compact` 与自动 compact 共用。
- **Post-compact**: `recent_tools.clear()` 在 `apply_auto_compact_result` 与 `slash_state` `/compact` 成功替换 history 之后执行。
- **Plan tip**: `plan_mode` 或 `executing_plan` 为真时，compact 成功后打印一行 dim 提示。
- **Cloud pull journal**（审计，与 compact 正交）：`docs/design/multi-agent-cloud-runtime.md` — `sync_marker` / `metadata.cloud_pull`。

## Environment (related)

- **`ASTRA_JOURNAL_CLOUD_EMPTY_ACK=1`**: REPL 启动时若 Matrix 可达但 pull 无数据，仍写 `sync_marker`（默认不写以免噪音）。详见 `multi-agent-cloud-runtime.md`。

## Related: large tool catalogs

- Claude Code **`ToolSearchTool`** vs astra（runtime **`ToolPool`** / 两阶段 vs **CLI 未暴露搜索工具**）：见 [tool-discovery-claude-code.md](./tool-discovery-claude-code.md)。
