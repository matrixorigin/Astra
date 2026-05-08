# Walkthrough — S04 · 断线续传（5h run + 17 次抖动 + pod 重启 + 双 tab）

> Driver: v0.2 设计文档 `web-agent-session-state.md`（19 gap 全部 resolved）
> 场景: `S04-flaky-network-reconnect.md` §1–§3（§8 未读，避免被引导）
> 目的: 以"真的要实现 S04"的视角，逐回合判定 v0.2 是否顺畅、哪里会卡、token 是否合理

---

## §1 场景标识与 v0.2 映射

一句话：**陈老师在高铁上 5 小时跑 MySQL→MatrixOne 迁移，期间网络断连 17 次、跨一次 pod 滚动升级、9:10 开了第二个 tab；目标是只点 2 次审批，不让 agent 重做任何一步**。被测 DAUT=**A1（服务端权威） / A7（durable run） / A8（waiting 是 durable 状态）**。

最吃紧的 v0.2 章节/字段：

- **§Durable Runs schema** @ L896–976：`agent_runs`（含 `owner_pod_id` L915、`owner_lease_expires_at` L916、`run_generation` L917、`last_event_idx` L918、`checkpoint_json` L920、`retry_scope` L924）+ `run_counters`（L941–952）+ `agent_run_events`（`uq_run_event_idx` L970、`uq_run_event_idempotency` L971、`producer_pod_id` L965）
- **§Run Event Ordering and Ownership** @ L985–1063：counter-based `event_idx` 分配（L987–991）、owner lease（L993–1005）、graceful vs crash（L997–1002）、`checkpoint_v1` schema（L1009–1016）、idempotency 约束（L1018–1022）、approval 事件 & SSE 15s/45s 心跳（L1024–1057）
- **§Web Event Watermark Atomicity (G19)** @ L500–548：IndexedDB 事件行 + `run_event_high_watermark` 同 transaction（L518–527）、gap 检测 replay 协议（L529–537）、多 tab via BroadcastChannel 无 primary（L539–545）
- **§Approval State and External Notification Adapter (G7)** @ L1217–1319：`session_approval_conditions` 子表（L1224–1244）、状态机（L1287–1299）、`expires_at_server` 契约（L1312–1313）
- **§Next-Action Confidence (G12)** @ L657–698：阈值表（L666–671）、single-turn cap 5（L686）、ask-user fatigue policy（L693–696）
- 辅助：**§Web Client Cache** L373–436 + **§Revision Reconciliation (G13)** L438–498 + **§Budget Policy v1** L2381–2440

---

## §2 运行前提

**Schema 初始态**（T01 之前）：

- `agent_sessions` 已有 `s_mig_2026_05_06`（上周起草过 plan）；`session_state_items(category='plan_state', item_key='plan:mysql_to_mo_migration', version=3)` 和 `(category='anchor', item_key='session:migration_goal')` 已存在；`state_revision.monotonic_id=11`。
- `agent_runs` 对本场景无活跃行；上周 `plan:...` 的创建 run 已 `status='completed'`。
- `run_counters` 对 `r_mig_g1234` 尚未创建（T01 首次 INSERT）。
- `session_device_leases` 有陈老师的 MacBook trusted lease（非首次设备），`last_monotonic_id=11`，`expires_at` 未过期。
- `context_manifest_reason_types` 已 seeded 包含 `initial_turn`、`approval_resume`、`post_compaction`（G1）。
- 无 pending approval，无 suggested_next_action。

**Workspace**：`authority='cloud'`（MatrixOne HTAP 集群 + MySQL source + validator），`edge_bridge_id=NULL`，`reachability_probe.last_ok_at` 15 分钟前刷新过（T01 不再重新探测）。工具集：`mysql.query` / `matrixone.exec` / `validate.checksum` / `plan.update` / `approval.request`（cloud-only，不经 edge bridge，**G11 不触发**）。

**Context window**：陈老师用 `gpt-4o-long`（200k token window），走 **`budget_v1_default`**（L2381–2406），**不是** `budget_v1_8k`（L2410–2440）。prompt-cache 对稳定系统 prefix 生效。

---

## §3 端到端运行轨迹（逐回合）

### T01 @ 07:05 — 启动 run（event_idx 0→3）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 打开 laptop，进 `s_mig_2026_05_06`，点「继续 plan」，`POST /chat/stream` 带 `client_watermarks={state_revision:11, run_event_high_watermark:0}` | (1) `INSERT agent_runs (run_id='r_mig_g1234', status='running', owner_pod_id='astra-runtime-6fdb', owner_lease_expires_at=NOW+60s, run_generation=1, last_event_idx=-1)` — 走 L896–938 的 schema；(2) `INSERT run_counters (run_id='r_mig_g1234', next_event_idx=0, owner_pod_id=...)` — L941；(3) 打开一个 DB tx：`SELECT next_event_idx FROM run_counters ... FOR UPDATE` → 0，INSERT `agent_run_events(event_idx=0, event_type='run_started', producer_pod_id='astra-runtime-6fdb')`，`UPDATE run_counters SET next_event_idx=1`，`UPDATE agent_runs SET last_event_idx=0` — L987–991；(4) `build_initial_context()` 写 `context_manifests(reason='initial_turn')` — L2836–2851；(5) 挂 SSE subscriber；(6) LLM 开跑，连续生成 `llm_round_started(1)` / `tool_call_start(2)` / `tool_call_end(3)` 三个事件 | SSE 依次推 id=0..3；前端 IndexedDB 在一个 tx 内写 `run_events` 四行 + `session_watermarks.run_event_high_watermark=3`（L518–527）；timeline 长出 1 条 llm round + 1 条 tool call |

**token 观察**：manifest 约 6.5k（anchor 400 + plan 600 + todos 300 + recent tail 1500 + tool schemas 2500 + reserved 500 + safety 200）。远低于 200k。

**v0.2 检视**:
- counter-based 分配（L987–991）vs 旧的 `MAX(event_idx)+1` scan：这个 run 首次写入就必须先 INSERT `run_counters`（否则后续 `FOR UPDATE` 锁空行）。v0.2 在 L941 CREATE 了表但没明说"run 创建时必须同时 INSERT run_counters 行"—— 属于实现细节，仍在 G15 闭环内。
- `run_generation=1` 的初始值在 L917 是 DEFAULT 1，正确。
- `producer_pod_id='astra-runtime-6fdb'` 写入每个 event（L965），后续 T11 pod 切换后，从 `event_idx=188` 开始 `producer_pod_id='astra-runtime-a81e'` — 审计链上可以直接看到切换点。
- `request_id='req_0001' / trace_id='trace_g1234'` 沿着 L928–929 + L967–968 同时写 `agent_runs` 与 `agent_run_events`，方便跨表 join 跟踪。

---

### T02–T03 @ 07:08 — D01 隧道 15s，出隧道重连（event_idx 3→9 期间断开）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| G1234 进隧道，`EventSource.onerror` 触发，客户端开始指数退避（500ms / 1s / 2s …） | **完全不知道客户端断开**：`tokio::broadcast` receiver drop，producer 继续把 `event_idx=4..9`（2 个 tool_call + 1 个 llm_round + heartbeat）写入 `agent_run_events`，成功 fanout 回 0 个订阅者（这是 A7 的 producer-doesn't-wait-for-subscriber 契约 @ S04§3 T02）；lease 每 20s 续期 — L915–916 | 顶栏「离线」提示条；timeline 冻结但不清空 |
| 15s 后出隧道，客户端发 `GET /chat/runs/r_mig_g1234/stream?last_index=3`（来自 IndexedDB 的 `run_event_high_watermark=3`）| 服务端按 L980 SSE replay：`SELECT ... WHERE run_id=? AND event_idx>3 ORDER BY event_idx LIMIT 1000`；先把 4..9 作为历史 replay，追完后切 live broadcast | SSE 收到 id=4..9，客户端按 L518–527 的 apply 协议在一个 tx 内写入六行 + 推进 watermark 到 9；UI 线性补齐 6 条事件，**无抖动** |

---

### T04 @ 07:40 — 审批 #1（DDL 高危，event_idx=46）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 22 批 INSERT 跑完，agent 要 DROP legacy_orders_2020_q1/q2 | (1) 走 counter 分配 `event_idx=46`，`INSERT agent_run_events(event_type='approval_request', payload_json={approval_id:'appr_ddl_0001', summary:'drop 2 legacy partitions', required_approvers:['u_chen'], expires_at_server:'2026-05-06T09:40:12Z'})` — payload 契约 @ L1041；(2) `UPDATE agent_runs SET status='waiting', waiting_for='tool_approval:appr_ddl_0001' WHERE run_id=? AND status='running'` — CAS guard 防止并发；(3) `INSERT session_state_items(category='approval_state', item_key='appr_ddl_0001', status='pending_approvers', payload_json={...})` — L1271–1285；**没有** condition，所以 `session_approval_conditions` 不写（但 G7 的表结构 L1224 已经 ready）；(4) approval state machine 进 `pending_approvers` — L1289 | SSE 推 id=46 event=approval_request；UI 在中央弹 approval card（三按钮：批准/拒绝/看执行计划）；tool timeline 的 MatrixOne node 亮黄色等待 |

**v0.2 检视**:
- `expires_at_server` 2h 窗口是 server 权威（L1312–1313），UI 按 `ttl_seconds` 显示倒计时但执行时服务端自己判过期 → 高铁信号抖动不会让用户看到"我按了批准但 server 说已过期"这种尴尬（因为 2h >> 任何抖动时长）。
- `required_approvers=['u_chen']` 且 requester=u_chen，属于典型 same-user approval，见 §4 F3。
- approval card 的「让我看执行计划」按钮会触发 `GET /chat/runs/{run_id}/artifacts` 拉 plan 预览（L2774 "artifacts":[...]），该操作不改变 run 状态也不消费 approval ttl —— v0.2 没规定这个交互的 read-only 路径，但 G8 Preview Template Registry 足以覆盖。

---

### T05–T06 @ 07:41–07:41:35 — 审批中 WiFi 抖动 20s + 用户批准（event_idx=47）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 正要点「看执行计划」时 WiFi 抖 20s | **Server 完全不动**：`agent_runs.status='waiting'` 保持；`expires_at_server='09:40:12Z'` 计时照跑（L1312-1313）；approval state 停在 `pending_approvers`；此期间 lease 续期写 `owner_lease_expires_at`，但不写 run event | approval card 还在，背景变橙；approval countdown 停在 UI 最后一次接到的秒数（客户端自行本地插值） |
| 重连 `GET /chat/runs/r_mig_g1234/stream?last_index=46` | replay 0 条新事件（waiting 期间按 L1052 只发 SSE 心跳，**不是 run event**），但 SSE 打开立刻发一个 `event: run_state_sync`（非 run event，是 stream 控制帧）告诉客户端 `status=waiting, waiting_for=appr_ddl_0001, approval_state_version=1` — 这是 A1 服务端权威原则的体现 | approval card 复活，背景恢复绿色；countdown 同步到服务端剩余时间 |
| 点「批准」，`POST /chat/runs/r_mig_g1234/input` body `{kind:'approval_response', approval_id:'appr_ddl_0001', decision:'approve', idempotency_key:'u_chen:appr_ddl_0001:approve'}` | (1) 按 L1018–1022，`(run_id, idempotency_key)` 去重 + `(approval_id, decision, actor_user_id)` 语义去重；(2) counter 分配 `event_idx=47`，`INSERT approval_decision(approver='u_chen', decision='approve', conditions_ref=[], decided_at_server=...)` — L1042；(3) `UPDATE agent_runs SET status='running', waiting_for=NULL WHERE ... AND status='waiting'`；(4) `UPDATE session_state_items ... SET status='running' (state machine L1289–1294 pending_approvers→approved→running)`，注意 **没有** `pending_requester_confirm` 阶段（因为是同一个 requester=approver，按 L1301–1303 的语义可跳过，但 v0.2 在此没有显式快捷通道 — 见 §4 friction F3） | SSE id=47；card 变绿打勾；DDL 开始执行，tool timeline 插入 `matrixone.exec` node |

---

### T07 @ 08:30 — D07 最长断连 4 分 10 秒（event_idx 47→98，server 推进 51 个事件）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 5G→4G 切换，SSE 断；客户端 2s/4s/8s/…/30s cap 无限重连 net::ERR_INTERNET_DISCONNECTED | 继续跑：counter 分配 `event_idx=48..98`，总共 51 个事件（~20 tool_call × 2 边 + 10 llm_round 分步 + 12 progress + 8 heartbeat 事件）。**注意歧义**：L1052 说 "server sends a heartbeat at least every 15 seconds" 指 SSE 流控帧，但 S04 §3 T07 把 8 个 heartbeat 算进了 `agent_run_events`。v0.2 没明说 heartbeat 是否写入 `agent_run_events` — 见 §4 friction F2 | UI 冻结；时间戳不动；"离线"指示条持续显示 |

---

### T08 @ 08:34 — 重连补齐 51 事件（IndexedDB watermark 被 D04 打断的边界条件）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 客户端发 `GET /chat/runs/r_mig_g1234/stream?last_index=46`（**不是 47**：D04 断线时 IndexedDB 的 `oncomplete` 没跑完，G19 的同-tx 约定在 D04 第一次失败；本次是该 commit 恢复后的第一次访问，watermark 留在 46）| 按 L520–526 的 apply 协议替 client 补齐：replay 47..98 共 52 个事件。客户端逐条 apply；`event_idx=47 approval_decision` 在 client `run_events` 表里已经不存在（D04 tx 回滚），**不是** 应用层 dedup 而是 G19 的 `event_idx <= current_watermark and event_hash matches: skip` 分支在 watermark=46 时不触发，所以 47 会被重新插入并正确推进 watermark 到 47—98；因为单 writer + `event_hash` 唯一，48..98 的 sequence 与 server 顺序严格一致 — L518–527 | UI 按 `event_idx` 顺序补出 52 条；approval card 已经是绿色打勾，**不会**因为 replay 47 产生视觉抖动（G19 apply 在本地是幂等替换）；timeline 长出 20 条 tool_call + 11 条 llm_round |

---

### T08.5 @ 08:55 / 09:03 / 09:48 — D08/D09/D12 三次短抖（统一说明，不单独列）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 三次分别 18s / 11s / 12s 断连，模式同 T02–T03 | 每次 server 都不感知 client 断；event_idx 连续推进；client 重连 `stream?last_index=<last_ok>` 拉补齐；apply 走 G19 原子 tx | UI 闪一下"离线/在线"指示条；事件在重连瞬间批量补齐；timeline 无缺口 |

**v0.2 检视**: 这类 <30s 抖动是 G15+G19 的"shake-out test"高频 case，契约上无新增压力。17 次断连里有 11 次都是这种形态（D01/D02/D03/D05/D06/D08/D09/D12/D15/D16/D17），v0.2 的组合路径完全闭环。

---

### T09 @ 09:10 — D10 开新 tab（非刷新，双订阅者）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 新 tab 打开 `/workspace?session_id=s_mig_2026_05_06`；tab#2 先 `GET /sessions/{id}/state?known_state_revision=0`（新 tab 的 IndexedDB 在 Chrome 下其实共享 origin，假设最坏情况独立） | (1) 按 L411–415 返回完整 hot projection + `active_run={run_id, status:'running', run_event_high_watermark:131}`；(2) device lease 重新握手：本设备已在 `session_device_leases` 有 trusted 行，只 `UPDATE last_monotonic_id=current`，不 insert — L454–473；(3) 再发 `GET /chat/runs/r_mig_g1234/stream?last_index=0` → replay 131 条 | tab#2 全量渲染 timeline（~120ms）；同时 tab#1 仍挂 SSE |
| tab#2 订阅并开始接收 live 事件 | `tokio::broadcast` 现在有 2 个订阅者，都收 `event_idx=132..`；**counter-based allocation 不受订阅者数量影响**（L987） | 两个 tab 的 timeline 完全一致；**A1 通过** |

**friction 伏笔**：tab#2 从 `/sessions/state` 拿到 `run_event_high_watermark=131`，但它发 stream 用的是 `last_index=0`（假定最坏情况）。两 API 之间的**契约对齐**没有在 v0.2 显式写，见 §5 新 gap。

---

### T10 @ 09:20 — D11 旧 tab 关电脑盖

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 合盖，tab#1 SSE EOF；tab#2 在外接显示器上继续 | subscriber 从 2 降到 1；event_idx 继续推进；不触发 lease 变化（同 device） | tab#2 无感；tab#1 进 background，不会自动重连（浏览器休眠） |

---

### T11 @ 10:00 — D13 pod 重启 + 用户 30 分钟离线（**最硬压测**，event_idx=187→188）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 运维 SIGTERM pod `astra-runtime-6fdb` | **Graceful shutdown 协议 (L997–1002)**：(1) RunEngine flush 剩余 event（最后 `event_idx=187` 落库）；(2) 写 `checkpoint_json={"version":"checkpoint_v1","graceful":true,"last_batch_id":"batch_147","extra":{}}`（L1007–1016）；(3) `UPDATE agent_runs SET status='running'`（**不标 failed**，因为 graceful=true — 修了原 `state_builder.rs` 的 in-memory 问题）；(4) 释放 `owner_lease_expires_at=NOW`（或让它 10s 内过期）；(5) tab#2 SSE 收到 EOF | tab#2 显示「服务端短暂不可用，重连中」；UI 不清空 |
| 新 pod `astra-runtime-a81e` 起来 | (1) `find_running_runs()` 按 L2796–2808 Phase 1 Exit criteria 找到 `r_mig_g1234`；(2) 读 `checkpoint_json.graceful=true` → 走 resume 分支；(3) CAS 拿 lease：`UPDATE run_counters SET owner_pod_id='astra-runtime-a81e', run_generation=2 WHERE owner_lease_expires_at<NOW OR owner_pod_id IS NULL`；(4) counter 分配 `event_idx=188`，`INSERT agent_run_events(event_type='run_resumed_after_restart', payload={previous_owner_pod_id:'astra-runtime-6fdb', owner_pod_id:'astra-runtime-a81e', run_generation:2, checkpoint_version:'checkpoint_v1'})` — payload 契约 @ L1050；(5) 从 batch_147 的**下一批** batch_148 开始执行 | （用户盖着笔记本没看到） |
| 10:00–10:30 离线 30 分钟 | 新 pod 跑到 event_idx≈205 | — |

**friction 伏笔**：checkpoint 只记 `last_batch_id=147`。如果 batch_147 内部有 5 个 tool call（SELECT MySQL → INSERT MatrixOne → validate → progress 等），shutdown 时这些事件已经全部落库（所以 event_idx 推到 187）；但 checkpoint 只粗粒度记 batch — 见 §4 friction F1。

**v0.2 检视（本回合是 A7 的决胜点）**:
- L997–999 "Normal rolling shutdown writes a checkpoint_v1, sets graceful=true, releases or expires the lease, and the next pod resumes" —— 本回合走这条路径。
- L1000–1002 "Crash recovery without a graceful checkpoint must not silently continue" —— SIGKILL / OOM 场景走另一条。S04 假设 graceful；如果改成 crash，v0.2 要求标 failed 或起新 run with retry_of=<old>，**用户会看到 approval 重走一遍**，不符合"5h 只点 2 次"的目标。
- L1003–1005 `run_generation++` + `producer_pod_id` 区分代际，调试上可以明确定位哪些事件是哪个 pod 产出。
- L1050 `run_resumed_after_restart` payload 必须含 `previous_owner_pod_id / owner_pod_id / run_generation / checkpoint_version` 4 字段 → v0.2 的契约对"UI 渲染系统消息"有充分信息。
- lease 过期机制：L916 `owner_lease_expires_at`，S04 §3 T11 假设 SIGTERM 时主动释放。如果 pod 没来得及释放就挂，新 pod 必须等 lease 自然过期（约 60s 窗口）才能拿 ownership，这期间 run 事实上暂停 —— v0.2 没明说该窗口的 UI 展示应该是 `waiting_for='pod_lease_handover'` 还是继续显示 `running`。
- Phase 1 Exit criteria L2796–2808 **八条里第 4/5/6 条**专门覆盖本场景：graceful vs crash、idempotent input、SSE heartbeat，都需要对应集成测试。

---

### T12 @ 10:30 — 用户开盖恢复

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| MacBook 唤醒，tab#2 Chrome resume，SSE 按上次 watermark 重连：`GET /chat/runs/r_mig_g1234/stream?last_index=131` | replay 132..205，74 个事件，里面有 `event_idx=188 run_resumed_after_restart`，`producer_pod_id` 从 6fdb 切到 a81e | UI 显示一条系统消息「服务端重启，已自动续接（生成代：1→2）」—— 该 UI 渲染需要前端识别 `event_type='run_resumed_after_restart'` 并渲染成特殊气泡（v0.2 没明说 UI 形态，仅约束 payload 字段 @ L1050） |
| 陈老师额外点「查看 run 状态」，`GET /chat/runs/r_mig_g1234` | 返回 `{status:'running', events_count:205, current_activity:{tool_name:'matrixone.exec', batch_id:156}}` | 状态面板显示：run 已连续跑 3h25min，205 个事件，当前批 156/280 |

**A7 证明点**：用户离线 30 分钟 + 服务端换了 pod，回来不需要重新描述任务、不需要重新审批、不需要重跑任何一批 INSERT。

---

### T13 @ 10:45 — 审批 #2（CREATE FULLTEXT INDEX，event_idx=218）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| agent 要 CREATE FULLTEXT INDEX idx_ft_desc ON mo_orders(description)，写 approval_request | (1) counter 分配 `event_idx=218`，写 `approval_request(approval_id='appr_ddl_0002', expires_at_server='12:45:00Z', summary='create fulltext index on 138M rows')`；(2) `UPDATE agent_runs SET status='waiting', waiting_for='tool_approval:appr_ddl_0002'`；(3) approval state 进 `pending_approvers`；(4) `session_state_items(category='approval_state', item_key='appr_ddl_0002', status='pending_approvers', payload_json.approval_state_version=1)` — 这里 `approval_state_version` 是 requester_confirm 回扣字段 @ L1046，**但 v0.2 没 spec 它如何 bump** — 见 §5 | SSE id=218；card 弹出 |
| D14 同时 5G 抖动 9s；waiting 期 server 除心跳帧没事件 | lease 每 20s 续期；expires_at_server 计时；`session_external_notifications` 没发（无外部 approver）；`approval_state.approval_state_version` 维持 1 | 断 9s 后 SSE 重连 `last_index=218` → replay 0 条 + `run_state_sync`（waiting） |

**v0.2 检视**:
- `approval_state_version` 在 L1046 是 `requester_confirm` payload 的必填字段，但 v0.2 没在 approval_state item 的 payload_json（L1273–1285）里列出它的 bump 规则 —— 见 §5 候选 gap（G20 候选 "waiting_for grammar"相邻问题，也可以合并到那里）。实际实现时可约定：`approval_state_version` 在每次 `approval_condition_modified` / `approval_decision` / `approval_retracted` 时 +1。
- 本场景无 external notification（no Slack/email），因此 L1246 `session_external_notifications` 表不写入，`notification_dispatched` / `notification_acknowledged` 事件不发。如果产品要求"高危 DDL 要 Slack 提醒"，就会多一条 `notification_dispatched(event_idx=219)` 紧跟 approval_request。
- D14 抖动 9s 和 D16 3s 形成对比：9s < 45s heartbeat 判死阈值（L1052–1054），所以 D14 客户端**其实不应该重连**。但浏览器 EventSource `onerror` 可能在更短时间（TCP RST 时）触发 → 这是 F2/F6 的灰色地带。S04 §3 T13 按客户端重连处理，v0.2 的"45s 判死"是保底策略，实际实现可能更激进。

---

### T14 @ 10:46 — 批准 #2（event_idx=219）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 点「批准」，`POST /input idempotency_key='u_chen:appr_ddl_0002:approve'` | (1) L1018 dedup；(2) `event_idx=219 approval_decision(approver='u_chen', conditions_ref=[], decided_at_server=...)`；(3) 状态机 pending_approvers → approved → running — L1289；(4) `UPDATE agent_runs SET status='running'`；(5) RunEngine 唤醒，CREATE INDEX 开跑，预期耗时 35min | card 绿色打勾；MatrixOne 工具 node 亮蓝色进度条 |

---

### T15 @ 11:30 — D16 heartbeat 窗口内 3s 闪断

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 3s TCP RST（基站切换）；客户端**按 L1052–1057 的 45s 判死**，3s 远低于 45s → 不触发 reconnect | 服务端无感；TCP 层重组；SSE 流不断 | UI 完全无感；timeline 连续 |

**正面案例**：这是 L1052 heartbeat 契约的典型收益 — 避免了一次 reconnect 带来的 stream?last_index=N 无用 HTTP 握手。

---

### T16 @ 11:33 — D17 WiFi 抖 7s（同 T02-T03，略）

---

### T17 @ 11:35 — run_finished（event_idx=561）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 对账通过 | (1) `event_idx=561 run_finished(status='completed', rows_total=138245119, checksum_mismatch=0, total_prompt_tokens=482910, total_completion_tokens=47120, total_tool_calls=203)`；(2) `UPDATE agent_runs SET status='completed', total_*=..., updated_at=NOW()`；(3) counter 行不删（保留审计）；(4) lease 释放 | SSE 最后一帧 completed；UI 显示完成总结卡片 + 下载迁移报告链接 |

---

## §4 发现的别扭点（Frictions）

### F1 — `checkpoint_v1.last_batch_id` 粒度 vs tool-call-level resume 语义歧义

- **Ref**: T11（pod 重启）；v0.2 L1007–1016（checkpoint_v1 schema）
- **Observation**: graceful shutdown 写 `last_batch_id="batch_147"`，但 shutdown 前最后一批内部可能已经推进到"MySQL SELECT 成功、MatrixOne INSERT 进行中、validate 未开始"这种中间态。`checkpoint_v1.extra` 是自由 JSON，v0.2 没规定必须记录 tool-call 级别断点。
- **Why it's friction**: 如果新 pod 从 batch_148 开始，batch_147 的 INSERT 可能已经在 MatrixOne 部分成功（500k 行可能落了 380k），直接跳过会**数据缺失**；如果重跑 batch_147，则 MatrixOne 要靠幂等（`INSERT ... ON DUPLICATE KEY` 或业务主键 UPSERT）才能安全。v0.2 把"重跑 batch_147 还是跳过"完全丢给应用层决定。
- **Severity**: **notable**（不是 blocking，因为 extra 字段给了出口；但 contract 没收口就容易被误实现成"最粗粒度 resume"）。

### F2 — Heartbeat 事件是否写入 `agent_run_events` 的语义冲突

- **Ref**: T07（S04 §3 写"8 个 heartbeat"计入 51 事件内）；v0.2 L1052–1057
- **Observation**: v0.2 L1052 明文 "server sends a heartbeat at least every 15 seconds"，但没说这个 heartbeat 是 **SSE 流控帧**（`:keepalive\n\n` 风格，不写 DB）还是 **`agent_run_events` 里的 event_type='heartbeat'**（写 DB、占 `event_idx`）。S04 §3 T07 按后者算（51=20×2+10+12+8+1 左右）。
- **Why it's friction**: 如果按流控帧实现，5h run 事件总数 ~200（不是 ~560），IndexedDB 存储和 replay 成本下降一个量级；但 SSE reconnect 时就无法精确知道"这次断连跨过了多少时间"。两种实现对存储/带宽/调试可观测性影响都不小。
- **Severity**: **notable**（实现可自选，但 contract 应该明说）。

### F3 — `pending_requester_confirm` 在"requester == approver"场景的快捷通道未明说

- **Ref**: T06（陈老师既是请求者又是审批者）；v0.2 L1287–1303 approval 状态机
- **Observation**: 状态机 `pending_approvers → approved → pending_requester_confirm → running` 是为外部审批通道设计的（L1301 "Approval through an external channel does not directly execute the tool"）。但本场景 requester=approver=`u_chen`，状态机字面走完四步会多一次 `requester_confirm` event，而实际实现几乎必然跳过。v0.2 没显式写"same-user shortcut"。
- **Why it's friction**: 不同团队实现可能：(a) 严格跑完四步，多产生一个 UI 确认弹窗；(b) 跳过，但审计链上少一个事件。两种实现都合法但**不一致**。
- **Severity**: **notable**（blocking 前需要 contract 收口）。

### F4 — Tab#2 冷启的 `/sessions/{id}/state` 与 `/chat/runs/{run_id}/stream?last_index=N` 之间的 watermark 约定未收口

- **Ref**: T09（开新 tab）；v0.2 L386–415（Web Client Cache response fields）+ L1052–1057（SSE replay）
- **Observation**: `/sessions/{id}/state` 返回 `active_run.run_event_high_watermark=131`（L396–397 意图）；tab#2 按理应该用 131 去发 stream。但如果 tab#2 的 IndexedDB 是空的（新 Chrome profile / 隐身窗口 / 私有模式 / 清除过浏览数据），它收到 131 的那一刻，本地并没有 0..131 的事件行 —— **直接用 131 作 last_index 会跳过历史 replay**，UI 显示一个只有最近事件的 timeline。v0.2 没明说 tab 冷启时应该：
  - (a) 丢弃 state response 的 watermark，从 0 replay（传输/渲染成本最高，正确性最好）；
  - (b) 信任 watermark，只显示 live（成本最低，UX 最差 — 新 tab 空 timeline）；
  - (c) 按需 fetch transcript 页（L418–425 scroll-up flow）来回填 historic 事件（成本中等，但 transcript API 和 run events API 是两条时间线 —— A2 要求它们分离）。
- **Why it's friction**: UX 上三种实现观感完全不同；也是 G15+G19 契约未完全闭环之处。S04 §3 T09 把它当"假设最坏情况独立 IndexedDB"一笔带过，但真实部署里这是高频路径（用户开多 profile / 私密浏览 / 新设备）。不收口会导致同一团队的前端和 QA 拿到不同"正确"答案。
- **Severity**: **blocking**（双 tab 是 S04 的核心假设，这里不收口会导致两 tab 显示不一致 —— A1 的底线被破坏）。

### F5 — `waiting_for` 字段格式 vs `session_state_items.approval_state` 子表的耦合未正式化

- **Ref**: T04/T13；v0.2 L914 `agent_runs.waiting_for VARCHAR(255)` + L1271–1285 approval_state payload
- **Observation**: S04 用 `waiting_for='tool_approval:appr_ddl_0001'` 这种前缀-冒号格式，v0.2 没给正式 grammar（如 `<reason>:<ref_id>`）；也没说 `waiting_for` 必须与 `session_state_items(category='approval_state').item_key` 一一对应。
- **Why it's friction**: 如果 waiting_for 字段手写拼错（比如漏了 `tool_approval:` 前缀），approval UI 能正常弹，但用 `waiting_for LIKE 'tool_approval:%'` 的可观测性查询会漏数据。
- **Severity**: **minor**（可在 Phase 4 State Projection v1 文档中补）。

### F6 — SSE `run_state_sync` 控制帧未列入 event_type 契约

- **Ref**: T06（重连后发 run_state_sync）；v0.2 L1024–1050 首类 run 事件列表
- **Observation**: 重连时服务端要告诉客户端"当前 status=waiting, waiting_for=..., approval_state_version=1"。S04 §3 T06 叫它 `run_state_sync`，这是**流层**控制帧，不应进 `agent_run_events`（因为会破坏 event_idx 单调）。v0.2 L1024 列出的是 `approval_*` / `requester_confirm` / `notification_*` / `edge_timeout` / `run_resumed_after_restart` 等**持久化**事件，**没有**控制帧这一类。
- **Why it's friction**: SSE 既要传 run event（按 event_idx 幂等），又要传状态同步帧（无 event_idx，纯 hint）—— 客户端需要区分两套处理逻辑。v0.2 实质上把这个需求留给了 `use-chat-stream.ts` 实现者。
- **Severity**: **minor**（但会出现在每次 reconnect 路径上，UI 代码要专门处理）。

### F7 — `run_generation` bump 后旧 pod 的 in-flight 写入如何拒绝未明说

- **Ref**: T11（pod 切换）；v0.2 L1003–1005 `run_generation` + L987–991 counter 协议
- **Observation**: 假设旧 pod SIGTERM 时刚好有一个 event 已进 `INSERT` 但未 commit。新 pod 拿 lease 后 `run_generation=2`，旧 pod 的 commit 成功了会怎样？L990 的 counter `FOR UPDATE` 可以阻止旧 pod 拿到新 counter 值，但**旧 pod 可能已经持有旧 counter 值（比如 188）并在本地执行到 INSERT**，这时 `uq_run_event_idx` 会在新 pod 第二次 INSERT 时报 duplicate。
- **Why it's friction**: 错误处理路径上两个 pod 可能短暂双写相同 event_idx；`uq_run_event_idempotency` 兜底，但错误日志会吵。更严格做法：`UNIQUE KEY (run_id, run_generation, event_idx)`。
- **Severity**: **minor**（v0.2 的 owner lease + counter 基本能防，但 race 细节未收口）。

### F8 — approval card 的 countdown 与 server time 的时钟漂移可观测性缺

- **Ref**: T04/T13；v0.2 L1312–1313 `expires_at_server` 契约
- **Observation**: `expires_at_server` 是 server 绝对时间，UI 按本地 `Date.now()` 做倒计时。高铁上 NTP 同步不稳，laptop 时钟可能漂移几十秒甚至分钟级。approval 剩 120s 显示的时候，server 那边可能只剩 95s —— 如果用户"快到点时"点批准，server 收到请求可能已经过期。v0.2 没规定 client 应该定期用 `Server-Date` HTTP header 或 SSE `id` 时间戳做时钟校准。
- **Why it's friction**: 低频但尴尬的 UX — 用户看到按钮还亮着就点了，结果弹 "approval expired"。陈老师这场 2h 窗口不会触发，但换成 TTL 5min 的场景就会暴露。
- **Severity**: **minor**（本场景不触发，但 pattern 普适）。

### F9 — 多 tab 通过 BroadcastChannel 同步 watermark 时的 stale 判定缺

- **Ref**: T09–T10（双 tab 切换）；v0.2 L539–545 "Tabs share watermarks through BroadcastChannel"
- **Observation**: tab#1 watermark=131，tab#2 watermark=0（冷启）。tab#2 发 stream 的同时收到 tab#1 广播的 `{watermark:131}`，它应该信任 tab#1 的广播跳过 replay 吗？v0.2 L544–545 说 "A tab ... ignores stale broadcasts"，但没定义"stale"的判据。按朴素实现，tab#2 发 stream 时会 race broadcast vs server replay，两条数据流同时推进 watermark 可能破坏 G19 原子性（两个 tx 各自推进）。
- **Why it's friction**: 这是 G19 L539–545 多 tab 协作章节里最模糊的点；S04 §3 T09 简化成"tab#2 独立 IndexedDB"，回避了该问题。真实场景（共享 IndexedDB + BroadcastChannel）需要明确谁是 watermark 推进权威。
- **Severity**: **minor**（降到 blocking 需要配合 F4；单独出现时 G19 L539–545 的 idempotent apply 兜底可以自我修复，不会丢事件，只是重复渲染）。

---

## §5 发现的新 gap 候选

`/sessions/{id}/state` vs stream API 之间的 watermark 契约没闭环，提出 **G20** 候选。其余别扭都能在 v0.2 既有章节内收口（属于 §4 friction，不是新 gap）。

```
- title: `/sessions/{id}/state` 与 `/chat/runs/{run_id}/stream` 之间的 watermark 语义对齐缺
- triggered_by_turn: T09（tab#2 冷启）
- related_v0.2_section: §Web Client Cache L373–436 + §Durable Runs L1052–1057
- severity: high
- rationale: 两个 API 都返回/接收 `run_event_high_watermark`，但一个是"我现在知道到哪了"（client → server 的协商），另一个是"服务端告诉我现在跑到哪了"（server → client 的报告）。冷启动的 tab#2 直接把 state.active_run.run_event_high_watermark 传给 stream?last_index= 会跳过历史 replay；如果客户端 IndexedDB 为空，历史就永远补不齐。v0.2 的 L403–415 open-session flow 只覆盖了"有 cached state"的情况，没覆盖"冷启动 + 已有 active run"的情况。
- proposed_fix: 在 §Web Client Cache 补一小节「Cold-Start Hydration」：tab 首次打开（local watermark=0 或缺失）必须先按 transcript API 拉历史，或 stream?last_index=0 全量 replay；server 在 /sessions/state 响应里加 `replay_required=true` 当检测到 `known_state_revision=0` 但 `active_run.run_event_high_watermark>0`。
```

```
- title: `checkpoint_v1.extra` 未定义 tool-call / batch-item 粒度断点的推荐结构
- triggered_by_turn: T11（pod 重启）
- related_v0.2_section: §Run Event Ordering and Ownership L1007–1016
- severity: medium
- rationale: checkpoint_v1 只强制 last_batch_id（用字符串），对 batch 内部的 sub-step（如"INSERT 到第 n 行"）留给 extra 自由 JSON。S04 这种 280 个 500k 批的迁移，如果新 pod 只能从 batch 起点重跑，最坏情况一次 pod 重启会重做 500k 行 INSERT — 对应用层 idempotency 依赖太强。建议规定 extra 的推荐 shape，尤其是 partial_progress（{step_index, total_steps, resumable_marker}）。
- proposed_fix: §Durable Runs 补 checkpoint_v1.extra 的 "recommended sub-step fields"，不强制但给出 schema hint；并在 Phase 1 Exit 加一条 "batch 级 + tool-call 级双层 checkpoint test"。
```

```
- title: `waiting_for` 字段 grammar 未 spec + 与 approval_state.item_key 的 FK 约束未明说
- triggered_by_turn: T04/T13
- related_v0.2_section: §Durable Runs L914 + §Approval State L1271–1285
- severity: low
- rationale: waiting_for 用前缀-冒号的非正式格式（tool_approval: / edge: / external:），解析靠约定。可观测性查询、告警规则、UI 渲染都依赖这个格式。不 spec 容易各处硬编码字符串。
- proposed_fix: 在 §Durable Runs 加 "waiting_for grammar: <reason>:<ref_id>"，列出全部合法 reason（tool_approval / edge_bridge / external_notification / requester_confirm），并说明 ref_id 必须与对应子表的主键一致。
```

其余别扭（F2 heartbeat 写不写库、F3 same-user approval 快捷通道、F6 run_state_sync 帧、F7 generation race）**不新开 gap**，建议在 G15 resolution_commit 下方追加 implementation clarifications（属于实现细节而非设计空白）。

---

## §6 Gap 组合效应检验

### 组合 1: G15 + G19（durable run 服务端顺序 + 客户端 watermark 原子）

- **运行时可能发生的坑**: T08 lastIndex=46 stale 的情形就是典型 — 服务端用 counter 严格按 event_idx 推进（G15），客户端在 D04 的 IndexedDB commit 没完成时 watermark 未跟上（G19 的 `begin transaction` 保证了不会推进到不存在的事件上）。双方加起来能做到"客户端看到的 run_event_high_watermark 一定对应 IndexedDB 里已持久化的最大 event_idx"。
- **event_hash 的作用**: L521 `if event_idx <= current_watermark and event_hash matches: skip`。这里 `event_hash` 是 G15 要求 server 在写 event 时一并存（L964 `event_hash VARCHAR(128)`）。如果 server 重启但重算 hash 时字段顺序不一致（比如 approval payload 里 required_approvers 的数组顺序），client 会误认为事件变了 —— G17 `content_hash` normalization 就是解决这种场景的。本场景 approval payload 字段稳定，不触发。
- **v0.2 是否覆盖**: **yes**。G19 L520–527 的 apply 协议 + G15 L987–991 的 counter 协议形成闭环。T08 场景下 replay 47..98 的 52 条事件全部在 client 一个 tx 内 apply，watermark 从 46 → 98。T11 pod 切换后 event_idx 仍然严格递增（G15 `uq_run_event_idx` 跨 pod 生效），G19 在 client 端不需要感知 pod 切换。
- **如果 no，建议**: N/A。但实现期要写 gap 检测的自动化测试（G19 test_plan "Gap replay test from last_ok_idx"），至少覆盖 (a) watermark stale 1 条、(b) watermark stale 50 条、(c) watermark 比 server 还大（不可能但需要 abort 分支） 三个 case。

### 组合 2: G7 + G12（approval 状态机 + next-action confidence）

- **运行时可能发生的坑**: T04 approval 弹出后，如果 run 在 waiting 期间还跑了 post-turn extractor（按 L819 的 PostTurnExtractor），对"用户下一步会怎么决定"做 suggestion 提取 — 这里 confidence 应该是多少？G12 的 `>=0.8 auto-accept` 是针对 next-action 的，不适用于 approval 本身（因为 approval 是强 user gate）。如果混淆，可能把"agent 猜用户会 approve"当 next-action 写入并影响上下文。
- **v0.2 是否覆盖**: **partial**。G12 L665–671 明说自动接受只对 next-action；G7 的 approval 必经 `approval_decision` 事件才能推进（L1042）。但 v0.2 **没显式禁止** "在 waiting 期间 post-turn extractor 产出涉及 approval 的 suggestion"。T04 这里 LLM 暂停，理论上不会触发 extractor；但如果实现把 approval_request 事件本身 fed 给 extractor，可能产出 "建议用户批准" 的低置信度 suggestion。
- **如果 partial 的补强**: 在 §Next-Action Confidence 补一条 "Excluded sources: approval events must not generate next-action suggestions for the approver themselves"；或者 suggestion expiry 里 `approval: 24h` (L688–691) 定义其实应改为 `approval suggestions are user-initiated only, not agent-extracted`。

### 组合 3: G15 + G2（durable run 所有权 + compaction invariants）

- **运行时可能发生的坑**: T11 pod 重启那一刻，**如果** compaction daemon 正好在新 pod 上起来，它会看到 `agent_runs.status='running'` 然后按 G2（§Compaction Invariants）的 "compaction trigger 必须检查 `agent_runs.status NOT IN ('running','waiting')`" 直接跳过这个 run —— 正确行为。但如果 compaction 被错误地当成"pod 启动时批量做的后台任务"实现，没看 run 状态就扫过去，可能踩上"running run 的 active plan_state 被压缩"。另一个更微妙的 race：graceful shutdown 写完 checkpoint 之后、新 pod 拿 lease 之前的短暂窗口（~10s）里，`agent_runs.status='running'` 且 `owner_pod_id=NULL`（或指向旧 pod id），compaction daemon 不应该把这种状态当成"没有 writer 在跑"而错误触发。
- **v0.2 是否覆盖**: **yes**。GAP-TRACKER G2 L88 + v0.2 §Compaction Invariants 明文禁止 status='running' 触发 compaction。G15 的 owner_pod_id + lease 又防止了两个 pod 同时跑 compaction。两个 gap 叠加后，compaction trigger 的正确判定应该是 `status NOT IN ('running','waiting') AND (owner_pod_id IS NULL OR owner_lease_expires_at < NOW() - INTERVAL 5 MINUTE)`（第二个条件防刚释放的 run 立即被扫）。
- **如果 no，建议**: N/A。但 Phase 4 / Phase 5 实现期需要集成测试 `test_no_compaction_during_running_run`（GAP-TRACKER G2 test_plan 已含）+ `test_no_compaction_during_pod_handover`（新增，模拟 10s ownership gap）。

### 组合 4: G15 + G13（run ownership + device lease）

- **运行时可能发生的坑**: T09 开新 tab 时，两 tab 共享同一 `session_device_leases` 行（同 device_fingerprint）。同时 run 的 ownership 在 server 侧（pod），不在 client 侧。两者是**正交**维度 —— 一个 device 可以订阅多个 run，一个 run 可以被多个 device 订阅。T11 pod 切换时 `run_generation` bump 不影响 device lease。
- **v0.2 是否覆盖**: **yes**。L454–473 device lease + L985–1005 run ownership 明确区分两个层面。L539–545 的 "no primary-tab lock" 进一步强调 client 端多 subscriber 无特殊角色。
- **如果 no，建议**: N/A。

### 组合 5: G7 + G15（approval 持久化 + run 所有权切换）

- **运行时可能发生的坑**: T11 pod 重启发生在 T04 approval #1 已完成、T13 approval #2 尚未发起 之间，没问题。但构造一个变形：**如果 pod 重启恰好在 approval pending 期间**（比如 T04 刚写完 approval_request event_idx=46 就 SIGTERM），新 pod resume 时必须保留 waiting 状态。v0.2 在这里需要 3 处协同：(1) `agent_runs.status='waiting'` 被 checkpoint 前就已经持久化，不依赖 checkpoint_json；(2) `session_state_items(category='approval_state')` 独立持久化（L1271–1285）；(3) `run_resumed_after_restart` 事件里**不**携带 approval 信息，UI 要自己从 state projection 重渲染 approval card。
- **v0.2 是否覆盖**: **partial**。(1) 和 (2) 明确覆盖；(3) 的 UI 渲染路径没显式写。client 收到 `run_resumed_after_restart` 后应该主动发 `GET /sessions/{id}/state` 重取 approval_state item（因为本地 approval_state 可能也过期了），这个 refresh 没在 v0.2 里定成契约。
- **如果 partial 的补强**: 在 §Web Client Cache 或 §Durable Runs 补一行"Client receiving run_resumed_after_restart MUST refetch session state projection to re-hydrate waiting_for-bound state items"。或者在 stream 层让 server 在 resume 之后立即发一个 `run_state_sync` 控制帧（见 F6）。

---

## §7 Token 实际 walkthrough（最紧张一轮：T13 审批 #2 前的 LLM 推理）

**场景**: T13 前一轮 LLM 推理（event_idx≈217），此时已经跑 150 多批 INSERT、经历过一次 pod 重启、5 小时 run 的第 3h40min，正要决定"是否发起 CREATE FULLTEXT INDEX 的 approval"。

**Budget 选择**: `budget_v1_default`（gpt-4o-long, 200k window），不用 `budget_v1_8k`（L2410）。但 v0.2 L2381–2406 没写 default 的硬上限数值，只有相对原则。所以下面是合理估算。

**前置盘点**（T13 前 run 的现状）:
- 已跑 2h40min，150+ 批 INSERT，event_idx ≈ 217；
- 有 2 条 error_state（batch_42 的 lock_wait_timeout，batch_89 的 duplicate key）；
- 有 1 条 benchmark（"MatrixOne 并发 insert 每秒 8k 行"）；
- 有 1 条 decision（"分区 DROP 采用 MDL 锁 5s 窗口"）；
- 有 1 条 finding（"description 列平均 1200 chars，倾向 fulltext"）；
- 有 3 段 L1b summary（batch 1–50 / 51–100 / 101–150，每段 ~400 token）；
- active todos 3 条；
- plan_state version=3 phase=4（"创建索引阶段"）。

| Zone | v0.2 原则 / cap | 本轮估算 | 计算过程 |
| --- | ---: | ---: | --- |
| Stable system prefix | 稳定，不含易变 fact | **1200** | system + 工具说明约束 + 身份/权限约束（cache-friendly） |
| Tool schemas | 按 active agent 剪 | **2400** | `mysql.query` 800 + `matrixone.exec` 900 + `validate.checksum` 400 + `approval.request` 300 |
| Session anchor | under hard cap L2389 | **500** | 迁移目标 + 白名单 5 表 + 批大小 + 审批规则，压到 500 内 |
| Plan/todos | 活跃 phase + next action | **900** | plan v3 phase 4（"创建索引"）+ next 3 todos（索引/validate/报告） |
| Delegation state | 本场景无 | **0** | parent_run_id=NULL |
| Recent tail | L2392 Last 2–4 turns | **1800** | 最近 4 轮：batch_155 完成、batch_156 完成、plan_update、approval 准备 |
| Summary (L1b) | 旧历史 | **1200** | 150 批 INSERT 的压缩摘要（by 50 批分段），batch_1-50/51-100/101-150 三段 summary |
| Retrieved facts | top-K by relevance | **1600** | 跨 run 的 error_state（有 2 个历史报错）+ benchmark（MatrixOne INSERT 每秒 8k 行）+ decision（上次 DDL 用 1 小时 lock window）+ finding（description 字段平均 1200 chars，倾向 fulltext 而非 vector） |
| Tool previews | L1b artifact 摘要 | **900** | 最近 3 个 artifact preview：batch_156 的 progress.json + checksum partial + 上次 DDL 的 plan_preview |
| Safety/approvals | pending | **0** | T13 前 no pending（approval #2 正要发起） |
| Reserved output | G10 L2425 有 500（8k 模板）；default 应≥1000 | **2000** | LLM 要输出 approval request + tool_call args |
| Safety buffer | G10 200；default ≥500 | **800** | — |
| **合计** | — | **~13300** | — |

**观察**:

- 13.3k / 200k = **6.6%**，离上限非常宽松。
- G10 的 `budget_v1_8k`（L2416–2426）合计 8700，**这个场景用不到**（长 context 模型）；但如果部署强制 8k 模型跑 5h 迁移，按 L2430–2432：tool schemas 和 retrieved facts 优先被剪 → tool schemas 只能留 `mysql.query` + `matrixone.exec` 2 个 schema（~1700）、retrieved facts 砍到 1000。recent tail 按 L2431 floor 1600 保底，仍能覆盖最近 3 轮。结论：**8k 模型跑这个场景理论可行但不推荐**，因为 retrieved facts 被砍后 DDL 决策缺少 historical error context（150 批中 2 次报错）。
- **prompt-cache 命中**: L2387 "Keep stable for cache" —— system prefix 1200 + tool schemas 2400 = 3600 稳定，5h 内 42 轮 LLM round，共享 cache 约省 150k 重复 token（每次节约 ~3500 × 42 ≈ 147k token 跨 run）。
- **manifest 持久化**（L2836 每轮写一条 manifest）：本轮 manifest header + item refs + token estimates，估计 2–3 KB JSON 写 `context_manifests` 表（不含 rendered full prompt，L600–613 "Do not store a huge rendered prompt body"）。42 轮 × 3KB ≈ 126KB 持久化开销，可忽略。
- **无 drop cascade 风险**：每 zone 都低于 cap，不触发 G12 的多候选生成、也不触发 compaction（L1002 running run 禁用 compaction）。

**额外对比 — 如果强制用 `budget_v1_8k`（G10 L2410–2440）跑此场景**：

| Zone | 8k cap (L2416) | 本轮期望 | 结果 |
| --- | ---: | ---: | --- |
| Session anchor | 200 | 500 | **超** 300，要压缩到「迁移目标 + 白名单表数量」 |
| Plan/todos | 400 | 900 | **超** 500，todos 只能留 top-1 next action |
| Recent tail | 2000 (floor 1600 L2431) | 1800 | 放得下 |
| Summary | 500 | 1200 | **超** 700，150 批压缩摘要只能保留最后 50 批 |
| Retrieved facts | 1000 | 1600 | **超** 600，丢掉 2 条 error_state 中较旧的一条（保留 benchmark + decision） |
| Tool previews | 500 | 900 | **超** 400，preview 只留最近 1 个 artifact |
| System + tool schemas | 3400 | 3600 | **超** 200，tool schemas 必须剪掉 validate.checksum（本轮可能不用） |
| Reserved + safety | 700 | 2800 | **超** 2100 —— 这是最硬伤，8k 模板预留 output 太小 |

总评：在 8k 模板下本轮需要激进剪裁至少 **3300 token**。按 L2430 "Tool schemas and retrieved facts are aggressively pruned first"，实际会砍 tool schemas / retrieved facts / summary。G10 L2433 "Vector retrieval is disabled by default in budget_v1_8k" → 如果本轮 error_state 查询走 vector（因为关键词不足 FTS），会被直接 disable，fallback 到 structured。**结论**：本场景用 8k 模型勉强可行但会明显降低 DDL 决策质量；v0.2 把 fallback 路径写得足够明确，实现侧应该挡在部署前（比如用 `model.context_window_tokens>=32000` 的 policy）。

**结论**: 最紧张一轮也只用了 6.6% 的窗口，本场景的长上下文模型根本不是压力来源；压力来自 **事件流一致性**（§4 F1-F7）而非 token。

---

---

## §7.5 实现要点清单（从推演到代码）

回到代码视角，S04 顺畅落地需要以下实现要点明确落到 Phase 1/2 的交付单：

1. **`run_counters` 行必须随 run 创建同时 INSERT**（v0.2 L941 只 CREATE TABLE，没强制 INSERT 时机）；建议在 `services/runs.rs` 的 `create_run` 里同事务写两张表。
2. **graceful shutdown 必须 fsync** `checkpoint_json` 后再返回；不然进程 coredump 时 checkpoint 仍在 OS buffer 可能丢（v0.2 没写这条，属于实现细节）。
3. **IndexedDB apply tx 要用 `readwrite` + `durability:"strict"` 选项**（Chrome 97+）；L518–527 的原子性依赖它，否则 page unload 时 tx 可能未落盘 —— 正是 T08 watermark=46 stale 的根因。
4. **SSE heartbeat 应该用 `:keepalive\n\n` 流控帧**（不是 run event）；15s 发一次（L1052），client 45s 阈值（L1053–1054）；这里 v0.2 未明说"heartbeat 不落库"，但按 F2 分析更合理。
5. **`/chat/runs/{run_id}/stream` 需要支持 `Last-Event-ID` 头作为 last_index 回退**（HTTP SSE 规范 ）；S04 §3 T03 用 query string，两种都允许，实现两条路径都要支持浏览器自动重连。
6. **`run_resumed_after_restart` 的 payload 必须可 UI 渲染**（L1050 四字段），前端要有 special case 把这条事件渲染成系统消息而非 tool call node。
7. **approval card 的 countdown 用 `expires_at_server` 而非 `ttl_seconds`**（L1312–1313）；client 做 time delta 插值，但到期判定必须等 server `approval_expired` 事件（否则跨时区 / 时钟漂移会误判）。
8. **Phase 1 集成测试至少 4 个**：graceful restart、crash restart（无 checkpoint）、idempotent double-approve、SSE 45s 判死；前两个必须跑真实 pod kill，不能只 mock。

---

## §7.6 本场景下 v0.2 的亮点

把 S04 完整走完之后，需要肯定 v0.2 已经把最硬的骨架补上了：

- **G15 counter-based event_idx 分配**（L987–991）是整个 durable run 正确性的基石，没有它 T07/T08/T11 都会在 event_idx race 上翻车。
- **G19 IndexedDB 原子 tx**（L518–527）把 client 端的 stale watermark 问题从"偶发数据错乱"降级为"最坏重复 replay"，T08 lastIndex=46 的边界 case 因此是良性的。
- **graceful vs crash 语义分离**（L997–1005）正面回应了 S04 §7 F7 提出的"find_running_runs 不能简单标 failed"，把 rolling deploy 的 5h run 从"每次发版都重跑"拉到"续接"。
- **`run_resumed_after_restart` event 契约**（L1050）让 UI 可以渲染明确的系统消息，而不是突然把 timeline 往前跳。
- **idempotency_key + 语义去重**（L1018–1022）让 T06/T14 的双击 approval 不会变成 double-DDL 灾难。
- **多 tab 无 primary 设计**（L539–545）把 T09 的双订阅者简化为"两个幂等 consumer"，避免了 leader election 这类复杂度。

以上 6 条里每一条都是 v0.2 patch 加进去的，对 S04 都是命中要害。

---

## §8 最终判断

- **本场景下 v0.2 是否可实现**: **⚠️（绝大部分可，有 1 处 blocking friction + 1 个新 gap 候选需要收口）**
- **顺畅度**: **⚠️ 有明显摩擦**
  - G15 + G19 的 durable run + watermark 原子链路非常扎实，17 次抖动 + pod 重启 + 30 分钟离线这些"硬核"压测都能走通；
  - 真正别扭的是 tab#2 冷启动（F4 / 候选 G20）、checkpoint 粒度（F1 / 候选 G-checkpoint）、heartbeat 写不写库（F2）这几处**没完全收口的实现契约**；
  - approval / confidence / compaction 等跨 gap 组合都在设计层闭环，实现期加集成测试即可。
- **一句话总结**: **durable run 的骨架已经能扛 S04 的主线，但双 tab 冷启动和 batch 内 checkpoint 的实现契约留给了应用层，容易长出不一致的实现 — 值得在 Phase 1 / Phase 2 Exit Criteria 补两条测试把缺口焊死**。
