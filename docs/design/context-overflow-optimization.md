# Context Overflow 优化方案对比

## 问题背景

Agent 多轮 tool 调用后 context 爆炸，导致 `context length exceeded` 错误，对话中断。

**根因**：单次 grep 返回 100KB（~25K tokens），3 次调用即超限。

---

## 方案对比

### 已完成：基础防护（当前状态）

| 层级 | 措施 | 效果 |
|------|------|------|
| L1 源头 | tool 输出 100KB → 30KB | 减少 70% |
| L2 累积 | chat_loop 截断 >50KB 结果 | 防单次爆炸 |
| L3 兜底 | context exceeded 自动 compact | 恢复能力 |

**优点**：简单、零延迟、已上线
**缺点**：硬截断丢失信息，规则固定不智能

---

### 准备优化：智能摘要 + mo-trustmem 集成

| 维度 | 当前 | 准备优化 |
|------|------|----------|
| 压缩方式 | 硬截断 30KB | LLM 摘要（仅 >10KB） |
| 存储 | 丢弃 | 原始输出存 mo-trustmem |
| 返回格式 | 截断文本 | 摘要 + `[memory:xxx]` |
| 信息保留 | ~30% | ~95% |

#### 实现设计

```python
# core/agent/tool_output_handler.py

from core.memory.store import MemoryStore
from core.memory.types import MemoryType

SUMMARY_THRESHOLD = 10 * 1024  # 10KB

async def process_tool_output(
    output: str, 
    tool_name: str,
    session_id: str,
    user_id: str,
    memory_store: MemoryStore,
    llm: LLMClient,
) -> str:
    """处理 tool 输出：小输出直接返回，大输出摘要+存 mo-trustmem"""
    
    if len(output) <= SUMMARY_THRESHOLD:
        return output
    
    # 1. 存入 mo-trustmem（完整保留，working 类型，session 隔离）
    memory = memory_store.create(
        user_id=user_id,
        content=output,
        memory_type=MemoryType.WORKING,  # 短期工作记忆
        session_id=session_id,           # session 隔离
        source=f"tool:{tool_name}",
        metadata={"tool": tool_name, "size": len(output)},
    )
    
    # 2. LLM 生成摘要
    summary = await llm.chat([{
        "role": "user",
        "content": f"Summarize this {tool_name} output in <500 chars. Keep file paths, counts, key matches:\n{output[:8000]}"
    }])
    
    # 3. 返回摘要 + 引用
    return f"{summary}\n\n[Full output ({len(output)} bytes): memory:{memory.memory_id}]"
```

#### 与 mo-trustmem 的协同

| mo-trustmem 能力 | 在本方案中的应用 |
|------------------|------------------|
| `MemoryType.WORKING` | tool 结果作为短期工作记忆 |
| `session_id` 隔离 | 不同 session 的 tool 结果互不干扰 |
| Hybrid Retrieval | agent 可通过语义搜索找回历史结果 |
| GovernanceScheduler | 自动清理过期的 tool 结果（confidence decay） |
| PITR | 支持时间旅行查询历史 tool 结果 |

#### 效果预估

| 指标 | 当前 | 准备优化 |
|------|------|----------|
| 单次 tool 输出 | 30KB | ~2KB（摘要） |
| 3 次 grep 累积 | 90KB | ~6KB |
| 信息保留率 | ~30% | ~95% |
| 额外延迟 | 0ms | ~100ms（仅大输出） |
| 额外成本 | $0 | ~$0.001/次（仅大输出） |

#### 工作量

- 开发：3-5 天
- 测试：2 天
- 风险：低（渐进式，可回滚）

---

### 完美优化：全链路智能 Context 管理

| 维度 | 准备优化 | 完美优化 |
|------|----------|----------|
| 摘要触发 | 固定阈值 10KB | 动态阈值（基于剩余 context） |
| 预算分配 | 无 | 按对话阶段动态分配 |
| mo-trustmem 集成 | 存储 + 引用 | 自动检索历史相关结果 |
| 模型选择 | 主 LLM 摘要 | 专用 TinyLLM（更快更便宜） |

#### 架构设计

```
┌─────────────────────────────────────────────────────────────┐
│                    Context Budget Manager                    │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │ Stage       │  │ Budget      │  │ mo-trustmem         │  │
│  │ Classifier  │→ │ Allocator   │→ │ Reference Resolver  │  │
│  │ (规则/ML)   │  │ (动态分配)  │  │ (自动展开/折叠)     │  │
│  └─────────────┘  └─────────────┘  └─────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│                    Tool Output Pipeline                      │
├─────────────────────────────────────────────────────────────┤
│  Tool Result → Size Check → [>threshold] → TinyLLM Summary  │
│                    ↓                              ↓          │
│              [<threshold]                   Store mo-trustmem│
│                    ↓                              ↓          │
│              Direct Return              Return Summary+Ref   │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│                    mo-trustmem Integration                   │
├─────────────────────────────────────────────────────────────┤
│  • Hybrid Retrieval：语义+关键词搜索历史 tool 结果          │
│  • 去重：相同 grep pattern 复用历史结果（via embedding）     │
│  • PITR：支持 "上次 grep 的结果" 类时间旅行查询             │
│  • Governance：自动清理低 confidence 的过期结果             │
└─────────────────────────────────────────────────────────────┘
```

#### 核心组件

```python
# 1. 动态预算分配
class ContextBudgetManager:
    def allocate(self, stage: str, remaining_tokens: int) -> dict:
        """根据阶段和剩余空间动态分配预算"""
        if stage == "query":
            return {"tool_output": min(2000, remaining_tokens * 0.3)}
        elif stage == "analysis":
            return {"tool_output": min(8000, remaining_tokens * 0.6)}
        else:
            return {"tool_output": min(4000, remaining_tokens * 0.4)}

# 2. 专用摘要模型
class TinySummarizer:
    """轻量级摘要模型，延迟 <30ms"""
    def __init__(self):
        self.model = load_model("tinyllm-summarizer-3M")
    
    async def summarize(self, text: str, max_tokens: int = 200) -> str:
        return await self.model.generate(
            f"Summarize:\n{text[:4000]}",
            max_tokens=max_tokens,
        )

# 3. mo-trustmem 自动检索（复用现有 Retriever）
from core.memory.retriever import MemoryRetriever

class ToolResultMemory:
    def __init__(self, retriever: MemoryRetriever):
        self.retriever = retriever
    
    async def find_similar(
        self, 
        tool_name: str, 
        params: dict,
        session_id: str,
        user_id: str,
    ) -> str | None:
        """查找相似的历史 tool 结果（利用 mo-trustmem 的 hybrid retrieval）"""
        results = self.retriever.retrieve(
            user_id=user_id,
            query=f"{tool_name} {json.dumps(params)}",
            session_id=session_id,
            memory_types=[MemoryType.WORKING],
            limit=1,
        )
        # 检查相似度（embedding 距离）
        if results and results[0].metadata.get("tool") == tool_name:
            return f"[Reusing previous result: memory:{results[0].memory_id}]"
        return None
```

#### 与 mo-trustmem 深度集成

| 完美优化功能 | mo-trustmem 支撑能力 |
|--------------|---------------------|
| 历史结果复用 | `MemoryRetriever.retrieve()` + embedding 相似度 |
| 时间旅行查询 | PITR snapshot + `SELECT * FROM memories {timestamp}` |
| 自动过期清理 | `GovernanceScheduler.run_cycle()` confidence decay |
| Session 隔离 | `session_id` 字段 + retriever 过滤 |
| 跨 session 共享 | `MemoryType.SEMANTIC` 提升（via TypedReflector） |

#### 效果预估

| 指标 | 准备优化 | 完美优化 |
|------|----------|----------|
| 单次 tool 输出 | ~2KB | ~1KB（动态压缩） |
| 重复查询 | 重新执行 | 复用历史（0 tokens） |
| 摘要延迟 | ~100ms | ~30ms（TinyLLM） |
| 摘要成本 | ~$0.001 | ~$0.0001（本地模型） |
| Context 利用率 | 固定 | 动态最优 |

#### 工作量

- 开发：2-3 周
- 测试：1 周
- 依赖：TinyLLM 部署（可选）、mo-trustmem 已就绪 ✅
- 风险：中（组件多，需充分测试）

---

## 推荐路径

```
Week 1-2: 准备优化
  ├── 实现 tool_output_handler.py
  ├── 集成 mo-trustmem 存储（MemoryType.WORKING）
  └── 测试 + 上线

Week 3-4: 完美优化（可选）
  ├── 部署 TinyLLM（或继续用主 LLM）
  ├── 实现动态预算
  └── mo-trustmem 自动检索（复用 MemoryRetriever）
```

## 决策建议

| 场景 | 推荐方案 |
|------|----------|
| 快速止血，资源有限 | **准备优化**（1周上线） |
| 追求极致，长期投入 | **完美优化**（1月完成） |
| 折中方案 | 准备优化上线 → 逐步迭代到完美优化 |

## 与 mo-trustmem 的协同总结

本优化方案充分利用 mo-trustmem 已有能力：

| mo-trustmem 能力 | 准备优化 | 完美优化 |
|------------------|----------|----------|
| MemoryStore.create() | ✅ 存储 tool 结果 | ✅ |
| MemoryType.WORKING | ✅ 短期工作记忆 | ✅ |
| session_id 隔离 | ✅ | ✅ |
| MemoryRetriever | - | ✅ 历史结果检索 |
| GovernanceScheduler | - | ✅ 自动清理 |
| PITR | - | ✅ 时间旅行 |
| TypedReflector | - | ✅ 提升为 SEMANTIC |

---

## 终极方案：mo-trustmem 原生上下文管理

### 设计哲学

| 传统方案（2023-2024） | 终极方案（2026） | 本质差异 |
|----------------------|------------------|----------|
| 规则压缩（硬编码阈值） | Memory 引用 + 摘要 | 从"丢弃"到"存储+引用" |
| LLM 摘要（额外成本） | 结构化摘要（规则生成） | 淘汰额外 LLM 调用 |
| 人工维护历史 | PITR 自动追溯 | 消除历史数据歧义 |

### 架构

```
Tool Output → Size Check → [>10KB] → Store mo-trustmem
                                          ↓
                                   Generate Summary (规则)
                                          ↓
                              Return: Summary + [memory:xxx]
                                          ↓
                              LLM sees summary, can request full via memory_read
```

### 核心实现

```python
# core/agent/tool_output_handler.py

from core.memory.store import MemoryStore
from core.memory.types import MemoryType

SUMMARY_THRESHOLD = 10 * 1024  # 10KB

def generate_structured_summary(output: str, tool_name: str) -> str:
    """规则生成结构化摘要（零 LLM 成本）"""
    lines = output.strip().split('\n')
    
    if tool_name == "grep":
        # grep: 统计 + 样本
        files = set()
        for line in lines[:500]:
            if ':' in line:
                files.add(line.split(':')[0])
        return (
            f"Found {len(lines)} matches in {len(files)} files.\n"
            f"Files: {', '.join(sorted(files)[:10])}{'...' if len(files) > 10 else ''}\n"
            f"Sample:\n" + '\n'.join(lines[:5])
        )
    
    elif tool_name == "shell":
        # shell: 头尾 + 统计
        if len(lines) > 20:
            return (
                f"Output: {len(lines)} lines, {len(output)} bytes\n"
                f"First 10 lines:\n" + '\n'.join(lines[:10]) + "\n...\n"
                f"Last 5 lines:\n" + '\n'.join(lines[-5:])
            )
    
    # 默认：头部截断
    return output[:2000] + f"\n... ({len(output)} bytes total)"


async def process_tool_output(
    output: str,
    tool_name: str,
    session_id: str,
    user_id: str,
    memory_store: MemoryStore,
) -> str:
    """处理 tool 输出：小输出直接返回，大输出摘要+存 mo-trustmem"""
    
    if len(output) <= SUMMARY_THRESHOLD:
        return output
    
    # 1. 存入 mo-trustmem（完整保留）
    memory = memory_store.create(
        user_id=user_id,
        content=output,
        memory_type=MemoryType.WORKING,
        session_id=session_id,
        source=f"tool:{tool_name}",
        metadata={"tool": tool_name, "size": len(output)},
    )
    
    # 2. 规则生成摘要（零 LLM 成本）
    summary = generate_structured_summary(output, tool_name)
    
    # 3. 返回摘要 + 引用
    return f"{summary}\n\n[Full output: memory:{memory.memory_id}]"
```

### 历史结果复用

```python
# core/agent/tool_output_handler.py

from core.memory.retriever import MemoryRetriever

async def find_similar_result(
    tool_name: str,
    params: dict,
    session_id: str,
    user_id: str,
    retriever: MemoryRetriever,
) -> str | None:
    """查找相似的历史 tool 结果（利用 mo-trustmem hybrid retrieval）"""
    
    # 构造查询：tool 名 + 参数
    query = f"{tool_name} {params.get('pattern', '')} {params.get('path', '')}"
    
    results = retriever.retrieve(
        user_id=user_id,
        query=query,
        session_id=session_id,
        memory_types=[MemoryType.WORKING],
        limit=1,
    )
    
    if results and results[0].metadata.get("tool") == tool_name:
        # 检查参数相似度
        old_params = results[0].metadata.get("params", {})
        if old_params.get("pattern") == params.get("pattern"):
            return f"[Reusing previous {tool_name} result: memory:{results[0].memory_id}]"
    
    return None
```

### 效果对比

| 指标 | 当前 | 准备优化 | 终极方案 |
|------|------|----------|----------|
| 单次 tool 输出 | 30KB | ~2KB（LLM摘要） | ~500B（规则摘要） |
| 摘要成本 | $0 | ~$0.001/次 | $0（规则生成） |
| 摘要延迟 | 0ms | ~100ms | ~1ms |
| 历史复用 | 无 | 无 | ✅ 自动检索 |
| 信息保留 | ~30% | ~95% | 100%（存 Memory） |

### 与 MatrixOne 能力对应

| 终极方案功能 | MatrixOne/mo-trustmem 能力 |
|--------------|---------------------------|
| 存储完整输出 | `MemoryStore.create()` |
| 历史检索 | `MemoryRetriever.retrieve()` + embedding |
| 时间旅行 | PITR: `SELECT * FROM memories {timestamp}` |
| 自动清理 | `GovernanceScheduler` confidence decay |
| Session 隔离 | `session_id` 字段 |

### 实施计划

```
Day 1: 实现 generate_structured_summary()（规则摘要）
Day 2: 实现 process_tool_output()（存储 + 摘要）
Day 3: 实现 find_similar_result()（历史复用）
Day 4: 集成到 chat_loop.py + 测试
Day 5: 上线 + 监控
```

### 关键纠正

| 原提案 | 纠正 |
|--------|------|
| "L2_DISTANCE 直接对 text" | 需要先 embedding，mo-trustmem 已有此能力 |
| "返回纯引用 `[memory:xxx]`" | 必须返回摘要+引用，否则 LLM 无法回答 |
| "timestamp 必须手动指定" | MatrixOne 自动记录 created_at |
| "用 LLM 摘要" | 用规则摘要，零成本零延迟 |
