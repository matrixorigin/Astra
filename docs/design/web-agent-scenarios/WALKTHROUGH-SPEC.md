# End-to-End Walkthrough Specification

> 目的: 拿 v0.2 设计文档（`web-agent-session-state.md` 3040 行，19 条 gap 已 patch）作为**唯一权威**，把 14 个场景的时间线**完整走一遍**，看每一回合在 v0.2 下**能不能顺畅进行、哪里别扭、哪里有用户体验的坑**。

## 核心任务

你不是在评审设计，而是在**以一个有经验的工程师视角模拟"如果我真的用 v0.2 这套设计来实现并运行这个场景"的结果**。

每个时间线回合都要回答 4 个问题：

1. **机制是否可达**（Mechanically feasible）：v0.2 的 schema、API、契约是否允许这个回合发生？
2. **体验是否顺畅**（UX smooth）：用户会不会觉得奇怪、延迟、突兀、不耐烦？
3. **是否有别扭点**（Friction）：开发者实现时会不会卡住？数据是否有 ambiguity？某个 corner case 是否会暴露？
4. **是否省 token**（Token efficient）：这一回合的 context manifest zone 分配是否合理？会不会触发 drop cascade？

## 产出格式

每位 walkthrough agent 创建一个文件：
`/Users/ghs-mo/MOWorkSpace/mo-agent-engine/docs/design/web-agent-scenarios/WALKTHROUGH-S<NN>.md`

严格八节（**与场景 §1–§8 不同**，此文件聚焦"运行轨迹"）：

### §1 场景标识与 v0.2 映射
- 一句话场景、被测 DAUT
- 本场景最重要的 v0.2 章节/字段（用 3–5 行列出，每行引 v0.2 具体行号）

### §2 运行前提
- 假设的 schema 初始状态（`agent_sessions` / `agent_runs` / 相关 projection 有无初值）
- 假设的 workspace（cloud / edge / hybrid）与 tool 集合
- 预期的 context window 大小（哪个 budget template）

### §3 端到端运行轨迹（**核心**）
- 按场景的 T1..Tn 顺序走
- 每回合含 **3 列**:
  - **User 动作** (原话/点击/输入)
  - **Server behavior**（引 v0.2 的哪些字段/API 变化，SQL 伪代码可选）
  - **User sees**（SSE 事件、UI 变化、manifest zone 实际 token 占用）
- **绝对不要抽象**。每回合 3–8 行，不到位就写不到位。

### §4 发现的别扭点（Frictions）
按重要度排序，每条含：
- **Ref**: 场景回合号 T.. / v0.2 章节
- **Observation**: 你看到了什么别扭
- **Why it's friction**: 这为什么是问题（UX / 实现 / token / 正确性）
- **Severity**: `blocking` / `notable` / `minor`

至少 3 条，上不封顶。如果这个场景下 v0.2 真的很顺滑，也可以说"未发现 blocking 别扭"，但 minor 的总会有。

### §5 发现的**新 gap 候选**
跟之前 17 条 gap 不一样的**新问题**。格式对齐 GAP-TRACKER 的字段：
```
- **title**:
- **triggered_by_turn**: T..
- **related_v0.2_section**: §..
- **severity**: (critical / high / medium / low)
- **rationale**: (为什么这是 gap 而不只是实现细节)
- **proposed_fix**: (一句话)
```
如果没有，**明确写"未发现新 gap 候选"**。

### §6 Gap 组合效应检验
举 2–3 个本场景涉及的 **gap 组合**（如 G10+G12、G7+G11、G2+G14+G6 等），判断它们叠加起来是否有冲突或坑。
每条：
- **组合**: Gx + Gy (+ Gz)
- **运行时可能发生的坑**:
- **v0.2 是否覆盖**: yes / partial / no
- **如果 no，建议**:

### §7 Token 实际 walkthrough
在场景最"紧张"的一轮（通常是中后段），列 manifest 每个 zone 的**实际** token 占用（估算即可），与 v0.2 §Budget Policy 定义的 cap 对比，看有无 overflow。
列出计算过程，不要只给结论。

### §8 最终判断
- **本场景下 v0.2 是否可实现**: ✅ / ⚠️ / ❌
- **顺畅度**: ✅ 顺畅 / ⚠️ 有明显摩擦 / ❌ 明显别扭
- **一句话总结**

## 长度约束

450–700 行之间。**450 是下限**（太短则推演没走透），**700 是上限**（太长则模型要超时）。

## 质量铁律

- **禁止**光说"已覆盖"不给具体行号
- **禁止**绕开场景原时间线自己改回合
- **禁止**因为"gap 已 resolved"就说"场景无问题" — 请你**真的以运行视角走一遍**
- **允许**指出"v0.2 写得对但实现会很难"，这属于 friction 而非 gap
- **允许**推荐开新 gap（G20+）—— 如果是真的 gap 而非 bikeshed

## 前置必读

1. `/Users/ghs-mo/MOWorkSpace/mo-agent-engine/docs/design/web-agent-session-state.md` v0.2 (3040 行)
2. 对应场景文件 `S<NN>-*.md`（只读它的 §1-§3 时间线，§8 反馈可略过以免被 prime）
3. `/Users/ghs-mo/MOWorkSpace/mo-agent-engine/docs/design/web-agent-scenarios/GAP-TRACKER.md`（19 条 gap 的最终状态 + resolution_commit 行号参考）

## 最后

每个 agent 完成后返回：
- 文件绝对路径 + 总行数
- 3 句话：`(1) 最严重的别扭点 / (2) 是否发现新 gap / (3) 整体可实现性判断`
