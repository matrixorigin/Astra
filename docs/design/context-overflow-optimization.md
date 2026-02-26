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

**状态**：✅ v1.1 完成

| 组件 | 文件 | 状态 |
|------|------|------|
| `MemoryType.TOOL_RESULT` | `core/memory/types.py` | ✅ |
| `process_tool_output()` | `core/agent/tool_output_handler.py` | ✅ |
| `find_similar_result()` | `core/agent/tool_output_handler.py` | ✅ |
| `SUMMARY_GENERATORS` | `core/agent/tool_output_handler.py` | ✅ |
| 单元测试 (13 tests) | `tests/unit/test_tool_output_handler.py` | ✅ |
| 集成到 chat_loop | `core/agent/chat_loop.py` | ✅ |
| 历史结果复用 | `core/agent/chat_loop.py` | ✅ |
