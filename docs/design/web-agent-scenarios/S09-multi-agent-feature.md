# S09 — PM 小陈派三 agent 并行交付「用户积分明细导出」feature

> Status: Draft v0.1
> DAUT: **A4, A5, A13**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：小陈，某 SaaS 公司 PM，5 年经验，业务背得滚瓜烂熟但
看到 SQL 就头大、React 组件更是一行也写不出。她管「会员积分」这
条业务线，客户催得急，她要拍脑袋决策并对进度负责，**不是自己写
代码**——她是在 astra web agent 上盯着三个 sub-agent 干活的"云端
甲方"。

**业务背景**：大客户提了个 P0 需求——「积分明细导出 CSV」，要求
三天交付。小陈打开 astra web，在主 session 里跟 product-manager-
agent（下称 **PM-agent**，作为 orchestrator）提需求。PM-agent 拆
成三个技术子任务，派给三个专业 sub-agent 并行干：
**fe-agent**（写 React 导出页面）、**be-agent**（写 FastAPI 导出接
口）、**dba-agent**（加索引 + 写迁移 SQL）。每个 sub-agent 有自己
独立的 child session，PM-agent 的父 session 通过 `delegation_state`
聚合进度，**不直接看子 session 的完整 transcript**。

**一句话目标**：主 session ~25 轮交互内（本文档展示核心 12 回合）
协调完三子 agent，最终聚合出可贴给 tech lead 的 PR 描述；父 session
manifest 每轮 ≤ 6k tokens，子 session 合计 ~60 轮 transcript **绝不**
整体塞回父。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 主 session 总轮数 | 12 回合（核心剧情） |
| 子 session 累计轮数 | ~60（fe 18 + be 24 + dba 18） |
| child session 数 | 3（fe/be/dba） |
| `agent_runs` 父子关系行 | 1 父 + N 子 run，经 `parent_run_id`/`delegation_id` 串联 |
| `session_state_items(category='delegation_state')` 条目 | 3 条，每条对应一个子 session |
| 跨 session artifact 引用次数 | 1（be-agent 引用 dba-agent 的迁移 SQL） |
| 父 session manifest `delegation_state` zone 硬预算 | ≤ 1500 tokens |

## 2. 被测 DAUT

S09 的核心矛盾是「一人派三 agent 并行 × 子 session 各自状态 × 父
session 不能被子 transcript 炸穿」，下列 3 条假设被同时压测。

| 编号 | 压测方式 |
| --- | --- |
| **A4** | 父 session 每轮 `context_manifests` 必须显式记录子 session 详情被压缩的事实：`manifest_items` 里有 `zone='delegation_state'` 的行 + `dropped_count` 覆盖"本轮 60 条子 session transcript 候选未进 prompt"；T5、T10、T12 三个关键节点 `reason` 分别是 `delegation_poll` / `partial_blocker_review` / `delegation_aggregate`，每个 manifest 的 `dropped_count ≥ 20`（至少压了 20 条子 session 候选），且 UI 的 Context 侧栏可展开看到每条 dropped 的 `source_table='session_state_items'` / `source_id=delegation_state:*`。 |
| **A5** | 父 session manifest 的 `delegation_state` zone 硬预算 ≤ 1500 tokens，即便 3 子 agent 同时跑（T3–T11 期间持续并行），这 1500 要覆盖 3 条子状态摘要（约每条 450–500 tokens：status + phase + last_summary + blocker）。预算抢占规则：子数越多，每子分得越少（3 子时每条 500、5 子时每条 300、单子时允许 1200）；任何单子的 `summary_text` 超 500 tokens 的部分必须 fallback 到 `title` + `artifact_ref`，不能挤占别的 zone。 |
| **A13** | `parent_run_id` / `delegation_id` 在 `agent_runs` 是一等字段，子 session 各自有独立 `session_id` 但每个子 run 带父 run 的 `parent_run_id`（通过 `idx_runs_parent` 可 O(log N) 反查所有子）；`session_state_items(category='delegation_state')` 在**父** session 上每条对应一个子 session，payload 含 `child_session_id`、`child_run_id`、`status`、`last_summary_ref`、`blocker`；子 session 自己**不**写 `delegation_state`（它们是被委派者，不是委派者）。这条假设直击设计文档 open question「child agents are first-class sessions, run children, or both」——S09 显式走"both"：session_id 独立 + run 父子关系显式。 |

## 3. 会话时间线

12 回合覆盖：T1 需求输入 + T2 拆分派发 + T3–T4 并行启动 + T5 PM
追问 + T6–T7 跨子 artifact 引用 + T8 be 卡外部 API + T9 fe 完成 +
T10 父看到部分完成 + T11 be 解锁完成 + T12 聚合 PR 描述。每回
合字段：**用户原话 / 动作**（PM 语气——急、不懂技术、催）、**父 +
子 state 变化**、**父 manifest zone 分配**（重点 `delegation_state`
占比）、**跨 session artifact**（如有）、**LLM 看不到**。

### T1 · Day1 10:00 PM 提需求

**用户原话**：「大客户催了三次了——帮我出一个『用户积分明细导
出 CSV』的功能，三天内上。后端要快，前端要能下载，数据库那边
别让表慢爆。你拆一下让团队干。」

**父 state 变化**：新增 `anchor`（目标：积分明细导出 CSV，截止 D+3
日 18:00，客户=大客户 X）；`plan_state` version=1，phase='scoping'；
新增 1 条 `decision`（key='product.scope.export_v1'，"MVP=CSV 导
出，按用户+时间范围过滤，最大 10 万行"）；尚未派发，暂无
`delegation_state`。

**父 manifest zone**：`system_static` 1200 / `tool_schemas` 1200
（PM 场景剪掉大部分代码工具，保留 delegation/artifact 查询）/
`session_anchor` 300 / `plan_todo` 200 / `recent_tail` 0。总 ≈ 2900，
`reason='initial_turn'`，`dropped_count=0`。

**LLM 看不到**：客户催单的原始邮件（小陈只贴了结论）。

### T2 · Day1 10:30 PM-agent 拆分并派发三子

**动作**：PM-agent 基于 T1 的 `anchor` + `decision` 拆 3 子任务：
FE（导出页 + 下载按钮 + 进度提示）/ BE（`GET /exports/points` 流
式接口 + 鉴权 + 限流）/ DBA（`points_detail` 表加 `(user_id,
created_at)` 复合索引 + 迁移脚本）。PM-agent 调用 `spawn_delegate`
工具**创建 3 个 child session**（`sess-fe-xxx` / `sess-be-yyy` /
`sess-dba-zzz`），同时在 `agent_runs` 表为每个子起 run（`parent_
run_id=父 run` / `delegation_id=del-export-v1-{fe,be,dba}`）。

**父 state 变化**：`plan_state` version=2，phase='delegating'；新增
3 条 `session_todos`（fe/be/dba 各一条，父 session 视角的"外包任
务"，`status='active'`，`payload.delegate_to=child_session_id`）；
**新增 3 条 `session_state_items(category='delegation_state')`**：
`item_key='delegation:fe'` / `:be` / `:dba`，每条 payload 含
`{child_session_id, child_run_id, status:'spawned', phase:null,
last_summary_ref:null, blocker:null, spawned_at}`，`token_estimate`
预留 120。`agent_run_events` 写入 3 条 `delegation_spawn` event。

**子 state 变化**（3 子 session 各自）：各子 session 在自己
`session_state_items` 里新建 `anchor`（继承父目标的子切片：FE 只看
到 UI 要求 / BE 只看到接口契约 / DBA 只看到索引建议）；各子 `plan_
state` version=1。子 session **不**写 `delegation_state`（它们不委派
别人）。

**跨 session artifact**：无（派发阶段）。

**父 manifest zone**：`system_static` 1200 / `tool_schemas` 1200 /
`session_anchor` 320 / `plan_todo` 320（3 条"派发给子"todo）/
`delegation_state` 420（3 条 × ~140 tokens，仅 status=spawned 骨架）
/ `recent_tail` 700 / `summary` 0。总 ≈ 4160，`reason='delegation_
spawn'`，`dropped_count=0`。

**LLM 看不到**：3 子 session 各自的 `anchor` 细节（子 session 自己
的 manifest 里有，但父 session 不重放；这是 A13 的核心体现）。

### T3 · Day1 11:00 三子 agent 并行开工（概述）

**动作**：三子 session 并行推进，各自走自己的 turn 循环。父 session
**不**因为子动了就要 bump manifest——直到子在 `delegation_state`
更新状态才触发父的新一轮渲染。

**子 session 进展概述**（压缩叙述，不是父 session 回合）：

- `sess-fe`：5 轮对话，已生成 `ExportPage.tsx` 骨架 + 一个
  `useExportJob` hook；子 session `plan_state` phase='coding_ui'；
  子 session 产出 1 条 `session_artifacts`（`artifact:fe-export-
  page-v0.1`）。
- `sess-be`：7 轮对话，FastAPI 路由 `/exports/points` 已起骨架，
  正在写分页 + 流式输出；子 session phase='coding_api'。
- `sess-dba`：3 轮对话，已分析 `points_detail` 当前索引，给出
  `(user_id, created_at DESC)` 复合索引方案；phase='drafting_
  migration'。

**父 state 变化**：父 session 本回合**无**用户输入，也无 PM-agent
新动作；三子通过"心跳式"更新父的 `delegation_state` payload
（通过 `update_projection` 调用，由子 session 的 runtime 触发）：3
条 `delegation_state` 的 `status` 从 `spawned` → `running`，`phase`
分别写入，`last_summary_ref` 指向子 session 最近一次 summary 行
的 `item_id`（若子 session 还没产 summary，则为 null）。
`session_state_item_events` 批量记录 3 条 mutation（audit）。

**父 manifest zone**：本回合不对父 session LLM 发起请求（没有用
户输入），因此**不新增** `context_manifests` 行；状态只是在 DB 侧
推进。这是 A13 的重要细节——父 session 的 turn 节奏由用户驱动，
子 session 的进展是**异步投影**到父的 `delegation_state`，不强制引
发父 turn。

**LLM 看不到**：本轮父 LLM 未被调用。

### T4 · Day1 14:30 PM-agent 主动汇总第一批进度

**动作**：父 session 收到 "delegation_state dirty"信号（3 条都已从
spawned→running 且都有 phase），PM-agent 在下一轮空闲时主动聚
合一次，不等 PM 问。

**父 state 变化**：`plan_state` version=3，phase='coordinating'；3
条 `delegation_state` 的 `summary_text` 被刷新，各 120–180 tokens
（"FE 已起页面骨架，待后端接口"/"BE 路由骨架完成，正写流式分
页"/"DBA 完成索引方案草稿，迁移脚本待审"）；**新增** 1 条
`summary`（key='summary:delegation_day1_round1'，token_estimate
≈ 220，父 session 对本轮协调的 L1b 叙述）。

**父 manifest zone**：`delegation_state` 600（3 × ~200 tokens，含
phase + summary_text）/ `plan_todo` 320 / `session_anchor` 320 /
`summary` 220 / `recent_tail` 500（T1–T2 的对话尾）/ system+tools
2400。总 ≈ 4360，`reason='delegation_poll'`，`dropped_count=3`
（3 子 session 各自未进入父 prompt 的完整 transcript 候选）。

**LLM 看不到**：3 子 session 的 15 轮 transcript 原文（累计 ~4k
tokens）；子 session 自己的 context manifest（父 session UI 的
Context 侧栏**是否**暴露子 manifest，本场景触碰到设计文档未定义
的边界，见 §7 F6）。

### T5 · Day1 16:00 PM 追问「FE 那边做到哪了？」

**用户原话**：「FE 那边做到哪了？客户下午要看个 demo。」

**动作**：PM-agent **不**重放 `sess-fe` 的 18 轮 transcript，直接走
一条 structured 查询：`SELECT payload_json, summary_text, updated_
at FROM session_state_items WHERE session_id=? AND category=
'delegation_state' AND item_key='delegation:fe'`，拿到单条 ~200
tokens 的摘要，用它直接回答 PM。

**父 state 变化**：新增 1 条 `decision` 无——只是查询性回合；但
`context_manifests` 写了一条 `reason='delegation_state_singleton_
query'` 的极小 manifest。

**父 manifest zone**：`delegation_state` 220（**只取 fe 单条**，be/
dba 本轮被降权到 title-only 共 80 tokens）/ `session_anchor` 320 /
`recent_tail` 400（仅本轮前半段）/ system+tools 2400 / `plan_todo`
180。总 ≈ 3600，**远低于**朴素方案（朴素要回放 fe 18 轮 ~5k tokens
才能回答）。`dropped_count` 极大（40+，代表 fe 子 session 的所有原
始 turn 都被替代为单条 delegation_state 摘要）。

**LLM 看不到**：`sess-fe` 的 18 轮 transcript，包括子 agent 写代码时
失败重试的细节（小陈不需要，也不应该看到）。

### T6 · Day1 17:30 DBA 产出迁移 SQL，跨 session 暴露

**动作**：`sess-dba` 完成迁移脚本，写入 `session_artifacts`
（`artifact_ref='artifact:dba-migration-v1'`，内容 = `ALTER TABLE
points_detail ADD INDEX idx_points_user_created (user_id,
created_at DESC);` + 回滚脚本，byte_size ≈ 3.8KB）。**关键**：
dba-agent 在**自己的** `delegation_state` 是没写权限的（delegation
是父→子方向），它通过回写**父 session 的** delegation:dba payload
完成跨 session 暴露：`payload.last_summary_ref='artifact:dba-
migration-v1'`、`payload.exposed_artifacts=['artifact:dba-
migration-v1']`、`status='awaiting_review'`。

**父 state 变化**：`delegation_state:dba` 的 payload 更新
（`exposed_artifacts` 字段新增）；`session_state_item_events` 写 1
条 mutation；父 session 的 `tool_ref` zone 候选多出一条（指向
artifact）。**新增** 1 条 `decision`（key='eng.dba.migration_ready_
v1'，payload 引用 artifact_ref）。

**跨 session artifact 关键设计**：artifact 本身是**全局** user-scoped
资源（`session_artifacts.user_id` + `artifact_ref` 为主键），不绑死
单个 session。`sess-be` 要引用时通过 `artifact_ref` 直接 load，权限
由 user_id 决定。**此处触碰设计缺口**：跨子 session 的 ACL 规则在
现有文档里不明（见 §7 F2 + §8 建议 2）。

**父 manifest zone**：`delegation_state` 620（3 条，dba 这条含
artifact_ref + awaiting_review 状态 ~260 tokens，fe/be 各 ~180）/
`tool_previews` 180（dba 迁移 SQL 的 preview_text，500 字符）/
其余照旧。总 ≈ 4500，`reason='delegation_update'`。

**LLM 看不到**：迁移 SQL 全文（artifact 落 DB，preview 进 prompt）；
dba-agent 推导索引方案的子 session 详细过程。

### T7 · Day2 09:30 be-agent 跨 session 引用 dba 的 SQL

**动作**：`sess-be` 推进到"接口要校验表索引已就位"，be-agent 走
父 session 的 `delegation_state:dba` 看到 `exposed_artifacts=
['artifact:dba-migration-v1']`，直接 load artifact 读取迁移 SQL 内
容，在自己子 session 的 `active_file` 投影里新增一条引用，并在 BE
代码里加一个 startup health check："若 `idx_points_user_created`
不存在则拒绝启动"。

**子 state 变化**（`sess-be`）：新增 `tool_ref`（`artifact:dba-
migration-v1`，render_mode=preview）；子 session 新增 1 条
`decision`（key='eng.be.index_dependency'，"BE 启动需验证 dba
索引已执行"）。

**父 state 变化**：`delegation_state:be` 的 `payload.deps=
['artifact:dba-migration-v1']` 首次填充；`session_state_item_events`
记录该 mutation。父 session 本回合**无**用户输入，状态纯投影更新。

**跨 session artifact 关键点**：这是 S09 最核心的跨 session 数据流
**——dba → 父 delegation_state → be 自己读**。父 session 扮演"信
息交换中枢"，不直接转发内容（只转发 `artifact_ref`）；be-agent 的
自主性体现在"我知道要去父的 delegation_state 里看同胞兄弟的产
出"，这个约定需要 skill 或 system prompt 固化（见 §8）。

**父 manifest zone**：本回合父 LLM 未被调用，不产生新 manifest。

**LLM 看不到**：父 session 本回合不渲染。

### T8 · Day2 11:00 be-agent 卡在外部 API（waiting_for_external）

**动作**：`sess-be` 调用外部鉴权服务 `auth.company.com/v2/export_
scope` 校验用户能导出的数据范围，外部服务 SLA 抖动、30 秒超时。
be-agent 的 run 进入 `waiting_for_external`，子 session `agent_runs`
行 `status='waiting_for_external'`、`waiting_for='auth.company.com
/v2/export_scope'`。be-agent 写回父 `delegation_state:be`：
`status='blocked'`、`blocker={type:'external_api', endpoint:'auth.
company.com/v2/export_scope', since:...}`。

**父 state 变化**：`delegation_state:be.payload.status='blocked'` +
`blocker` 填充；**新增** 1 条 `error_state`（key='delegation.be.
external_timeout_v1'，`status='active'`，payload 含 endpoint 和首次
发生时间）。父 session 本回合仍无用户输入（后台静悄悄 blocker）。

**父 session 显式问题**：设计文档的 session visible state 枚举
（`idle`/`running`/`waiting_for_user`/`waiting_for_edge`/`waiting_
for_external`/...）说的是**单个 run**的状态。父 run 本身是 `idle`
（等 PM 说话），但子 run 是 `waiting_for_external`，**父的 UI 该
显示什么？**这是 F4 的失败点（见 §7）。设计应明确：父 run 状态
默认是 `idle`；`delegation_state.blocker` 需要在 UI 上独立
badge 显示"1 子 blocked"。

### T9 · Day2 14:00 fe-agent 完成

**动作**：`sess-fe` 收尾，产出最终 `ExportPage.tsx` + 集成测试通过；
fe-agent 回写父 `delegation_state:fe`：`status='completed'`、
`completed_at=...`、`last_summary_ref='summary:fe-final-v1'`（子
session 自己写的 final summary，~160 tokens）、`exposed_artifacts=
['artifact:fe-export-page-v1','artifact:fe-unit-test-v1']`。

**父 state 变化**：`delegation_state:fe.status` 由 `running` →
`completed`；对应的父 `session_todos` 里那条"派发给 fe"todo 被标
`status='done'`；`plan_state` version=4，phase='partial_complete'。

**父 manifest zone**：父 session 本回合也没有用户输入（静默完成），
DB 侧投影更新完即停。

### T10 · Day2 15:00 PM-agent 主动告诉小陈「部分 blocker」

**动作**：父 session 有个周期性的"delegation_state 健康检查"规则
（policy），如果检测到 `delegation_state.*.status='blocked'` 超过 2
小时，PM-agent 会在下一轮主动向 PM 发 `ask_user` + 状态汇报。
T10 触发：T8 的 be blocker 已 4 小时未解。

**PM-agent 输出**（父 LLM 真正被调用的一轮）：「小陈，进度汇报：
FE 已完成（✅），DBA 的迁移 SQL 已产出等你或 tech lead 过（⏳），
BE 从上午 11 点开始卡在 `auth.company.com` 外部鉴权超时（❌，
已 4 小时）。你要我让 be-agent 切 mock 继续，还是等运维恢复？」

**父 state 变化**：本回合 PM-agent 发起一次综合查询：3 条
`delegation_state` 全量 + 1 条 `error_state`（be blocker）+ 1 条
`decision`（T1 的 scope.export_v1），聚合成状态汇报；`context_
manifests` 写 1 条 `reason='partial_blocker_review'` 的 manifest。

**父 manifest zone**：`delegation_state` 1480（逼近 1500 上限——
fe 已完成这条压到 ~300 tokens title+完成时间；be blocked 这条扩到
~750 tokens 含 blocker 详情；dba awaiting review 这条 ~430
tokens）/ `error_state` 180 / `decision` 220 / `plan_todo` 240 /
`session_anchor` 320 / system+tools 2400 / `recent_tail` 600。总 ≈
5440，`dropped_count=42`（3 子 session 累计 42 轮 transcript 候选
未进 prompt），`reason='partial_blocker_review'`。

**LLM 看不到**：3 子 session 累计 42 轮的 transcript；be-agent 与
外部 API 的重试日志（已 > 20 次重试，各种 5xx，艰难地进
`session_tool_outputs` 但绝不进父 prompt）。

### T11 · Day2 15:30 小陈决策 + be 解锁完成

**用户原话**：「切 mock 继续，运维那边我去催——别让进度停。」

**动作**：PM-agent 把决策下发给 `sess-be`（通过修改父 delegation_
state:be 的 `payload.directive='use_mock_auth'`），be-agent 在自
己子 session 读到 directive 后切了 mock，20 分钟后跑通剩余步骤，
产出最终接口 + 集成测试；回写父 `delegation_state:be.status=
'completed'`、`last_summary_ref='summary:be-final-v1'`、
`exposed_artifacts=['artifact:be-export-api-v1','artifact:be-
integration-test-v1']`；`error_state` 那条 blocker 标
`status='resolved'`。DBA 那边小陈口头让她过，也标 completed。

**父 state 变化**：3 条 `delegation_state` 全部 `status=completed`；
`plan_state` version=5，phase='delivery_ready'；`session_todos` 3
条全 done；**新增** 1 条 `decision`（key='eng.be.mock_auth_
directive'，payload="运维未恢复前 be-agent 用 mock，已记录 tech
debt"）。

**父 manifest zone**：`delegation_state` 920（3 条完成态摘要，压
缩到每条 ~300 tokens）/ `decision` 280 / `plan_todo` 160（收尾
项）/ `session_anchor` 320 / system+tools 2400 / `recent_tail`
800。总 ≈ 4880。

### T12 · Day2 16:30 聚合 PR 描述

**用户原话**：「把三边产出合成一个 PR 描述发给 tech lead——要能
看出每边做了什么、哪些 artifact 关联、mock auth 是临时方案。」

**动作**：PM-agent 执行**纯 structured 聚合**：一次性查 3 条
`delegation_state`（取 `last_summary_ref` + `exposed_artifacts`）+
全部 `decision` + 全部 `error_state`，拼成 PR 描述模板：
- 概述（引 anchor + scope.export_v1 decision）
- FE 变更（引 delegation:fe.last_summary + 2 artifact）
- BE 变更（引 delegation:be.last_summary + 2 artifact + mock_auth
  tech debt decision）
- DBA 变更（引 delegation:dba.last_summary + 1 artifact）
- Known issues（引 error_state.resolved blocker + mock_auth
  decision）

**父 state 变化**：产出 1 条 `session_artifacts`（`artifact:pr-
description-export-v1`，~1.2KB）；`plan_state` version=6，
phase='delivered'；新增 `anchor` 子项（"export_delivered"）。

**父 manifest zone**：`delegation_state` 破例扩到 1500（全量 3 条
final summary 合并进入交付）/ `decision` 380（全部 decision 进
retrieved_facts）/ `tool_previews` 180（PR 描述 artifact preview）
/ `session_anchor` 400 / system+tools 2400 / `recent_tail` 500。总
≈ 5360，`reason='delegation_aggregate'`，`dropped_count=55+`（~60
子 session 轮原始 transcript 全部未进 prompt）。

**LLM 看不到**：60 轮子 session 累计 transcript（估算 ~18k tokens）；
PR 描述 artifact 全文（只进 preview）。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 3 子 session 并行 × 60 轮累计 transcript，不能全塞父 prompt | A13：子 session 独立 `session_id`，父只通过 `delegation_state.summary_text` + `last_summary_ref` 聚合 |
| 父 manifest `delegation_state` zone 硬预算 1500，3 子均分还要留 blocker 扩展空间 | A5：预算按子数动态调（3 子时 500/条、单子时 1200、5 子时 300/条）；单子超支走 title-only + artifact_ref fallback |
| PM 追问单子时不能触发全量回放 | A13 + A4：structured 单条查询 `WHERE item_key='delegation:fe'`，其他子降权为 title-only；manifest `dropped_count` 显式披露 |
| 跨子 session artifact 共享（dba→be）不能退化为父 session 转发内容 | A13：父的 `delegation_state.exposed_artifacts` 只存 `artifact_ref`；be-agent 自己 load，`session_artifacts` 以 user_id scope |
| 子 session `waiting_for_external` 父 session UI 必须可见 | A8 + A13：子 run 的 `agent_runs.status`、`waiting_for` 通过 `delegation_state.blocker` 投影到父 |
| 子 session 完成后 decision/summary 聚合要能反向回溯 | A4：manifest_items 的 `source_table='session_state_items'` + `source_id` 可链回子 session 原始 artifact |
| 3 子 session 同时并发写父 `delegation_state` 不能相互覆盖 | `session_state_items` 的 `UNIQUE KEY uq_state_current(session_id, scope, category, item_key)` + `session_state_item_events` previous_hash/next_hash 做乐观锁 |

## 5. 预期 state projection 演化

下面是关键节点快照（按 category 分组 row 数；父/子 session 分开
列；`agent_runs` 的父子关系单列）。

**T2（派发后）** — 父 session：

| 投影 | rows | active | 说明 |
| --- | --- | --- | --- |
| anchor | 1 | 1 | 导出 CSV 目标 |
| plan_state | 1 | 1 | version=2，delegating |
| decision | 1 | 1 | scope.export_v1 |
| session_todos | 3 | 3 | 派发给 fe/be/dba 各一条 |
| delegation_state | 3 | 3 | fe/be/dba，status=spawned |

**T2 子 session（各自）**：`anchor` 1 条 + `plan_state` version=1，
**无** `delegation_state`（子不委派）。

**T2 agent_runs**：1 父 run（`sess-pm`，无 parent_run_id）+ 3 子
run（每个带 `parent_run_id=父` / `delegation_id=del-export-v1-{
fe,be,dba}`），`idx_runs_parent` 反查 O(log N)。

**T7（dba 产出 + be 引用后）** — 父 session：

| 投影 | rows | active | 说明 |
| --- | --- | --- | --- |
| plan_state | 1 | 1 | version=3，coordinating |
| decision | 2 | 2 | +dba.migration_ready_v1 |
| delegation_state | 3 | 3 | fe=running / be=running+deps=[dba-migration] / dba=awaiting_review+exposed=[dba-migration] |
| session_todos | 3 | 3 | 全 active |
| summary | 1 | 1 | delegation_day1_round1 |

**T7 子 session（`sess-be`）**：`tool_ref` 1（引 `artifact:dba-
migration-v1`）+ `decision` 1（be.index_dependency）。跨 session
artifact 的 ACL 关键：`session_artifacts.user_id` 需一致；当前 S09
默认 3 子 session 都在同一 user_id 下，若跨 user（orchestrator 派
给外部 user 的 agent）则此路不通，**设计需明确**。

**T12（交付）** — 父 session：

| 投影 | rows | active | 说明 |
| --- | --- | --- | --- |
| plan_state | 1 | 1 | version=6，delivered |
| decision | 4 | 4 | scope + dba.migration + be.mock_auth + delivery |
| delegation_state | 3 | 3 | 3 条 status=completed |
| error_state | 1 | 0 | be blocker resolved |
| session_artifacts | 1 新 | 1 | pr-description-export-v1 |
| anchor | 2 | 2 | 主目标 + export_delivered |

**关键不变量**：子 session 的 `session_state_items` 行数 **从不**
被父 session 直接修改（只能通过子 session runtime 自己写回或经
约定 API 写父的 `delegation_state` payload）；`agent_runs` 父子边
在 run 完成后仍保留，不做级联删除。

## 6. Token 节省估算

**朴素方案**（父 session 每轮把 3 子 session 完整 transcript 全部
拼进来）：

- 子 session 累计 transcript：fe 18 轮 × ~260 tokens/轮 ≈ 4700，
  be 24 轮 × ~300 ≈ 7200，dba 18 轮 × ~240 ≈ 4320，合计 **≈
  16.2k** tokens
- 3 子各自的 system + tool_schemas（朴素方案重复塞）≈ 3 × 2400 =
  7200
- 父 session 自己的 anchor/plan/decision/recent_tail ≈ 3000
- 朴素方案在 T12（交付聚合）需全量塞：**≈ 16.2k + 7.2k + 3k ≈
  26.4k** tokens 进父 prompt；若加上父 session 自己 12 轮 recent
  tail（~3.5k），**逼近 30k**，中高端模型勉强扛住但 cache miss
  高、延迟大；且模型在 30k 噪声里很难精确聚合 PR 描述。

**本设计方案**（T12 实际进父 prompt 的 token）：

| zone | T12 | 说明 |
| --- | --- | --- |
| delegation_state | 1500 | 3 条 final summary（每条 ~500 tokens，含 status/phase/last_summary_ref/exposed_artifacts） |
| decision（retrieved_facts） | 380 | 4 条 decision（scope / dba / be.mock / delivery） |
| tool_previews | 180 | PR 描述 preview |
| session_anchor | 400 | 主目标 + 交付子项 |
| recent_tail | 500 | T10–T12 父 session 最近 3 轮 |
| summary | 220 | delegation_day1_round1 L1b |
| system_static + tool_schemas | 2400 | 常驻 |
| **有效"会话状态"合计** | **≈ 3180** | delegation + decision + preview + anchor + tail + summary |

**对比**：

| 指标 | 朴素方案 | 本设计 | 节省 |
| --- | --- | --- | --- |
| 父 prompt 会话状态部分 | 26.4k | 3.18k | **~88%** |
| 子 session token 复用 | 重复塞 3 次 system+tools（~7.2k 浪费） | 子 session 自己有独立 manifest 独立预算，父不重复 | 结构性节省 |
| 跨 session artifact | 全文拷贝到父 prompt（可能重复） | 只传 `artifact_ref`（~40 tokens/条） | 带宽节省 |

**计算式**：节省比例 = (26400 − 3180) / 26400 ≈ **87.95%**。更关
键的是**结构层面**：子 session 的 manifest 有独立预算，父 session
的 60 轮子 transcript 从不以原始形态进入父 prompt；**以 `delegation_
state` + `artifact_ref` 做信息边界**，任意一子爆炸式增长都不会污染
父的预算。

**另一个节省维度**：T5「PM 追问 FE 进度」——朴素方案要回放 fe 18
轮 ~5k tokens 才能回答，本设计只查单条 `delegation_state:fe` ≈ 200
tokens，**节省 ≈ 96%**。这种"细粒度提问"在真实 PM 工作流里高频发
生，累计节省非线性放大。

## 7. 潜在失败模式

**F1 · `delegation_state` 更新不及时，父 session 看到过时状态**：
子 session runtime 更新父 `delegation_state` 走的是异步 mutation
队列，若队列延迟（比如 MatrixOne 写入抖动），T5 小陈追问 FE 时
父 session 读到的还是 15 分钟前的快照，报给 PM 的 phase 是错的。
**可观测信号**：`session_state_item_events.created_at` 与 `session_
state_items.updated_at` 的 gap 超 60s；PM 问完 FE 实际状态跟
PM-agent 的回答不一致。**缓解**：子 session 更新父 delegation_state
时走 sync 路径（同 session DB pool 内的事务），或在 payload 里带
`last_child_turn_idx` 让 PM-agent 自查新鲜度。

**F2 · 跨 session artifact ACL 权限不明**：T7 be-agent 去读 dba 产
出的 `artifact:dba-migration-v1`，设计只说 `session_artifacts.user_
id` 做 scope，**没说**同一 user_id 下的两个 sub-agent session 是否
默认互可见，以及**能否跨 user**（orchestrator 派给"公司公共"
agent 账号的场景）。若 ACL 是"任一 user_id 匹配即通"，可能泄露
另一子 session 不该暴露的 artifact；若是"必须同 session"，跨 session
引用直接失败。**可观测信号**：be-agent 调用 load artifact 返回 403
或意外拿到无关 artifact；审计无 ACL 检查记录。**缓解**：§8 建议 2。

**F3 · 子 session summary 未压缩被塞回父**：若子 session 的
`last_summary_ref` 指向的 row `token_estimate` 超 500（比如子自
己的 compaction 没触发或 summary 超长），父 `delegation_state`
zone 直接被单个子挤爆 1500 预算，其他子 title-only 都塞不下。
**可观测信号**：`context_manifests.total_estimated_tokens` 在 T10
/T12 跳涨；`context_manifest_items` 里 `zone='delegation_state'`
且 `included=true` 的某条 `token_estimate > 800`。**缓解**：A5 的
硬限必须由 assembler 强制，超出就 fallback title + artifact_ref。

**F4 · `waiting_for_external` 在父 session 不可见**：父 run 本身
`status='idle'`（PM 没说话），子 run `status='waiting_for_
external'`，若 UI 只看父 run 状态，PM 完全不知道 be 卡了 4 小时；
若规则检查（T10 的健康检查 policy）也没跑，就变成"静默 blocker"。
**可观测信号**：主 session 列表 badge 显示 `idle` 但 `delegation_
state.be.blocker` 非空且 `since` 超阈值；T10 该触发的 ask_user 未
发出。**缓解**：设计补充"父 session visible state 派生规则"——当
任一子 delegation_state 为 blocked > T 分钟，父 session 状态聚合显
示为 `attention_needed`（新状态），或至少 UI 加一个 blocker badge。

**F5 · PR 描述聚合依赖子 session structured decision，若子只写自
然语言则聚合失真**：T12 PM-agent 聚合 PR 描述时预设每个子
session 有 `decision` 行（含 rationale/artifact_ref），但实际子
runtime 可能没强制要求 agent 写 `decision`——如果子只有 summary
自然语言，PM-agent 只能拼"三段话"，失去结构化字段（比如引用具体
artifact_ref 就找不到索引）。**可观测信号**：T12 产出的 PR 描述里
artifact_ref 缺失或手写错拼；`SELECT COUNT(*) FROM session_state_
items WHERE session_id IN (子 session 列表) AND category='decision'`
返回 0 或很少。**缓解**：子 session 的 runtime prompt 里约束"关键
决策必须走 structured decision write"，或父 session 在 completion
前主动请子返回 structured summary（新增协议）。

**F6 · 子 session 的 `context_manifest` 在父 session UI 可见性
未定义**：小陈在父 session 打开 Context 侧栏想看"be-agent 到底
看到了什么输入"——当前设计的 `GET /sessions/{id}/context/latest`
只返 `{id}` 对应的 manifest，没说父 session UI 是否应能跳转到子
session manifest，以及权限如何传递。**可观测信号**：UI 侧栏只显
示父 manifest；用户点 "查看 be 子 session 上下文" 404 或权限失
败。**缓解**：设计补充"父 session 视图可深链到 child_session_id
的 manifest，ACL 沿用 user_id scope"。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Open Questions "Multi-agent web UX: are child agents first-
   class sessions, run children, or both?"——S09 显示必须是
   **both**：session_id 各自独立（为了独立 manifest 预算 + 独立
   retention + 独立 transcript hydration），run 层面通过 `parent_
   run_id` / `delegation_id` 显式父子（为了 reconnect、cancel
   级联、durable run 串联）。单一视角（只 session 或只 run）都无
   法同时满足 A4 + A5 + A13。建议将此 open question 在 v0.2
   中明确为 "both" 并写入 §Durable Runs 和 §Session State
   Projection 两节。
2. §Session State Projection — `delegation_state` 的 payload
   schema 未 spec：S09 要求至少含 `child_session_id`、`child_
   run_id`、`status`、`phase`、`last_summary_ref`、`blocker`、
   `deps`、`exposed_artifacts`、`directive`（父→子下发）、
   `spawned_at`、`last_child_turn_idx`。当前文档只列 category
   名字，没给字段，导致 A13 无法落地。
3. §Context Window Design — `delegation_state` zone 的 token
   预算未在 §Turn Lifecycle 的 zone 表中出现（现有表只列 anchor
   /plan_todo/recent_tail/summary/retrieved_facts/tool_previews
   /workspace 等）。3+ 子 agent 并行时这个 zone 是硬需求，需作
   为一等 zone 加入，默认预算 1200–1500，按子数动态调。

**三条具体建议**：

- **`delegation_state.last_summary_ref` 字段化**：在 S09 的 payload
  schema 里把 `last_summary_ref` 明确为 "指向子 session
  `session_state_items(category='summary')` 的 item_id"，配上
  `last_summary_token_estimate` 做预算预校验（父 assembler 在
  load 前就能知道要不要 fallback 到 title-only）。这条直接解决
  F3。
- **跨 session artifact ACL 规范**：新增文档章节「Artifact Access
  Scopes」，规定 `session_artifacts` 的可见范围三级：`private`
  （仅本 session）、`delegation`（父 session 的 delegation 子树全
  可见，需 `parent_session_id` 字段或查询 `agent_runs.parent_
  run_id` 路径）、`user`（同 user 全可见）。默认值建议
  `delegation`，让 S09 的 dba→be 共享开箱即用，同时不泄露到无关
  session。直接解决 F2。
- **父 session manifest `delegation_state` zone 动态预算公式**：
  在 §Context Window Design 补一段算式：`per_child_budget =
  min(1500 / active_children, hard_floor=200)`，且保留 10%
  overhead 给 blocker 扩展。当 blocker 发生时，该子的预算临时扩
  到 2× `per_child_budget`，从 `recent_tail` 借（因为 blocker 诊
  断不需要最近对话细节）。这条直接回应 A5 在 S09 的具体数值要
  求，也顺带让 F4 的 "blocker 不可见" 在 manifest 层先暴露。
