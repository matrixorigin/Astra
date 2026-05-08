# S07 — 金融公司 DBA 老张等 48 小时审批链删高危线上表

> Status: Draft v0.1
> DAUT: **A7, A8, A14**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：老张，某持牌金融公司 DBA，12 年经验。做事稳、说话短、
见过两次因为手抖 DROP 被通报的兄弟。公司有一套强制审批流（上级
→ 风控 → CTO），他自己非常认这套流程——不是不信任自己，是怕出
事的时候没人签字背锅。

**业务背景**：运维周会定了一批清理任务，三张废弃业务的线上表
（`user_behavior_logs_2019`、`legacy_risk_scoring_v1`、
`marketing_campaign_archive_2020`）要 DROP。数据早已冷备，业务方书
面确认不用了。老张打算走 astra web agent 起一轮 SQL 执行，让 agent
负责生成、预估、落盘 audit，自己只在关键节点确认。问题是：周五夜
里起的单，审批链要过周末，整条链路最少 48 小时。期间 astra 服务可
能发版、DB 可能做 schema migration、等待态必须无损恢复。

**一句话目标**：让 agent 在 48 小时审批等待 + 服务重启 + 条件修改
+ 执行中二次暂停的综合压力下，始终把审批状态、执行意图和 audit
trail 完整绑定在同一个 `run_id` 上，任何一步都可被回溯、被 CTO 事
后签字确认。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 会话总轮数 | 12 回合 |
| 会话持续时间 | 约 61.5 小时（周五 21:50 → 周一 11:30） |
| 审批参与人 | 4 人（老张 / 上级 DBA 主管 / 风控 Leader / CTO） |
| run 进入 waiting 总时长 | ≈ 60 小时 |
| astra 服务发版 | 2 次（T5 周日 20:00、T7 周一 09:30） |
| DB schema migration | 1 次（T7 同窗口内） |
| `agent_run_events` 累计 | 约 28 条（覆盖 4 次审批 + 2 次条件改动 + 2 次恢复 + 2 次暂停 + 确认/完成/导出） |
| `approval_state` 项数 | 最终 1 条 active（item_key='approval:drop-3tables-run123'），payload 含审批 timeline |

## 2. 被测 DAUT

S07 的核心矛盾是「超长等待 × 多人审批 × 基础设施扰动 × VPC edge
连接」，下列 3 条假设被同时压测。

| 编号 | 压测方式 |
| --- | --- |
| **A7** | `run_id=run:drop-3tables-20260508` 生命周期跨 61.5 小时 + 2 次 astra 服务重启 + 1 次 DB migration。每次重启后 `state_builder.rs` 必须从 MatrixOne `agent_runs`（status / waiting_for / checkpoint_json）+ `agent_run_events`（按 event_idx 顺序）完整重建 run，SSE 续接靠 `last_index=` 参数，不走 `InMemoryRunStateStore`。T5/T7 两次重启后 run 不能变成 "orphaned" 或 "retry"；`retry_count` 保持 0；老张和各审批者在 Web 打开的"pending approval"卡片 URL 依然有效，点击后能拿到完整上下文。 |
| **A8** | 审批不是内存态。T2 agent 识别高危时写一条 `session_state_items(category='approval_state', item_key='approval:drop-3tables-run123', status='pending')`，同时写一条 `agent_run_events(event_type='approval_request', payload_json=<diff + 影响面 + 所需审批人列表>)`，run.status 切到 `waiting_for_user`、`waiting_for='approvers:zhang_lead,risk_lead,cto'`。T3/T4/T6 每一次审批动作（approve / approve_with_conditions / condition_modify）都是**新的** `agent_run_events` 行，event_idx 单调递增，条件变更 diff 放 payload_json，前一条事件不被改写。最终 CTO 同意这条事件的 `run_id` 与 T2 的请求事件严格同一 run_id；审批链任何一步脱链都视为失败。 |
| **A14** | 生产 DB 位于客户 VPC 内，workspace authority 是 **edge**：`session_state_items(category='workspace_state', payload_json={authority:'edge', edge_bridge_id:'edge:vpc-fin-prod', executable_tools:['sql.explain','sql.dry_run','sql.execute']})`。cloud 只持久化 session + audit trail，SQL 真正执行通过 edge bridge 在客户 VPC 的 MatrixOne 上跑。T9 发现表大小异常时，大小探测本身也是 edge 侧 `sql.explain` 返回。合规要求：**审批 trail 必须在 cloud**（CTO 在公司 SSO 系统里签字），但**执行面必须在 edge**（DB 连接不出 VPC）。两条数据面在同一个 run_id 下绑定。 |

## 3. 会话时间线

12 个回合覆盖：T1 起单 → T2 识别高危入 waiting → T3/T4/T6 三级审批
+ 条件改动 → T5/T7 两次基础设施扰动 → T8 老张确认执行 → T9 执行
中二次暂停 → T10 "继续吧" 语义歧义 → T11 执行完成 → T12 审计导出。
每回合字段：**用户原话**（老张语气：稳、短，上级/风控/CTO 更正式）、
**state 变化**（agent_runs / agent_run_events / session_state_items）、
**manifest zone**（workspace authority 与 approval 提示如何进 prompt）、
**audit 记录**（可被 CTO 审阅的条目）。

### T1 · 周五 21:50「三张废弃表，按周会纪要清了」

**老张**：「周会纪要里那三张表——`user_behavior_logs_2019`、
`legacy_risk_scoring_v1`、`marketing_campaign_archive_2020`，业务方
签字了，冷备已确认。你先 explain 一下，估个大小，再让我看执行计
划。」

**state 变化**：新建 `agent_runs(run_id='run:drop-3tables-20260508',
status='running', trigger_type='user_message', execution_mode=
'edge')`；新增 `workspace_state`（authority=edge，edge_bridge_id=
`edge:vpc-fin-prod`）；`anchor` 写入（"清理三张废弃表，走完整审批
链"）；`agent_run_events` 写 evt_idx=1 `turn_started`、evt_idx=2
`tool_call:sql.explain`（edge 侧返回表元数据 + rowcount 估算）。

**manifest zone**：`workspace_state` 280（edge 标识 + 工具清单）/
`anchor` 260 / `tool_schemas` 1200（仅 edge 可用的 SQL 工具集） /
`recent_tail` 0 / `system_static` 1200。总 ≈ 2940。

**audit**：`agent_run_events` 2 条；`conversation_log` 1 条用户消息
 + 1 条 agent plan 输出。

### T2 · 周五 22:00 agent 识别高危、进 `waiting_for_user`

**agent 侧**：DROP TABLE 命中 high_risk 规则（生产环境 + 表大小 >
1GB + 无恢复窗口声明）；同时三张表命中"跨业务批量高危"元规则，
要求四级审批链。agent 不执行，请求审批。

**state 变化**：新增 `session_state_items(category='approval_state',
item_key='approval:drop-3tables-run123', status='pending', payload_
json={risk_level:'high', required_approvers:['zhang_lead','risk_
lead','cto'], requester:'zhang', sql_diff:<三条 DROP 语句>,
estimated_impact:{tables:3, total_rows_est:620M, total_bytes_est:
34GB}, conditions:[]}, token_estimate=420)`；写 `agent_run_events
(event_idx=3, event_type='approval_request', payload_json=<同上>)`；
**`agent_runs` UPDATE**：`status='waiting_for_user'`，`waiting_for=
'approvers:zhang_lead,risk_lead,cto'`，`checkpoint_json` 记录"已完
成 explain，待审批后执行 DROP 批次"。

**manifest zone**：本轮不再调 LLM（agent 已决定 pause）；下一轮
prompt 里 `approval_state` 进 `Safety/approvals` zone，token ≈ 420。

**audit**：evt_idx 1..3；`session_state_item_events` 1 条 mutation=
`insert`。老张的 Web 界面显示 "⏸ 等待审批（3/3 人未签）"。

### T3 · 周六 10:30 上级"有条件批准"

**上级（DBA 主管）**：（在 Web 打开 pending approval 卡片）「批，但
`user_behavior_logs_2019` 之前有个数据科学同学引用过，你先留 7 天
回收窗口，不要直接 DROP，改成 RENAME 到 `_trash_` 前缀。其他两张
直接 DROP。」

**state 变化**：`agent_run_events` 写 evt_idx=4 `approval_decision`
（approver='zhang_lead'，decision='approve_with_conditions'，
conditions=[{table:'user_behavior_logs_2019', strategy:'rename_to_
trash_7d'}]）；`approval_state` UPDATE payload_json（approvals 数
组 append 1 条、conditions 数组合并这一条），version bump 1，
`status` 仍为 `pending`（还差 2 人）。**`agent_runs` 不动**，继续
`waiting_for_user`。

**manifest zone**：无 LLM 调用（仅接收审批输入）。

**audit**：evt_idx=4 写入；`session_state_item_events` mutation=
`update`，previous_hash/next_hash 记录 payload 变更。

### T4 · 周六 15:00 上级又改条件

**上级**：「刚跟风控对齐了一下，他们建议你先在 preprod 环境跑一遍
完整流程（RENAME + DROP），产生一份执行报告附在审批里，我们再走
CTO。你按这个更新审批单。」

**state 变化**：`agent_run_events` 写 evt_idx=5 `approval_condition_
modified`（approver='zhang_lead'，condition_diff={added: [{env:
'preprod', require:'full_dry_run_report'}]}）；`approval_state`
payload_json append 新 condition，version bump 2；审批链状态不变
（仍 pending，不重置已签字）。agent **异步**在 preprod 跑 dry-run
（走另一个独立 run `run:dryrun-preprod-20260509`，通过 edge bridge
到 preprod MatrixOne），结束后产出 `session_artifacts(artifact_ref
='artifact:dryrun-report-preprod-v1', byte_size≈12KB)`，把 ref 写
回主 run 的 `approval_state.payload_json.evidence[]`。

**manifest zone**：agent 发起子 run 时走自己的 context；主 run 不
更新 manifest。

**audit**：evt_idx=5；`session_state_item_events` 再一条 update；
preprod dry-run 产生独立的 `agent_runs` + 其 evt 链（与主 run 通
过 `approval_state.payload.evidence` 引用关联，不是 parent_run_id）。

### T5 · 周日 20:00 astra 服务发版 #1

**场景**：运维团队周日晚做 astra runtime 0.7.3 → 0.7.4 常规发版，
滚动重启，`InMemoryRunStateStore` 那一份内存态全部丢失。重启后监
控侧看到 `run:drop-3tables-20260508` 仍在 `waiting_for_user`。

**state 变化**：重启后 `state_builder.rs` 冷启动加载：`SELECT *
FROM agent_runs WHERE status IN ('running','waiting_for_user',
'waiting_for_edge') AND updated_at > ?` 拿回 run 行；再按 `SELECT
* FROM agent_run_events WHERE run_id=? ORDER BY event_idx ASC`
拿回 1..5 全部事件，重建 in-memory run state；`checkpoint_json`
用来恢复"已 explain 完、待执行 DROP 批次"的中间语义。`agent_runs.
retry_count` 保持 0（不是重试）；run 继续 `waiting_for_user`。

**manifest zone**：无 LLM 调用。

**audit**：不写新事件（发版不是业务动作）；但 runtime observability
记录 "run rehydrated from DB, last_event_idx=5"。老张的 Web 侧在发
版窗口收到一次 SSE 断线，重连时 `GET /chat/runs/run:drop-3tables-
20260508/stream?last_index=5` 直接续上，页面不闪不丢。

### T6 · 周一 09:00 CTO 终审同意

**CTO**（通过 SSO 直接进入审批页）：「看过 preprod 报告，rename 策
略 OK，放行。但要求执行时间窗口限定在今天 10:00–12:00，执行完立
即导出 audit 给合规。」

**state 变化**：`agent_run_events` 写 evt_idx=6 `approval_decision`
（approver='risk_lead'，此前风控在 T5/T6 之间的某时刻已静默签字，
简化并入 T6 表述）、evt_idx=7 `approval_decision`（approver='cto'，
decision='approve'，conditions=[{window:'2026-05-11T10:00/12:00+08:
00', post_action:'export_audit'}]）；`approval_state` UPDATE：
`status='approved'`，approvals 数组满 3/3，conditions 合并 T3/T4/T6
全部条件。`agent_runs` UPDATE：`waiting_for` 清空，但 `status` 暂
不切回 running——还需要**发起人**老张自己做最终 "confirm to
execute" 动作（设计文档 Interaction Semantics：审批通过 ≠ 立即执
行，仍需 requester 确认）。status 改为 `waiting_for_user`，
`waiting_for='requester_confirm:zhang'`。

**manifest zone**：无 LLM 调用。

**audit**：evt_idx 6、7 写入；`approval_state` version bump 3。

### T7 · 周一 09:30 astra 发版 #2 + DB schema migration

**场景**：同一个窗口里，运维做 astra 0.7.4 → 0.7.5 修一个鉴权热
修，同时 `agent_run_events` 表做了一次 schema migration（新增
`agent_id VARCHAR(128) NULL` 列，是在线 `ADD COLUMN`，对存量行填
NULL，向后兼容）。

**state 变化**：runtime 重启 + schema 扩列。rehydrate 流程同 T5：
先 SELECT agent_runs、再按 event_idx 读 1..7 条事件。新 schema 列
对旧事件返回 NULL，`RunEngine` 解析时按可选字段处理，不报错。run
状态保持 `waiting_for_user`（requester_confirm）。

**manifest zone**：无 LLM 调用。

**audit**：不写业务事件；但 DB migration 本身在 DB 层有独立 DDL
audit，可与 `agent_run_events` 的 created_at 时间窗交叉对账。老张
这一刻没打开 Web，不感知。

### T8 · 周一 10:30 老张确认执行

**老张**（上班打开 Web，看到"审批已通过，待您确认执行"）：「看过
了，按 rename + drop 两步来，开搞。」

**state 变化**：`agent_run_events` 写 evt_idx=8 `requester_confirm`
（payload_json={confirmed_by:'zhang', conditions_snapshot:<T6 最终
conditions 快照>, approval_state_version:3}）；`agent_runs` UPDATE
`status='running'`，`waiting_for=NULL`。agent 开始按计划执行：先
RENAME `user_behavior_logs_2019` → `_trash_user_behavior_logs_
2019`（通过 edge bridge），再 DROP `legacy_risk_scoring_v1`、再
DROP `marketing_campaign_archive_2020`。每条 SQL 执行都写 `tool_
call` + `tool_result` 事件对，event_idx 递增到 12。

**manifest zone**：LLM 本轮重新拿到完整 context：`approval_state`
（approved，全 conditions） / `workspace_state`（edge） / `anchor`
/ `recent_tail`（T1 原始需求 + T8 确认）/ `tool_schemas`（edge SQL
执行工具）。总 ≈ 4200。

**audit**：evt_idx 8..12。

### T9 · 周一 10:45 发现 `marketing_campaign_archive_2020` 比预估大 10 倍

**agent 侧**：执行到第三张表，edge 侧 `sql.explain` 返回该表实际
大小 100GB（不是周会纪要里估的 10GB，原因：归档压缩后元数据显示
为逻辑大小而非物理大小）。命中 `bytes_actual_vs_estimated_ratio >
5x` 的运行时 guard rule，agent 自动 pause 不执行 DROP。

**state 变化**：`agent_run_events` 写 evt_idx=13 `runtime_guard_
triggered`（rule='oversize_table_guard'，observed={bytes: 107_
374_182_400}，estimated={bytes: 10_737_418_240}）、evt_idx=14
`pause_requested`（reason='oversize_table'）；`agent_runs` UPDATE
`status='waiting_for_user'`，`waiting_for='requester_confirm:zhang
(size_anomaly)'`；**新增**一条 `session_state_items(category=
'approval_state', item_key='approval:oversize-check-run123',
status='pending', payload_json={trigger:'oversize_guard',
observed_bytes:100GB, prior_approval_ref:'approval:drop-3tables-
run123@v3'})`——是否复用前一次 approval 作为"预授权"由设计决定，
**当前实现采用"新增 pending"而非自动继承**（见 §7 F6）。

**manifest zone**：暂停不调 LLM。

**audit**：evt_idx 13、14；第二条 `approval_state` 行插入，
`session_state_item_events` mutation=`insert`。

### T10 · 周一 10:50 老张「继续吧，放心」

**老张**：「继续吧，放心，这张表就是归档的，物理大小大是因为没
compact，没业务依赖。」

**state 变化**：`NextActionExtractor` 面对"继续吧" + 当前有一条
pending `approval:oversize-check`，优先级执行：step 1 structured
event 匹配到"requester 对 oversize_check pending 的自然语言答复"；
但**自然语言"继续吧"**在设计文档层面**不等价于** `POST /chat/
runs/{run_id}/input` 的 confirm 动作——这里设计文档没定义清楚（见
§7 F5）。当前实现策略：agent **不自动** approve，写一条
`suggested_next_action`（source='rule', confidence='medium',
candidates=['confirm_oversize_then_drop','cancel_and_export_
partial']），同时在 Web 上弹一个确认按钮，让老张点击"确认继续 -
含 100GB DROP"。老张点击后 `agent_run_events` 写 evt_idx=15
`requester_confirm`（payload_json={confirm_target:'approval:
oversize-check-run123', linked_approval_ref:'approval:drop-3tables-
run123@v3'}）；oversize `approval_state` UPDATE `status=
'approved_by_requester'`（注意不是 `approved`，因为没走四级审批，
只是发起人在已批的范围内做"尺寸 delta 确认"）；`agent_runs`
UPDATE `status='running'`。

**manifest zone**：`approval_state` zone 扩到 720（两条 approval
并列渲染） / `anchor` 320 / `recent_tail` 1100 / `system_static +
tool_schemas` 2600。总 ≈ 4740。

**audit**：evt_idx=15；**关键**：oversize approval 与原始 approval
通过 `linked_approval_ref` 绑定，合规侧可沿这条引用回到 CTO 签字。

### T11 · 周一 11:20 执行完成

**agent**：DROP `marketing_campaign_archive_2020` 成功（edge 侧耗
时 28 分钟），三张表处置全部完成。

**state 变化**：`agent_run_events` 写 evt_idx=16/17 `tool_call/
result` 对、evt_idx=18 `run_completed`（payload_json 含最终三表
处置 summary）；`agent_runs` UPDATE `status='completed'`，
`waiting_for=NULL`，`updated_at` 刷新；两条 `approval_state` 行
都 UPDATE 为 `status='fulfilled'`。

**manifest zone**：run 完成不再渲染新 prompt；Web 侧显示"全部完
成"。

**audit**：evt_idx 16..18；总事件数 18 条。

### T12 · 周一 11:30 审计导出

**老张**：「按 CTO 要求，把这次清理的完整 audit 导出给合规，要能
看出谁在什么时候批的、条件是什么、执行用了多久、有没有异常暂停。」

**state 变化**：agent 发起 `SELECT * FROM agent_run_events WHERE
run_id='run:drop-3tables-20260508' ORDER BY event_idx ASC`（18 条）
+ `SELECT * FROM session_state_items WHERE run_id=? AND category=
'approval_state'`（2 条：主审批 + oversize delta）+ 引用的 preprod
dry-run 子 run 的事件链；把 3 路数据按时间轴渲染成合规报告。产出
`session_artifacts(artifact_ref='artifact:audit-drop-3tables-
20260511', byte_size≈86KB)`，preview 500 字符进 prompt。

**manifest zone**：`retrieved_facts` 破例扩到 1800（审批 timeline
+ 条件变更 diff + oversize 异常记录全部进入交付）/ `recent_tail`
300（T11/T12）/ `tool_previews` 180（audit preview）。总 ≈ 4800。

**audit**：最终产物本身是新的 `session_artifacts`，但其内容完全
由现有 durable 表拼装，不产生新的业务事件。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 48 小时 waiting，2 次 runtime 重启，1 次 DB migration | A7：`agent_runs` + `agent_run_events` 是唯一权威；`state_builder.rs` 冷启动按 status + event_idx 完整 rehydrate；`InMemoryRunStateStore` 不承担持久化职责 |
| 4 位审批者、条件被改 2 次，链上不能丢签 | A8：每一次 decision / condition_modify 是独立 `agent_run_events`；`approval_state.payload_json.approvals[]` 追加式（不覆盖），version 单调 bump |
| 发起方 "确认执行" 与审批通过是不同阶段 | Interaction Semantics：`waiting_for_user` 有两种子态（approvers / requester_confirm），UI 区分展示 |
| 生产 DB 在 VPC 内，但审批 trail 在 cloud | A14：`workspace_state.authority='edge'` + edge_bridge_id；cloud 持久化 session+审批事件，edge 持久化 SQL 执行日志，两条数据面通过同一 run_id 关联 |
| 二次暂停产生独立 approval 还是复用原 approval | 设计未定义：当前实现采用"新建 oversize-check approval + linked_approval_ref 指向原 approval"，避免"预授权无限外延" |
| "继续吧"语义：对发起人 vs 对审批人不同 | 设计只讲了「low confidence → suggested_next_action + ask_user」，未明确"发起人答复 pending"这类细分状态机（见 §8） |
| SSE 断线 + URL 直链稳定 | `last_index=5` 续接；pending approval 页面即便断 48 小时重开，也按 run_id 查 approval_state 即时渲染 |

## 5. 预期 state projection 演化

按关键节点给快照。`session_state_items` 按 category 分组列 row 数
（括号内为 `status` 分布）。

**T2（周五 22:00 请求审批）**：

| 投影 | rows | 说明 |
| --- | --- | --- |
| anchor | 1 | 清理三张废弃表 |
| workspace_state | 1 | authority=edge |
| approval_state | 1 | status=pending, required=3 |
| agent_runs | 1 | status=waiting_for_user, waiting_for='approvers:...' |
| agent_run_events | 3 | turn_started / tool_call:explain / approval_request |

**T6（周一 09:00 CTO 同意）**：

| 投影 | rows | 说明 |
| --- | --- | --- |
| approval_state | 1 | status=approved, approvals=3/3, conditions 含 rename + preprod_report + window + export_audit |
| agent_runs | 1 | status=waiting_for_user, waiting_for='requester_confirm:zhang' |
| agent_run_events | 7 | +approval_decision × 3（上级/风控/CTO） + condition_modified × 1（T4） |
| session_state_item_events | 4 | approval_state 的 insert + 3 次 update |

**T8（周一 10:30 老张 confirm 执行）**：

| 投影 | rows | 说明 |
| --- | --- | --- |
| agent_runs | 1 | status=running, waiting_for=NULL |
| agent_run_events | 12 | +requester_confirm + tool_call/result × 2（rename + drop） |
| session_artifacts | 1 | preprod dry-run 报告 ref 已在 approval.evidence |

**T11（周一 11:20 完成）**：

| 投影 | rows | 说明 |
| --- | --- | --- |
| approval_state | 2 | 主审批 fulfilled + oversize-check fulfilled |
| agent_runs | 1 | status=completed |
| agent_run_events | 18 | 全链路 evt_idx=1..18，event_type 涵盖 turn_started / tool_call / tool_result / approval_request / approval_decision × 3 / approval_condition_modified / requester_confirm × 2 / runtime_guard_triggered / pause_requested / run_completed |
| session_state_item_events | ≥ 8 | 每次 approval_state 变更留档 |

**关键不变量**：整个 61.5 小时 + 2 次重启 + 1 次 schema migration，
`run_id` 保持同一；`agent_run_events.event_idx` 在 `(run_id, event_
idx)` 上全局连续、无空洞、无重复（`UNIQUE KEY uq_run_event_idx`
约束）；`approval_state` 物理 delete 计数 = 0。

## 6. "Token 节省估算" — **不是 token，是合规正确性**

此场景与 S01/S02/S05 不同：核心价值**不是省 token**，而是 agent
48 小时后恢复时不会凭空脑补审批条件。仍给出对比，但结论要加粗读。

**朴素方案**（依赖 transcript replay 重建 approval 状态）：

- T8 老张回来确认时，agent 若没有 durable `approval_state`，只能把
  T2–T7 的 transcript 全量拼回 prompt 让 LLM "理解当前审批进度"
- 4 位审批者的原话 + 2 次条件改动 + preprod 报告附件的 12KB 摘要
  + 2 次 runtime 重启的上下文提示 ≈ **18k tokens**
- LLM 必须从自然语言里解析出"当前条件是什么"——容易出错：比如
  上级 T4 的"先在 preprod 跑一遍"和 CTO T6 的"窗口限定 10:00–12:00"
  有没有冲突？LLM 可能"脑补"出一个不存在的条件
- 最大风险：LLM 漏读 rename 策略 → 直接 DROP `user_behavior_logs_
  2019` → 合规事故，无法从 transcript 指控模型错误

**本设计方案**（结构化 `approval_state` + `agent_run_events`）：

| zone | T8 | 说明 |
| --- | --- | --- |
| approval_state | 480 | 单条 payload_json 渲染：3/3 approvals + 条件清单（rename_7d、preprod_report ref、window、post_action） |
| workspace_state | 280 | edge authority + bridge id + 可用工具 |
| anchor | 260 | 清理三张表的原始目标 |
| recent_tail | 900 | T1（需求）+ T8（老张确认）两段 |
| retrieved_facts | 360 | 引用 preprod dry-run artifact preview |
| system_static + tool_schemas | 2400 | 常驻 |
| **审批相关有效状态** | **≈ 480** | 一条结构化行覆盖全部审批事实 |

**对比**：

| 指标 | 朴素方案 | 本设计 | 差值 |
| --- | --- | --- | --- |
| 审批信息塞 prompt 的 token | 18000 | 480 | 37.5× 精简 |
| "条件被 LLM 脑补漏读"风险 | 高 | **零**（结构化字段） | 合规可审 |
| audit 导出（T12） | 需回放 transcript | 3 张表 JOIN 直出 | 毫秒级 |
| 48h 后 rehydrate 正确性 | 取决于 LLM | 取决于 DB | **确定性 vs 概率性** |

> **this is not about token saving, this is about compliance.** 金
> 融行业 DROP 生产表是"监管事后可查"的动作。agent 的职责是把每一
> 个签字、每一条条件、每一次中断原因永久绑定到同一个 `run_id` 上，
> 让 CTO 事后能签字确认这条 audit trail 就是当时执行的 trail。
> Token 省下来是副产品；真正关键是"LLM 不参与审批语义理解"。

## 7. 潜在失败模式

**F1 · condition 半结构化只存 payload_json，查询不到细节**：当前
`approval_state.payload_json.conditions[]` 是 JSON 数组，MatrixOne
schema note 明确说 "production queries must not filter inside JSON
payloads"。合规若想问"所有要求 preprod dry_run 的审批单"，无法用
WHERE 子句筛，只能全表扫 JSON。**可观测信号**：合规导出查询耗时
随审批单总数线性上升；`EXPLAIN ANALYZE` 显示对 `session_state_
items` 全表扫。

**F2 · VPC edge 断连时 run 卡在 `running` 而非切 `waiting_for_
edge`**：T8 开始执行 DROP 时，若 edge bridge 丢连，设计里有
`waiting_for_edge` 这个状态但当前实现（`InMemoryRunStateStore` +
未完成的 edge bridge wiring）可能让 run 停留在 `running`，
`agent_run_events` 不写事件，老张在 Web 上看到"执行中"但没进度。
**可观测信号**：`agent_runs.updated_at` 停滞 > 5 分钟但 `status=
'running'`；`agent_run_events` 最新事件是 `tool_call` 而没有对应
的 `tool_result`。

**F3 · 审批 token 过期但 agent 仍接受 confirm**：T6 CTO 批的窗口
是 "10:00–12:00"，但老张真正点 confirm 是 10:30，到 T11 执行完成
是 11:20，尚在窗口内。**假设** T9 guard 触发后老张当天没回，周二
才回——窗口已过，agent 应拒绝继续执行并要求重新走审批。当前设计
未规定 `approval_state.conditions[].window` 的**运行时再校验**时
机（只在 confirm 时校验？还是每次 tool_call 前都校验？）。**可观
测信号**：run 在窗口外仍然 `status='running'` 并发 DROP；no event
type for `approval_window_check`.

**F4 · 外部通知通道在设计文档未定义**：四级审批链实际需要 Lark/
Slack/企业微信/邮件主动推送（"有一个 approval 在等你"），但
web-agent-session-state.md 只谈 `waiting_for_external` 状态和
`POST /chat/runs/{run_id}/input`，没有 adapter 定义。T3 上级周六
能打开 Web 是"运气好"——他刚好看了手机。**可观测信号**：
`waiting_for_user` 平均停留时长 P90 远超业务 SLA；无 `notification_
dispatched` 事件类型。

**F5 · T10 「继续吧」被 agent 误判为新任务而非 resume**：如果
`NextActionExtractor` 没有"当前有 pending approval 等 requester 答
复"这条 structured short-circuit，小模型可能把"继续吧，放心"解读
为"开始一个新的清理任务"或"继续做一个无关的 todo"。**可观测信
号**：evt_idx=15 的 event_type 不是 `requester_confirm` 而是
`turn_started` 并跟着一串新的 tool_call；`approval:oversize-check`
永远 pending 直到超时。

**F6 · 二次确认 approval_state 复用策略缺失**：T9 产生 oversize
delta 时，现有设计没规定：(a) 是创建新 `approval_state` 行（
current 选择），(b) 还是在原 `approval_state.payload_json` 里 append
一条 "runtime_delta" sub-record，(c) 还是触发整条审批链重走。不同
选择对合规可追性差异巨大。(a) 行数多但易查；(b) 原行变"活文档"，
payload 无限膨胀；(c) 安全但会把 CTO 吵醒。当前选 (a) + `linked_
approval_ref`，但文档未定此规约。**可观测信号**：不同团队实现对
oversize 的处理路径不一致；audit 导出时合规询问"这条 delta 到底算
不算新审批"。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Proposed Schema §2 `session_state_items` — `approval_state` 只
   列了一行 category 描述（"pending or remembered approvals"），没
   给出 `payload_json` 的 sub-schema。S07 要求 `approvals[]` /
   `conditions[]` / `required_approvers[]` / `evidence[]` /
   `linked_approval_ref` 这些字段是合规必需，建议在设计文档把它们
   从"自由 JSON"提升为"约定 schema"。
2. §Interaction Semantics — 目前只说"Approval/question prompts
   must be durable run events"，但没定义"requester 自己的
   confirm"是否单独一类事件，也没定义"approval 通过 ≠ 立即执行、
   需发起人再确认"这个双段状态机。S07 的 T6 → T8 两段就是这个
   gap。建议补一张状态转换图：`pending_approvers → approved →
   pending_requester_confirm → running → completed`。
3. §Phase 1 Run Durability — 当前仍使用 `InMemoryRunStateStore`
   是 known gap；S07 明确要求"48 小时 + 2 次重启 + 1 次 schema
   migration 无损"是生产上线前的硬阻塞。建议在 Exit criteria 里加
   一条"48h 等待态跨重启 rehydrate 通过"的集成测试门槛。

**三条具体建议**：

- **`approval_state` 结构化 condition 字段**：把 `conditions[]`
  从 payload_json 内部提升为独立子表 `session_approval_conditions`
  （approval_item_id / condition_type / condition_spec_json /
  check_trigger / added_by / added_at），支持按 condition_type +
  check_trigger 的索引查询（例如 "所有 window 类条件在窗口过期后
  是否被正确 check"）。
- **`waiting_for_edge` 超时转 failed 策略明确化**：在设计文档
  Interaction Semantics 章补一段"若 `waiting_for_edge` 持续 > N
  分钟（默认 300s），run 应自动写 `edge_timeout` 事件并切换到
  `failed` 或 `waiting_for_user`（取决于重要性）"，并规定这个超
  时是 per-run-configurable。
- **外部通知 adapter 首节**：在 §Interaction Semantics 后新增
  §External Notification Adapter，定义 approval 请求对外推送的协
  议（payload / 幂等 key / delivery receipt 事件），以及对应的
  `agent_run_events` 新事件类型 `notification_dispatched` /
  `notification_acknowledged`。即便 MVP 只实现 Lark 一家，协议先
  定清楚，避免 T3 上级"只因为刚好看手机"这种运气依赖。
