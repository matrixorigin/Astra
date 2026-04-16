# Session Continuity & Crash Recovery — Design Document

**Status**: Draft v2 (architecture-grounded revision)
**Author**: astra-engine team
**Date**: 2026-04-16
**Problem**: 中断/崩溃的session零留存，无法追溯、无法续接、无法吸取教训

## 1. 设计原则

| # | 原则 | 含义 | 约束 |
|---|------|------|------|
| P1 | 极好用户体验 | 用户无需显式操作，系统自动感知和恢复 | 不强制交互，智能推荐 |
| P2 | 状态可追溯 | 任何中断点可精确定位：哪一步、什么错误、为什么 | journal已有，需打通读取链路 |
| P3 | 持续对话能力 | 上下文不丢失，说"继续"就能继续 | token必须可控 |
| P4 | Token不爆炸 | 恢复注入≤500t，全量数据按需加载 | 分层恢复，不一次性注入全部 |

## 2. 现状盘点（代码实查）

### 2.1 已有基础设施

| 组件 | 位置 | 实际能力 |
|------|------|---------|
| **Journal事件流** | `services/session_journal.rs` | append-only JSONL，`JournalEventType` 含 `InterruptionRecorded`/`Turn`/`TurnError`/`StallDetected`/`SessionEnd` 等 30+ 事件类型 |
| **InterruptionRecord** | `runtime/turn/interruption.rs` | 11种 `InterruptionKind`，含 `is_resumable()` 判断 + `ResumeAction` 枚举 + `build_resume_guidance_with_context()` 已生成结构化恢复指导 |
| **HeavyCheckpoint** | `runtime/pipeline/step_protocol.rs` | 含 `messages`/`blocked_tools`/`interruption`/`compaction_state`/`approval_overrides`/`delegation_*` |
| **RestoredSession** | `services/session_restore.rs` | 含 `conversation_messages`/`blocked_tools`/`executing_plan_json`/`plan_goal`/`plan_corrections`/`contract_json`/`last_context_trace` |
| **RestoredBreakpoint** | `services/session_restore.rs` | 含 `tool_health_entries`/`correction_history_json`/`composite_snapshot` |
| **CompositeSnapshot** | `core/composite_snapshot.rs` | session/data/memory/git/workspace 五维快照 |
| **L0 Anchor** | `runtime/turn/cloud/session_memory_protocol.rs` | `extract_anchor()` 生成 ~50t 锚点，`SESSION_MEMORY_PREFIX = "[session-memory:v1]"` |
| **L1 Session Memory** | `runtime/turn/cloud/session_memory_protocol.rs` | 10个section，stored ≤4000t，injection ≤2000t，`compress_to_injection()` 零LLM压缩 |
| **Session Memory Extract** | `runtime/turn/cloud/session_memory_extract.rs` | 双阈值触发（token增长 AND tool call次数），模板含 `## Errors & Corrections` + `## Learnings` 两个独立section |
| **`extract_learnings_for_backflow()`** | `session_memory_extract.rs:205` | 提取 `Learnings` + `Errors & Corrections` 两个section做跨session复用 |
| **`list_sessions_by_time()`** | `session_journal.rs:921` | 按mtime排序取最近N个session ID |
| **`peek_session_meta()`** | `session_journal.rs:978` | 读journal头20行提取 `first_prompt`/`model`/`created_at` |
| **`/resume` 命令** | `slash_session.rs` | CLI交互式选择 + `build_resume_guidance_with_context()` 生成guidance |
| **resume_guidance注入** | `repl_turn.rs:275` | `state.resume_guidance.take()` → prepend到 `effective_line` |

### 2.2 关键发现（与原设计文档的偏差）

1. **Session Memory模板已有 `## Learnings` section** — 原设计说"不新增 `## Lessons` section，复用 `## Errors & Corrections`"，但实际模板（`SESSION_MEMORY_TEMPLATE`）已有独立的 `## Learnings` section，且 `extract_learnings_for_backflow()` 已分别提取两个section。**不应合并，应保持分离。**

2. **`compress_to_injection()` 已有过滤逻辑** — Errors & Corrections 只保留 `unresolved`/`USER CORRECTION`/`❌`/`🔧` 标记的条目。Learnings section 在 injection 中被完全省略。**恢复时需要显式包含 Learnings。**

3. **`SessionPeek` 确实没有 `status` 字段** — 但 `peek_session_meta()` 只读头20行。判断中断需要读 tail，这是正确的。

4. **`RestoredSession` 已经非常丰富** — 含 plan state、contract、blocked tools、conversation messages。原设计提议新增 `error_lessons`/`interruption_summary` 字段，但这些信息已可从 `HeavyCheckpoint.interruption` 和 session memory 中获取，**不需要新增字段**。

5. **`build_resume_guidance_with_context()` 已经很完善** — 按 interruption kind 生成针对性建议，含 compaction context。恢复时只需调用此函数 + 补充 session memory 摘要即可。

## 3. 修正后的设计：两层恢复协议

原设计的三层（L0/L1/L2）过度设计。实际只需要两层：

### 3.1 Layer 0 — 启动探测 + 提示（≤100t）

**时机**：新session的第一个turn之前

**动作**：扫描本地session列表，读 journal tail 判断是否中断

```rust
/// 读journal文件最后N行（从文件末尾反向读取，不加载全文件）。
/// 返回解析成功的事件列表。
pub fn scan_journal_tail(session_id: &str, n_lines: usize) -> Option<Vec<JournalTailEntry>> {
    let path = journal_dir().join(format!("{session_id}.jsonl"));
    let content = std::fs::read_to_string(&path).ok()?;
    // 从末尾取最后n_lines行
    let lines: Vec<&str> = content.lines().rev().take(n_lines).collect();
    let mut entries = Vec::new();
    for line in lines.into_iter().rev() {
        if let Some(entry) = parse_tail_entry(line) {
            entries.push(entry);
        }
    }
    Some(entries)
}

/// 轻量级tail entry — 只提取判断所需的字段，不反序列化完整JournalEvent。
pub struct JournalTailEntry {
    pub event_type: String,     // "turn", "session_end", "interruption_recorded" 等
    pub ts: Option<String>,
    pub interruption_kind: Option<String>,  // 仅 interruption_recorded 时有值
    pub resumable: Option<bool>,
}
```

**中断判断逻辑**：

```rust
enum SessionEndState {
    Completed,                          // 最后事件 == SessionEnd
    Interrupted { kind: String },       // 最后事件 == InterruptionRecorded
    Zombie,                             // 有活动事件但无SessionEnd
}

fn classify_session_end_state(session_id: &str) -> SessionEndState {
    let tail = scan_journal_tail(session_id, 20)?;
    // 从后往前找第一个有意义的事件（跳过 Checkpoint 等辅助事件）
    for entry in tail.iter().rev() {
        match entry.event_type.as_str() {
            "session_end" => return SessionEndState::Completed,
            "interruption_recorded" => {
                return SessionEndState::Interrupted {
                    kind: entry.interruption_kind.clone().unwrap_or_default(),
                };
            }
            "turn" | "turn_error" | "stall_detected" | "plan_progress"
            | "delegation_completed" => {
                return SessionEndState::Zombie;
            }
            _ => continue, // 跳过 checkpoint, config_change 等
        }
    }
    SessionEndState::Completed // 空journal或只有辅助事件
}
```

**防误判当前运行中的session**：检查 journal 文件的 mtime，如果距今 < 60秒，跳过（可能是当前正在运行的session）。

```rust
fn build_recovery_anchor() -> Option<String> {
    let recent = list_sessions_by_time(5).ok()?;
    let current_sid = current_session_id(); // 当前session的ID，排除自己
    for sid in &recent {
        if Some(sid.as_str()) == current_sid.as_deref() { continue; }
        // 防误判：mtime < 60s 的跳过
        if journal_mtime_age_secs(sid) < 60 { continue; }
        match classify_session_end_state(sid) {
            SessionEndState::Interrupted { kind } => {
                let peek = peek_session_meta(sid)?;
                let prompt = peek.first_prompt.as_deref().unwrap_or("(unknown task)");
                return Some(format!(
                    "[session-recovery] 上次session {short} 在做\"{prompt}\"时中断({kind})。\n\
                     说\"继续\"恢复，或开始新任务。",
                    short = &sid[..8],
                ));
            }
            SessionEndState::Zombie => {
                let peek = peek_session_meta(sid)?;
                let prompt = peek.first_prompt.as_deref().unwrap_or("(unknown task)");
                return Some(format!(
                    "[session-recovery] 上次session {short} 在做\"{prompt}\"时异常退出。\n\
                     说\"继续\"恢复，或开始新任务。",
                    short = &sid[..8],
                ));
            }
            SessionEndState::Completed => continue,
        }
    }
    None
}
```

**注入位置**：system prompt 末尾（和 L0 session anchor 一起），享受 prompt cache。

**衰减**：prompt assembly 时检查 `current_turn > 3`，超过后不再包含。

**token成本**：~100t，仅在有可恢复session时注入。

### 3.2 Layer 1 — 精准恢复（≤500t）

**时机**：用户说"继续" / `/resume`

**动作**：复用已有的 `restore_to_checkpoint` + `build_resume_guidance_with_context`，补充 session memory 摘要。

**关键改动**：不新增 `RestoredSession` 字段，而是在恢复流程中组装 Recovery Context。

```rust
fn build_recovery_context(session_id: &str) -> Option<String> {
    let restored = restore_session(session_id)?;
    let mut ctx = String::new();

    // 1. 已有的 resume guidance（来自 HeavyCheckpoint.interruption）
    //    build_resume_guidance_with_context() 已按 kind 生成针对性建议
    if let Some(guidance) = load_interruption_guidance(session_id) {
        ctx.push_str(&guidance);
    }

    // 2. Session Memory 摘要（如果有）
    //    读取 session memory markdown，提取 Errors & Corrections + Learnings
    if let Some(memory_md) = read_session_memory_file(session_id) {
        let learnings = extract_learnings_for_backflow(&memory_md);
        for (section, content) in &learnings {
            if !content.trim().is_empty() {
                ctx.push_str(&format!("\n[{section}]\n{content}\n"));
            }
        }
    }

    // 3. Plan 进度（如果有活跃 plan）
    if let Some(plan_json) = &restored.executing_plan_json {
        if let Some(summary) = summarize_plan_progress(plan_json) {
            ctx.push_str(&format!("\n[plan-progress] {summary}\n"));
        }
    }

    // 4. Blocked tools
    if !restored.blocked_tools.is_empty() {
        ctx.push_str(&format!(
            "\n[blocked-tools] {}\n",
            restored.blocked_tools.join(", ")
        ));
    }

    if ctx.is_empty() { None } else { Some(ctx) }
}
```

**注入位置**：`repl_turn.rs:275`，和现有 `resume_guidance` 相同路径。

**token成本**：≤500t。

### 3.3 深度回溯（不作为独立层）

原设计的 L2 "深度回溯"不需要作为恢复协议的一部分。用户追问"之前做了什么"时，已有的 `/debug <session_id>` 和 `astra audit turns` 完全覆盖此需求。不需要新增任何代码。

## 4. 用户体验流程

### 场景A：正常启动，有可恢复session

```
用户: hi
系统: [L0探测] 发现上次session abc12345 在做"修复auth模块的登录bug"时中断(budget_exhausted)。
       说"继续"恢复，或开始新任务。

用户: 继续
系统: [L1恢复] 调用 /resume abc12345
       注入 resume_guidance + session memory learnings
       "好的，上次修auth.rs时build失败了。我从checkpoint继续。"
```

### 场景B：Ctrl-C中断后重启

```
[用户Ctrl-C → InterruptionRecorded {kind: UserCancelled}]
[重启astra]
系统: [L0探测] 上次session xyz789 在做"实现分布式缓存"时被取消。
       说"继续"恢复，或开始新任务。

用户: 那次走错了，换方向
系统: [L1恢复 + Learnings注入]
       "明白。上次的Learnings: HashMap方案在并发场景会死锁。
        建议这次用分段锁。需要规划新方案吗？"
```

### 场景C：Plan中断后恢复

```
[Plan在subtask 3/7时Ctrl-C]
[重启astra]
系统: [L0探测] 上次session有活跃Plan "重构缓存模块"。
       说"继续"恢复计划，或开始新任务。

用户: 继续
系统: [L1恢复] 从 RestoredSession.executing_plan_json 解析进度
       "Plan进度: 3/7完成。从子任务4继续。"
```

## 5. Token效率策略

### 5.1 分层注入

| 层 | 触发 | Token | 内容 |
|----|------|-------|------|
| L0 Recovery Anchor | 启动时有zombie/中断 | ~100t | "有中断 + 任务名 + 中断类型" |
| L1 Recovery Context | 用户说"继续" | ≤500t | resume_guidance + learnings + plan进度 + blocked tools |

### 5.2 Cache-friendly

- L0 Anchor 在 system prompt 末尾，享受 prompt cache
- L1 Recovery Context 在 first user message 前（`repl_turn.rs:275` 现有路径）

### 5.3 渐进衰减

- L0 Anchor：3轮后不再包含（prompt assembly 时判断 `current_turn > 3`）
- L1 Recovery Context：恢复后被 compaction 自然吸收为 session memory

## 6. 实现路径

### Phase 1 — 启动探测（最小改动）

**新增 1 个函数 + 1 个调用点**：

| 改动 | 位置 | 说明 |
|------|------|------|
| `scan_journal_tail()` | `services/session_journal.rs` | 读journal最后N行，返回轻量级 `JournalTailEntry` |
| `classify_session_end_state()` | 同上 | 基于tail判断 Completed/Interrupted/Zombie |
| `build_recovery_anchor()` | 同上或新文件 | 组装L0 anchor字符串 |
| 调用点 | agentic loop bootstrap 或 `repl_runtime.rs` 初始化 | 在第一个turn前调用，结果存入 `ReplState` |
| prompt assembly | 现有 system prompt 构建逻辑 | 检查 `state.recovery_anchor`，turn ≤ 3 时追加 |

**不改动**：`RestoredSession`、`/resume`、`build_resume_guidance_with_context` — 全部复用。

### Phase 2 — 恢复时补充 Session Memory Learnings

**改动 1 个函数**：

| 改动 | 位置 | 说明 |
|------|------|------|
| 增强 `/resume` 恢复流程 | `slash_session.rs` | 恢复时额外读取 session memory file，提取 Learnings + Errors & Corrections，追加到 `resume_guidance` |

**具体**：在 `slash_session.rs` 的 `/resume` handler 中，`build_resume_guidance_with_context()` 之后，追加 session memory learnings：

```rust
// 现有代码
let guidance = build_resume_guidance_with_context(&interruption_json, compaction_ctx.as_ref());

// 新增：追加 session memory learnings
if let Some(memory_md) = read_session_memory_file(&session_id) {
    let learnings = extract_learnings_for_backflow(&memory_md);
    for (section, content) in &learnings {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            guidance.push_str(&format!("\n[{section}] {}\n",
                truncate_to_token_budget(trimmed, 150)));
        }
    }
}
```

### Phase 3 — 错误学习持久化改进

**不新增 section**。现有模板已有 `## Errors & Corrections` 和 `## Learnings`。

改进 session memory extraction prompt，确保：
- 每次 TurnError 后触发一次 extraction（降低 `min_tool_calls_between_updates` 阈值）
- Learnings section 按 `[pattern]`/`[correction]`/`[avoidance]` 标签分类
- 每个session限5条 learnings（extraction prompt 中约束）

## 7. 关键决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 恢复层数 | 2层（不是3层） | L2"深度回溯"已被 `/debug` 和 `astra audit` 覆盖 |
| `RestoredSession` 新字段 | 不新增 | `HeavyCheckpoint.interruption` + session memory 已包含所需信息 |
| Learnings 存储位置 | 保持 `## Learnings` 独立section | 代码实际已有此section + `extract_learnings_for_backflow()` 已分别提取 |
| 中断判断 | journal tail 扫描 | `SessionPeek` 无 status 字段；journal tail 更准确 |
| 防误判运行中session | mtime < 60s 跳过 + 排除当前 session_id | 避免把正在运行的session误判为zombie |
| 恢复触发 | 用户确认（不强制） | P1原则：不打断新任务意图 |
| 多session | 只推荐最近1个 | `/resume` 查看全部 |
| L0衰减 | turn > 3 不包含 | prompt assembly 时判断 |
| Fork验证 | 不做 | 过度设计，恢复后直接继续即可，错误由 TurnGuard 兜底 |
| Cloud恢复 | 不在此设计中 | `cloud_sync.rs` 已有 delta push/pull，恢复是其自然延伸，不需要额外协议 |
| Web Agent恢复 | 不在此设计中 | server-side SSE loop + zombie检测已有，断线重连是网络层问题 |
| Plan恢复 | 复用 `RestoredSession.executing_plan_json` | 已有字段，只需在L1中展示进度 |

## 8. 不做什么

- ❌ 不新增 `RestoredSession` 字段（已有信息足够）
- ❌ 不新增 L2 深度回溯层（`/debug` + `astra audit` 已覆盖）
- ❌ 不做 Fork 验证恢复（TurnGuard 已兜底错误检测）
- ❌ 不在此设计中处理 Cloud/Web Agent 恢复（已有基础设施的自然延伸）
- ❌ 不合并 `## Learnings` 和 `## Errors & Corrections`（代码已分离）
- ❌ 不在每次turn扫描旧session
- ❌ 不自动恢复（不问用户就强制恢复会打断新任务）
- ❌ 不注入全量 conversation_messages
- ❌ 不给 `SessionPeek` 加 status 字段

## 9. 新增API清单

| API | 位置 | Phase |
|-----|------|-------|
| `scan_journal_tail(session_id, n_lines) -> Option<Vec<JournalTailEntry>>` | `session_journal.rs` | 1 |
| `classify_session_end_state(session_id) -> SessionEndState` | `session_journal.rs` | 1 |
| `build_recovery_anchor() -> Option<String>` | `session_journal.rs` 或新文件 | 1 |
| `read_session_memory_file(session_id) -> Option<String>` | `session_memory_extract.rs` 或 `session_restore.rs` | 2 |

**总计 4 个新函数**，0 个新 struct 字段。

## 10. 验收标准

| # | 标准 | 验证方式 |
|---|------|---------|
| A1 | Ctrl-C中断后重启，系统自动提示可恢复 | 手动测试 |
| A2 | 说"继续"后，agent知道之前在做什么、遇到什么问题 | 检查注入的 recovery context |
| A3 | Recovery Anchor ≤100t | token计数 |
| A4 | Recovery Context ≤500t | token计数 |
| A5 | 正常启动（无中断session）零额外token | 验证无 anchor |
| A6 | 错误教训跨session可见 | session A 的 Learnings 在 session B 恢复时注入 |
| A7 | L0 Anchor 在3轮后消失 | 第4轮 system prompt 不含 anchor |
| A8 | 运行中的session不被误判为zombie | mtime < 60s 跳过 |
| A9 | Plan中断后恢复显示进度 | 恢复时展示 subtask 完成状态 |

## 11. 风险与缓解

| 风险 | 缓解 |
|------|------|
| journal文件损坏 → L0探测失败 | `scan_journal_tail` 返回 `Option`，失败时静默跳过 |
| 多个zombie session | 只取最近1个，`/resume` 查看全部 |
| session memory file 不存在（短session未触发extraction） | 降级：只用 `build_resume_guidance_with_context()`，不注入 learnings |
| `read_to_string` 对大journal文件慢 | Phase 1 可接受；后续优化为 seek-to-end 反向读取 |
