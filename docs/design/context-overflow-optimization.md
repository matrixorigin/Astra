# Context Overflow 优化方案对比

## 问题背景

Agent 多轮 tool 调用后 context 爆炸，导致 `context length exceeded` 错误，对话中断。

**根因**：单次 grep 返回 100KB（~25K tokens），3 次调用即超限。

---

## 方案对比

### 已完成：基础防护

| 层级 | 措施 | 效果 | 状态 |
|------|------|------|------|
| L1 源头 | tool 输出 100KB → 30KB | 减少 70% | ✅ 已上线 |
| L2 累积 | chat_loop 截断 >50KB 结果 | 防单次爆炸 | ✅ 已上线 |
| L3 兜底 | context exceeded 自动 compact | 恢复能力 | ✅ 已上线 |

---

### 已完成：终极方案（mo-trustmem 原生上下文管理）

**状态**：✅ v1.3 完成

| 组件 | 文件 | 状态 |
|------|------|------|
| `MemoryType.TOOL_RESULT` | `core/memory/types.py` | ✅ |
| `process_tool_output()` | `core/agent/tool_output_handler.py` | ✅ |
| `find_similar_result()` | `core/agent/tool_output_handler.py` | ✅ |
| `compute_dynamic_threshold()` | `core/agent/tool_output_handler.py` | ✅ |
| `expand_memory_reference()` | `core/agent/tool_output_handler.py` | ✅ |
| `register_summary_strategy()` | `core/agent/tool_output_handler.py` | ✅ |
| `MEMORY_EXPAND_TOOL_SCHEMA` | `core/agent/tool_output_handler.py` | ✅ |
| Staleness check (max_age) | `core/agent/tool_output_handler.py` | ✅ |
| 单元测试 (26 tests) | `tests/unit/test_tool_output_handler.py` | ✅ |
| 集成到 chat_loop | `core/agent/chat_loop.py` | ✅ |

### 功能清单

| 功能 | 描述 |
|------|------|
| 规则摘要 | grep/shell/git/json/file 结构化摘要，零 LLM 成本 |
| 注册制策略 | `register_summary_strategy()` 支持自定义 tool |
| mo-trustmem 存储 | 完整输出存 `TOOL_RESULT` 类型，session 隔离 |
| Memory 引用 | 返回 `[memory:xxx]` 引用，LLM 可展开 |
| 引用展开 | `expand_memory_reference()` 支持行范围/query 过滤 |
| 历史复用 | 相同 tool+params 复用历史结果 |
| Staleness 检查 | 默认 5 分钟过期，防止复用过时结果 |
| 动态预算 | 基于剩余 context 调整阈值（2KB~50KB） |
