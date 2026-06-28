# Permission Policy 重构方案（breaking design）

Status: implementation-aligned v2  
Date: 2026-06-28  
Scope: permission modes, approval prompts, risk classification, `auto`, long-running autonomous execution

## 结论

不兼容旧行为后，最好的用户体验是：

```text
auto = 放手跑，不再问我工具授权。
```

`auto` 必须是最放松的长期运行模式。它不应该在 `bash`、git、敏感路径、explicit approval gate、sub-agent mailbox 上突然弹出授权确认。

风险检测继续存在，但它的输出是：

```text
audit + telemetry + model-visible hint
```

而不是：

```text
approval prompt / blocked tool / hidden retry trap
```

内部可以把这个工具审批策略叫 `SkipPermissions`，但用户不应该需要理解 `skip_permissions` 这个额外模式名。用户只需要知道：`auto` 就是不打断。

唯一例外是结构性不可执行条件：未知工具、无效 schema、runtime 能力缺失、静态危险命令或明确 admin guardrail。这些不是“需要授权”，而是 runtime 无法或不应执行，必须 deterministic deny 并给出具体原因。

## 当前问题

现在 `auto` 仍会弹 approval：

```text
Approval · Bash
Risk · Bash · Git destructive
Git safety: cd + git compound command blocked, git -c flag blocked
Yes / Yes, and don't ask again / No
```

这说明现有 `auto` 同时承担了多个互相冲突的含义：

```text
Tool approval policy       工具是否需要用户授权
Human interaction policy   是否允许 ask_user 打断用户
Risk classification        工具调用有什么风险
Safety execution policy    runtime 是否真的能执行
```

结果是用户无法预测：

- 大多数工具自动执行；
- 某些风险突然弹窗；
- 子 agent 可能转而请求 parent approval；
- 模型看到错误文案后可能误以为工具被禁；
- “Yes, and don't ask again” 又引入额外规则状态。

长期任务最怕这种半自动：不是严格安全，也不是可靠自主。

## 第一性原则

权限系统应该回答一个问题：

```text
这个模式下，runtime 是否需要人类授权才能执行这个工具？
```

风险系统回答另一个问题：

```text
这个工具调用有哪些风险，应该如何记录和提示？
```

二者不能混为一谈。

核心不变量：

1. `auto` 不产生 approval prompt。
2. `auto` 不等待用户点 Yes。
3. `auto` 不把工具加入 restricted tools。
4. `auto` 不向 parent mailbox 请求 permission。
5. 风险检测不关闭，但风险不升级成授权阻塞。
6. 工具失败只能来自真实执行失败、schema 无效、工具不存在、runtime 能力缺失，不能来自“需要人类授权”。
7. LLM 可见文案不得暗示工具被禁用，除非工具确实不存在或当前 mode 明确是 `ci`/只读。

## Canonical modes

破坏式重构后，持久化和 CLI 接受的模式只保留五个 canonical strings：

| Mode | 工具授权 | ask_user | 目标体验 |
| --- | --- | --- | --- |
| `ask` | 需要时问用户 | 真问用户 | 最大控制 |
| `edits` | workspace-local edits 自动，其它问用户 | 真问用户 | 日常编码 |
| `plan` | 只读/计划，不执行 mutation | 交互 shell 中真问用户 | 研究和方案 |
| `auto` | 不问工具授权，直接执行 | `auto_unanswered` | 长期自主运行 |
| `ci` | mutation/execute deterministic deny | hidden | 无人值守 fail-closed |

移除或迁移旧用户名：

- `prompt` -> `ask`
- `accept_edits` -> `edits`
- `deny` -> `ci`
- `skip_permissions` 不作为用户模式暴露，它只是 `auto` 的内部工具审批策略。

旧 alias 不做兼容解析。拒绝旧名字比静默迁移更干净，因为权限模式必须是用户能准确预测的执行契约。

## Policy 分层

`PermissionMode` 不应继续作为权限逻辑本体。它只应该是 user-facing preset。

```rust
pub struct RuntimePolicy {
    pub tool_approval: ToolApprovalPolicy,
    pub human_interaction: HumanInteractionPolicy,
    pub risk_handling: RiskHandlingPolicy,
    pub execution_scope: ExecutionScopePolicy,
}

pub enum ToolApprovalPolicy {
    PromptForMutations,
    AcceptWorkspaceEdits,
    PlanReadOnly,
    SkipPermissions,
    DenyMutations,
}

pub enum HumanInteractionPolicy {
    PromptUser,
    AutoUnanswered,
    Unavailable,
}

pub enum RiskHandlingPolicy {
    PromptOnRisk,
    DenyOnRisk,
    WarnOnly,
}

pub enum ExecutionScopePolicy {
    Normal,
    ReadOnly,
    FailClosed,
}
```

Preset 映射：

```text
ask:
  tool_approval = PromptForMutations
  human_interaction = PromptUser
  risk_handling = PromptOnRisk
  execution_scope = Normal

edits:
  tool_approval = AcceptWorkspaceEdits
  human_interaction = PromptUser
  risk_handling = PromptOnRisk
  execution_scope = Normal

plan:
  tool_approval = PlanReadOnly
  human_interaction = PromptUser
  risk_handling = DenyOnRisk
  execution_scope = ReadOnly

auto:
  tool_approval = SkipPermissions
  human_interaction = AutoUnanswered
  risk_handling = WarnOnly
  execution_scope = Normal

ci:
  tool_approval = DenyMutations
  human_interaction = Unavailable
  risk_handling = DenyOnRisk
  execution_scope = FailClosed
```

`plan` 的 human interaction 还需要 host capability 约束：有 TTY、非 silent、存在 ask_user sink 时为 `PromptUser`，否则为 `Unavailable`。这不是权限降级，而是 transport 能力事实。

## Permission engine 行为

当前 engine 的 `HardDecision` 把授权、风险、deny 混在一起：

```rust
pub enum HardDecision {
    Allow,
    Deny { reason: String },
    NeedExternal { prompt: ApprovalPrompt },
}
```

重构后，permission evaluation 应输出完整 envelope：

```rust
pub struct PermissionEvaluation {
    pub decision: ExecutionDecision,
    pub risk_tags: Vec<RiskTag>,
    pub risk_notes: Vec<String>,
    pub trace: Vec<DecisionTraceStep>,
}

pub enum ExecutionDecision {
    Allow,
    Deny { reason: String },
    NeedUserApproval { prompt: ApprovalPrompt },
}
```

规则：

- `NeedUserApproval` 只能由 `ToolApprovalPolicy` 产生。
- `RiskHandlingPolicy::WarnOnly` 不允许产生 `NeedUserApproval`。
- `auto` 下所有 git safety、sensitive path、explicit approval gate 都降级为 `risk_notes`。
- `auto` 下 deny 只允许用于结构性不可执行条件：unknown tool、invalid schema、runtime capability missing、static dangerous command、explicit admin guardrail。
- `plan`/`ci` 下 deny 是确定性结果，不弹 approval。

测试不变量：

```text
permission_mode == auto => NeedUserApproval is a bug
```

这条不变量应该通过 engine/unit tests 和 end-to-end permission sync tests 同时覆盖。

## Git safety

Git safety 继续检测，但不在 `auto` 下阻塞。

当前例子：

```bash
cd /home/xupeng/astra && git diff origin/main...HEAD --stat | awk ...
```

重构后：

```text
ask / edits:
  可以弹 approval，因为用户选择了交互控制。

plan:
  mutation/execute deny；read-only git command 可按 read-only 策略允许。

auto:
  不弹 approval。
  记录 risk note。
  执行命令。
```

history/audit 示例：

```text
Risk · Bash · GitDestructive
Git safety: cd + git compound command; git -c config risk
Policy: auto -> executed without approval
```

不要再展示：

```text
Yes
Yes, and don't ask again
No
```

## Explicit allow/deny rules

破坏式重构后，不要让 “Yes, and don't ask again” 成为核心路径。

原因：

- 它把一次 UI 选择变成长期规则状态；
- 它让模型和用户都难以预测下一次是否会弹；
- 它不能解决长期自主运行，反而制造规则债务。

建议：

- `ask`/`edits` 可以保留一次性 approval；
- 长期规则必须通过显式配置 UI/命令管理；
- `auto` 不读取 approval memory 来决定是否弹窗，因为它本来就不弹窗；
- explicit deny/admin guardrail 可以 deterministic deny，但文案必须清楚说明是用户配置的 guardrail，不是工具坏了。

## Sub-agent / mailbox

`auto` 必须跨 agent 继承。

规则：

- parent 是 `auto`，child 也是 `auto`，除非用户显式创建更严格 child policy；
- child 不向 parent mailbox 请求 permission approval；
- child 本地执行 permission evaluation，risk 进入 child trace 并汇总给 parent；
- parent 只接收结果、风险摘要和失败原因，不处理 approval 队列。

## ask_user

`auto` 下 `ask_user` 可见，但返回 `auto_unanswered`。

这与工具授权是两个独立维度：

```text
Tool approval:
  auto -> SkipPermissions -> no approval prompt

Human interaction:
  auto -> AutoUnanswered -> no user prompt
```

不要通过隐藏 `ask_user` 来实现无人值守。隐藏会让模型无法表达不确定性。

## LLM-visible 文案

权限层输出给模型的文案必须避免错误心智模型。

禁止：

```text
tool disabled
tool banned
blocked duplicate
permission denied
can't remember
```

推荐：

```text
Policy: auto. Risk was recorded and did not block execution.
Policy: plan. Mutation was not executed because this mode is read-only.
Policy: ci. Tool requires mutation/execute capability, which is unavailable in this preset.
```

工具失败计数也只能作为 health hint，不得暗示 tool ban。模型需要学到的心智模型是：

```text
The tool is available. The previous invocation failed. Change arguments, inspect evidence, or use a different validation path.
```

不要把失败累积转换成 hidden restriction。工具到底是“用错了”还是“坏了”通常只有模型通过新证据才能判断。

## TUI

`auto` 下不创建 approval card。

风险展示应是非阻塞 history item：

```text
Risk · Bash
Policy · auto executed without approval
```

状态栏应清楚显示：

```text
Auto
```

tooltip/copy：

```text
Runs tools without approval prompts. Clarification requests are recorded as unanswered.
```

## 测试覆盖

必须补齐 unhappy path：

1. `auto` 下 hard git violation 不产生 `NeedUserApproval`。
2. `auto` 下 sensitive path 不产生 approval prompt，风险进入 audit。
3. `auto` 下 explicit approval gated tool 不产生 approval prompt。
4. `auto` 下 sub-agent 不发 mailbox permission request。
5. `auto` 下 TUI 不创建 approval card。
6. `auto` 下 risk history item 正常渲染。
7. `ask` 下同一 git risk 仍能产生 approval prompt。
8. `edits` 下 workspace-local edit 自动，bash 仍可 prompt。
9. `plan` 下 mutation/execute deterministic deny，不 prompt。
10. `ci` 下 ask_user hidden，mutation/execute deterministic deny。
11. LLM-visible 文案不包含 `disabled` / `banned` / `blocked duplicate` / `can't remember`。
12. Parent `auto` 下 child 不请求 permission approval。
13. `prompt`/`accept_edits`/`deny` 被拒绝，不静默映射。
14. `ci` 作为 canonical fail-closed preset 可解析、可持久化、可恢复。
15. static dangerous command 在 `auto` 下仍 deterministic deny，且文案说明是 guardrail，不是 approval。

## 实施顺序

1. 定义 `RuntimePolicy` 和四个 policy 维度。
2. 把 user-facing mode 解析成 preset，不再让 mode 直接参与散落的条件判断。
3. 将旧 `PermissionMode::{Prompt, AcceptEdits, Deny}` 迁移为 `{Ask, Edits, Ci}`。
4. 把 `auto` 映射为 `ToolApprovalPolicy::SkipPermissions`。
5. 修改 `evaluate_permission`：risk collection 与 approval decision 分离。
6. 修改 approval sink：`auto` 下任何 `NeedUserApproval` 都是 bug，测试中 panic/fail。
7. 修改 sub-agent permission sync：`auto` 不走 mailbox approval。
8. 修改 TUI：`auto` 下渲染 risk history，不渲染 approval card。
9. 修改 ask_user：`auto` 下 structured `auto_unanswered`。
10. 删除或迁移旧 alias、旧 mode enum、旧 snapshot 名称。
11. 删除或迁移 “Yes, and don't ask again” 作为核心 flow 的依赖测试。

## 验收标准

大型重构任务在 `auto` 下运行时：

- 不出现工具授权弹窗；
- 不需要用户点 Yes；
- 子 agent 不因权限请求卡住；
- git/sensitive/explicit risk 只记录、不阻塞；
- 模型不会误以为 bash/edit/read_file 被禁用；
- 工具失败来自真实执行失败，而不是 permission approval；
- 最终输出能列出执行过的高风险动作和关键假设。
