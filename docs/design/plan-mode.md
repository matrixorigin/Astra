# Plan Mode 设计与产品需求

> 这是 plan mode 的**唯一**设计文档。覆盖产品需求、用户旅程、四种部署形态下的契约、不变量与测试边界。
> 历史版本（v2/v3）已删除——本文反映 `no_direct_cli_db_ops` 分支之后的真实代码。

---

## 1. 产品定位

Plan mode 是一个**先对齐、再执行**的约束机制。它解决一类具体痛点：

- 复杂任务里 agent 直接动手，方向错了浪费上下文窗口和用户耐心；
- 跨文件 / 涉及架构选择 / 影响生产的改动需要用户先看到"打算怎么做"再批准；
- 有时用户只是想让 agent 调研一段代码，不想它顺手改。

Plan mode 不是"第二个聊天模式"，更不是"额外的工作流"。它就是一个**临时的 capability 限制**：

- 写工具被运行时挡住，模型连 schema 都看不到（避免无意义重试）；
- 模型输出一份计划文本（Markdown）；
- 用户审批，选择执行模式（auto / edit / default）或继续打磨；
- 退出 plan mode 后，agent 在选定模式下执行，已批准的计划文本被注入下一回合的 volatile lane 让模型记得。

**核心原则**：
1. **Schema 是真理，prompt 是引导，运行时是兜底。** 模型看不到的工具就不会调用；模型即使想调也被阻拦；prompt 解释为什么这两件事一致。
2. **进入 plan 是用户的事，退出 plan 是模型的事。** 用户按 Shift+Tab 或敲 `/permissions plan` 进入；模型调用 `exit_plan_mode(plan="…")` 把计划交给用户审批。
3. **永远不阻塞用户。** 没有云、没有 token、没有网络、子 agent 跑批——任何场景下 plan mode 都能进、能出。

---

## 2. 部署形态

代码里 plan mode 的语义随**执行面**而不同。本节是后续所有契约的前置事实。

| 形态 | 描述 | 工具在哪执行 | plan mode 真相源 |
|------|------|-------------|----------------|
| **CLI 模式** | 终端用户跑 `astra`，TUI + 本地工具 | 本地 edge | `perm_manager.mode() == Plan` |
| **edge-cloud 模式** | 同 CLI，但 chat turn 走 thin-client → 云端 server | 本地 edge | `perm_manager.mode() == Plan` + 可选云镜像 |
| **web-agent 模式** | 浏览器 / 后台 worker，全云端 | 服务端 `ServerToolExecutor` | DB `agent_sessions.active_plan_id` |
| **子任务 / 委托** | spawn_agent / 委托代理 / 子流程 | 视父继承 | `InheritedPermissions::mode == Plan` |

> 有些"plan"概念在 `astra-runtime/server/plan_handlers.rs` 表示"多子任务执行计划"（durable plan executor）。那是不同产品。本文 plan mode 仅指"先调研、再批准、再执行"的工作流。两者通过 `plans` 表共享存储但用例正交。

---

## 3. 用户旅程

### 3.1 旅程 A：Shift+Tab 进入 plan mode（CLI / edge-cloud 主流）

最常见的入口。用户**不创建任何云端记录**，纯本地翻转权限。

```
用户在 TUI 里 → 按 Shift+Tab
  ↓
状态栏切到 "plan"
  ↓
下一回合开始时，schema 阶段把所有 mutating 工具裁掉（read_file / grep / glob / web_fetch 等只读保留 + exit_plan_mode 保留）
  ↓
volatile lane 注入一行 [mode=plan] read-only investigation. Author the plan, then call exit_plan_mode(plan="<markdown>") for user approval.
  ↓
用户输入需求 → 模型只读调研 → 输出计划 → 调 exit_plan_mode(plan="…")
  ↓
TUI 弹 4 选项 overlay：
  - Approve & start in auto
  - Approve & start in edit
  - Approve & start in default
  - Keep planning — let me give feedback
  ↓
用户选 "auto" → 切到 PermissionMode::Auto，下回合开始 push 一行 [mode=auto] User approved the plan; you are now executing in auto mode. → 模型继续，全工具放开
```

**零网络调用**。整个旅程没有任何 HTTP 请求触发。

### 3.2 旅程 B：`/permissions plan` 进入

效果与 Shift+Tab 等价。Slash 命令是 Shift+Tab 的备用入口（脚本化、文档化场景）。

### 3.3 旅程 C：`enter_plan_mode(goal="…")` 工具进入（云端工作流）

模型主动进入 plan mode（罕见——通常是模型自己判断"这个任务该先 plan"）。

```
模型调 enter_plan_mode(goal="Ship auth")
  ↓
ToolExecutor 双路径选择：
  - 有 cloud token + active session → POST /plans 创建 phase=planning 行 + stage Plan 到 pending 槽
  - 离线 / 无 token → 跳过云调用，仅 stage Plan 到 pending 槽
  ↓
下回合开始 → 应用 pending mode → perm_manager 切到 Plan
  ↓
之后流程同旅程 A
```

### 3.4 旅程 D：Web agent

```
浏览器用户给任务 → server 上的 agent loop 跑
  ↓
模型调 enter_plan_mode(goal="…") → 创建 plans 行，agent_sessions.active_plan_id 设
  ↓
ServerToolExecutor 的 plan_mode_authoring_active() 探针返回 true
  ↓
所有 mutating 工具被运行时拒绝
  ↓
模型调 exit_plan_mode(plan="…", approved=true|false)
  - approved=true → 清 active_plan_id，写工具放开
  - approved=false → 留在 planning，等下一轮调研
```

**关键**：Web agent 没有交互式 overlay，靠 `approved` 参数显式指定（headless 契约）。

---

## 4. 9 个不变量（Invariants）

这些是**测试钉死的契约**。`rust/crates/astra-cli/src/edge_tools/tests/executor_core_tests.rs` 和 `rust/crates/astra-cli/src/cli/session_state.rs` 里有对应测试。

### I1：Shift+Tab 路径零云调用

进入 plan mode（Shift+Tab、`/permissions plan`、离线 `enter_plan_mode`）**绝不**触发 HTTP 请求或创建 cloud `plans` 行。失败模式：曾经的 bug 是离线时 `enter_plan_mode` 报 `Error: requires authenticated cloud session`，违背"用户按 Shift+Tab 必然成功"。

> 测试：`enter_plan_mode_falls_back_to_local_when_cloud_unavailable`

### I2：云路径与本地路径都 stage 同一权限模式切换

无论 `enter_plan_mode` 走云路径还是本地路径，都必须把 `PermissionMode::Plan` 写进 `pending_permission_mode_change` 槽。

- 失败模式：云路径只更新 cloud row，不改 perm_manager —— 模型看到工具 schema 没变（host 没收到信号），plan 模式没真正生效。

> 测试：`enter_plan_mode_stages_perm_mode_change_to_plan_even_on_cloud_path`

### I3：`exit_plan_mode` 双路径

- 云端有 phase=planning 行 → 走云路径（POST `/plans/{id}/exit-plan-mode`）+ overlay
- 云端没有该行（Shift+Tab 入口、离线、网络故障）→ 走本地路径，纯 overlay + perm_manager 切换

无论哪条路径，approve 后下回合 schema 解锁、模型看到 `[mode=auto/edit/default]` volatile marker。

- 失败模式（已修，session d9b5119f）：旧实现硬要求 cloud row，Shift+Tab 入口的 exit 直接报 `no active planning plan found`，agent 永远卡在 plan 模式。

> 测试：`exit_plan_mode_shift_tab_path_works_without_cloud_plan_record`、`exit_plan_mode_local_path_makes_zero_cloud_calls`

### I4：服务端写工具守卫与 CLI 状态完全独立

`ServerToolExecutor::plan_mode_authoring_active()` **只**查 DB（`agent_sessions.active_plan_id` + `plans.phase`）。不访问 CLI 的 `perm_manager`、`cloud_plan_mirror` 或任何本地状态。

- 失败模式：把 CLI 状态泄漏到服务端守卫会让 web-agent 行为依赖另一台机器上的客户端状态——不可接受。

> 由 server-side `plan_mode_authoring_active` 单元测试覆盖。

### I5：子任务通过 InheritedPermissions 传 plan 模式

`spawn_agent` 创建的子代理通过 `InheritedPermissions::mode == Plan` 继承 plan 模式，**不**通过任何全局 / 进程内状态。

- 失败模式：父进程在 plan 模式，子进程不在——继承错乱。

### I6：CLI 进程内单一真相源 = `perm_manager.mode()`

任何"我现在在不在 plan 模式"的判断都必须经 `state.plan_mode_active()`，它派生自 `perm_manager.mode() == Plan`。

- **不可以**直接读 `state.cloud_plan_mirror.is_some()` 来判断（那是云镜像，Shift+Tab 入口下永远是 None 但仍在 plan 模式）。
- **不可以**维护任何"plan_mode 副本"字段。

> 测试：`plan_mode_active_tracks_perm_manager_only`

### I7：TUI 状态显示派生自 perm_manager

状态栏、footer、follow-up suggestion gate、nudge 抑制——所有 UI 层"是否在 plan 模式"判断都通过 `state.plan_mode_active()`。

- 重命名 `state.plan_mode → state.cloud_plan_mirror` 后，`is_some()` 检查全部迁移到 `plan_mode_active()`。

### I8：mid-turn Shift+Tab 不打断当前回合

按 Shift+Tab 在回合进行中合法。已发出的 LLM 请求继续完成，已发出的工具调用继续执行。**下一回合**起点重新读 `perm_manager.mode()`，组装新 schema。

- 失败模式：mid-turn 撤销已发出的工具调用 → 不一致；或 mid-turn 强制重发 → token 浪费。

### I9：Plan 文本通过工具参数传递

模型 → `exit_plan_mode(plan="<markdown>")` → ToolExecutor 收到 plan 文本 → overlay 把 plan 文本作为 `context` 字段展示给用户 → approve 后 push 到 volatile lane 给下回合的模型。

- **不**写到 `state.plan_mode.markdown`（该字段已删除）。
- **不**走任何持久化层（除非用户在云路径上，那时 server 把 plan_md 写进 plans 行）。

---

## 5. 用户入口与权限模式映射

CLI 的权限模式有 5 档（底层 enum 保留 5 档以兼容服务端契约），TUI 暴露 4 档循环：

| 模式（底层） | TUI 显示名 | Shift+Tab 顺序 | 含义 |
|------|------|----|---------|
| `Prompt` | **Default** | 1 | 写工具运行时弹审批 |
| `Auto` | **Auto** | 2 | 全部 auto-approve |
| `AcceptEdits` | **Edit** | 3 | 自动批准 workspace edit；shell 仍审批 |
| `Plan` | **Plan** | 4 | 只读调研 + exit_plan_mode |
| `Deny` | (隐藏，仅 CI) | — | 全部 deny |

`Shift+Tab` 在 Default → Auto → Edit → Plan 之间循环。`/permissions <mode>` 显式指定也可。

`Deny` 不在循环中——它是 CI / 测试场景的环境变量入口，不应被用户误按进入。

---

## 6. 工具契约

### 6.1 `enter_plan_mode`

```
入参: { "goal": string (必填) }
返回: 文本消息 ("Entered plan mode (local). goal=\"…\". …" 或带 plan_id 的云端版本)
副作用:
  - 必然: pending_permission_mode_change = Some(Plan)
  - 仅云路径: 创建 phase=planning 的 plans 行
```

### 6.2 `exit_plan_mode`

```
入参:
  {
    "plan": string,           // 必填或推荐：Markdown 计划文本
    "plan_md"|"plan_markdown": // 别名，向后兼容
    "approved": bool          // 可选：显式跳过 overlay
  }
返回: 文本消息描述执行结果
副作用（按场景）:
  approved=true:
    - 云路径：POST /plans/{id}/exit-plan-mode { approved: true, plan_md }
    - 本地路径：仅 overlay
    - 二者都: pending_permission_mode_change = Some(用户选的 mode)
  approved=false:
    - 云路径：POST /plans/{id}/exit-plan-mode { approved: false, plan_md }
    - 本地路径：仅打印消息
    - 二者都不切 perm_manager
  approved 缺省 + 有 overlay sink:
    弹 4 选项，按用户选择行为同上
  approved 缺省 + 无 overlay sink (headless):
    返回 actionable error: "Re-call with `approved=true` or `approved=false`"
```

### 6.3 Permission engine 的 plan mode 豁免

`PLAN_MODE_REQUIRED_TOOLS = ["enter_plan_mode", "exit_plan_mode"]`。这两个工具**永远**通过 plan mode 的运行时门，否则模型在 plan 模式下没有任何退出方式（曾经的 bug，session 4cb6b459）。

`tool_schema_prune::plan_mode_restrictions()` 用同一个常量决定 schema 里保留哪些工具——schema 与 runtime 的真理统一。

---

## 7. CLI loop host 的回合契约

每个回合开始时（`cli_loop_host::execute_turn` 入口）：

1. **drain pending mode change**：从 ToolExecutor 读出 `pending_permission_mode_change`，应用到 `perm_manager.set_mode(...)`。
2. **install ask_user_request_tx**：往 ToolExecutor 装当前回合的 overlay 通道。
3. **inject plan-mode marker**：如果 `state.plan_mode_active()`，往 volatile lane push `PlanModeMarker`（singleton kind，重复 push 不堆叠）。

回合结束时（`on_turn_completed`）：

4. **uninstall overlay**：清 ToolExecutor 上的 `ask_user_request_tx`，避免泄漏到 background sub-runs。

mode 切换不打断当前回合（I8）：所有 setter 的副作用都在**下一**回合的入口被读取。

---

## 8. 已知边界 & 未做

### 8.1 不变行为（未来不能破）

- 模型在非 plan 模式调 `exit_plan_mode` 不会 silent no-op：会触发 overlay（如果有 sink），让用户感知"模型在错误时机调了这个工具"。
- 模型在 plan 模式只输出 markdown 不调 `exit_plan_mode` ≠ 隐式 approve。用户得通过别的途径退出（Shift+Tab、`/permissions <mode>`、显式取消）。
- 用户在 overlay 里取消（Esc / 关闭通道）= 等价于 "Keep planning"，plan 不批准、不切模式。

### 8.2 暂未实现（已知 Gap）

- **模型在 plan 模式只输出 markdown 不调 exit_plan_mode**：当前 UI 不主动提示。后续可加 stop_hook 检测"plan-shaped output 但未调 exit_plan_mode"并 nudge。
- **CLI / Web 模式合并**：依然两套入口（CLI 走 `perm_manager`，web 走 DB）。两者通过 invariant I4/I5 解耦，目前**不需要**合并；如未来要做"同一会话跨 CLI 和 web 切换"，需要重新评估。
- **`/plan "goal"` slash 命令**：保留，是云路径 `enter_plan_mode` 的便捷入口（用于 plan-executor 多子任务流）。它走的是 plan-mode-v3 旧文档里描述的 `plan>` 流程，与 Shift+Tab 入口的关系是**互不干扰**——同一会话用两个入口会有两个真相源（一个 perm_manager，一个 cloud row），按 invariant I6/I4 各自为政。

### 8.3 与 plan executor 产品的边界

`astra-plan` crate / `plan_handlers.rs` 处理的是另一个产品：**多子任务可执行计划**（plan executor / durable agent runs）。它通过 `plans.phase=executing` 和 `plan_step_runs` 表驱动。Plan mode 与它共享 `plans` 表存储，但生命周期独立。

具体：
- Plan mode 只关心 `phase=planning` 和 `phase=refining` 两个相位。
- Plan executor 关心 `phase=executing`、`subtasks`、`step_runs`。
- `exit_plan_mode(approved=true)` 把行从 `planning → refining`，**不**自动启动 executor。executor 由 `/plans/{id}/execute` 显式触发。

---

## 9. 测试覆盖

`cargo nextest run -p astra-cli -E "test(plan_mode) | test(exit_plan) | test(enter_plan)"` 跑完所有 plan-mode 测试。当前 31 个，全绿。

### 关键测试到 invariant 的映射

| Invariant | 测试 | 文件 |
|-----------|------|------|
| I1 | `enter_plan_mode_falls_back_to_local_when_cloud_unavailable` | `executor_core_tests.rs` |
| I1 | `exit_plan_mode_local_path_makes_zero_cloud_calls` | `executor_core_tests.rs` |
| I2 | `enter_plan_mode_stages_perm_mode_change_to_plan_even_on_cloud_path` | `executor_core_tests.rs` |
| I3 | `exit_plan_mode_shift_tab_path_works_without_cloud_plan_record` | `executor_core_tests.rs` |
| I3 | `exit_plan_mode_overlay_approve_auto_records_pending_mode_change` | `executor_core_tests.rs` |
| I3 | `exit_plan_mode_overlay_keep_planning_leaves_plan_open` | `executor_core_tests.rs` |
| I3 | `exit_plan_mode_without_overlay_or_approved_returns_actionable_error` | `executor_core_tests.rs` |
| I3 | `exit_plan_mode_accepts_plan_alias_and_explicit_approved_skips_overlay` | `executor_core_tests.rs` |
| I3 全周期 | `enter_plan_mode_then_exit_full_cycle_offline` | `executor_core_tests.rs` |
| I4 | `plan_mode_authoring_active_*`（server-side） | `server_tool_executor` 测试 |
| I6 + I7 | `plan_mode_active_tracks_perm_manager_only` | `session_state.rs` |
| I9（permission） | `plan_mode_allows_plan_control_tools_so_model_can_exit` | `permission_engine.rs` |
| Schema 真理 | `plan_mode_keeps_read_only_and_required_tools` | `tool_schema_prune.rs` |

### 已知会话回归

- `4cb6b459` — `exit_plan_mode` 被 plan mode 自身 deny，agent 永久卡死。修复：`PLAN_MODE_REQUIRED_TOOLS` 加入 permission engine 豁免。
- `d9b5119f` — Shift+Tab 入口的 `exit_plan_mode` 报 `no active planning plan found`。修复：双路径分流。

---

## 10. 实现关键点速查

| 文件 | 职责 |
|------|------|
| `astra-cli/src/cli/session_state.rs` | `SessionState.cloud_plan_mirror`、`plan_mode_active()` 派生函数、I7 测试 |
| `astra-cli/src/edge_tools.rs` | `enter_plan_mode_remote` / `exit_plan_mode_remote` 双路径；`pending_permission_mode_change` 槽 |
| `astra-cli/src/cli/chat_stream/sse_loop/cli_loop_host.rs` | 回合入口 drain pending mode、install overlay、push PlanModeMarker |
| `astra-cli/src/cli/chat_stream/sse_loop/agentic_loop_turn.rs` | schema 阶段 plan-mode 裁剪 |
| `astra-turn-core/src/tool_schema_prune.rs` | `PLAN_MODE_REQUIRED_TOOLS` 常量、`plan_mode_restrictions` |
| `astra-turn-core/src/permission_engine.rs` | plan mode short-circuit + plan-control 工具豁免 |
| `astra-runtime/src/turn/agentic_loop_host.rs` | `VolatileKind::PlanModeMarker`（singleton） |
| `astra-runtime/src/server/server_tool_executor.rs` | server-side `plan_mode_authoring_active()` 探针，独立于 CLI 状态 |
| `astra-runtime/src/server/plan_handlers.rs` | `/plans` HTTP handlers（cloud workflow 后端） |
