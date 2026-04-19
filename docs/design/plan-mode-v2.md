# Plan Mode V2 — 系统性重构设计

## 现状问题

| # | 问题 | 根因 | 严重度 |
|---|------|------|--------|
| 1 | Ctrl-C 无响应 | `handle_goal_submission` 中 `post_chat_turn` + `collect_sse_with_preview` 是裸 await，没有 `tokio::select!` + `ctrl_c()` | P0 |
| 2 | 无超时 | `post_chat_turn` 无 timeout，SSE 流无总超时，服务端不响应则永久阻塞 | P0 |
| 3 | JSON 解析脆弱 | 依赖 LLM "自觉"输出 JSON，无 structured output，失败无重试 | P1 |
| 4 | 一次性全量生成 | 复杂任务 10+ subtasks 一次生成，质量衰减，无法利用中间结果 | P1 |
| 5 | 交互黑盒 | 生成过程只有 ThinkingPreviewPane，无阶段反馈，clarification 无 inquire 选择 | P1 |
| 6 | plan_interaction.rs 过大 | 1900+ 行，混合 UI/解析/LLM 调用/状态管理/执行调度 | P2 |
| 7 | PlanPhase 状态机未使用 | `plan.rs` 定义了完整的 PlanPhase 状态机，但 `plan_interaction.rs` 直接操作 PlanModeState，状态机形同虚设 | P2 |

## 重构目标

1. Plan 生成过程可取消（Ctrl-C）、有超时、有进度反馈
2. 渐进式生成：先 outline → 用户确认 → 再细化 subtasks
3. JSON 解析鲁棒：structured output 优先，失败自动重试
4. 交互式协商：用户在每个阶段都能介入
5. 代码结构清晰：状态机驱动，UI/逻辑分离

## 架构设计

### 核心变更：从"一次生成"到"对话式规划"

```
旧流程:
  goal → [LLM: 一次生成完整 JSON] → parse → display → execute

新流程:
  goal → [Phase 1: Outline] → 用户确认/修改
       → [Phase 2: Detail]  → 用户确认/修改
       → [Review & Execute]
```

### 模块拆分

```
astra-plan/src/
  lib.rs              # re-exports
  plan.rs             # PlanPhase 状态机 (保留，真正使用起来)
  decompose.rs        # 拆分为以下模块 ↓
  project_scan.rs     # analyze_project, ProjectContext (从 decompose.rs 提取)
  prompt.rs           # LLM prompt 模板 (从 decompose.rs 提取)
  parse.rs            # JSON 解析 + 鲁棒性 (从 decompose.rs 提取)
  clarify.rs          # 澄清问答 (从 decompose.rs 提取)
  version.rs          # 版本历史 + diff (从 decompose.rs 提取)
  timeline.rs         # ExecutionTimeline (从 decompose.rs 提取)
  template.rs         # 模板匹配 (从 decompose.rs 提取)
  format.rs           # 格式化输出 (从 decompose.rs 提取)

astra-cli/src/cli/
  plan_interaction.rs # 拆分为以下模块 ↓
  plan/
    mod.rs            # PlanController — 状态机驱动的顶层协调器
    generate.rs       # PlanGenerator — 可取消的 LLM 调用 + 渐进生成
    display.rs        # PlanDisplay — 所有 plan UI 渲染
    input.rs          # PlanInput — 用户交互 (inquire 集成)
    commands.rs       # PlanCommand 处理 (从 plan_interaction.rs 提取)
  plan_executor.rs    # 保留，微调
  plan_monitor.rs     # 保留，微调
  plan_runtime.rs     # 保留，微调
```

### 1. 可取消的 Plan 生成

新增 `PlanGenerator`，封装 LLM 调用 + 取消 + 超时：

```rust
// astra-cli/src/cli/plan/generate.rs

use tokio_util::sync::CancellationToken;

pub struct PlanGenerateConfig {
    pub request_timeout: Duration,      // post_chat_turn 超时, default 30s
    pub stream_timeout: Duration,       // SSE 流总超时, default 120s
    pub idle_timeout: Duration,         // SSE 无数据超时, default 30s
    pub max_retries: u32,               // JSON 解析失败重试, default 1
}

impl Default for PlanGenerateConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            stream_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(30),
            max_retries: 1,
        }
    }
}

pub enum GenerateOutcome {
    Plan(TaskPlan),
    Clarifications(Vec<ClarificationQuestion>),
    Cancelled,
    Failed(String),
}

/// 可取消的 plan 生成。
///
/// 在 REPL 调用侧:
/// ```
/// let cancel = CancellationToken::new();
/// tokio::select! {
///     result = generator.generate(&goal, &context, &cancel) => { ... }
///     _ = tokio::signal::ctrl_c() => {
///         cancel.cancel();
///         eprintln!("  Cancelled.");
///     }
/// }
/// ```
pub async fn generate_plan(
    api: &ThinClient,
    token: &str,
    goal: &str,
    context: &ProjectContext,
    config: &PlanGenerateConfig,
    cancel: &CancellationToken,
    on_progress: impl Fn(GenerateProgress),
) -> GenerateOutcome {
    // 1. post_chat_turn_timeout (request_timeout)
    // 2. collect_sse_cancellable (stream_timeout + idle_timeout + cancel)
    // 3. parse → 失败则 retry with correction prompt
    todo!()
}
```

关键：`collect_sse_cancellable` 替代 `collect_sse_with_preview`：

```rust
/// 可取消的 SSE 收集，带超时和进度回调。
pub async fn collect_sse_cancellable(
    resp: Response,
    cancel: &CancellationToken,
    stream_timeout: Duration,
    idle_timeout: Duration,
    on_chunk: impl FnMut(&str),  // 每个 text_delta 回调
) -> SseTextResult {
    let deadline = Instant::now() + stream_timeout;
    let mut last_data = Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                result.stream_error = Some("Cancelled by user".into());
                break;
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        last_data = Instant::now();
                        // ... process SSE events, call on_chunk for text_delta
                    }
                    Some(Err(e)) => { /* stream error */ break; }
                    None => break, // stream ended
                }
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                result.stream_error = Some("Stream timeout".into());
                break;
            }
            _ = tokio::time::sleep_until((last_data + idle_timeout).into()) => {
                result.stream_error = Some("Idle timeout".into());
                break;
            }
        }
    }
    result
}
```

### 2. 渐进式生成

两阶段 prompt 替代一次性 decomposition_prompt：

**Phase 1 — Outline（快速，3-5s）：**

```
你是高级软件架构师。根据以下目标和项目上下文，生成一个高层执行大纲。

目标: {goal}
项目: {context_summary}

输出 JSON:
{
  "phases": [
    {
      "id": "phase-1",
      "title": "...",
      "description": "...",
      "estimated_subtasks": 2-3,
      "key_files": ["..."]
    }
  ],
  "total_effort": "small|medium|large",
  "questions": []  // 如果需要澄清，放这里
}

规则:
- 2-4 个 phase，每个 phase 是一个逻辑阶段
- 不需要细化到具体 subtask
- 如果有不确定的地方，在 questions 里提问
```

用户看到 outline 后可以：
- 确认 → 进入 Phase 2
- 修改 → 重新生成 outline
- 跳过 → 直接一次性生成（兼容旧行为）

**Phase 2 — Detail（按 phase 逐个细化）：**

```
基于以下大纲的 phase "{phase_id}"，生成具体的 subtasks。

大纲: {outline_json}
当前 phase: {phase}
已完成的 phases: {completed_phases_summary}

输出 JSON:
{
  "subtasks": [...]  // 同现有格式
}
```

好处：
- 每次 LLM 调用更短更快
- 后续 phase 可以利用前面 phase 的执行结果
- 用户可以在 phase 之间调整方向

### 3. JSON 解析鲁棒性

三层防御：

```rust
// astra-plan/src/parse.rs

pub fn parse_plan_robust(text: &str) -> Result<TaskPlan, ParseError> {
    // Layer 1: 直接解析
    if let Ok(plan) = parse_plan_strict(text) {
        return Ok(plan);
    }

    // Layer 2: 提取 JSON 片段
    //   - strip markdown fences (```json ... ```)
    //   - 找最外层 { ... } 或 [ ... ]
    //   - 修复常见问题: trailing comma, 单引号, 注释
    if let Ok(plan) = extract_and_parse(text) {
        return Ok(plan);
    }

    // Layer 3: 宽松解析
    //   - 尝试 YAML 解析 (LLM 有时输出 YAML)
    //   - 尝试从 markdown list 结构化提取
    Err(ParseError::AllStrategiesFailed {
        original: text.to_string(),
        // 附带原文，供重试 prompt 使用
    })
}

pub struct ParseError {
    pub message: String,
    pub original: String,
    pub strategies_tried: Vec<String>,
}
```

重试机制（在 `generate_plan` 中）：

```rust
// 第一次解析失败后，用修正 prompt 重试
let retry_prompt = format!(
    "你之前的回复不是有效的 JSON。错误: {error}\n\
     请只输出 JSON，不要包含任何其他文本。\n\
     原始目标: {goal}"
);
```

### 4. 交互式协商

**Clarification 用 inquire：**

```rust
// astra-cli/src/cli/plan/input.rs

pub fn ask_clarification(q: &ClarificationQuestion) -> ClarificationAnswer {
    let options: Vec<String> = q.options.iter().enumerate()
        .map(|(i, opt)| {
            if q.default == Some(i) {
                format!("{opt} (default)")
            } else {
                opt.clone()
            }
        })
        .collect();

    // 添加自由输入选项
    let mut all_options = options;
    all_options.push("Other (type your answer)".into());

    match inquire::Select::new(&q.question, all_options)
        .with_render_config(plan_select_theme())
        .with_starting_cursor(q.default.unwrap_or(0))
        .prompt()
    {
        Ok(choice) if choice.starts_with("Other") => {
            match inquire::Text::new("Your answer:").prompt() {
                Ok(text) => ClarificationAnswer::Freeform(text),
                Err(_) => ClarificationAnswer::Invalid("Cancelled".into()),
            }
        }
        Ok(choice) => {
            let idx = q.options.iter().position(|o| choice.starts_with(o)).unwrap_or(0);
            ClarificationAnswer::Selected(idx)
        }
        Err(_) => ClarificationAnswer::Invalid("Cancelled".into()),
    }
}
```

**Outline 确认用 inquire：**

```rust
pub fn confirm_outline(phases: &[PlanPhase]) -> OutlineChoice {
    let options = vec![
        format!("✓  Looks good, detail all {} phases", phases.len()),
        "✏  Modify (describe changes)".into(),
        "⏭  Skip outline, generate full plan directly".into(),
        "✕  Cancel".into(),
    ];

    match inquire::Select::new("Plan outline:", options)
        .with_render_config(plan_select_theme())
        .prompt()
    {
        Ok(c) if c.starts_with('✓') => OutlineChoice::Confirm,
        Ok(c) if c.starts_with('✏') => OutlineChoice::Edit,
        Ok(c) if c.starts_with('⏭') => OutlineChoice::SkipToFull,
        _ => OutlineChoice::Cancel,
    }
}
```

**生成过程进度反馈：**

```rust
pub enum GenerateProgress {
    /// 项目扫描完成
    ProjectScanned { files: usize, languages: Vec<String> },
    /// 模板搜索完成
    TemplatesSearched { found: usize },
    /// LLM 请求已发送
    RequestSent,
    /// 收到第一个 token
    FirstToken,
    /// 流式接收中
    Streaming { tokens: usize, elapsed: Duration },
    /// 解析中
    Parsing,
    /// 解析失败，重试中
    RetryingParse { attempt: u32, error: String },
}
```

显示效果：

```
  ✓ Scanned project: Rust · 805 files · main
  ✓ Found 2 similar templates
  ⋯ Generating outline…  (3.2s, ~420 tokens)
  ✓ Outline ready

  Phase 1: Setup project structure
    ~2 subtasks · files: src/lib.rs, src/main.rs

  Phase 2: Implement core logic
    ~3 subtasks · files: src/handler.rs, src/model.rs

  Phase 3: Testing & verification
    ~2 subtasks

  ▸ Plan outline:
  > ✓  Looks good, detail all 3 phases
    ✏  Modify (describe changes)
    ⏭  Skip outline, generate full plan directly
    ✕  Cancel
```

### 5. 状态机驱动

让 `PlanPhase` 状态机真正驱动流程，而不是 `plan_interaction.rs` 里的 ad-hoc 逻辑：

```rust
// astra-cli/src/cli/plan/mod.rs

pub struct PlanController {
    phase: PlanPhase,
    generator: PlanGenerator,
    display: PlanDisplay,
}

impl PlanController {
    /// 处理用户输入，返回下一步动作。
    pub async fn handle_input(
        &mut self,
        input: &str,
        cancel: &CancellationToken,
    ) -> PlanInputResult {
        match &self.phase {
            PlanPhase::Idle => self.handle_idle(input).await,
            PlanPhase::Planning { .. } => self.handle_planning(input, cancel).await,
            PlanPhase::Refining { .. } => self.handle_refining(input, cancel).await,
            PlanPhase::Executing { .. } => self.handle_executing(input).await,
            PlanPhase::Paused { .. } => self.handle_paused(input).await,
            _ => PlanInputResult::Handled,
        }
    }

    /// 状态转换 — 所有转换通过这里，保证合法性。
    fn transition(&mut self, action: PlanAction) -> Result<(), PlanTransitionError> {
        self.phase = self.phase.take().transition(action)?;
        Ok(())
    }
}
```

### 6. 新增 Outline 数据结构

```rust
// astra-plan/src/decompose.rs (或新文件 outline.rs)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanOutline {
    pub phases: Vec<OutlinePhase>,
    pub total_effort: String,
    pub questions: Vec<ClarificationQuestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutlinePhase {
    pub id: String,
    pub title: String,
    pub description: String,
    pub estimated_subtasks: usize,
    pub key_files: Vec<String>,
}
```

## 实施计划

### Phase 1: 基础设施 (P0 修复) ✅
1. ✅ 实现 `collect_sse_cancellable` — 可取消、有超时的 SSE 收集
2. ✅ `handle_goal_submission` 中用 `tokio::select!` + `ctrl_c()` 包裹 LLM 调用
3. ✅ `post_chat_turn` 调用改用 `post_chat_turn_timeout`
4. ✅ 所有 plan LLM 调用路径加超时

### Phase 2: JSON 鲁棒性 ✅
5. ✅ 实现 `extract_json_robust` 多层解析（trailing commas, comments）
6. ✅ 生成失败自动重试（`plan_generate_with_retry` 带修正 prompt）
7. ✅ `parse_plan_response` 自动使用 robust 解析

### Phase 3: 渐进式生成 ✅
8. ✅ 新增 `PlanOutline` / `OutlinePhase` 数据结构（`astra-plan/src/outline.rs`）
9. ✅ 实现两阶段 prompt（`outline_prompt` → `phase_detail_prompt`）
10. ✅ `handle_goal_submission` 先生成 outline → 用户确认 → 逐 phase 展开
11. ✅ 优雅降级：outline 解析失败自动回退到一次性全量生成

### Phase 4: 交互优化 ✅
12. ✅ Clarification 改用 `inquire::Select`（`ask_clarification_interactive`）
13. ✅ Outline 确认用 `inquire::Select`（`prompt_outline_confirmation`）
14. ✅ Phase 展开过程分阶段进度反馈（每个 phase 显示进度）
15. ✅ 取消时保留已完成 phase 的 subtasks

### Phase 5: 代码重构 ✅
16. ✅ 新增 `outline.rs` 模块（独立于 267KB 的 decompose.rs）
17. ✅ 提取 `accept_generated_plan` / `expand_outline_to_plan` / `handle_outline_clarifications` 独立函数
18. ✅ `handle_goal_submission` 从 ~100 行 monolith 重构为清晰的阶段调度

## 不做的事

- 不改 plan executor / plan monitor（执行阶段已经有 Ctrl-C 处理）
- 不改 TaskPlan / SubtaskPlan 数据结构（向后兼容持久化）
- 不改 plan 命令解析（PlanCommand::parse 已经够好）
