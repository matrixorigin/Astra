# WALKTHROUGH-S07 — 金融 DBA 老张等 48h 审批链删高危表（v0.2 推演）

> Status: Draft
> Scope: 以 v0.2 session-state 设计（3040 行，19 gap 已 patch）走 S07 的 12 回合
> Companion: `S07-long-approval-wait.md` §1-§3；`web-agent-session-state.md` v0.2

## §1 场景标识与 v0.2 映射

- 一句话：老张周五 22:00 提 3 张高危 DROP，48h+ 四级审批链，期间 2 次 astra 滚动发版 + 1 次 DB schema migration + 1 次执行中二次暂停；DAUT **A7/A8/A14**。
- 本场景最相关的 v0.2 锚点：
  - **G7 审批态机 + conditions 子表** @L1217–1319：`session_approval_conditions` / `session_external_notifications`；状态机 `pending_approvers → approved → pending_requester_confirm → running → completed`；`approved` 不等于执行，需 requester_confirm；`expires_at_server` 基于服务端时钟。
  - **G15 durable run + run_counters** @L896–1063：`agent_runs.owner_pod_id` + `run_generation` + `run_counters.next_event_idx` 单写者事务，`UNIQUE (run_id, event_idx)` 是终态守卫；新事件 `approval_condition_modified` / `requester_confirm` / `run_resumed_after_restart` / `edge_timeout`；`POST /chat/runs/{id}/input` 强制 idempotency_key；`approval` 语义 tuple `(approval_id, decision, actor_user_id)` 额外去重。
  - **G11 workspace reachability** @L2665–2716：`reachability_probe` + online/reconnecting/offline/detached；cloud 转发 edge 工具前 200ms HEAD 探活，不在浏览器本地降级。
  - **G12 next-action confidence** @L657–698：≥0.8 auto / 0.5–0.8 ask / <0.5 澄清；approval 类 suggestion TTL=24h。
  - **Interaction Semantics** @L2630–2650：`waiting_for_edge` 300s 超时并写 `edge_timeout`；approval 必须 durable；可见状态枚举不含业务子态（如 "requester_confirm" 仅在 `waiting_for` 字段里体现）。
  - **Budget Policy v1** @L2381–2407：`Safety/approvals` 是常驻 zone。

## §2 运行前提

- 初始 schema：`agent_sessions` 已有老张的 session；`agent_runs` 空；`session_state_items` 只有默认 `workspace_state`。
- Workspace authority：**edge**，`edge_bridge_id='edge:vpc-fin-prod'`，`reachability_probe.last_ok_at='周五 21:49'`；edge 可执行工具 `sql.explain/sql.dry_run/sql.execute`，cloud 仅持久化。
- 模型窗口：主模型 Claude Opus 200k，`budget_template_id='budget_v1_200k'`（非 8k）；NextActionExtractor 走 budget_v1_8k 同一套阈值。
- Pod 初值：`agent_runs.owner_pod_id='pod-astra-073-A'`，`run_generation=1`，`retry_count=0`。
- 审批者身份池：`zhang_lead`（上级 DBA 主管）、`risk_lead`（风控 Leader）、`cto`（首席技术官）；外部通知走 Lark adapter（v0.2 只给契约，实现侧 MVP）。
- SSE 契约：server 15s heartbeat，client 45s 判死；多 tab 共享 watermark。

## §3 端到端运行轨迹（12 回合）

### T1 · 周五 21:50 — 起单，要 explain + 执行计划

- **User 动作**：老张输入「周会纪要里那三张表——`user_behavior_logs_2019`、`legacy_risk_scoring_v1`、`marketing_campaign_archive_2020`，业务方签字了，冷备已确认。你先 explain 一下，估个大小，再让我看执行计划。」
- **Server behavior**：INSERT `agent_runs(run_id='run:drop-3tables-20260508', status='running', execution_mode='edge', owner_pod_id='pod-A', run_generation=1, trigger_type='user_message')`；INSERT `run_counters(run_id, next_event_idx=0)`；workspace_state 已存在；`anchor` 写入；通过 `run_counters` 事务分配 evt_idx=0 `turn_started`、evt_idx=1 `tool_call:sql.explain`（cloud relay 先 200ms HEAD 探活 `reachability_probe`=online）、evt_idx=2 `tool_result`（表元数据：三表 rowcount 估算 620M、总 bytes_est=34GB，其中 `marketing_campaign_archive_2020` 逻辑大小仅 10GB）。
- **User sees**：SSE stream 事件 0..2；Web 侧显示 explain 结果卡片、workspace badge "edge: vpc-fin-prod ✅"；manifest zones：`workspace_state`=280 / `anchor`=260 / `tool_schemas`=1200（edge SQL 子集）/ `recent_tail`=0 / `system_static`=1200，合计 ≈ 2940 tokens。

### T2 · 周五 22:00 — agent 识别高危，入 waiting_for_user

- **User 动作**：无（agent 主动 pause）。
- **Server behavior**：DROP 命中 high_risk 元规则（生产 + 跨业务批量）；事务分配 evt_idx=3 `approval_request`（payload `{approval_id:'approval:drop-3tables-run123', required_approvers:['zhang_lead','risk_lead','cto'], requested_by:'zhang', summary, expires_at_server=<now+48h>}`）；INSERT `session_state_items(category='approval_state', item_key='approval:drop-3tables-run123', status='pending_approvers', payload_json={approvals:[], condition_refs:[], evidence:[], linked_approval_ref:null, ttl_seconds=172800})`；INSERT 3 条 `session_external_notifications`（adapter='lark', recipient_ref=each approver, `idempotency_key=sha(approval_id+approver)`），对应 evt_idx=4/5/6 `notification_dispatched`；UPDATE `agent_runs status='waiting_for_user', waiting_for='approvers:zhang_lead,risk_lead,cto'`，checkpoint_json 写 "已完成 explain，待四级审批"。
- **User sees**：Web 显示 "⏸ 等待审批（0/3 签）"；pending approval 卡片 URL 直链；三位审批人收到 Lark push 带审批 URL + ttl 倒计时。manifest：approvals zone=420，本轮无新 LLM 调用。

### T3 · 周六 10:30 — 上级"有条件批准"（rename 策略）

- **User 动作**：zhang_lead 在 Web 打开 pending 卡片：「批，但 `user_behavior_logs_2019` 之前有数据科学同学引用过，改成 RENAME 到 `_trash_` 前缀，保留 7 天。其他两张直接 DROP。」
- **Server behavior**：`POST /chat/runs/run:drop-3tables-20260508/input` 带 `idempotency_key`；服务端按 `(approval_id, 'approve_with_conditions', 'zhang_lead')` 语义去重；`run_counters` 事务分配 evt_idx=7 `approval_decision`（conditions_ref=['cond_rename_7d_ubl2019']）；INSERT `session_approval_conditions(condition_id='cond_rename_7d_ubl2019', condition_type='rename_and_hold', check_trigger='pre_execute', condition_spec_json={target:'user_behavior_logs_2019', strategy:'rename_to_trash', hold_days:7}, added_by='zhang_lead', status='active')`；UPDATE `approval_state.payload_json.approvals[]` append、`condition_refs[]` 追加、version bump。`agent_runs` 不动（仍 pending，还差 2 人）；INSERT evt_idx=8 `notification_acknowledged`（adapter='lark', external_ref=lark_msg_id）。
- **User sees**：上级看到"您已批准，条件已记录"；老张 Web 显示 "1/3 签"；新增条件行 UI 气泡。manifest：本轮仅接收输入，不调 LLM。

### T4 · 周六 15:00 — 上级改条件（+ preprod dry-run）

- **User 动作**：zhang_lead：「刚跟风控对齐，先在 preprod 跑一遍完整流程，产生报告附到审批单。」
- **Server behavior**：`POST …/input` → evt_idx=9 `approval_condition_modified`（`{approval_id, condition_id='cond_preprod_report', operation='add', previous_hash, next_hash}`）；INSERT `session_approval_conditions(condition_type='dry_run_evidence', check_trigger='pre_execute', spec={env:'preprod', require:'full_dry_run_report'})`；agent 异步起子 run `run:dryrun-preprod-20260509`（独立 `agent_runs` 行，`parent_run_id=null`，但主 approval.evidence[] 会引 artifact_ref）；子 run 完成后写 `session_artifacts(artifact_ref='artifact:dryrun-report-preprod-v1', byte_size≈12KB)`，回填 `approval_state.payload_json.evidence[]`。approvals 已签数组**不重置**（v0.2 G7 状态机没把 "condition_modified" 退回 pending_approvers）。
- **User sees**：老张 Web 上 conditions 列表 +1 条；evidence 链接可点；审批已签的上级不用重签。manifest：子 run 有自己的 context；主 run manifest 不变。

### T5 · 周日 20:00 — astra runtime 0.7.3→0.7.4 滚动发版 #1

- **User 动作**：无。
- **Server behavior**：旧 pod `pod-A` rolling shutdown：写 `checkpoint_v1{graceful:true, last_batch_id}`，释放 `owner_lease_expires_at`；新 pod `pod-B` 启动 → `state_builder.rs` `SELECT * FROM agent_runs WHERE status IN ('running','waiting_for_user','waiting_for_edge')` → 按 `event_idx ASC` 读 events 0..9 rehydrate → 校验 checkpoint.graceful=true → `run_counters` 事务 takeover：`run_generation=2`，`owner_pod_id='pod-B'`；evt_idx=10 `run_resumed_after_restart`（`{previous_owner_pod_id:'pod-A', owner_pod_id:'pod-B', run_generation:2, checkpoint_version:'checkpoint_v1'}`）；`retry_count` 保持 0。SSE 端老张的浏览器 45s 内收到 reconnect，`GET …/stream?last_index=9` 续接，拿到 evt 10。
- **User sees**：Web 短暂 reconnect toast；pending approval 卡片内容不变；审批者的 Lark 消息无任何变化（ttl 未变）。无 LLM 调用。

### T6 · 周一 09:00 — 风控静默签字 + CTO 终审（带执行窗口 + 导出要求）

- **User 动作**：risk_lead 从 Lark 点链接走 SSO 静默 approve（evt_idx=11）；CTO 走 SSO：「看过 preprod 报告，rename OK。执行时间窗口限定今天 10:00–12:00，执行完立即导出 audit 给合规。」
- **Server behavior**：evt_idx=11 `approval_decision`（risk_lead, approve, conditions=[]）；evt_idx=12 `approval_decision`（cto, approve, conditions_ref=['cond_exec_window','cond_post_export']）；INSERT 两条 `session_approval_conditions`（`condition_type='execution_window', check_trigger='pre_execute+on_tool_call', spec={start:'2026-05-11T10:00+08', end:12:00}`；`condition_type='post_export', check_trigger='post_complete'`）；UPDATE `approval_state.status='approved'`，approvals=3/3；按 v0.2 G7 状态机推进到 `pending_requester_confirm`；UPDATE `agent_runs.waiting_for='requester_confirm:zhang'`，`status` 仍 `waiting_for_user`（这是 v0.2 明确取舍：visible state 枚举不细分，子态放 waiting_for 字段）。
- **User sees**：老张收 Lark「审批通过，待您确认执行」；Web 卡片切换到 "Requester confirm pending"；approvals zone=480。

### T7 · 周一 09:30 — astra 0.7.4→0.7.5 发版 #2 + DB schema migration

- **User 动作**：无（运维窗口）。
- **Server behavior**：pod-B graceful shutdown → pod-C 起。同窗口 DBA 给 `agent_run_events` 在线 `ADD COLUMN agent_id VARCHAR(128) NULL`（v0.2 schema L962 已预留，幂等）。rehydrate：SELECT runs、events 0..12、`run_counters` takeover → `run_generation=3`；evt_idx=13 `run_resumed_after_restart`。新列对 0..12 旧行返回 NULL，`RunEngine` 按可选字段处理不报错。`retry_count` 保持 0。`waiting_for` 保持 `requester_confirm:zhang`；approval ttl 仍有 18h 余量。
- **User sees**：老张未打开 Web，不感知。审批者的 Lark 消息链接仍有效（server-side expires_at_server 不变）。manifest：无 LLM 调用。DB DDL audit 独立于 `agent_run_events`，可交叉对账。

### T8 · 周一 10:30 — 老张 requester confirm，进入执行

- **User 动作**：老张打开 Web：「看过了，按 rename + drop 两步来，开搞。」点击 "Confirm execution" 按钮（带条件 snapshot 预览）。
- **Server behavior**：`POST …/input` 带 idempotency_key；服务端按 `(approval_id, 'confirm', 'zhang')` 二级去重；evt_idx=14 `requester_confirm`（`{approval_id:'approval:drop-3tables-run123', confirmed_by:'zhang', approval_state_version:3, confirmed_at_server}`）；执行前逐条校验 `session_approval_conditions WHERE check_trigger IN ('pre_execute','pre_execute+on_tool_call')`：rename_and_hold/preprod_report/execution_window 全绿。UPDATE `approval_state.status='running'`、`agent_runs.status='running', waiting_for=NULL`。cloud relay 转发 edge 工具前 200ms 探活（`reachability_probe.last_ok_at` 刷新）；evt_idx=15 `tool_call:sql.execute` RENAME → evt_idx=16 `tool_result` OK；evt_idx=17 `tool_call` DROP legacy_risk_scoring_v1 → 18 OK。
- **User sees**：Web 上"执行中"进度卡；SSE 实时流 15/16/17/18；manifest 本轮重新调 LLM，见 §7。

### T9 · 周一 10:45 — `marketing_campaign_archive_2020` 实际 100GB，runtime guard 触发二次暂停

- **User 动作**：无（agent 自动 pause）。
- **Server behavior**：执行第三张表前 edge 侧 `sql.explain` 返回物理大小 100GB（逻辑 10GB 是归档压缩后元数据）；命中 `bytes_actual_vs_estimated_ratio > 5x` runtime guard；evt_idx=19 `runtime_guard_triggered`、evt_idx=20 `pause_requested(reason='oversize_table')`。设计决策（v0.2 G7 未显式说二次暂停，但 `linked_approval_ref` 字段 @L1281 明示支持）：**新建** `session_state_items(category='approval_state', item_key='approval:oversize-check-run123', status='pending_requester_confirm', payload_json={linked_approval_ref:'approval:drop-3tables-run123@v3', trigger:'oversize_guard', observed_bytes:107374182400, estimated_bytes:10737418240, required_approvers:[], ttl_seconds=3600})`，不走四级重审 —— 原审批范围仅变动为 requester-only delta（F6 讨论）。UPDATE `agent_runs.waiting_for='requester_confirm:zhang(size_anomaly)'`。执行 window 条件 `check_trigger='pre_execute+on_tool_call'` 重新校验：现在 10:45 仍在 10–12 窗口内 ✅。
- **User sees**：Web "⏸ 尺寸异常，待您确认"；卡片显示对比 10GB vs 100GB；Lark push 只发给老张，不打扰 CTO。

### T10 · 周一 10:50 — 老张口语「继续吧，放心」

- **User 动作**：老张自然语言输入：「继续吧，放心，这张表就是归档的，物理大小大是因为没 compact。」
- **Server behavior**：`NextActionExtractor` 按 v0.2 G12 优先级：step 1 structured events 命中 "当前有 pending approval:oversize-check 等 requester 答复"，但 "继续吧" 不是结构化 confirm 事件；step 2 cheap rule（"继续"/"放心"）→ `suggestion(source='rule', confidence=0.7, candidates=['confirm_oversize_then_drop','cancel_and_export_partial'])`，0.5–0.8 段 → **ask_user** 而非 auto-accept；同时保留现有结构化 suggestion（不覆盖）。Web 弹出确认按钮 "确认继续 - 含 100GB DROP"；老张点击 → `POST …/input` → evt_idx=21 `requester_confirm`（`{approval_id:'approval:oversize-check-run123', linked_approval_ref:'approval:drop-3tables-run123@v3', confirmed_by:'zhang', approval_state_version:1}`）。UPDATE 第二条 `approval_state.status='running'`；UPDATE `agent_runs.status='running'`。evt_idx=22 `tool_call:sql.execute` DROP marketing_campaign_archive_2020 起飞。
- **User sees**：老张先看到澄清卡、再看到 "执行中"；第一次没被小模型直接放过的体验。

### T11 · 周一 11:20 — 执行完成

- **User 动作**：无（监控）。
- **Server behavior**：DROP 100GB 表 edge 侧耗时 28 分钟；evt_idx=23 `tool_result`（OK）；evt_idx=24 `run_completed`（summary：3 表处置 + 实际耗时 + conditions 命中清单）；UPDATE 两条 `approval_state.status='completed'`（G7 终态，"fulfilled" 属老 v0.1 文案，v0.2 统一为 `completed`）。UPDATE `agent_runs.status='completed', waiting_for=NULL, updated_at`。`session_approval_conditions` 全部 status 由 active → 保留（不软删，仅靠 approval_state 终态表达）；**post_complete** 触发 `cond_post_export` — agent 自发地把"导出 audit"加入 plan_state/todo_state。
- **User sees**：Web "✅ 全部完成" + 后置 todo "导出 audit"。

### T12 · 周一 11:30 — 审计导出

- **User 动作**：老张：「按 CTO 要求，把这次清理的完整 audit 导出给合规。」
- **Server behavior**：agent 触发工具 `audit.export`：JOIN `agent_run_events`（24 条）+ `session_state_items(category='approval_state' AND run_id=?)`（2 条）+ `session_approval_conditions WHERE run_id=?`（5 条：rename_7d / preprod_report / exec_window / post_export / oversize_linked）+ `session_external_notifications WHERE run_id=?`（4 条 dispatched/ack）+ 子 run `run:dryrun-preprod-20260509` 的事件链；时间轴渲染成 PDF/JSON 双格式。产出 `session_artifacts(artifact_ref='artifact:audit-drop-3tables-20260511', byte_size≈86KB)`，preview 500 字符进 prompt。evt_idx=25 `tool_call:audit.export`、26 `tool_result`、27 `run_completed`（audit 是新 sub-run 还是主 run 的尾部，v0.2 未硬性规定；此处并入主 run 的尾部事件）。
- **User sees**：Web 弹出下载链；合规邮件发出。§5 讨论 "audit run 应否独立 run_id" 的 gap 候选。

## §4 发现的别扭点（Frictions）

### F1 · T10 "继续吧" 面对两条同时存在的 pending approval —— G12 规则匹配并非唯一解

- **Ref**：T10 / §Next-Action Confidence State Machine L659–698
- **Observation**：T10 瞬间系统里同时有 `approval:drop-3tables-run123` (status=running) 和 `approval:oversize-check-run123` (status=pending_requester_confirm)；虽然前者 running 理应排除，但 G12 规则优先级仅描述 confidence 档位，不明确"同 run 多 approval 时的目标选择算法"。
- **Why friction**：若实现者简单按 `WHERE status LIKE 'pending%'` 筛到 oversize 一条是运气好；真实场景里可能还有 suggested_next_action 候选同时存在，候选集合与 approval 集合的 join 策略没定。
- **Severity**：notable

### F2 · `requester_confirm` 事件语义 tuple 未覆盖 linked_approval_ref

- **Ref**：T10 evt_idx=21 / L1042 `approval_decision` tuple 与 L1046 `requester_confirm` payload
- **Observation**：v0.2 对 `POST …/input` 的 approval 语义去重是 `(approval_id, decision, actor_user_id)`（L1021）。`requester_confirm` 的 payload 只有 `approval_id/confirmed_by/approval_state_version/confirmed_at_server`，没有 `linked_approval_ref`。若老张 T10 浏览器误双击，第二次 POST 用同 idempotency_key 可以去重；但如果换 tab 提交又恰好点了原审批而不是 oversize 审批（两条都 pending 状态的窗口），会不会写成错 approval_id 的 confirm？
- **Why friction**：实现上需要前端把 `linked_approval_ref` 作为 URL 参数带过来，且后端要校验 linked_approval_ref 与当前 pending 的 oversize_check item_key 一致。契约层没强制。
- **Severity**：notable

### F3 · `execution_window` 条件的 `check_trigger='pre_execute+on_tool_call'` 复合枚举

- **Ref**：T6/T9 / G7 L1230–1232 `check_trigger VARCHAR(64)`
- **Observation**：S07 条件里执行窗口需要在 pre-execute **和**每次 tool_call 前双校验（否则 T9 二次暂停后若拖到 12:05 才确认，仍可能越窗口执行）。v0.2 `check_trigger` 是单值 VARCHAR，没定义复合语义；枚举本身也未列出。
- **Why friction**：实现要么把条件拆成两行（condition_type 同但 check_trigger 不同），要么自定义分隔符字符串，前者优于后者但 v0.2 未明示。运行时忘了 on_tool_call 这一侧校验，会产生"审批通过但执行越窗口"的合规事故。
- **Severity**：**blocking**（合规）

### F4 · T7 schema migration 跨越时 `run_generation` 与 `event_idx` 分别由 takeover / 事务分配，但 DDL 期间的窗口无法保证 `run_counters` 事务行锁不被 migration 打断

- **Ref**：T7 / G15 L987–1004
- **Observation**：`ADD COLUMN` 在 MatrixOne 是否在线 non-blocking 要看实现，若 `run_counters` 处于事务内而 DDL 恰好在该行上排队，takeover 事务可能超时；v0.2 没给 "DDL 期间 runs 暂停写入" 的协议。
- **Why friction**：生产上 DDL + rolling restart 重叠（S07 T7 就是这个窗口），可能触发 run 停滞几秒到几十秒，`owner_lease_expires_at` 若短会误判丢主。
- **Severity**：notable

### F5 · 两条 approval_state 行在 audit 导出时的时间线合并需额外 pass

- **Ref**：T12 / G7 L1217–1319 + §Interaction Semantics
- **Observation**：合规报告需要"沿 linked_approval_ref 回到 CTO 签字"；JOIN 时要递归解 `approval_state.payload_json.linked_approval_ref`（JSON 字段，不在 `session_approval_conditions` 也不在 run_events）。v0.2 G7 schema 没在 `session_state_items` 上加 `linked_approval_ref VARCHAR` 列。
- **Why friction**：MatrixOne 不允许 payload_json 过滤（设计文档 §2 note），递归解链要靠应用层循环；数据量大时慢。
- **Severity**：notable

### F6 · T9 "二次暂停复用预授权 vs 新建 approval" 策略仍靠约定

- **Ref**：T9 / G7 `linked_approval_ref` + S07 §7 F6
- **Observation**：v0.2 G7 提供 `linked_approval_ref` 字段，但没在状态机里显式指定"linked approval 是否享受 requester-only 确认"的规则。S07 当前实现选"新建 + requester-only"；其他场景（如 S13 风险升级）可能期望重走四级。
- **Why friction**：不同团队对 linked approval 的 "必需签字集合" 理解不一致，审批谁签、能否绕开 CTO，合规会问。
- **Severity**：notable

## §5 发现的新 gap 候选

**gap 候选 1**：

- **title**: `session_approval_conditions.check_trigger` 复合语义缺
- **triggered_by_turn**: T6, T9
- **related_v0.2_section**: §Approval State and External Notification Adapter @L1224–1244
- **severity**: high
- **rationale**: `VARCHAR(64)` 单值无法表达"pre_execute + on_tool_call 双点校验"；实际合规场景常见（窗口、额度、密钥有效期都需要多点校验）。强制拆行会让条件数随 trigger 种类指数扩。
- **proposed_fix**: 把 `check_trigger` 改成 `SET`/多行拆分的 enum 枚举，并在状态机明确 "每个 trigger 命中点都要跑一次 eval"。

**gap 候选 2**：

- **title**: `linked_approval_ref` 未在 `session_state_items` 作为独立索引列
- **triggered_by_turn**: T9, T12
- **related_v0.2_section**: §Approval State contract @L1273–1285, §Session State Projection
- **severity**: medium
- **rationale**: audit 导出、合规查询常需"给我所有挂在 approval:drop-3tables-run123 上的子审批"，payload_json 内 JSON 字段不许过滤。
- **proposed_fix**: 在 `session_state_items` 为 category='approval_state' 加一列 `linked_parent_item_id VARCHAR(128) NULL` 并建索引；payload_json 保留语义冗余。

**gap 候选 3**：

- **title**: "audit export" 作为 post_complete 条件的动作事件类型未定义
- **triggered_by_turn**: T12
- **related_v0.2_section**: §Run Event payload contracts @L1037–1051
- **severity**: low
- **rationale**: 当前 T12 用普通 `tool_call/tool_result` 实现 audit 导出。但 `post_complete` condition 触发的动作最好有专用事件类型（如 `post_condition_discharged`）便于反向查询"所有 post_export 条件都履约了吗"。
- **proposed_fix**: 新事件 `post_condition_discharged(condition_id, artifact_ref, discharged_at_server)`。

## §6 Gap 组合效应检验

### 组合 A：G7 + G15（审批跨 2 次重启 + 1 次 schema migration）

- **运行时可能发生的坑**：T5/T7 两次重启。G15 要求 rehydrate 先读 `agent_runs` 再按 `event_idx` 读 events，`run_counters` takeover bump `run_generation`；G7 审批态机需要从 events 序列重建 approval_state 投影。若 rehydrate 顺序（events 先 or approval_state 先）不统一，存在短暂的"approval 是 approved 但 agent_runs 还是 waiting_for_user 且 waiting_for 残留旧值"的不一致窗口。
- **v0.2 是否覆盖**：partial。G15 规定了 events 单调序、takeover bump generation；G7 规定了 state machine。但二者的**读侧重建顺序**未明确约定。
- **建议**：在 §Run Event Ordering and Ownership 补一句 "State projections MUST be derived from events, not read from `session_state_items` during rehydrate；rehydrate 完成前 API 不暴露 run"。

### 组合 B：G7 + G12（"继续吧" 在 pending approval 之上）

- **运行时可能发生的坑**：T10 场景。G12 给了 confidence 阈值；G7 给了审批态机。但 G12 的 "结构化事件优先" 条款里，"pending approval 等 requester 答复" 是结构化还是规则？v0.2 L676 的 state machine 示例仅给 "structured_event hit / rule hit / small_model hit" 三类，没把 "当前 run 有 pending approval" 列为 structured hit。
- **v0.2 是否覆盖**：partial。实现者可能把它归为 structured 自动执行（危险），也可能归为 rule ask_user（当前 S07 选择）。差异可能导致未确认直接 DROP 100GB。
- **建议**：G12 状态机明确列出"run 有 pending approval 时，自然语言 '继续' 类 ≤0.79，走 ask_user；唯一 exception 是 structured confirm button click"。

### 组合 C：G7 + 外部通知 adapter + G11 workspace reachability

- **运行时可能发生的坑**：T2 发 Lark 通知给 CTO；若此刻 edge bridge 进入 reconnecting（G11），`waiting_for_edge` 与 `waiting_for_user` 状态互相覆盖谁优先？CTO 周末点 Lark 链接时，若 edge offline，页面上 approve 按钮能否生效？v0.2 规定 "approval 是 cloud 侧事件，与 edge 无关"，但 UI 展示的 workspace badge 会显示"edge offline"，让 CTO 怀疑能否批。
- **v0.2 是否覆盖**：partial。G11 的 online/offline 状态只管工具可见性；但审批按钮的 enable 语义需要明确"不依赖 edge"，否则 UX 退化。
- **建议**：在 §Workspace Reachability and Degradation Semantics 补一段 "Approval controls are cloud-side and MUST remain enabled regardless of edge status"。

## §7 Token 实际 walkthrough —— T8 requester_confirm 一轮 manifest

最紧张的 LLM 调用发生在 T8（老张 confirm 后 agent 要生成执行计划 + 调用 edge tool）。按 `budget_v1_200k` 估算（单位：tokens）：

| Zone | 预算上限 | 实际占用 | 说明 |
|---|---:|---:|---|
| `system_static` | 1800 | 1200 | 常驻 prompt + 安全契约 |
| `tool_schemas` | 3500 | 1600 | edge SQL 工具 + audit 工具（剪枝后） |
| `skills` | 1200 | 300 | 只活 "DBA ops skill" 一个 |
| `session_anchor` | 400 | 260 | "清理三张废弃表，走完整审批链" |
| `plan_todo` | 1500 | 420 | 当前 plan：rename → drop × 2 + export_audit |
| `recent_tail` | 6000 | 1100 | T1 原始需求 + T6 CTO 批注 + T8 老张确认（过滤掉 T3/T4/T7 纯系统事件） |
| `summary` | 1500 | 200 | L1b summary 极简（审批阶段无需 summary） |
| `retrieved_facts` | 2500 | 360 | preprod dry-run artifact preview |
| `tool_previews` | 1500 | 180 | explain 结果前 500 字符 |
| `safety_approvals` | 1200 | 480 | 关键：单条 approval_state 结构化行 —— 3/3 approvals + 5 条 conditions（含 exec_window/rename_7d/preprod_report/post_export/oversize_linked 预置） |
| **Soft total** | — | **6100** | 远低于 200k，无 overflow 风险 |
| `reserved_output` | 3000 | 3000 | 预留 |
| `safety_buffer` | 500 | 500 | |

**关键计算对比**：若走 transcript replay（朴素方案），4 位审批者原话 + 2 次 condition 改动 + 2 次 restart 提示 ≈ 18000 tokens；结构化 approval_state 仅用 480 tokens，**37.5× 精简**，并且 "条件被 LLM 脑补漏读" 风险归零（条件是 DB 行而非自然语言）。

**drop cascade 检查**：v0.2 §Budget Policy v1 L2396 明确 "Safety/approvals 始终包含"；`approval_state` zone 不会被压到 0。即使 recent_tail 被压（T1 原始需求保留 80%，T6 CTO 批注保留 100%，T8 老张确认保留 100%），也无越界。

**token provenance**：`context_manifest_items` 每行必写 `zone/source_table/source_id/source_hash/reason`（v0.2 L1474–1492）；T8 本轮 reason 应为 `approval_resume`（v0.2 L1514 reason 枚举）。

## §8 最终判断

- **本场景下 v0.2 是否可实现**：⚠️（绝大多数 blocking 已在 G7/G15/G11/G12 resolution 下闭环；仍有 F3 复合 check_trigger 属合规 blocker，其余 frictions 属 notable 实现约定）。
- **顺畅度**：⚠️ 有明显但可管理的摩擦。12 回合里 T5/T7 的重启、T6 的 pending_requester_confirm 切换、T8 的 conditions 校验、T10 的 G12 仲裁都能按 v0.2 原路径走通；真正需要补的是 check_trigger 复合枚举、linked_approval_ref 列化、审批按钮与 edge 状态解耦三处契约说明。
- **一句话总结**：v0.2 把 S07 从"靠自然语言 replay 审批语义"彻底改写成"靠结构化 approval_state + durable events + condition 子表"，合规正确性提升一个数量级；剩下的是把 3–4 条约定写进契约而不是留给实现默契。
