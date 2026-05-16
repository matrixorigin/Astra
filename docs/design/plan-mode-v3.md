# Plan Mode V3 — 当前产品设计、工作方式与测试评估

> 本文档描述**当前分支真实存在**的 plan 产品行为与实现边界，不记录已经删除的旧兼容层，也不把“理想未来态”写成现状。
>
> 结论先行：
> - **CLI local** 的用户入口已经收敛为 **`/plan`**。
> - **`/plan <description>`** 与“先输入 `/plan` 再给第一句目标”在本地 CLI 上都会先创建/link cloud plan，再把目标作为正常 chat turn 发出去。
> - **Web agent / thin remote CLI** 仍然通过 server-side 的 `enter_plan_mode` / `exit_plan_mode` 工具驱动 plan 生命周期。
> - **CLI local 与 server/web 的用户入口不同，但底层都收敛到 cloud plan lifecycle**；本地只多了一层 client-backed wrapper 和 `plan>` UI mirror。

---

## 1. 设计目标

Plan 不是“第二个聊天模式集合”，而是一个**先对齐、再执行**的约束机制。当前设计追求四件事：

1. **用户入口最小化**：本地 CLI 只保留 `/plan`，避免 `/plan status`、`/plan show`、`/plan enter` 这类产品面噪音。
2. **执行面清晰**：在哪个进程执行工具，就只暴露那个执行面能真的执行的工具。
3. **用户旅程顺滑**：进入 plan 后，用户既可以显式输入命令，也可以直接说自然语言目标或改动意见。
4. **高风险工作先对齐**：plan 用于复杂实现、架构选择、跨文件改动、授权/数据流等高影响任务，不是所有任务都强制先 plan。

---

## 2. 三种执行 surface

当前代码里真正重要的边界不是“Web vs CLI”，而是**执行发生在哪里**。

| Surface | 面向谁 | 工具在哪里执行 | 可见工具 |
|---|---|---|---|
| `CliLocal` | 本地 `astra` CLI | 用户本机 edge 进程 | client builtin + client MCP |
| `CliRemote` | thin/remote CLI | API server | server builtin + server MCP |
| `Web` | Web agent | API server | server builtin + server MCP |

实现入口在 `runtime/src/capabilities.rs`：

- `CapabilitySurface::CliLocal`
- `CapabilitySurface::CliRemote`
- `CapabilitySurface::Web`

对应工具面解析：

- `cli_local_tool_schemas(...)`
- `cli_remote_tool_schemas(...)`
- `server_runtime_tool_schemas()`

### 2.1 Edge-cloud（CLI local）方式

这是现在本地 CLI 的主路径：

1. 用户在本地终端输入 `/plan`
2. 本地 CLI 先进入一个轻量 `plan>` UI / pending 状态
3. 首个 goal 或 `/plan <description>` 会通过 `/plans` 把 plan mode 链接到 cloud session
4. 之后的规划相关 LLM 请求仍然发往云端 API
5. 但真正的工具执行发生在**本地 edge executor**
6. 所以本地 CLI 可以安全地操作当前 checkout、shell、本地 MCP、本地技能目录

这是一种**云端推理 + 本地执行**的混合模式。

### 2.2 Web-agent 方式

Web agent 基本是**全 server-side**：

1. 浏览器只承载 UI
2. 会话状态和 plan 状态保存在服务端
3. 工具由 `ServerToolExecutor` 执行
4. plan 生命周期通过 `enter_plan_mode` / `exit_plan_mode` 工具驱动

这里没有本地 edge executor 可用，所以不能假装它有 CLI local 的能力。

### 2.3 为什么要区分 server-only / edge-only

这是**执行真相**，不是命名游戏。

- **edge-only**：只有本地 CLI 进程能做，例如操作当前仓库、本地 shell、本地 MCP。
- **server-only**：只有 API server 能做，例如 Web/remote 会话里的 server executor 生命周期工具。

这轮清理删掉的，不是 plan 能力本身，而是**旧的本地 one-shot `/plan` fallback 与假执行面**：

- 删除了本地 `plan_mode_client`
- 删除了 `PlanOnlyChat` / `chat_plan_only`
- 删除了 `slash_memory.rs` 里那条旧 `/plan` 本地分解 / plan_interaction fallback 链
- 保留并重新引入了 CLI local 的 client-backed `enter_plan_mode` / `exit_plan_mode` wrapper，用来把本地会话对齐到 cloud `/plans` 生命周期

这样做的直接收益是：

1. 本地 `/plan` 不再和旧的本地分解循环打架
2. 本地 `tool_search(select:...)` 不再返回本地根本无法执行的 server-only 工具
3. 用户不会再触发那条“看起来像新 `/plan`，实际上还在走旧 REPL /plan”的假 surface

如果继续删掉 server-only 那套，会直接影响：

- Web agent 的 plan 生命周期
- remote/thin CLI 的 plan 生命周期
- `exit_plan_mode(approved=true)` 后的 server-side 任务播种与解锁逻辑

所以当前合理边界是：**删掉本地伪兼容，不删掉 server 的真实执行路径**。

---

## 3. 当前用户可见行为

## 3.1 CLI local：`/plan` 是唯一 slash 入口

用户可见语义现在只有两种：

| 输入 | 行为 |
|---|---|
| `/plan` | 进入 pending `plan>`；若当前已有活跃 plan，则 approve + 退出 |
| `/plan <description>` | 立即 link cloud plan mode，并把该描述作为首个规划 turn 发送 |

文档入口见 `docs/reference/slash-commands.md`。

当前**没有**用户可见的：

- `/plan decompose`
- `/plan enter`
- `/plan auto`
- `/plan status`
- `/plan pause`
- `/plan exit`
- `/plan show`

这些都已经从 slash surface 删除。

## 3.2 `plan>` 内的交互

进入 `plan>` 后，用户输入会被 `handle_plan_mode_input(...)` 处理。它的分流规则是：

1. **先尝试结构化命令**
   - `go` / `execute`
   - `step`
   - `status`
   - `show`
   - `pause`
   - `resume`
   - `cancel`
   - `help`
2. **否则处理 entry choice**
   - 新建目标
   - 继续当前 plan
   - 重开
   - 退出
3. **否则当作自然语言改 plan**
4. **若 plan 正在暂停/等待恢复时**：
   - `/slash` 会回派到主 REPL
   - 普通文本会放弃当前 paused plan，回到普通聊天

这意味着 `plan>` 是一个**半命令、半自然语言**的交互面，而不是纯命令模式。

---

## 4. `/plan` 与“隐式触发”的真实关系

这里要分清两个“隐式”概念。

### 4.1 仍然存在的隐式：进入后首句目标

这是当前本地 CLI 的真实产品行为：

1. 用户输入 `/plan`
2. CLI 展示 entry card
3. 用户直接输入一句自然语言目标
4. 该输入走 `handle_plan_mode_input(...)`
5. 再进入 `handle_goal_submission(...)`

而 `/plan <description>` 的实现也是：

1. 若当前还没 `plan_mode`
2. 先初始化 `PlanModeState`
3. 再把 `<description>` 直接传给 `handle_plan_mode_input(...)`

**所以这两个用户旅程在 CLI local 上已经对齐**：

- `/plan` → 再输入首句目标
- `/plan <description>`

它们最终都会落到同一个 `handle_goal_submission(...)`。

### 4.2 不再是当前产品主路径的隐式：全局 auto-suggest

`astra-plan/src/decompose.rs` 里仍然有：

- `should_suggest_plan_mode(...)`
- `classify_plan_suggestion(...)`
- `PlanKind::{Executable, Analytical}`

但从当前 CLI 调用关系看，**本地 CLI 已经没有一个用户可见的“全局自动弹出 Enter plan mode?” 主路径**。分类逻辑现在主要用于：

1. 在 `/plan` 内部区分 **executable** vs **analytical**
2. 为 journal 元数据打 `kind`

这点非常重要：**实现里还保留“是否建议 plan”的分类器，并不等于产品上仍有 auto-suggest 入口**。

### 4.3 设计判断

这是合理的收敛：

- 显式 `/plan` 的语义更稳定，也更像 Claude Code
- 避免了“普通聊天时被突然打断，询问要不要进入 plan”的 UX 噪音
- 代价是：系统不会主动帮用户切进 plan，需要用户自己判断

当前代码里的分类器因此更像**内部能力**，不是**产品承诺**。

---

## 5. CLI local 的完整用户旅程

## 5.1 旅程 A：`/plan`

```text
用户输入 /plan
  -> 若已有 saved plan，则恢复并显示
  -> 否则分析当前项目，创建空的 PlanModeState
  -> 显示 plan> 与 entry card
  -> 用户输入目标
  -> handle_goal_submission(goal)
```

### UX 评价

优点：

- 入口简单
- 恢复态可见
- 不强迫用户一开始就写完整目标

问题：

- `/plan` 同时承担“进入”和“退出”语义，虽然简单，但第一次上手时略隐式
- 若用户只记得 slash，不记得 `plan>` 内部命令，仍有轻微学习成本

## 5.2 旅程 B：`/plan <description>`

```text
用户输入 /plan 实现认证系统
  -> 初始化 PlanModeState（若未初始化）
  -> 直接把描述传给 handle_plan_mode_input(...)
  -> handle_goal_submission(goal)
```

### UX 评价

这是当前本地 CLI **最顺滑** 的 plan 入口。它满足：

- 单次输入即可进入并开始规划
- 与 Claude Code 的 `/plan [description]` 语义更接近
- 不需要用户先理解 entry card 再补一句目标

## 5.3 旅程 C：`plan>` 中自然语言改 plan

```text
plan> 把数据库迁移拆得更细一点
  -> 非结构化命令
  -> 构造 plan_mode_prompt
  -> 发送给 LLM
  -> 尝试用 JSON 更新 plan
  -> 保存 PlanModeState
```

### UX 评价

这条路径很合理，因为它允许用户把 plan 当作**可编辑草稿**，而不是只能用命令式接口微调。

## 5.4 旅程 D：分析型问题

若 goal 被 `classify_plan_suggestion(...)` 判断为 `Analytical`，CLI local 不走 subtasks executor，而走：

```text
goal
  -> handle_analytical_goal(...)
  -> analytical_prompt
  -> 生成 ResearchPlan
  -> 渲染为研究/评估问题
  -> 退出 plan_mode，回到普通 chat
```

### UX 评价

这是当前设计里很合理的一点：

- “评估 / 比较 / 权衡”不应该硬塞进可执行 subtasks
- 分析型 plan 的产物本来就应该是“问题与研究路径”，不是待执行任务树

但它也带来一个认知点：

- 用户输入 `/plan` 并不总会得到“可执行计划”
- 有时得到的是一次性的研究计划，然后直接退出回普通 chat

这需要文档明确，否则用户会误以为 plan 一定会进入持续编辑态。

## 5.5 旅程 E：执行与放弃

生成 plan 后，用户可在 `plan>` 里：

- `go` / `execute`
- `step`
- `pause`
- `resume`
- `cancel`

当 plan 处于 paused / executor-active 态时：

- 输入 `/help` 等 slash：**不放弃 plan**，回派到 REPL
- 输入普通文本：**放弃 plan**，并作为普通 chat 发送

### UX 评价

这个设计很实用：

- slash 仍然可用，不会把用户锁死在 plan UI
- 普通聊天则视为“我不继续 plan 了”

风险也存在：

- 用户如果不知道当前处于 paused plan，随手打一段话，可能会意外 abandon plan
- 这类行为应该在 TUI 上继续强化可见反馈

---

## 6. CLI local 的工作原理

## 6.1 入口状态

核心状态主要是：

- `state.plan_mode: Option<PlanModeState>`
- `state.executing_plan`
- `state.plan_handle`
- `pending_plan_resume_digest`

`PlanModeState` 会做 workspace-scoped 本地持久化，因此 `/plan` 能恢复上次未完成的本地 plan。

## 6.2 目标提交

`handle_goal_submission(...)` 做的事情：

1. 确保 journal writer 存在
2. 记录 `"stage": "entered"` 的 lifecycle 事件
3. enrich 模板上下文
4. 用 `classify_plan_suggestion(...)` 判断是：
   - `Executable`
   - `Analytical`
5. 根据类型进入不同路径

## 6.3 Executable goal

当前 executable 路径大致是：

```text
goal
  -> outline_prompt
  -> outline / clarification
  -> parse_plan_response
  -> normalize_simple_greenfield_plan
  -> 写回 PlanModeState
  -> 交互确认（execute all / step by step / edit）
```

这条路径是**对话式 plan 编辑器 + executor 前台**的组合。

## 6.4 Analytical goal

分析型路径是 one-shot：

```text
goal
  -> analytical_prompt
  -> parse_analytical_response
  -> format_research_plan
  -> 清理 plan_mode
```

这条路径不会启动 executor。

## 6.5 Resume 提示

`astra-plan/src/plan_resume.rs` 提供：

- `plan_resume_digest(...)`
- `plan_resume_system_prompt_section(...)`
- `message_signals_resume(...)`

它们的作用是：

1. 给后续回合注入“当前还有 plan 在执行/暂停”的提醒
2. 窄匹配用户是否表达了“继续 plan”

这有助于减轻 plan 中断后的状态丢失感。

---

## 7. Web agent / server-side 的工作原理

Web agent 与 CLI local **不是同一条 plan 机制**。

## 7.1 进入 plan mode

Web / remote 侧通过 `enter_plan_mode` 工具进入 plan：

```text
enter_plan_mode(goal)
  -> plan repo load/create
  -> set_active_plan(session_id, plan_id)
  -> supersede 旧 plan 的 seeded todos（best effort）
  -> invalidate cache
  -> journal: plan_mode_entered
```

效果：

- 当前 session 被标记为“正在 authoring a plan”
- 写工具被 permission gate 阻断
- 读工具仍可用

## 7.2 退出 plan mode

Web / remote 侧通过 `exit_plan_mode(plan, approved)` 退出：

- `approved = false`
  - plan 保持打开
  - 写工具继续被挡住
  - 继续 authoring
- `approved = true`
  - session 的 `active_plan_id` 被清空
  - 写工具解锁
  - 若存在 `plan_todo_sink`，将 subtasks seed 到 `session_plan_todos`

重点：**`exit_plan_mode(approved=true)` 并不等于“立刻开始执行 plan”**。

它只是：

1. 把 authoring 阶段结束
2. 把 plan 转成用户可见/可执行的待办树
3. 让后续 turn 或 `/plans/{id}/execute` 再去执行

## 7.3 为什么 Web 需要这套 enter/exit 工具

因为 Web 没有 `plan>` 这种本地交互壳，也没有本地 edge executor。它必须靠显式 lifecycle 工具来表达：

- 现在正在 plan authoring
- 现在可以解锁写入
- 当前 approved plan 是什么

### UX 评价

server/web 这套机制在执行约束上是合理的：

- 进入后明确禁止写
- 退出时明确以“审批 plan”为边界
- approved plan 可以转成真实 todo 树

但它也明显比本地 CLI 更“系统内部化”：

- 对终端用户不如 `/plan` 直观
- 更偏 agent-internal lifecycle
- 更适合 headless / web / remote 执行面

---

## 8. 当前设计为什么合理

## 8.1 `/plan` 收敛成唯一本地入口是对的

这是当前最清晰的产品语义：

- 用户只需要记住一个入口
- `/plan <description>` 已经覆盖最快路径
- `/plan` 再输入目标覆盖探索式路径
- 删除 `/plan status/show/exit` 让 slash 面更干净

## 8.2 “本地 edge surface” 与 “server surface” 必须分开

如果不分开，会出现两类坏事：

1. 模型在本地看到 server-only 工具，做出不可能完成的调用
2. 用户以为两个 surface 一样，实际上权限、执行位置、文件系统都不同

当前把 CLI local 工具面收缩到 `local_tool_schemas()` 是正确的。

## 8.3 Analytical 与 Executable 分流是正确的

“评估一个方案是否合理”本来就不是 subtasks executor 的问题。

若把所有 `/plan` 都强行变成任务树，会导致：

- 不自然的 subtasks
- 错误的执行预期
- 分析型问题被过度工程化

当前 one-shot analytical plan 更自然。

---

## 9. 当前设计仍然不够好的地方

## 9.1 CLI local 与 Web/server 仍未统一

这是当前最大的结构性现实：

- CLI local：`/plan` + `plan>`
- Web/server：`enter_plan_mode` + `exit_plan_mode`

它们表达的是相同产品意图，但不是同一个交互抽象。

这不是 bug，但意味着：

- 文档必须明确区分 surface
- 测试必须覆盖两套旅程
- 未来若追求完全统一，还需要更大规模重构

## 9.2 `plan>` 内部命令仍然偏多

虽然 slash surface 已极大收敛，但 `plan>` 里仍有：

- `status`
- `show`
- `pause`
- `resume`
- `cancel`
- `help`

它们不是 slash 级噪音了，但仍然说明 `plan>` 目前是一个“命令壳 + 对话编辑器”的混合体，不是极致简单的单一交互。

## 9.3 全局 auto-suggest 语义不够一致

当前存在一个中间态：

- 分类器还在
- 但全局自动建议进入 plan 并不是当前主产品路径

这本身没错，但如果文档、提示语、测试仍把它当成核心体验，就会误导维护者。

## 9.4 paused plan 遇到普通文本就 abandon，虽然高效但有误伤风险

这是合理的 power-user 行为，但需要更强的界面提示，否则用户可能不知道：

- 我现在还在 paused plan 上下文里
- 这句话会触发 abandon

---

## 10. 测试覆盖现状

当前 plan 相关测试面并不弱，但它是**分散覆盖**，不是一条完整产品旅程的端到端保护。

## 10.1 已有覆盖

### CLI local

- `astra-cli/src/cli/plan_interaction.rs`
  - paused 时 `/slash` 会 `DispatchSlash`
  - paused 时普通文本会 `SendAsChat`
  - 执行后退出 plan mode 的行为
  - JSON 替换/修复逻辑
- `astra-cli/src/cli/slash_memory.rs`
  - `/plan` slash surface
- `astra-plan/src/plan.rs`
  - `PlanCommand::parse(...)`
  - `PlanPhase` 相关状态机约束
- `astra-plan/src/decompose.rs`
  - `parse_plan_entry_choice(...)`
  - `format_plan_entry_card(...)`
  - `classify_plan_suggestion(...)`
  - `should_suggest_plan_mode(...)`
- `astra-plan/src/plan_resume.rs`
  - digest
  - resume signal
  - system prompt section

### Tool surface / capability gating

- `astra-cli/src/edge_tools/tests/schema_tests.rs`
  - 本地 CLI catalog 暴露 client-backed `enter_plan_mode` / `exit_plan_mode`
- `astra-cli/src/edge_tools/tests/executor_core_tests.rs`
  - 本地 executor 不再接受旧 plan-mode no-op/重定向兼容
- `runtime/src/prompts/system.rs`
  - plan lifecycle 指导留在工具 schema，不复制进全局 prompt

### Server / Web

- `runtime/src/server/server_tool_executor.rs`
  - `enter_plan_mode`
  - `exit_plan_mode(approved=true|false)`
  - seeding `session_plan_todos`
  - stale todo supersede
  - write guard unblock / keep blocking
- `runtime/src/turn/permission_gate.rs`
  - plan mode 下始终允许 `exit_plan_mode`
  - 写工具阻断逻辑

## 10.2 明显空洞

以下仍然缺少更强的产品级保护：

1. **没有单个 E2E 测试覆盖 CLI local 的完整显式旅程**
   - `/plan`
   - 输入 goal
   - 生成 plan
   - `go` / `step`
   - 暂停 / 恢复 / abandon

2. **没有对 `/plan <description>` 与 “进入后首句 goal” 的等价性测试**
   - 这是当前重要产品语义，应该被显式锁住

3. **没有跨 surface 的一致性测试**
   - CLI local 的 plan
   - Web/server 的 enter/exit lifecycle
   - 二者是否对用户表达同样的产品意图，目前主要靠人工理解

4. **没有“文档与实现一致性”的测试**
   - 当前文档曾长期落后于真实代码，这说明这里很容易漂移

5. **Analytical 路径的 UX 级测试仍偏少**
   - 目前更偏解析与单点函数覆盖
   - 缺少“用户从 `/plan` 进入后拿到一次性研究计划并回落到普通 chat”的产品测试

## 10.3 结论

测试并非“不完备”，而是：

- **实现级测试不少**
- **产品旅程级测试不够**
- **跨 surface 对齐测试更弱**

当前最值得补的，不是再加更多 parser test，而是：

1. CLI local 的 `/plan` 显式旅程测试
2. `/plan <description>` 与隐式首句 goal 的等价测试
3. server/web 与 CLI local 的语义对照测试

---

## 11. 推荐的维护原则

后续继续演进 plan 时，建议坚持这几个原则：

1. **不要再扩张本地 slash surface**
   - 本地入口继续保持 `/plan` 即可

2. **不要把 server-only 工具重新暴露给 CLI local**
   - 本地看见的工具，必须是本地真的能执行的工具

3. **把“分类器存在”与“产品 auto-suggest 存在”分开描述**
   - 这是两个不同层级的问题

4. **新增行为优先补产品旅程测试**
   - 不要只补 parser / helper 单测

5. **如果未来要统一 CLI 与 Web plan 语义，应先统一生命周期抽象，再统一表面命名**
   - 否则只会再次长出兼容层

---

## 12. 当前状态摘要

当前 plan 的产品状态可以浓缩成下面这几句：

- **CLI local**：用户只用 `/plan`；`/plan <description>` 与进入后首句 goal 已对齐。
- **CLI local 工作方式**：云端推理，本地执行；`plan>` 是自然语言编辑 + 命令控制的混合界面。
- **Web / remote**：通过 `enter_plan_mode` / `exit_plan_mode` 进行 server-side plan authoring 与 approval。
- **server-only / edge-only 的边界现在是刻意设计，不是遗留噪音。**
- **兼容层已经删掉不少，但两个 surface 还没有统一成同一个产品抽象。**
