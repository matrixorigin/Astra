# WALKTHROUGH S09 — PM 派三 agent 并行交付「用户积分明细导出」

> 推演视角：工程师拿 v0.2 设计（3040 行 + 19 gap 已 patch）实际跑 S09
> 12 回合看每一处卡不卡。

## §1 场景标识与 v0.2 映射

- **一句话场景**：PM 小陈在主 session 让 PM-agent 作 orchestrator 拆 3 子任务，
  派发 fe/be/dba **三个独立 child session** 并行交付 CSV 导出 feature，
  父 session 仅用 `delegation_state` 聚合，**不重放子 transcript**；
  12 回合核心剧情末端聚合产出 PR 描述 artifact。
- **被测 DAUT**：A4（manifest 显式 drop 子 transcript）、A5（`delegation_state`
  zone 1500 硬顶 × 3 子均分）、A13（`agent_runs.parent_run_id` + 独立
  `agent_sessions` 的 "both" 模式）。
- **最相关 v0.2 字段/章节（行号具体）**：
  - `agent_runs` schema（`parent_run_id`, `root_run_id`, `ancestor_path`,
    `depth`, `delegation_id`, `retry_scope`, `waiting_for`, `idx_runs_parent`）
    @ L899–939。
  - `session_delegations` projection 表 + Delegation Contract "both" 答案
    @ L1328–1410。
  - `delegation_state` 动态预算公式 `min(1200, max(200, floor(1500/n)))`
    @ L2446–2470（G18）。
  - `session_artifacts.access_scope` enum `{private, delegation, user}` +
    "parent session and descendants" 定义 @ L1715–1720（G9）。
  - session visible states 枚举含 `waiting_for_external` @ L2647–2649。

## §2 运行前提

- Schema 初值：小陈账号已建，主 session `sess-pm-001` 存在，`agent_sessions`
  有 1 行；`agent_runs` / `session_state_items` / `session_delegations` 均空。
  PM-agent 已注册含 `spawn_delegate`、`session_state.update_projection`、
  `artifacts.fetch`、`/chat/runs/{id}/delegation-summary` 四类工具。
- Workspace：`cloud`（PM 不写代码，三子都跑服务端 sandbox，无 edge bridge）；
  `workspace_state.authority='cloud'`，`tool_whitelist` 对 PM-agent 只放出
  delegation/读 artifact/只读 SQL explain；fe/be/dba 子 session 各自有 full
  coding tool 集。
- Context window：父 session 用 `budget_template_id='budget_v1_32k'`（主流程
  默认）；子 session 各自也是 32k。父最关注 `delegation_state` 1500 硬顶
  （G18 L2447）与 `recent_tail` 剩余空间。预估父每轮总 prompt ≤ 6k。
- 产品前提：客户 P0，3 天交付；PM-agent system prompt 固化"兄弟产出必须经父
  delegation_state 中转，不允许两个子 session 之间直连"这一 invariant。

## §3 端到端运行轨迹

### T1 · Day1 10:00 PM 提需求

**User 动作**："大客户催了三次——帮我出『用户积分明细导出 CSV』，三天内上，
后端要快、前端能下、数据库别慢爆。你拆一下。"

**Server behavior**：父 run `run-pm-001` 新建（`parent_run_id=NULL`,
`root_run_id=run-pm-001`, `depth=0`, `trigger_type='user_message'`，L908/912）；
写入 `anchor`（key=`goal.export_points_v1`）+ `decision`
（key=`product.scope.export_v1`，"MVP=CSV, user+time 过滤, ≤10 万行"）+
`plan_state` version=1 phase='scoping'；尚无 `session_delegations` 行。
`context_manifests` 写 1 行 `reason='initial_turn'`（L1451, L1495 枚举约束）。

**User sees**：SSE `run_started` → `message_delta`（PM-agent 复述需求确认）→
`plan_updated`；Context 侧栏显示 ~2900 tokens（`system_static` 1200 /
`tool_schemas` 1200 / `session_anchor` 300 / `plan_todo` 200），
`dropped_count=0`。UX 顺畅，无阻碍。

### T2 · Day1 10:30 PM-agent 拆 3 子并派发

**User 动作**：无用户输入；PM-agent 自主执行工具调用。

**Server behavior**（**关键点 — G4 "both" 验证**）：PM-agent 连续 3 次调用
`spawn_delegate(agent_id=fe|be|dba, directive=...)`。根据 G4 L1366–1368，
"child sessions created only when child needs independent identity: user-visible
tab, long-lived resume, separate permissions, or cross-session reuse" —
S09 这 3 个子**每个都符合**"long-lived resume + cross-session reuse"（dba
的 SQL 要被 be 读），因此**3 子都建独立 `agent_sessions`** 行
（`sess-fe-xxx`/`sess-be-yyy`/`sess-dba-zzz`）。同步建 3 条 `agent_runs`
子 run（`parent_run_id=run-pm-001`, `root_run_id=run-pm-001`, `depth=1`,
`ancestor_path='run-pm-001/run-child-X'`, `delegation_id=del-export-v1-{fe,be,dba}`,
L902–908）。同事务写 3 条 `session_delegations` + 3 条
`session_state_items(category='delegation_state', item_key='delegation:fe|be|dba')`
（L1413–1424 Projection Sync Contract 强约束同事务）。

**User sees**：SSE 一次喷 3 条 `delegation_spawned` + 3 条 `state_item_inserted`；
UI 左栏从单 tab 变 1 父 tab + 3 子 tab（但 3 子 tab 默认折叠在"派发组"）。
父 manifest `delegation_state` zone 占 420 tokens（3 × ~140 骨架），
`reason='delegation_spawn'`。**F1 friction**：`spawn_delegate` 工具 schema
v0.2 **并未**显式列出（§API Additions L2730+ 无它），只是 PM-agent 内部约定，
实现者得自造 tool contract。

### T3 · Day1 11:00 三子并行开工（静默投影）

**User 动作**：无；小陈关掉浏览器 tab 去开会。

**Server behavior**：3 子 session 各自跑 turn 循环：`sess-fe` 5 轮、`sess-be`
7 轮、`sess-dba` 3 轮。每个子 session 在结束自己 turn 时**不直接写父**，
而是通过后台 projection worker 把子 `session_state_items(category='summary')`
的 `item_id` 写到父 `session_delegations.last_summary_ref` + 同步父
`delegation_state` item（L1371–1375 + L1413–1424 双写约束）；`status` 从
`spawned` → `running`，`phase` 分别为 `coding_ui`/`coding_api`/`drafting_migration`。

**User sees**：无 — 父 LLM 本轮未被调用，不写 `context_manifests`。
**F2 friction**：L2854 "delegation_state zone never exceeds its cap" 的检查点
是 assembly 时；但子 session 每 push 一次状态就 trigger 一次潜在的父 manifest
重算信号吗？v0.2 **未约定**"父 LLM 何时被 delegation 更新唤醒"——工程实现
得自造 policy（S09 原文假设"下一轮空闲时主动"，但 policy 无规范）。

### T4 · Day1 14:30 PM-agent 主动聚合首轮进度

**User 动作**：无；policy 触发（3 子都已 running+phase 非 null），PM-agent
主动跑一次聚合 turn。

**Server behavior**：PM-agent 读 3 条 `session_delegations` +
`session_state_items(category='delegation_state')`。assembly 按 G18 L2451 公式：
`n=3 → per_child_budget = min(1200, max(200, floor(1500/3))) = 500`。
逐子预检 `last_summary_token_estimate`（L2460）：fe=160 / be=180 / dba=150，
全部 ≤ 500，直接展开 summary。`plan_state` → version=3 phase='coordinating'；
新建父 `summary`（key=`summary:delegation_day1_r1`，~220 tokens）。

**User sees**：SSE `message_delta`（"3 子都在跑：FE 页面骨架 / BE 路由骨架 /
DBA 索引方案..."）；Context 侧栏显示 `delegation_state` 600 tokens（3 × ~200，
含 phase + summary_text），总 manifest ≈ 4360，`reason='delegation_poll'`，
`dropped_count=3`（3 子未进 prompt 的完整 transcript 候选）。**F3 friction**：
`dropped_count=3` 与场景 A4 声称"≥20"差 1 个数量级——因为 v0.2 只按"候选 zone
条目数"计算 dropped，不是按"子 session 轮数"。count 语义歧义。

### T5 · Day1 16:00 PM 追问 FE 进度

**User 动作**："FE 那边做到哪了？客户下午要看 demo。"

**Server behavior**：PM-agent 识别需"单子 drill-down"，调用
`GET /chat/runs/{child_fe_run_id}/delegation-summary`（L1409 + L2738 已定义）。
返回 JSON 形如 `{delegation_id, child_session_id, status, phase, last_summary_ref,
last_summary_token_estimate, exposed_artifacts, blocker}`（L1384–1402 shape
定义）。**但是 API 返回 shape 里没有 `summary_text` 字段本身**——只有 ref；
调用方还需 follow-up 一次 `GET /sessions/{child_sess}/state?...` 或按 ref
再查一次 `session_state_items`，这是 **F4 friction**。

**User sees**：SSE 200ms 内返回"FE 已完成 ExportPage.tsx 骨架 + useExportJob
hook（子 5 轮），预计还有 12 轮"；Context 侧栏 `delegation_state` 本轮 220
tokens（只加载 fe 单条全量，be/dba 按 L2462 fallback 到 title-only 各 ~40），
总 manifest ≈ 3600，`reason='delegation_state_singleton_query'`。**F4**：这个
reason v0.2 `context_manifest_reason_types` 种子列表（L66 GAP-TRACKER G1）
**并未枚举** `delegation_state_singleton_query`，写入会被 validation 拦。

### T6 · Day2 17:30 DBA 产出迁移 SQL（跨 session 暴露起点）

**User 动作**：无。

**Server behavior**：`sess-dba` 子 turn 收尾，写 `session_artifacts`
（`artifact_id=artifact-dba-mig-v1`, `session_id=sess-dba`,
`access_scope='delegation'`（L1682 默认），`byte_size=3.8KB`）。同时子 agent
回写**父**的 `session_delegations.exposed_artifacts_json=['artifact-dba-mig-v1']` +
`delegation_state:dba.status='awaiting_review'`（L1343 字段）。父 `decision`
+1（key=`eng.dba.migration_ready_v1`）。

**User sees**：SSE `state_item_updated` (delegation:dba) + `artifact_exposed`；
Context 侧栏 `delegation_state` 620 tokens（dba 260 含 artifact_ref，fe/be 各
180）+ `tool_previews` 180（迁移 SQL preview 500 char）。**F5 friction**：
L1718 `access_scope='delegation'` 定义是 "parent session **and descendants**
in the delegation tree"；dba 的**兄弟** be 既不是 dba 的 ancestor 也不是
descendant —— **严格读**，be 不能 load dba 的 artifact raw bytes（S10 §6 组合 4
已识别此 blocker，L305–307）。

### T7 · Day2 09:30 be-agent 跨 session 引用 dba 的 SQL

**User 动作**：无。

**Server behavior**：`sess-be` 的 be-agent 通过其内置"check sibling produce"
skill 查父 `session_delegations WHERE session_id=sess-pm-001 AND
child_session_id=sess-dba-zzz` 拿到 `exposed_artifacts_json`。接着调用
`GET /artifacts/artifact-dba-mig-v1/download-url`（L2740–2741 API 形式）。
**此处服务端 ACL 逻辑按 G9 L1715–1720 严格判定**会 403：be 不是 dba 的 descendant。
绕过方式：要么把 dba 的 artifact `access_scope='user'`（放宽过度），
要么 PM-agent 主动把 artifact 再绑到**父** session 让 access_scope 命中父即可，
但这走形式主义且 v0.2 未定义"artifact 重绑父"操作。**此处假设**工程实现
按 S10 §6 的建议放宽 `delegation` scope 为"同 root_run_id tree"，
be-agent 读成功；子 `decision` +1（`eng.be.index_dependency`），父
`delegation_state:be.deps_json=['artifact-dba-mig-v1']`（L1344 字段）。

**User sees**：无父 LLM 调用。UI 只有一条轻量 "artifact cross-referenced" 横条。

### T8 · Day2 11:00 be-agent 卡 waiting_for_external

**User 动作**：无。

**Server behavior**：be 子 run 调 `auth.company.com/v2/export_scope` 30s
超时；`agent_runs.status='waiting_for_external'`, `waiting_for=
'auth.company.com/v2/export_scope'`（L914 字段 + L2649 enum 合法）。
子 agent 同步回写父 `session_delegations.blocker_json={type:external_api,
endpoint:..., since:...}` + `delegation_state:be.status='blocked'`（L1345）。
父新建 `error_state` item（L154 GAP-TRACKER category 扩展）。

**User sees**：**此处触关键问题 F6**：父 session `visible_state` 枚举定义在
run 级（L2648）；父 run 本身是 `idle`（等 PM 说话），UI 该显示什么？按 L2648
纯 enum 选则父是 `idle`；但"1 子 blocked"UX 上绝不能 silent — 需要额外
badge。v0.2 **未**规定"父 UI 如何聚合子 run 状态"。实际会是 Context 侧栏出
红色 "1 child blocked" tag，但没 SSE 事件 schema 名（没有 `child_blocked`
事件枚举）。

### T9 · Day2 14:00 fe-agent 完成

**User 动作**：无。

**Server behavior**：`sess-fe` 子 turn 收尾，写 `session_delegations:fe.status=
'completed'` + `last_summary_ref=summary:fe-final-v1` + `exposed_artifacts_json=
['artifact-fe-page-v1','artifact-fe-test-v1']`。同事务
`session_state_items:delegation:fe.status='archived'`（L1421）；父
`session_todos` 对应那条"派发给 fe" → `status='done'`；父 `plan_state`
version=4 phase='partial_complete'。

**User sees**：SSE `delegation_completed`；UI 左栏 fe 子 tab 标绿 ✓，
父 session tab 顶部进度条 1/3 → 2/3（含 dba awaiting）。**本轮父 LLM 未调
用**（无新 user 输入，也无 policy 触发）。**F7**：fe 完成的 `state_item_events`
mutation 类型是 `archive`（L1161）还是 `update`？v0.2 两者都允许但语义歧义
—— "completed 子 delegation 的 state item 是否算 archived?" L1421 明确说 archived，
但 UI 树仍要显示它，archive 语义与 UI 显示的矛盾得靠 query filter 规避。

### T10 · Day2 15:00 PM-agent 周期 healthcheck 报 blocker

**User 动作**：无；父 session policy: "若任一 delegation.blocker 存在 >2h 则
主动 ask_user"（S09 §3 T10 描述，v0.2 L2854 未明文约束策略，但允许）。

**Server behavior**：父 LLM 本轮被真正调用。assembly 触发"blocker-review"
扩展规则（L2465–2467）："a child with active blocker may temporarily use
`2 * per_child_budget` by borrowing from `recent_tail`, manifest records
`reason='partial_blocker_review'`"。be blocker 用 2×500=1000 tokens；
fe 已 completed 降级至 title-only ~100；dba awaiting ~400；**总
`delegation_state` = 1000 + 100 + 400 = 1500**，刚好贴上限。`recent_tail` 从
默认 800 降到 600（借出 200）。写 1 条 `reason='partial_blocker_review'`
manifest（L2467 枚举合法），`dropped_count=42`。

**User sees**：SSE 一次推 message："小陈 — FE ✅ / DBA ⏳ / BE ❌ 卡外部鉴权
4h。切 mock 继续还是等运维？"；Context 侧栏 `delegation_state` 1500 满格
高亮红色 "近上限"，`recent_tail` 从 800 降至 600（v0.2 L2466 公式输出）。
UX 紧张但合理。

### T11 · Day2 15:30 小陈决策 + be 解锁

**User 动作**："切 mock 继续，运维我去催。"

**Server behavior**：PM-agent 写父 `session_delegations:be.directive='use_mock_auth'`
+ 同步 `session_state_items(delegation:be).payload_json.directive` 更新
（Projection Sync L1415–1419 同事务）。be 子 session runtime 轮询到 directive
变更（**F8 friction**：v0.2 没定义子 session 从父 delegation 拉 directive 的
推送/轮询 contract，只有父→子 spawn 时通过 `directive` 字段初始化，运行时
directive 更新如何通知子是 open 的），假设实现用"子 session 每轮开始读
`session_delegations WHERE child_run_id=<self>` pull 一次"。20 分钟后 be
收尾，父 3 条 delegation 全 `status='completed'`；父 `error_state`
`status='resolved'`；父新 `decision`（`eng.be.mock_auth_directive`）。

**User sees**：SSE `message_delta` PM-agent 复述收到 + 20 分钟后连续 3 条
`delegation_completed`；UI 左栏 3 子全标 ✓，父进度条满。

### T12 · Day2 16:30 聚合 PR 描述

**User 动作**："把三边产出合成 PR 描述发给 tech lead，要能看每边做了什么、
artifact 关联、mock auth 是临时方案。"

**Server behavior**：PM-agent 走**纯 structured 聚合**：一次 SQL
`SELECT * FROM session_delegations WHERE session_id=? AND root_run_id=? ORDER BY
spawned_at`（L1353 `idx_delegations_session_status` 命中）+ 按
`last_summary_ref` 批量 `SELECT FROM session_state_items WHERE item_id IN (...)`
一次拉全 3 条 summary 文本 + 全部 `decision` + 已 resolved `error_state`。
拼装 PR 描述模板写成新 `session_artifacts(artifact-pr-desc-v1, access_scope=
'user')` ~1.2KB。`plan_state` → version=6 phase='delivered'；`anchor` 加子项
`export_delivered`。

**User sees**：SSE `message_delta`（PR 描述正文 stream 出来）+
`artifact_created`；Context 侧栏 `delegation_state` 破例拉满 1500（3 条 final
summary 全展），`reason='delegation_aggregate'`（L66 已枚举），
`dropped_count=55+`。小陈 copy 丢 tech lead 群。UX 干净。**F9**：
`delegation_aggregate` 在 L66 已种子化 ✓，但"拉满 1500 破例"在 G18 只写了
blocker 扩到 2× 的场景，**aggregate 场景拉满没明确许可**，属灰色地带。

## §4 发现的别扭点（Frictions）

按严重度排序。

- **[F5] — 跨兄弟子 session artifact 共享被 G9 L1718 严格定义卡死**
  - **Ref**: T6/T7；v0.2 §Artifact Access Scope L1715–1720（G9）
  - **Observation**: `access_scope='delegation'` 定义是 "parent and descendants";
    dba 的兄弟 be 不是 descendant，严格读 ACL 应 403。S09 全场景 **P0 数据流**
    就是 dba→be，这一 block 直接杀死场景。
  - **Why friction**: 正确性 + 实现摩擦；工程师只能"放宽语义"或"加绕行代码"，
    但两者都偏离 v0.2 spec。**S10 已以新 gap 形式提过同问题**（见 §5 比对）。
  - **Severity**: **blocking**

- **[F6] — 父 session UI 状态聚合规则缺**
  - **Ref**: T8；v0.2 L2647–2649 visible states enum
  - **Observation**: enum 是**单 run** 的状态；父 run=idle 但有子 blocked，
    UI 该 badge 什么？无 SSE 事件名。
  - **Why friction**: UX + 实现；每家前端实现会自造 convention，不一致。
  - **Severity**: notable

- **[F8] — Directive 运行时更新的父→子通知契约缺**
  - **Ref**: T11；v0.2 §Delegation Contract L1360–1380
  - **Observation**: `directive` 字段 spawn 时写入有据，**运行时改 directive
    如何通知正在跑的子 run**？无 push/poll 契约。
  - **Why friction**: 实现摩擦；子 runtime 要么轮询（浪费）要么加额外事件总线。
  - **Severity**: notable

- **[F4] — `/chat/runs/{id}/delegation-summary` 返回 shape 不含
  `summary_text` 本体**
  - **Ref**: T5；v0.2 L1384–1402 + L1409
  - **Observation**: shape 里只有 `last_summary_ref`（ID），调用方要 follow-up
    再查一次 `session_state_items`。PM 问 FE 进度这种**小问答**一轮多一次 RT。
  - **Why friction**: 延迟 + token；UX 可以接受，实现绕得不优雅。
  - **Severity**: notable

- **[F3] — `dropped_count` 语义对"子 session 轮数"无法表达**
  - **Ref**: T4/T10/T12；v0.2 L1452 + §Context Manifest Items
  - **Observation**: A4 期望 `dropped_count ≥ 20`（反映"60 条子 transcript 候选
    被压"），但 v0.2 `dropped_count` 按**候选 zone 条目数**累加，对 3 子 session
    只会 +3 而非 +60。度量与场景期望错位。
  - **Why friction**: 观测性；UX 的 Context 侧栏数字撒谎。
  - **Severity**: notable

- **[F1] — `spawn_delegate` 工具 schema 无官方定义**
  - **Ref**: T2；v0.2 §API Additions L2730+
  - **Observation**: 3 子靠 PM-agent 内部约定起，工具 contract 不在 §API
    列表。实现者要造一个。
  - **Why friction**: 实现；不同 agent system 规格会飘移，无法 evaluate。
  - **Severity**: minor

- **[F2] — "delegation_state 变更何时唤醒父 LLM" policy 无规范**
  - **Ref**: T3 末尾
  - **Observation**: 子投影写回父是 async，但"父何时 turn"是 policy 空白。
  - **Why friction**: UX + 成本；过频撞 token，过疏撞延迟。
  - **Severity**: minor

- **[F7] — 子 delegation 完成后 state_item 状态歧义**
  - **Ref**: T9；v0.2 L1161 + L1421
  - **Observation**: `archive` mutation 与 UI 仍需显示 completed 子的冲突。
  - **Severity**: minor

- **[F9] — `delegation_state` zone 拉满 1500 的"aggregate 破例"未授权**
  - **Ref**: T12；v0.2 G18 L2465–2467
  - **Observation**: G18 只对 blocker 给 2× 扩展，aggregate 场景没 grant。
  - **Severity**: minor

## §5 发现的新 gap 候选

**未发现全新 gap，但确认 1 条已在 S10 识别的"跨兄弟子 session artifact ACL"
在 S09 是同等 blocker，且在 S09 被**多次触发**（T6/T7/T12）而非边缘用例。**

同时提出 1 条边缘性新 gap：

- **title**: 父 session 视角的"子 run 聚合状态"UI 契约缺
- **triggered_by_turn**: T8
- **related_v0.2_section**: §Run State Machine L2647–2649 + §Context Side Panel
  L2645
- **severity**: medium
- **rationale**: v0.2 `visible_states` 是**单 run**状态；当父 run=idle 但
  N 子 run 分别处于 `running` / `waiting_for_external` / `waiting_for_edge` /
  `completed` 时，父 session tab 的 top-level 状态 + SSE 事件 `child_state_*`
  无官方定义。每家前端实现会自造 convention，跨产品难对齐；而 UX 上"父 tab
  能否一眼看到子 blocker"是 S09 场景的可用性核心。
- **proposed_fix**: 在 L2647 后追加一段"Parent session aggregated state"：
  定义 `parent_aggregated_state ∈ {all_idle, some_running, some_blocked,
  all_completed, failure_bubbled}` + 对应 SSE 事件名
  `parent_child_state_rollup`；rollup 由 `session_delegations.status` group by
  计算。

## §6 Gap 组合效应检验

### 组合 1 · G4 + G9 — "both" 模式 × 兄弟 artifact ACL

- **组合**: G4（"both"：child sessions 独立）+ G9（access_scope='delegation'
  只覆盖 ancestors/descendants）
- **运行时可能发生的坑**: 正因 G4 选 "both" 让 fe/be/dba 都建 **独立
  child_session**，它们在 session 维度互为平级；而 G9 的 delegation scope
  恰好不覆盖"同 root 兄弟子"。S09 的 P0 数据流 dba→be 就踩死在两个 gap 的
  **交集**里。
- **v0.2 是否覆盖**: **no**
- **如果 no，建议**: G9 的 access_scope='delegation' 应显式扩为"同 root_run_id
  subtree 内任意 session"，或新增 `access_scope='root_tree'` 档。此建议已在
  WALKTHROUGH-S10 §6 组合 4 提出，S09 强化其必要性。

### 组合 2 · G4 + G18 + G14 — blocker 时的预算 × retry

- **组合**: G4（session_delegations.blocker_json）+ G18（blocker 2×
  per_child_budget）+ G14（retry_scope=siblings）
- **运行时可能发生的坑**: T10 be blocker 拉 1000 tokens；若小陈选"retry
  siblings"（不是 S09 走的路线，但合理），`agent_runs.retry_scope='siblings'`
  会同时重派 fe+be+dba 新 run，**3 个新 run 都会 spawn 子 session 吗还是复用
  原 child_session_id**？G14 L1196 "retry 写新 run + retry_of=old" 对 run 层
  明确，但对"child_session_id 是否复用"未说；若不复用则 3 个新 session 炸开。
  同时 G18 per_child_budget 会在 retry 期间有 6 条 delegation（3 old superseded
  + 3 new），`n=3 还是 6`？
- **v0.2 是否覆盖**: **partial** — retry_scope 有，但 retry 对 delegation
  projection 语义未细化。
- **如果 no，建议**: §Delegation Retry 节补一段 "siblings retry 默认复用
  child_session_id，新 run 以 retry_of 关联；G18 `active_children` 只数
  非 superseded 的 delegation"。

### 组合 3 · G1 + G18 — 新 reason 枚举 × 动态预算

- **组合**: G1（`context_manifest_reason_types` enum 强约束）+ G18（预算公式）
- **运行时可能发生的坑**: T5 的 `delegation_state_singleton_query`、T12 的
  "aggregate 拉满 1500" 这两个场景**都**依赖 G1 seed 枚举；L66 提到
  "delegation" reason class 有 seed 但**明细枚举**未列全。若新 reason 需
  seed 一次 migration，每次工程变更都要动 DB schema 和 Rust enum，迭代摩擦高。
- **v0.2 是否覆盖**: partial
- **建议**: 把 L2451 公式代入的每个路径用到的 reason 值在 §Delegation State
  Budget 附录里列全（至少 `delegation_spawn / delegation_poll /
  delegation_update / partial_blocker_review / delegation_aggregate /
  delegation_state_singleton_query`），一次 seed 到底。

### 组合 4 · G14 + G9 — bubble_up × 过期 artifact

- **组合**: G14（`bubble_up` 支持子关键 finding 冒泡）+ G9（artifact status
  enum 含 `expired`）
- **运行时可能发生的坑**: S09 场景未触发，但**想象**:如果 dba 完成后 artifact
  按 retention `default` 7 天后过期，T+8 小陈回看 PR 描述再 drill-down 时
  `delegation:dba.exposed_artifacts` 指向的 `artifact-dba-mig-v1.status=
  'expired'`。G9 L1733–1738 定义"context builder 渲染历史 artifact summary"
  但 **未定义 `GET /chat/runs/{run_id}/delegation-summary` 的 API 响应**
  如何标注该 artifact 已 expired。
- **v0.2 是否覆盖**: partial
- **建议**: `/delegation-summary` 的 `exposed_artifacts[]` 元素加
  `status` 字段映射 G9 artifact status。

## §7 Token 实际 walkthrough（T4 父 manifest / T10 blocker 扩展）

选 T4 和 T10 两个关键节点（T4 是"常态 3 子并行首次聚合"，T10 是"blocker
扩展借 recent_tail"）。

### T4 · n=3 / 常态预算分配

- G18 公式: `per_child_budget = min(1200, max(200, floor(1500/3))) = 500`
- 实际 child summary token estimates（从 `session_delegations.last_summary_token_estimate` 读）:
  - fe: 160 tokens (summary + phase + status + ref)
  - be: 180 tokens
  - dba: 150 tokens
- 对比: 每条 ≤ 500 ✓，无一超支；不触发 fallback (L2462)。
- 实际 `delegation_state` zone 写入:
  - 3 × ~200 = 600 tokens（含 L1384–1402 fields 完整 JSON 序列化开销）
  - **备注**：160 的原始 summary + status/phase/artifact_ref ~40 token 元数据 → ~200/条
- 其他 zone:
  - `system_static`: 1200
  - `tool_schemas`: 1200（pruned 到 delegation + artifact 查询工具）
  - `session_anchor`: 320
  - `plan_todo`: 320（3 条派发 todo）
  - `summary`: 220（L1b）
  - `recent_tail`: 500（T1–T2 对话尾）
  - `retrieved_facts`: 0（无）
  - `tool_previews`: 0
- 总计: 1200 + 1200 + 320 + 320 + 600 + 220 + 500 = **4360 tokens**
- `budget_v1_32k` 默认每 zone cap 宽松，无 overflow；`dropped_count=3`
  （zone 层面 drop；与 A4 声称的 ≥20 存在 §4 F3 的 metric 错位）。

### T10 · blocker 借 recent_tail 扩展

- G18 公式仍 `per_child_budget=500`；blocker 扩展规则 L2465–2467 对 be 启用
  `2 * per_child_budget = 1000`。
- 子预算分配:
  - fe (completed, title-only): ~100 tokens
  - be (blocked, expanded): 1000 tokens（含 blocker 详情 + 重试日志 snippet）
  - dba (awaiting, full): ~400 tokens
  - 合计: 100 + 1000 + 400 = **1500 tokens**，**精确贴 L2447 hard cap**
- `recent_tail` 从默认 800 → 借出 200 → 600 tokens（实现 L2466 "borrowing
  from recent_tail"）
- 其他 zone: `system_static` 1200 / `tool_schemas` 2400（含 error_state query
  工具 +1） / `session_anchor` 320 / `error_state` 180（新 retrieved_fact） /
  `decision` 220 / `plan_todo` 240 / `summary` 220
- 总计: 1200 + 2400 + 320 + 180 + 220 + 240 + 220 + 1500 + 600 = **6880
  tokens** → 接近场景 §3 T10 的 5440 估算但更胖（场景估算未含 error_state +
  decision 单独 zone）
- Manifest 写入 `reason='partial_blocker_review'`（L2467 枚举合法✓），
  `dropped_count=42`。
- **结论**：G18 公式在 T10 的极限场景下**刚好可行**但 0 余量；若 be 的 blocker
  详情 + 重试日志想多放 100 tokens 就会挤穿 1500 hard cap 要走
  title-only fallback。工程实现需 `last_summary_token_estimate` 预检严格。

## §8 最终判断

- **本场景下 v0.2 是否可实现**: ⚠️ — 核心"3 子并行 + delegation 聚合 + 动态预算"
  可跑；但 **T6/T7 兄弟子 session artifact ACL** 在 G9 严格语义下 block，
  要么工程绕行要么必须修 G9（S10 已识别同问题，S09 在多个回合重复触发）。
- **顺畅度**: ⚠️ 有明显摩擦 — 9 条 friction 中 1 blocking + 4 notable；
  F5 不修则 S09 不能顺畅跑；F6/F8 影响 UX 一致性；F3/F4 是观测与实现细节。
- **一句话总结**：v0.2 对单父-单子 delegation 基本成型，**但"3 个独立子
  session 并行且必须共享 artifact"这一 S09 核心形态暴露 G9 access_scope 定义
  的盲区**——在此修复前，S09 只能靠工程约定绕行；G18 预算公式在 T10 blocker
  极限下刚好贴顶 0 余量，实现需严格预检。
