# Plan Mode V3 — 设计文档

> 本文档描述 plan-mode-v3 的最终落地设计，以当前 stacked 实现为准。
> 前版本见 `plan-mode-v2.md`（已被本版本取代）。

## 背景与动机

V2 修复了 Ctrl-C 无响应、JSON 解析脆弱等 P0/P1 问题，但遗留了以下问题：

| # | 问题 | 类型 |
|---|------|------|
| B1 | `should_suggest_plan_mode` 误判分析型问题为可执行 plan | bug |
| B2 | auto-suggest 使用阻塞 `stdin().read_line`，卡死 REPL | bug/UX |
| B3 | `is_resume_command` 单 token 过激匹配（`go`/`next`/`继续`） | bug |
| B4 | `recover_plan_for_resume` 对空 plan 误报 AllCompleted | bug |
| B5 | `progress_pct` 空 subtasks 返回 100% | bug |
| B6 | plan 生成失败/cancel 不清理 `plan_mode` 状态 | bug |
| B7 | `clear_saved_state()` 删错文件名（`.bak` vs `.json.bak`） | bug |
| B8 | `maybe_restore_pending_plan_mode` 跨 workspace 不安全 | bug |
| A1 | 仅支持"可执行"plan，分析型问题无路径 | 架构 |
| A4 | journal 事件 metadata 无结构化字段 | 可观测性 |
| A5 | 持久化单文件，无 workspace 隔离 | 持久化 |

V3 目标：**完整修复上述问题，新增分析型 plan 路径，规范化 journal/persistence 契约**。

---

## 核心设计

### D1. PlanKind — 区分两种 plan

```rust
pub enum PlanKind {
    Executable,  // 旧路径：subtasks 必填，executor 跑
    Analytical,  // 新路径：phases 是研究/评估问题；不跑 executor
}

pub struct PlanSuggestion {
    pub kind: PlanKind,
    pub reason: String,
}
```

`classify_plan_suggestion(input)` 返回 `Option<PlanSuggestion>`，对分析型输入返回 `kind: Analytical`。

**分析型识别规则**（`looks_analytical`）：
- 英文前缀：`should i`, `is it`, `which is better`, `compare`, `evaluate`, `assess`, `what are the tradeoffs`, `pros and cons`, `help me decide`, `review`, `analyze`, `analyse`
- 中文前缀：`评估`, `分析`, `比较`, `哪个更好`, `是否应该`, `优缺点`, `权衡`, `帮我决定`, `审查`

### D2. Auto-suggest UX（非阻塞倒计时）

替换旧的 `stdin().read_line` 阻塞调用：

```
📋  <reason>
💡  Enter plan mode? [y/N] (5s) — ⏎/n to skip
```

- 使用 `crossterm::event::poll(Duration)` + 250ms tick
- **默认 5s 不响应 = No**（`TimedOut`）
- 仅 `y`/`Y`/`是` 接受；`Enter`/`Esc`/`n`/`N`/任意其他键 = Declined
- Ctrl-C/Ctrl-D = Interrupted（向上传播取消信号）
- 非 TTY 环境降级为静默 Declined

实现：`astra-cli/src/cli/plan_auto_suggest.rs`

### D3. Analytical Plan 路径

分析型问题走独立的 `ResearchPlan` 生成器，不经过 subtask executor：

```
analytical goal
  → generate_research_plan(goal, context)
  → ResearchPlan { summary, questions: Vec<ResearchQuestion> }
  → format_research_plan_display()
  → 输出到 chat，退出 plan mode
```

`ResearchPlan` 结构：
```rust
pub struct ResearchPlan {
    pub summary: String,
    pub questions: Vec<ResearchQuestion>,
}

pub struct ResearchQuestion {
    pub id: String,
    pub question: String,
    pub why: String,
    pub how: String,
}
```

实现：`astra-plan/src/analytical.rs`

### D4. Cursor 风格后台执行（N1）

`execute`/`go` 命令启动后立即退出 plan mode，executor 在后台运行：

```
用户: execute
  → state.plan_mode = None          // 退出 plan mode
  → PlanModeState::clear_saved_state()  // 清理持久化文件
  → state.executing_plan = Some(plan)   // 后台执行状态
  → spawn_plan_executor(ctx, selector)  // 后台 tokio task
  → REPL 回到普通 chat
```

用户可用 `/plan status` 查看后台进度。进度通过 `flush_plan_updates_between_prompts` 在每次 prompt 间渲染。

### D5. JSON 解析鲁棒性（P3）

`extract_json_robust(text)` 处理 LLM 常见输出问题：
1. 剥离 ` ```json ` / ` ``` ` fence
2. 修复 trailing comma（`},]` → `}]`）
3. 修复单引号（`'key'` → `"key"`）
4. 失败时触发 retry prompt（最多 2 次）

### D6. Workspace-scoped Persistence（P6）

状态文件路径含 cwd hash，避免跨 workspace 污染：

```
~/.astra/plans/<8-char-hash>/plan_state.json
~/.astra/plans/<8-char-hash>/plan_state.json.bak
```

hash 算法：`DefaultHasher` of canonicalized cwd（16 hex chars，取前 8）。

**加载时验证**：`PlanModeState::matches_workspace(current_cwd)` 检查 `context.root` 是否与当前 cwd 匹配（允许子目录）。不匹配则拒绝恢复，打印警告。

**清理时机**：
- `execute`/`go` 启动后立即调用 `clear_saved_state()`
- `cancel`/`exit` 退出 plan mode 时调用
- 生成失败/取消时调用（`abort_plan_mode_after_failure`）

`clear_saved_state_at(path)` 同时删除 `.json` 和 `.json.bak`。

---

## Journal / 可观测性契约

所有 plan 事件使用 `JournalEventType::PlanLifecycle` 或 `PlanEdit`，metadata 包含结构化字段。

### 事件清单

| 事件 | stage 字段 | 额外字段 |
|------|-----------|---------|
| 进入 plan mode | `entered` | `goal`, `kind` (`executable`/`analytical`), `started_at_ms` |
| 生成失败/取消 | `<stage>` (e.g. `outline`) | `reason` |
| 分析型 plan 完成 | `analytical_delivered` | `kind: "analytical"` |
| 执行开始 | — | `mode` (`auto`/`step_by_step`), `subtask_count` |
| 执行完成 | `completed` | `elapsed_ms`, `items_done`, `items_total`, `pct` |
| 执行失败 | `error` | `error`, `items_done`, `items_total` |
| 执行暂停 | `paused` | `elapsed_ms`, `items_done`, `items_total`, `pct`, `remaining`, `blocked_ids` |
| Cancel 命令 | `cancelled` | — |
| Pause 命令 | `pause_requested` | — |
| Resume 命令 | `resumed` | — |

### 示例

```json
{"type":"plan_lifecycle","ts":"...","metadata":{
  "summary":"Plan execution completed",
  "detail":{"stage":"completed","elapsed_ms":12340,"items_done":5,"items_total":5,"pct":100}
}}
```

---

## 持久化契约

| 状态 | 存储位置 | 生命周期 |
|------|---------|---------|
| `PlanModeState` (goal + subtasks + context) | `~/.astra/plans/<hash>/plan_state.json` | 进入 plan mode 时写入，execute/cancel/exit 时删除 |
| 备份 | `plan_state.json.bak` | 原子写时保留上一版本，clear 时一并删除 |
| 后台执行状态 | 内存 (`state.executing_plan`) | 仅内存，不落盘 |
| `plan_handle` | 内存 | 仅内存 |

---

## 三条主路径

### 路径 1：分析型问题

```
用户输入 "评估 X 方案是否合理"
  → auto-suggest: kind=Analytical, 提示语不同
  → 用户接受 → 进入 plan mode
  → handle_goal_submission → analytical path
  → generate_research_plan(goal)
  → 输出 ResearchPlan（summary + questions）
  → 退出 plan mode → 回到普通 chat
```

### 路径 2：可执行目标

```
用户输入 "实现 feature X"
  → auto-suggest: kind=Executable
  → 用户接受 → 进入 plan mode
  → handle_goal_submission → executable path
  → outline → subtask generation → review
  → 用户: execute
  → 退出 plan mode，executor 后台运行
  → 用户: /plan status  → 查看进度
```

### 路径 3：Pause / Resume / Cancel

```
后台执行中:
  /plan pause   → 发送 PlanCommand::Pause 给 executor
  /plan resume  → 发送 PlanCommand::Resume { corrections }
  /plan cancel  → shutdown_plan_executor + clear_saved_state + plan_mode=None
```

---

## 已修复 Bug 清单

| Bug | 修复方式 |
|-----|---------|
| B1 auto-suggest 误判分析型 | `looks_analytical` 启发式 + `PlanKind` 分类 |
| B2 阻塞 stdin | `crossterm::event::poll` 非阻塞倒计时 |
| B3 is_resume_command 过激 | 只接受 `/resume`/`/continue`/`resume`/`continue`；`继续` 仅在有非空 plan 时识别 |
| B4 空 plan 误报 AllCompleted | `PlanResumeRecovery::EmptyNoSubtasks` 分支 |
| B5 progress_pct 空返回 100% | 空 subtasks 返回 0% |
| B6 生成失败不清理 plan_mode | `abort_plan_mode_after_failure` 统一清理 |
| B7 clear_saved_state 删错文件 | `clear_saved_state_at` 同时删 `.json` 和 `.json.bak` |
| B8 跨 workspace 恢复 | `matches_workspace` 验证 + 拒绝恢复 |

---

## Out of Scope（本版本不做）

- Phase 6 cloud sync（`cloud_plan_state` 表 + `pull/push_active_plan_pack`）
- `PlanArtifact` enum（独立 artifact 类型，当前 analytical/executable 共用入口）
- `PlanController` / `PlanPhase` 状态机驱动（当前仍是 ad-hoc if/else）
- `decompose.rs` / `plan_interaction.rs` 大规模模块拆分（R5）
- CRC32 → blake3 升级
- web/ frontend plan UI

---

## 验证

```bash
cd rust && cargo test -p astra-plan
cd rust && cargo test -p astra-cli plan
cd rust && cargo clippy --workspace --all-targets -- -D warnings
make format && make check && make test-offline
```

关键测试：
- `decompose::tests::classify_plan_suggestion_distinguishes_kinds`
- `decompose::tests::clear_saved_state_at_removes_both_state_and_json_bak`
- `plan_auto_suggest::tests::*`（8 个 keystroke 分类测试）
- `plan_monitor::tests::plan_completed_journal_has_stage_and_elapsed_ms`
- `plan_monitor::tests::plan_error_journal_has_stage_and_error_field`
- `plan_monitor::tests::plan_paused_journal_has_stage_elapsed_ms_and_items`
- `plan_monitor::tests::cancel_journal_has_stage_cancelled`
- `repl_runtime::tests::maybe_restore_pending_plan_mode_activates_saved_plan`
- `repl_runtime::tests::maybe_restore_pending_plan_mode_rejects_workspace_mismatch`
