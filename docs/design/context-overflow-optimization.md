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

**状态**：✅ v1.4 完成

| 组件 | 文件 | 状态 |
|------|------|------|
| `MemoryType.TOOL_RESULT` | `core/memory/types.py` | ✅ |
| `process_tool_output()` | `core/agent/tool_output_handler.py` | ✅ |
| `find_similar_result()` | `core/agent/tool_output_handler.py` | ✅ |
| `compute_dynamic_threshold()` | `core/agent/tool_output_handler.py` | ✅ |
| `expand_memory_reference()` | `core/agent/tool_output_handler.py` | ✅ |
| `register_summary_strategy()` | `core/agent/tool_output_handler.py` | ✅ |
| `is_summarizable()` | `core/agent/tool_output_handler.py` | ✅ |
| `MEMORY_EXPAND_TOOL_SCHEMA` | `core/agent/tool_output_handler.py` | ✅ |
| `ContextBudgetManager` | `core/context/budget_manager.py` | ✅ |
| `TurnBudgetTracker` | `core/context/budget_manager.py` | ✅ |
| `classify_stage()` | `core/context/budget_manager.py` | ✅ |
| `tool_context_metrics.py` | `core/agent/tool_context_metrics.py` | ✅ |
| Compact memory preservation | `core/context/compaction.py` | ✅ |
| TOOL_RESULT TTL config | `core/memory/config.py` | ✅ |
| 单元测试 (50 tests) | `tests/unit/test_tool_*.py` etc | ✅ |
| 集成到 chat_loop | `core/agent/chat_loop.py` | ✅ |

### 功能清单

| 功能 | 描述 |
|------|------|
| 规则摘要 | grep/shell/git/json/file 结构化摘要，零 LLM 成本 |
| 注册制策略 | `register_summary_strategy()` 支持自定义 tool |
| 可摘要性检测 | `is_summarizable()` 区分代码文件等不可摘要内容 |
| mo-trustmem 存储 | 完整输出存 `TOOL_RESULT` 类型，session 隔离 |
| Memory 引用 | 返回 `[memory:xxx]` 引用，LLM 可展开 |
| 引用展开 | `expand_memory_reference()` 支持行范围/query/max_chars |
| 历史复用 | 相同 tool+params 复用历史结果 |
| Staleness 检查 | 默认 5 分钟过期，防止复用过时结果 |
| 动态预算 | 基于剩余 context 调整阈值（2KB~50KB） |
| 全局预算管理 | `ContextBudgetManager` 按阶段分配 |
| Turn 级累积 | `TurnBudgetTracker` 防止多 tool 累积溢出 |
| Compact 兼容 | 压缩时保留 memory 引用 |
| 失败降级 | mo-trustmem 不可用时 fallback 到截断 |
| 监控指标 | 摘要率、复用率、压缩比等 |
| Replay 确定性 | 摘要存入 metadata + version |
| TTL 管理 | TOOL_RESULT 独立 24h TTL |
| 流式累积 | `StreamingOutputAccumulator` 处理未知大小流式输出 |

### v1.5 流式输出累积机制

**文件**: `core/agent/streaming_output_handler.py`

处理 `make test`、`docker build` 等长时间运行命令的流式输出：

```python
# 策略
1. 累积输出到 buffer
2. 超过阈值（默认 10KB）时切换到 "storage mode"
3. 存储到 mo-trustmem，返回 summary + reference

# 使用
accumulator = StreamingOutputAccumulator(tool_name, session_id, user_id, memory_store)
for chunk in stream:
    accumulator.accumulate(chunk)
result = accumulator.finalize()  # 返回完整输出或 summary+ref
```

**特性**：
- 小输出（<阈值）直接返回完整内容
- 大输出自动切换存储模式
- 错误行检测并高亮
- 支持异步流处理
