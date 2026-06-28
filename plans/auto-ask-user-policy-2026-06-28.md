# Auto 模式 ask_user 策略（breaking design）

Status: implementation-aligned v2  
Date: 2026-06-28  
Scope: `ask_user`, human interaction policy, auto-mode UX, long-running agent reliability

## 结论

最佳用户体验下，`auto` 的契约必须简单：

```text
用户已经授权 agent 自主推进。不要弹窗，不要等待，不要伪造用户回答。
```

因此 `ask_user` 在 `auto` 下应该仍然可见，但调用后必须立即返回结构化的 `auto_unanswered`。这让模型可以表达不确定性，同时不会中断用户。

不要做兼容旧行为的折中：

- 不要在 `auto` 下隐藏 `ask_user`；
- 不要在 `auto` 下弹用户确认；
- 不要自动选择第一个 option；
- 不要返回空字符串冒充用户答案；
- 不要把 `ask_user` 调用失败写成 “tool disabled / unavailable”。

## 第一性定义

`ask_user` 是一条信息通道：

```text
LLM -> Human -> LLM
```

它表达的是模型遇到了偏好、需求、业务约束上的不确定性。这个信号本身有价值，即使当前模式不允许打断用户。

`auto` 是一个执行契约：

```text
LLM owns the next decision.
Runtime must not interrupt.
Runtime must not pretend the user answered.
```

所以 `auto` 下的正确行为不是“不能问”，而是“可以声明想问，但 runtime 明确告诉模型：现在没有人类答案，请你自行决策”。

## Canonical modes

不兼容旧行为后，交互语义收敛成五个 canonical persisted strings：

| Mode | ask_user 可见 | 调用行为 | 用户是否被打断 |
| --- | --- | --- | --- |
| `ask` | yes | prompt user | yes |
| `edits` | yes | prompt user | yes |
| `plan` | yes in interactive shells | prompt user when interactive | yes when interactive |
| `auto` | yes | return `auto_unanswered` | no |
| `ci` | no | hidden | 无人值守 fail-closed |

说明：

- `auto` 是最放松的长期运行模式：模型可以声明想问，但 runtime 不打断用户。
- `ask`/`edits`/`plan` 在有 TTY 和 ask_user sink 时保留真实人类澄清能力。
- `plan` 在 headless/non-interactive 运行中降级为 unavailable，而不是伪造用户回答。
- `ci` 是无交互环境，不应暴露没有响应语义的工具。
- 旧的 `prompt`/`accept_edits`/`deny` 不再作为兼容 alias 接受；需要显式使用 `ask`/`edits`/`ci`。
- `Prompt`/`Headless`/`NonInteractive` 只允许作为内部 transport/host capability，不允许作为用户心智模型。

## Runtime policy

不要再用 `allows_ask_user()` 一个布尔值推导所有行为。需要显式 policy：

```rust
pub struct HumanInteractionPolicy {
    pub ask_user_behavior: AskUserBehavior,
}

pub enum AskUserBehavior {
    PromptUser,
    AutoUnanswered,
    Hidden,
}
```

工具 schema 的可见性由 behavior 决定：

```rust
impl AskUserBehavior {
    pub fn visible(self) -> bool {
        matches!(self, Self::PromptUser | Self::AutoUnanswered)
    }

    pub fn can_pause(self) -> bool {
        matches!(self, Self::PromptUser)
    }
}
```

核心不变量：

```text
visible != can_pause
```

这是从 Claude Code 可借鉴的部分：工具可见性和是否弹交互 UI 是两个独立维度。Astra 不应照搬 Claude Code 的空答案语义。

实现约束：

- `AutoUnanswered.visible() == true`，否则模型会失去表达不确定性的结构化通道。
- `AutoUnanswered.can_pause() == false`，否则 `auto` 的长期运行契约被破坏。
- `Hidden` 只用于 `ci`、headless/non-interactive、缺少 sink 的 transport。
- 所有 envelope 都必须是成功 tool result；只有 schema/transport/runtime bug 才能返回 tool error。

## Auto result envelope

`auto` 下 `ask_user` 调用必须成功返回非错误结果：

```json
{
  "status": "auto_unanswered",
  "source": "runtime_policy",
  "answers": [],
  "reason": "auto mode does not interrupt the user",
  "instruction": "Continue with best judgment. Prefer reversible actions, use conservative defaults for irreversible decisions, and state material assumptions in the final response."
}
```

语义：

- 这不是用户回答；
- 这不是取消；
- 这不是 timeout；
- 这不是工具错误；
- 这不是 permission denial；
- 这是 runtime 明确表达“无人类输入可用”的结果。

返回值必须是结构化 envelope，而不是空字符串。空字符串无法区分：

- 用户真的输入了空值；
- runtime 没有打断用户；
- UI 通道失败；
- tool implementation bug。

## User-answer result envelope

真实用户回答也使用同一 envelope：

```json
{
  "status": "user_answered",
  "source": "user",
  "answers": [
    {
      "question_id": "framework",
      "answers": ["React"]
    }
  ]
}
```

取消、超时、交互错误也必须有明确状态：

```text
cancelled
timeout
interaction_error
```

不要把这些折叠成普通 tool error；否则模型会误判工具被禁或运行时坏了。

## Anti-patterns

### 隐藏 ask_user

隐藏会让模型失去表达不确定性的结构化通道。它只能猜、放弃，或者在普通文本里问用户。这会把“不打断用户”错误实现成“不允许模型承认不确定”。

### 自动选择第一个 option

第一个 option 只是 schema 顺序，不是用户偏好。自动选择它会制造假确定性，并且让模型可以通过 option 排序间接替用户做决定。

### 返回空答案

空答案不是 provenance。长期 session 中，模型会把它理解成“用户没意见”或“默认通过”，这是错误心智模型。

### 用错误表示 auto_unanswered

`auto_unanswered` 是正常 policy 结果，不是错误。返回 error 会让模型以为 `ask_user` 被禁、通道坏了，或者自己不应该再表达不确定性。

## 防空转

`auto` 下 `ask_user` 可见，所以必须防止模型反复问同一个问题。

同一 turn 内，按以下字段生成 fingerprint：

- question text；
- options；
- allow_freeform；
- selection cardinality；
- topic/category if present。

同一 fingerprint 第二次调用返回：

```json
{
  "status": "auto_unanswered_duplicate",
  "source": "runtime_policy",
  "answers": [],
  "reason": "same clarification was already unavailable in this turn",
  "instruction": "Do not call ask_user again for this question. Decide with stated assumptions, gather factual evidence, or stop with a concrete blocker."
}
```

规则：

- duplicate 仍是非错误结果；
- duplicate guard 是 per-turn 状态，进入下一 turn 后重置；
- 不要把 `ask_user` 加入 restricted tools；
- 不要生成 “blocked duplicate” 这种会让模型误解工具不可用的文案；
- 跨 turn 可以重新问一次，但 TurnGuard 应记录 stale clarification pressure。

如果同一个 session 长期出现 `auto_unanswered`，runtime 应把它当作 stale uncertainty signal，而不是禁用工具。正确引导是让模型收敛为：

```text
state assumptions -> choose reversible/default path -> verify factual evidence -> report unresolved preference
```

## Evidence 规则

`ask_user` 不是 factual evidence。

它不应计入：

- evidence tool count；
- progress proof；
- context freshness proof；
- outcome confidence 的事实来源。

它只应计入：

- uncertainty signal；
- user preference signal；
- blocker/assumption audit。

## Events

事件命名必须区分是否真的提示了用户：

```text
ask_user_prompted
ask_user_submitted
ask_user_cancelled
ask_user_timeout
ask_user_auto_unanswered
ask_user_auto_duplicate
```

`auto` 下不要记录 `ask_user_prompted`，因为没有真的提示用户。

## UI

`auto` 下不要出现弹窗，也不要在 TUI 里展示待处理 approval。

可以在 history 中用非阻塞记录：

```text
AskUser · auto unanswered
Question: Which deployment target should be used?
Policy: auto -> continued without interrupting the user
```

这条记录用于审计，不用于阻塞 agent。

## 测试覆盖

必须覆盖 unhappy path：

1. `auto` 下 `ask_user` 在 schema 中可见。
2. `auto` 下调用 `ask_user` 不访问 `AskUserGate`。
3. `auto` 下返回 `auto_unanswered`，且不是 tool error。
4. `auto` 下重复同一问题返回 `auto_unanswered_duplicate`。
5. duplicate guard 在新 turn 重置。
6. `auto_unanswered` 不计入 factual evidence。
7. `ask`/`edits`/interactive `plan` 下仍能真实提示用户。
8. headless/non-interactive `plan` 与 `ci` 下 `ask_user` 隐藏。
9. 所有 auto 文案不得包含 `disabled` / `banned` / `not available` / `permission denied`。
10. `ask_user_auto_unanswered` 不生成 pending approval card。
11. 长 session 中 stale clarification pressure 会进入 health telemetry，但不会禁用工具。

## 实施顺序

1. 定义 `AskUserBehavior` 并从 mode preset 解析。
2. 用 `ask_user_behavior().visible()` 替换布尔 `allows_ask_user()` 决策。
3. 修改 tool schema restriction：`AutoUnanswered` 下不隐藏 `ask_user`。
4. 修改 `execute_ask_user`：`AutoUnanswered` 直接返回 structured envelope。
5. 加 per-turn duplicate fingerprint guard。
6. 修改 events 和 history rendering。
7. 修改 TurnGuard：记录 clarification pressure，不禁用工具。
8. 补齐 runtime/TUI/CLI tests。

## 验收标准

在 `auto` 下：

- 模型能调用 `ask_user` 表达不确定性；
- 用户不会被打断；
- runtime 不会伪造答案；
- 模型不会收到“工具被禁用”的误导；
- 重复 ask_user 不会形成循环；
- 最终回复能清楚列出关键假设或 blocker。
