# WALKTHROUGH-S10 — 深层 delegation（5 层安全扫描）端到端运行推演

> Scenario: [S10-deep-delegation.md](S10-deep-delegation.md) §1–§3（v0.1）
> v0.2 ref: [../web-agent-session-state.md](../web-agent-session-state.md)（3040 行，G1–G19 resolved）
> Role: 有经验的工程师以"真的用 v0.2 实现"的视角走 T1–T10

---

## §1 场景标识与 v0.2 映射

**一句话场景**：CISO 一句"跑 Q1 全量供应链漏洞扫描"，经 L0→L1 orch→L2 planner→L3×4 executor→L4×N reviewer 共 5 层 delegation，输出 ~30 条 severity ≥ high 的真阳清单，过程中 L0 prompt 始终不触达 L3/L4 原始 transcript。

**被测 DAUT**：A4（5 层 manifest 彻底隔离 raw）、A13（5 层 `parent_run_id`/`ancestor_path` 可单次回溯）、A15（T8 "重做 executor-2" 是 structured retry 不是新任务）。

**本场景最重要的 v0.2 章节/字段**：
- `agent_runs` schema：`parent_run_id / root_run_id / ancestor_path / depth / retry_of / retry_scope`（v0.2 L900–938）
- G4 `session_delegations` 投影表 + Projection Sync Contract（v0.2 L1321–1426）
- G14 `session_state_item_events.mutation` enum 含 `bubble_up` / `apply_suggestion` + retry contract（v0.2 L1154–1215）
- G18 `delegation_state` zone 动态预算 `min(1200, max(200, floor(1500/n)))`（v0.2 L2442–2471）
- G9 `session_artifacts.access_scope='delegation'`：parent + descendants 共享（v0.2 L1671–1749）
- G1 `context_manifest_reason_types` 含 `delegation_poll` / `delegation_aggregate` / `partial_blocker_review`（v0.2 L1500–1527）
- History Reconstruction Modes 新增第 4 模式 Delegation tree drill-down（v0.2 L1989–1998）

---

## §2 运行前提

**schema 初始状态**：
- `agent_sessions`：无 `sess-vuln-annual-q1`，T1 新建；`project_id` 设为 `proj-vuln-q1`（G9 L1697）以便 retention 沿 project 走而不是 session。
- `agent_runs` / `run_counters` / `session_delegations` / `session_state_items` / `session_state_item_events` / `context_manifests` / `session_artifacts`：空；`context_manifest_reason_types` 已 seed G1 的 18 个 reason。
- 所有子 agent 走 **run children**（非独立 `agent_sessions`），符合 G4 "run children is the default"（v0.2 L1365–1368）。

**workspace**：单一 cloud workspace `ws-security-q1`（L1–L4 同 workspace，不做 cloud↔edge 跨切；扫描引擎挂在 L3 tool_schemas，S3 接收 raw artifact）。

**Budget template**：所有层默认用 200k 窗口策略（v0.2 §Budget Policy v1 L2381）；L1/L2 在 Claude-3.7 200k、L3/L4 在同代模型但工具集小不同。**不**使用 `budget_v1_8k`。L0 UI 本身不跑 LLM（仅 orchestrator L1 入口）。

**DAUT 指标基线（从场景 §1 表）**：15 runs, 11 delegation rows, ~680 tool calls, ~160 raw artifact → ~60 summarized, 30 最终 finding, 1 bubble_up, 1 retry（subtree）。

---

## §3 端到端运行轨迹（T1–T10）

### T1 · CISO 发起年度扫描（L0 会话建立）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 输入："启动 Q1 年度供应链漏洞扫描…只含 high 与 critical 真阳…按仓库分组附最小复现" | (1) INSERT `agent_sessions` row `sess-vuln-annual-q1`（设 `project_id=proj-vuln-q1`，L1697）。(2) INSERT `session_state_items(category='anchor', item_key='mission', payload_json={scope:'tier1+tier2', engines:['SAST','SCA','secret'], severity_filter:'>=high'}, priority=100)`。(3) INSERT `agent_runs` `run-L1-orch`：`parent_run_id=NULL`, `root_run_id=run-L1-orch`（自指）, `ancestor_path='run-L1-orch'`, `depth=0`, `trigger_type='user_message'`, `status='running'`, `retry_scope='node'`（default L924）。(4) `run_counters` 新行，`next_event_idx=0`。(5) L1 首轮 manifest：`reason='initial_turn'`（L1504）。 | SSE 流打开；SSE 首事件 `run_started`；UI 顶部显示"orchestrator 启动"；没有 delegation tree（尚未派发）。manifest zone 实占：system_static 1200 + tool_schemas 1100（仅 delegation-dispatch + state-query）+ skills 600 + session_anchor 450 + plan_todo 0 + recent_tail 300 + workspace 200 = **~3850**。 |

**机制可达**：✅。**顺畅**：✅。**别扭**：L0 session 创建时没有地方自动注入"你是 orchestrator"这个 agent 角色——需要 L1 `agent_id` 由谁写入（G4 L908 有列但未定 seed 流程）。记为 F-minor-1。

---

### T2 · L1 orchestrator 拆顶层任务并派 L2 planner

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| （无用户输入；L1 自主决策派 planner） | (1) 事务开始。(2) INSERT `agent_runs` `run-L2-plan`：`parent_run_id='run-L1-orch'`, `root_run_id='run-L1-orch'`, `ancestor_path='run-L1-orch/run-L2-plan'`, `depth=1`, `delegation_id='dl-plan'`, `trigger_type='delegation'`, `retry_scope='node'`。(3) INSERT `session_delegations`（G4 L1328）：`delegation_id='dl-plan'`, `parent_run_id='run-L1-orch'`, `child_run_id='run-L2-plan'`, `root_run_id='run-L1-orch'`, `ancestor_path='run-L1-orch/run-L2-plan'`, `depth=1`, `status='running'`, `phase='planning'`, `directive='repo-inventory+risk-ranking'`, `last_summary_token_estimate=0`。(4) INSERT `session_state_items(category='delegation_state', item_key='delegation:dl-plan', token_estimate≈120)`——同事务写入（Projection Sync Contract L1411–1424）。(5) 事务提交。(6) L1 下一轮 manifest `reason='delegation_poll'`（L1516）。 | 界面出现 1 节点树："orchestrator → planner(running)"；`delegation_state` zone 1 child，G18 `per_child_budget = min(1200, max(200, floor(1500/1))) = 1200`，实际用 120 token（远低于 cap，健康）。 |

**机制可达**：✅（G4+L1411 强制同事务 UPSERT）。**顺畅**：✅。**别扭**：Projection Sync 要求**原子事务**跨 `agent_runs` + `session_delegations` + `session_state_items` 三张表——如果后端裁成 microservice，会成为跨服务分布式事务。记为 F-minor-2。

---

### T3 · planner(L2) 产出分组计划回写 L1

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| （无用户输入） | (1) L2 跑 CMDB 查询→分 4 组 → UPSERT `session_artifacts artifact:repo-groups-by-priority.json`（`access_scope='delegation'` 默认 L1682，L1 可见）。(2) INSERT L2 `session_state_items(category='summary', item_key='summary:run-L2-plan', version=1, token_estimate=350)`。(3) UPDATE `session_delegations` WHERE delegation_id='dl-plan' SET `status='completed'`, `last_summary_ref='summary:run-L2-plan'`, `last_summary_token_estimate=350`, `exposed_artifacts_json=['artifact:repo-groups-by-priority.json']`。(4) 同事务 UPSERT 对应 `session_state_items(category='delegation_state', item_key='delegation:dl-plan')` 状态切 `archived`（L1421 closing 契约）。(5) INSERT `session_state_item_events(mutation='archive', previous_version=1, next_version=2)`。(6) `agent_runs.run-L2-plan.status='completed'`。 | L1 下一轮 manifest `reason='delegation_aggregate'`（L1518）：`delegation_state` 展开 summary 350 + artifact ref；`plan_todo` 展开 4 组待派；总 ~5100。界面：planner 节点变灰 ✓，artifact "repo-groups-by-priority.json" 可点击下钻。 |

**机制可达**：✅。**顺畅**：✅。**别扭**：L2 `summary` item 切 `archived` 的时机——G14 L1211 明确 backlog todos 不 archive，但没说 delegation 完成时 summary item 是否 archive。若 L1 后续 T10 聚合需要读这个 summary，`archived` 是否会被 compaction 物理清掉？依 G2 白名单 `summary` 不在保护名单里（对比 `plan_state` / `decision` / `finding` 在名单里）。记为 F-notable-1。

---

### T4 · L1 并行派 4 个 L3 executor（核心压力回合）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| （无；L1 读 artifact 后分 4 组派执行） | (1) 4 次事务并行（每个独立 `request_id`）：INSERT `agent_runs run-L3-exec-{1..4}`（parent=`run-L1-orch`, root=`run-L1-orch`, `ancestor_path='run-L1-orch/run-L3-exec-N'`, depth=1, delegation_id=`dl-exec-N`）。(2) 4 × `session_delegations` row（depth=1，因为父是 L1 本身；注意 L0 只是 session 不是 run，L1 即 root）。(3) 4 × `session_state_items(category='delegation_state', item_key='delegation:dl-exec-N', token_estimate≈150)`。(4) 每个 executor 本级首轮 manifest `reason='delegation_poll'`；L1 自己的下一轮 manifest 同 reason（因为此轮是"派完后看 4 child"）。 | UI 树：orchestrator → [exec-1..4] 并行 running；每个 exec 下挂 13 仓库 todo。 |

**manifest zone（L1 本级，此轮最紧张）**：
- system_static 1200 / tool_schemas 900 / skills 600 / session_anchor 450
- `delegation_state` zone：**G18 公式 n=4 → per_child_budget = min(1200, max(200, floor(1500/4))) = min(1200, max(200, 375)) = 375 token/child，4 × 375 = 1500**（贴满 hard cap）。
- 场景 §3 T4 写 "4 条 child summary 各 ~150 token" → 实际每 child 150 token，4×150=600，远低于 G18 的 1500 cap，所以 **健康、不溢出**。
- plan_todo 500 / recent_tail 700 / summary 350（L2 plan 的 roll-up）/ workspace 200 / 合计 **≈5700**（与场景 §6 表一致）。

**机制可达**：✅（G4+G18 配合）。**顺畅**：✅。

**关于 G18 "375 token/child 合理吗" 的判断**：
1. **理论上限合理**：4 child 同活、无 blocker 时每个 375 token，够装 `{title 40 + phase 20 + status 10 + blocker null + 1 行 directive 80 + exposed_artifacts 1 ref 40 + last_summary_excerpt 150} ≈ 340`。贴着 cap。
2. **但**当某 child blocker 激活（L2466 `2 * per_child_budget = 750` 借自 recent_tail）→ 其他 3 child 仍守 375 → 总 750+3×375=1875 > 1500 hard cap。G18 没说清这种情况是 expand 后 total cap 提升到 2250（1500+borrow 750）还是压缩其他 child。**记为 F-notable-2 / 新 gap 候选**。
3. **场景实际（~150/child）远低于 cap**：S10 的 child summary 设计很精简（run_id+scope+repo_count），说明 G18 的 375 cap 对本场景过于宽裕。
4. **当 n→很大**时公式退化：n=11（场景 T9 峰值 delegation_state 行数）→ `floor(1500/11)=136` < 200 floor → 强制 200 → total=2200 > 1500 hard cap。**G18 自相矛盾**：floor=200 与 total_cap=1500 不能同时满足 n≥8（200×8=1600）。**记为 F-blocking-1 / 新 gap 候选**。

---

### T5 · executor-2 派 L4 reviewer 处理 raw SAST

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| （无） | (1) exec-2 扫完 3 仓库后累计 12 MB raw。INSERT `session_artifacts artifact:sast-raw-exec2-batch1.jsonl` (`access_scope='delegation'`, `derived_from=NULL`, `retention_policy='project_long_term'` 因 `agent_sessions.project_id` 已设)。(2) INSERT `agent_runs run-L4-rev-2`：`parent_run_id='run-L3-exec-2'`, `root_run_id='run-L1-orch'`, `ancestor_path='run-L1-orch/run-L3-exec-2/run-L4-rev-2'`, `depth=2`, `delegation_id='dl-review-2'`。(3) 对应 `session_delegations` row `depth=2`。(4) 关键：此 delegation 的 `session_id` 指向 **哪个 session**？G4 L1331 说 `session_delegations.session_id` 是"session identifier"——若子是 run child 则共享父 session id。所以所有 5 层的 `session_delegations.session_id = sess-vuln-annual-q1`。 | L1 **不感知** L4 派发（场景 §3 T5 显式要求）。L1 的 `delegation_state` zone 仍显示 `exec-2: running / 3 of 13 repos`，不展开 reviewer。 |

**manifest zone 对比**：
- L3 exec-2 本级：`delegation_state` 仅 1 child（reviewer），n=1 → per_child=1200 cap，实占 180。OK。
- L4 reviewer 本级：`system_static 1200 / tool_schemas 600（triage rules + artifact read）/ session_anchor 500（mission+dedup_policy）/ retrieved_facts 400（历史误报规则库 top-20，来自 `scope='user'` 的 `rejected_pattern` 类别 L2318–2322）/ 合计 ≈ 3600`。

**机制可达**：✅（G4 ancestor_path 链 + G9 access_scope='delegation' 让 L4 可读 L3 产出的 raw）。**顺畅**：✅。**别扭**：
- L4 reviewer 从 `scope='user'` 的 `rejected_pattern` 类别拉历史误报规则（G6 L2318）—— G6 L2324 要求 `origin_session_id` 真实列存储，但**此处 `rejected_pattern` 是跨 session 库**，不一定能每条都有 `origin_session_id`（如果是人工初始化种的规则）。记为 F-minor-3。
- **L1 "不感知" L4 派发的实现**：L1 的 context assembler 读 `session_delegations WHERE parent_run_id='run-L1-orch' AND status='running'` 只返回 4 行 L3；L4 的行 `parent_run_id='run-L3-exec-2'` 自动不出现。这依赖 L1 assembler **不递归**去展开 child 的 child。v0.2 L1378 明确"Parent context assembly renders delegation as bounded structured summaries"——**设计符合**。

---

### T6 · 用户在 L0 查"payments-service 扫到哪了"

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| "payments-service 是 tier-1 关键仓库，我现在就想看它扫到哪一步" | **关键：此回合不走 LLM**。走 History Reconstruction Mode 4 "Delegation tree drill-down"（v0.2 L1998）。API 路径：`GET /sessions/{id}/delegations?root_run_id=run-L1-orch`（v0.2 L2729）→ 拿 11 行 delegation tree → 在其中按 `exposed_artifacts_json` 或 `directive` / `scope_section` 字段反查哪个 exec 归属 `payments-service` → 定位 `run-L3-exec-2` → 再 `GET /chat/runs/run-L3-exec-2/delegation-summary`（v0.2 L2738）拿其 phase/progress → 同样对 L4 reviewer。返回合成文本。 | UI 直出气泡："exec-2 正在 triage batch-1，payments-service 已完成 SAST+SCA，secret scan 排队中，2 条 medium 待 reviewer 复核。" 无 LLM 调用，无新 manifest 行。 |

**跨 5 层查询走哪个 API**：
- **主 API**：`GET /sessions/{session_id}/delegations?root_run_id=run-L1-orch`（v0.2 L2729）—— **一次 index lookup**，底层 SQL 走 `idx_delegations_root_depth (root_run_id, depth, updated_at)`（v0.2 L1355）。返回完整 11 行平坦列表（不是递归 CTE），因为 `root_run_id` 已预计算。
- **细节 API**：`GET /chat/runs/{run_id}/delegation-summary`（L2738）取单 child 的 phase + exposed_artifacts + blocker。
- **仓库名→run 映射**：场景 §4 建议"按 repo 名索引成独立 `session_state_items(category='delegation_state', item_key='exec-N:repo:payments-service')`"——**v0.2 没要求**这种细粒度 item_key。实际实现要么在 `session_delegations.directive` 里塞仓库列表 JSON 做后端 filter（MatrixOne 的 JSON 索引能力未 spec），要么依赖 `exposed_artifacts_json` 里挂的 per-repo artifact_ref 反查。**记为 F-notable-3 / 新 gap 候选**：v0.2 没规定 per-leaf-work (like per-repo) 的投影粒度，drill-down API 无法按业务 key 定位。

**机制可达**：⚠️ partial（API 有，但业务 key → run 的映射未 spec）。**顺畅**：⚠️。**Token**：0（纯 DB，场景 §6 表第 5 列）。

---

### T7 · reviewer-3 发现 critical 漏洞，bubble_up 5 层写入链路

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| （无用户输入） | **bubble_up 链路（v0.2 G14 L1167–1179 + G18）**：<br/>**步骤 1**：L4 reviewer-3 内部生成 finding，INSERT `session_state_items(category='finding', item_key='finding:cve-2022-22965-like:auth-gateway', session_id=sess-vuln-annual-q1, priority=0, payload_json={severity:critical, repo, rule_id, artifact_ref})`。<br/>**步骤 2**：同事务 INSERT `session_state_item_events(mutation='bubble_up', payload_json={bubble_seq:1, severity:'critical', source_run_id:'run-L4-rev-3', original_item_id:'finding:...', bubble_target_scope:'root_session', summary:'Critical RCE in auth-gateway', artifact_refs:[...]})`（v0.2 L1167–1179）。<br/>**步骤 3**：payload 里 `bubble_target_scope='root_session'` 触发 projector（异步 worker 订阅 bubble_up 事件）去 UPDATE 链路上每层的 `session_delegations` row：exec-3 的 delegation row `blocker_json` patch `{critical_findings_count:+1}`，同父 run（run-L1-orch）的 delegation row 同样 patch；同时 UPSERT `session_state_items(category='anchor', item_key='alerts[0]', priority=0, payload_json={severity:critical, repo, rule, artifact_ref})` 到 root session 的 anchor。<br/>**步骤 4**：UI 订阅 `bubble_up` 事件（v0.2 L1206 "Root-session UI may subscribe only to `bubble_up` state events"），L0 顶部弹红色 banner。 | L0 顶 banner："🔴 Critical: RCE in auth-gateway (CVE-2022-22965-like)，点击查看"；`session_anchor` zone 下轮若 L1 被唤醒会多 1 条 alert，但此轮 **不触发 LLM 调用**。 |

**state_item_events 写入链路（逐层）**：
1. L4→L3：`session_state_item_events` 只写 1 条（`mutation='bubble_up'` on 源 item），但 worker 需派生 N 条派生事件（每层 1 条 `update` on `session_delegations` 的 `blocker_json`）—— **v0.2 没说清** bubble_up 是单事件还是链式事件。若是单事件 + worker fan-out，则 5 层写的是 1 + 4 = 5 条 events；若每层 `session_delegations.blocker_json` UPDATE 自带 event → 10 条。
2. L3→L2：本场景 L2 planner 已完成（T3），其 delegation row 状态 archived。L4→L3→L1 实际只跨 3 层（不经 L2，L2 不在当前 active 链）。场景 §3 T7 写"逐层向 L2/L1"是误导——**正确链路**按 `ancestor_path='run-L1-orch/run-L3-exec-3/run-L4-rev-3'` 走 3 跳：rev-3 → exec-3 → orch-1。
3. 每跳写入 **token 成本**（§7 细算）：每条 event payload ~200 byte，5 层最坏 5 条 events + 3 条 `session_delegations.blocker_json` patch；写入延迟（MatrixOne 单行 insert ~5 ms，batch transaction ~20 ms）→ 链路总 <100 ms，低于场景 §3 T7 承诺的 500 ms。

**机制可达**：✅（G14 mutation 枚举 + payload 契约 + G18 "blocker 2× per_child_budget"）。**顺畅**：✅（UI 只订阅 root-session bubble_up 事件 L1206，非常轻）。**别扭**：
- **`ancestor_path` 是 **目标路径**，bubble 从叶→根用的是反向遍历**。v0.2 L1336 给了 `ancestor_path TEXT`，但没说明反向遍历的 SQL 或字符串解析规则（拆 `/`？）。记为 F-notable-4。
- **bubble_seq 是 per-session 还是 per-item？** L1171 给了字段但未定义作用域。多个 critical 并发 bubble 时，`bubble_seq` 乱序会让 UI 误显顺序。记为 F-minor-4。
- **中间层 L2 已 archived**：bubble 路径经过已完成的 delegation，是否需要给它写事件？L2 在 T3 已 closed，事件"打扰"已归档节点的语义不明。记为 F-minor-5。

---

### T8 · 用户跨层纠偏（retry_scope='subtree' 判断）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| "把 executor-2 的数据库扫描重做一遍，其它组不用动" | **A15 分级**（v0.2 L637–656 + G12 L657–698）：<br/>**步骤 1 structured**：解析 "executor-2" → 检索 `session_delegations WHERE session_id=... AND directive LIKE '%group-2%' AND status='running'` → 命中 `run-L3-exec-2`，confidence=1.0。<br/>**步骤 2 rule**：关键词 "重做 / rerun" → "partial retry with scope filter"。<br/>**步骤 3 small-model**：解释 "数据库扫描" → "sast-rule-set:sqli-*"，confidence≈0.7。<br/>**步骤 4**：综合 suggestion `{target_run:'run-L3-exec-2', scope:'sast-rule-set:sqli-*', action:'retry_subtree'}`，confidence=max(structured 1.0, rule 0.9, small-model 0.7) → 按 G12 L669 "≥0.8 auto-accept" 阈值被接受。<br/>**步骤 5**：INSERT `session_state_item_events(mutation='apply_suggestion', payload_json={suggested_next_action_id:'sna_xxx', chosen_candidate_id:'cand_retry_subtree', source:'user_explicit', confidence_at_accept:1.0})`（v0.2 L1181–1190）。<br/>**步骤 6 retry**：INSERT `agent_runs run-L3-exec-2-retry`，`retry_of='run-L3-exec-2'`, `retry_scope='subtree'`（L924 + L1194）, `parent_run_id='run-L1-orch'`, `delegation_id='dl-exec-2-retry'`。UPDATE 原 `run-L3-exec-2 SET status='superseded'`（L1059 + L1197）。<br/>**步骤 7 级联 subtree**：因 `retry_scope='subtree'`，派生 `run-L4-rev-2-retry`，原 L4 reviewer 切 `superseded`。 | UI delegation 树并列显示 exec-2（灰，可 drill-down audit）+ exec-2-retry（新 running）；L0 发了一条系统消息确认 retry 已生效。 |

**retry_scope='node' vs 'subtree' 判断**：
- v0.2 L1194 枚举 `node / subtree / siblings`，但**没给判断规则**（在何种场景选哪个）。
- 本场景正确判断：因为 exec-2 已派出 reviewer-2 并 reviewer 持有 SAST rule set（在其 retrieved_facts zone），如果只 retry exec-2 node → 新 exec-2-retry 可能复用 L4 reviewer-2 的旧误报规则集 → 还是漏 SQL 注入变种。所以**必须 subtree**。
- 但 v0.2 没把这个判定逻辑编入 assembler——**由谁决定 subtree？** 小模型建议里应该含 `retry_scope` 字段，用户 confirm 时要能看到 "这会重派 reviewer" 的提示。v0.2 L1181 `apply_suggestion.payload_json` 缺 `retry_scope` 字段。记为 F-notable-5 / 新 gap 候选。
- **siblings 用例**：若用户说 "所有组重做" 应该 `retry_scope='siblings'` 把 exec-1..4 一起重派 + 保留 parent orchestrator。本场景不触发，但设计应测过。

**manifest zone（L1 下一轮被唤醒）**：
- `delegation_state` zone n 从 4 增到 5（exec-1/2-superseded/2-retry/3/4）但 G18 只对 `status='running'` 计数 → active_children 仍 4（exec-2 切 superseded 后不算 active）→ per_child 仍 375。`session_delegations` L1353 `idx_delegations_session_status` 支持该过滤。**OK**。

**机制可达**：⚠️（retry_scope 判断规则未编码）。**顺畅**：✅。**Token**：L0 本回合 ~120 token 用户输入 + 0 LLM 调用（A15 正确路径）。

---

### T9 · 所有 executor 完成，reviewer triage 收敛

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| （无） | 4 个 exec（含 exec-2-retry）+ 各自 reviewer `status='completed'`。每个 L3 写 `session_state_items(category='summary', version=2, token_estimate≤400)`。每个 `session_delegations.status='completed', last_summary_ref` 就绪。 | UI 树全绿；顶部仍挂 T7 critical banner；用户界面显示 "准备收尾，等待 reporter"。 |

**manifest**：L1 下一轮 `reason='delegation_aggregate'`（L1518）。此时 `session_state_items(category='delegation_state')` 总计 11 条（1 plan archived + 4 exec + 4 review + 2 retry-pair，按场景 §5 T10 表），但 active=0。

**机制可达**：✅。**顺畅**：✅。**别扭**：retry-pair 里的 "retry 新行 + superseded 旧行" 在 UI 默认是否折叠？v0.2 L1199 只说 "superseded branches are muted"，没说默认折叠/展开策略。记为 F-minor-6。

---

### T10 · L1 派 reporter 子 run 生成最终报告

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| "收尾。按仓库分组出最终报告…别放误报、别把 reviewer 内部讨论塞进来" | (1) INSERT `agent_runs run-L2-report`（与 planner 并列的 L2），`parent_run_id='run-L1-orch'`, `ancestor_path='run-L1-orch/run-L2-report'`, `depth=1`, `delegation_id='dl-report'`。(2) reporter manifest `reason='tree_structured_report'`（L1512）。(3) reporter 只 SELECT：4 条 exec summary（category='summary'，`scope='session'`，via `session_delegations.last_summary_ref`）+ T8 decision row（category='decision'，G2 保护名单）+ `session_artifacts WHERE severity>=high` 过滤 30 条 finding 全文。**不回放** L3/L4 transcript。(4) 输出写 `session_state_items(category='summary', item_key='final-report', version=1)` + artifact。(5) L1 run `status='completed'`, `agent_sessions.status='archived'`。 | L0 界面：Markdown 报告，顶部 T7 critical banner 保留；每条 finding 点击下钻到 artifact（`GET /artifacts/{id}/download-url` L2741 presigned URL）。 |

**manifest zone（reporter 本级）**：system_static 1200 / tool_schemas 700 / skills 400 / session_anchor 500 / plan_todo 200 / recent_tail 800 / summary 2100 / retrieved_facts 3800 / delegation_state 400（含所有 L3 最终 summary roll-up）/ workspace 200 / **合计 ≈ 10300**（与场景 §6 T10 列一致）。

**机制可达**：✅。**顺畅**：✅。**别扭**：reporter 作为独立 L2 run 而非 L1 自己跑——好处是隔离 tool_schemas；但 L1 和 reporter 其实都需要读同一批 summary/decision/finding row，两轮 manifest 重复拉取（加 prompt cache 命中率会高，但 DB 查询是重复）。记为 F-minor-7。

---

## §4 发现的别扭点

### F-blocking-1 · G18 公式在 n≥8 时自相矛盾（**关键**）

- **Ref**: T4, T9；v0.2 §Delegation State Budget L2442–2471
- **Observation**: 公式 `per_child_budget = min(1200, max(200, floor(1500/n)))` 同时声明 "total 1500 hard cap" + "floor=200"。当 n≥8 时 `200*n > 1500`，两约束无法同时成立。T9 场景 `session_state_items(delegation_state)` 达 11 行（含 superseded），若 UI 某刻展开全部 active=8（假设另一次 retry）→ 公式输出 200，总计 1600 超 cap。
- **Why friction**: assembler 实现要么违反 floor 要么溢出 hard cap，无确定行为。不同实现会分歧。
- **Severity**: **blocking**（契约矛盾）

### F-blocking-2 · retry_scope 判定规则缺失

- **Ref**: T8；v0.2 L1194 retry_scope enum
- **Observation**: enum 有 `node / subtree / siblings`，但没给"何时选哪个"的判定逻辑或契约，也没给 `apply_suggestion.payload_json` 里是否必须含 `retry_scope`。实现可能把 subtree 用例当 node 处理 → 漏级联重派 reviewer。
- **Why friction**: 正确性 blocking（场景 F3 已在 §7 警告）；小模型 suggestion 没 schema 位置放 retry_scope，assembler 无法确定性派生新 run。
- **Severity**: **blocking**

### F-notable-1 · T3 archived summary 与 T10 reporter 消费的冲突

- **Ref**: T3, T10；v0.2 G2 §Compaction Invariants + G14 L1421
- **Observation**: T3 L2 delegation 完成→对应 `session_state_items(category='delegation_state').status='archived'`（L1421）。T10 reporter 要从 11 条 delegation_state 里抽最终 summary 做 aggregate。archived 行仍保留、可读（G14 L1198 "no state item physically deleted"），但 G2 白名单里 `summary` 不在保护名单（v0.2 L86–90 白名单是 `plan_state/decision/todo_state/error_state/delegation_state(active)/finding/benchmark/citation`）→ L2 的 summary item 可能被 compaction 物理 replace 掉。
- **Why friction**: 长会话（例如扫描跑 2 天）触发多轮 compaction，L2 planner 的 summary 若被替换，T10 reporter 拿到的是"截断后的 summary"而非原始 350 token 的 plan 叙述。
- **Severity**: **notable**

### F-notable-2 · G18 blocker 借 2× budget 的 total cap 无定义

- **Ref**: T4；v0.2 L2466–2467
- **Observation**: "If a child has an active blocker, that child may temporarily use `2 * per_child_budget` by borrowing from recent_tail" —— 但 total 1500 cap 是否同时放宽？`recent_tail` 借出去后该 zone 自己是否也 underflow？
- **Why friction**: 5 层场景下，T7 bubble_up 激活的 critical blocker 会让 G18 临时扩容但 recent_tail 会被挤空 → L1 下一轮的 user input 重放不足。
- **Severity**: notable

### F-notable-3 · 业务 key → run_id 的投影粒度未 spec

- **Ref**: T6；v0.2 G4 `session_delegations.directive` 是 TEXT
- **Observation**: T6 要按仓库名（"payments-service"）定位 run，需要某张表能 WHERE by repo name。v0.2 没规定 per-repo/per-task 粒度的投影，要么塞 JSON（MatrixOne JSON 索引能力未 spec，见 GAP-TRACKER 未提）要么生成大量细粒度 `session_state_items`（token 爆炸）。
- **Why friction**: drill-down API 存在但 payload 结构不支撑业务 key 查询。
- **Severity**: notable

### F-notable-4 · `ancestor_path` 反向遍历规则缺

- **Ref**: T7；v0.2 L1336
- **Observation**: `ancestor_path='run-L1/run-L3-exec-3/run-L4-rev-3'` 是正向路径字符串。bubble 从 L4 反向写各层 delegation row，需解析 `/` split 拿到 L3 = ancestor[-2], L1 = ancestor[0]。v0.2 无 helper SQL 或 lib 契约。
- **Why friction**: 每家后端会各写各的 parser；run_id 里若有 `/` 会破 parser。
- **Severity**: notable

### F-notable-5 · `apply_suggestion.payload_json` 缺 `retry_scope` 位

- **Ref**: T8；v0.2 L1181–1190
- **Observation**: apply_suggestion payload 契约列出 `suggested_next_action_id / chosen_candidate_id / source / confidence_at_accept` 4 字段，没给 retry_scope 位置。场景实现被迫把 retry_scope 塞到 `chosen_candidate_id` 里或新增字段。
- **Why friction**: 与 F-blocking-2 同源。
- **Severity**: notable

### F-minor-1..7（汇总）

- F-minor-1：T1 L1 agent_id seed 流程未定
- F-minor-2：Projection Sync Contract 跨 3 表原子事务对分布式后端重
- F-minor-3：L4 reviewer 从 `rejected_pattern scope='user'` 拉规则时 `origin_session_id` 可能缺
- F-minor-4：`bubble_seq` 作用域（per-session/per-item）未定义
- F-minor-5：bubble_up 经过已 archived delegation 时是否写事件未定义
- F-minor-6：UI 折叠 superseded retry 兄弟的默认策略未定义
- F-minor-7：reporter L2 与 L1 重复拉 summary/decision，prompt cache 可救但 DB I/O 重复

---

## §5 发现的新 gap 候选

### G20. Delegation zone budget 公式在大 n 自相矛盾

- **title**: G18 公式在 `active_children ≥ 8` 时 floor=200 与 total=1500 不能同时成立
- **triggered_by_turn**: T4（n=4 健康），T9 峰值可至 n=8+（含 retry pair）
- **related_v0.2_section**: §Delegation State Budget L2442–2471
- **severity**: high
- **rationale**: 这是 **契约级矛盾**，不是实现细节。两个硬约束互斥时 assembler 无确定行为。S10 本场景 n=4 不触发，但 retry + nested review 组合下 n 可达 10+。
- **proposed_fix**: 明确 "当 n ≥ 8：每 child 仍用 200 floor，总 cap 动态放松到 `max(1500, 200*n)`"，并把此条写入 assembler test matrix。

### G21. `retry_scope` 判定规则 + `apply_suggestion` schema 扩展

- **title**: retry_scope 枚举缺选择规则；apply_suggestion payload 缺 `retry_scope` 字段
- **triggered_by_turn**: T8
- **related_v0.2_section**: §Delegation Retry and Bubble-Up Contract L1192–1200 + L1181–1190
- **severity**: high
- **rationale**: enum 有三个值但没告诉 assembler/小模型何时选 subtree（持状态的 child 存在？）vs node（纯计算）vs siblings（全面重派）。T8 若判错会漏重派 reviewer → finding 丢失。
- **proposed_fix**:
  1. 在 Delegation Retry 节加判定表："child 有 active delegation_state row → 默认 subtree；child 无子 + tool 无状态 → node；用户显式 '所有' → siblings"。
  2. `apply_suggestion.payload_json` 加 `retry_scope` 字段并校验 enum 值。

### G22. Delegation 业务 key 投影粒度

- **title**: drill-down API 无法按业务 key（仓库名/文件名）定位叶 run
- **triggered_by_turn**: T6
- **related_v0.2_section**: §Session State Projection + §Delegation APIs
- **severity**: medium
- **rationale**: v0.2 的 `session_delegations` 粒度是 delegation_id（通常对应一个 child run），但 S10 这种"1 个 L3 run 跑 13 仓库"的情况，用户不会按 exec_id 问，会按 repo 名问。v0.2 没给 per-leaf-work 的 state item 或 index 契约。
- **proposed_fix**: 规定 "若 child run 的 directive 含 `scope_items[]`，则额外写 `session_state_items(category='delegation_leaf', item_key=scope_item)` 作为 per-item 索引；drill-down API 支持按 leaf key 反查 delegation_id"。

---

## §6 Gap 组合效应检验

### 组合 1 · G4 + G14 · 深层 retry + 投影同步

- **运行时可能发生的坑**：T8 retry 创建 `run-L3-exec-2-retry`，同事务需 (a) INSERT `agent_runs` (b) UPDATE 原 agent_runs `status='superseded'` (c) INSERT 新 `session_delegations` (d) UPSERT `session_state_items(category='delegation_state')` for retry (e) UPDATE 原 delegation row `status='superseded'` (f) 级联 step c–e for L4-rev-2-retry。**6 行跨 3 表 × 2 run（L3+L4）= 12 写操作在一事务内**。MatrixOne 单事务大小若有限制会失败。
- **v0.2 是否覆盖**：partial（L1411 要求同事务，但没给事务大小上限；也没给 retry subtree 的级联事务模板）。
- **建议**：在 Delegation Retry 节给一段 SQL 伪代码模板，覆盖 subtree 场景 N 个 descendant 的原子写。

### 组合 2 · G4 + G18 · delegation_state zone 溢出 + projection sync

- **运行时可能发生的坑**：T9 峰值 11 个 delegation_state row（含 archived + superseded），assembler 先 SELECT `WHERE status='running'` 过滤（走 `idx_delegations_session_status`），再按 G18 分预算。但若某 reviewer 临时切 `blocked`，L2466 让它借 2× per_child，而此时 G20 的 n≥8 矛盾已激活 → assembler 行为未定义。projection sync（L1421）要求状态切换同事务更新两表——若事务失败 rollback，zone 计算基于 stale 数据。
- **v0.2 是否覆盖**：partial（G18 公式本身有 G20 矛盾；G4 sync 有契约但与 G18 borrow 语义未拼齐）。
- **建议**：G18 + G4 合并一段 "Zone assembly after projection rollback 的一致性契约"，明确 retry/borrow/rollback 的顺序与兜底。

### 组合 3 · G14 retry_scope + G14 bubble_up

- **运行时可能发生的坑**：T7 bubble_up 正在从 L4-rev-3 向 L1 传播时，T8 用户同时发出 "重做 exec-2"——若实现把 T8 的 apply_suggestion 处理优先于 T7 的 bubble_up，会出现 "L1 anchor 先收到 retry marker 再收到 critical alert" 的倒序。UI 显示顺序与业务重要性不符。更糟：retry subtree 若误命中 exec-3（同 group 被混淆），可能重派正在 bubble 的那棵子树，reviewer-3 的 finding 被 superseded 覆盖。
- **v0.2 是否覆盖**：no（v0.2 L1167 bubble_seq 没规定 global seq；v0.2 没规定 bubble_up 与 apply_suggestion 的相对优先级）。
- **建议**：规定 "anchor 上 `alerts[*]` 排序按 `priority DESC, bubble_seq DESC`；retry 只能作用于 `status='running'` 且 `!has_active_bubble` 的 run；反之 retry 必须等 bubble 链完成"。

### 组合 4 · G9 access_scope + 跨层 artifact 读写

- **运行时可能发生的坑**：T10 reporter (L2) 要读 exec-1..4 (L3) 和 reviewer-*的 (L4) artifact。G9 L1718 access_scope='delegation' 的定义是 "parent session and descendants in the delegation tree"——**reporter 是 orch 的 child 而非 exec 的 parent**，它与 exec 是"兄弟（同 root_run_id）但不同 ancestor_path"。按 L1718 严格解释：reporter 不是 exec 的 ancestor 也不是 descendant，**不能**读 exec 的 `access_scope='delegation'` artifact。
- **v0.2 是否覆盖**：**no**（G9 的 delegation scope 没覆盖"同 root 兄弟子树"的 artifact 共享）。
- **建议**：G9 扩展 access_scope='delegation' 语义为 "anywhere under the same `root_run_id` tree"，或新增 `access_scope='root_tree'`。

---

## §7 Token 实际 walkthrough（T7 bubble_up 5 层链路 + T4 紧张回合）

### T4（L1 本级 4 child 派发后）—— delegation_state zone 压力测算

| zone | 实测（场景§6 T4 列） | G18/Budget v1 cap | 盈余/溢出 |
| --- | ---: | ---: | --- |
| system_static | 1200 | ≈1200 stable（L1533） | ok |
| tool_schemas | 900 | "pruned" 无硬 cap | ok |
| skills | 600 | — | ok |
| session_anchor | 450 | "hard cap"（无具体数） | 推测 500 内 ok |
| plan_todo | 500 | "always include active" | ok |
| `delegation_state` | 600（4×150） | **G18 total 1500**（n=4 → per_child 375，贴满 1500） | ok（实际 600 < 1500，40% 余量）|
| recent_tail | 700 | "last 2-4 turns" | ok |
| summary | 350 | — | ok |
| workspace | 200 | — | ok |
| **合计** | **5700** | 200k 模型 | 远低于 cap |

**G18 "375 token/child 合理吗" 最终结论**：
- **n=4 本场景下合理**：场景实际每 child 150 token（title+status+scope+repo_count），远低于 375 上限，有 225 token 冗余可供未来加字段。
- **n≥8 不合理**：G20 新 gap 候选揭示的矛盾。
- **边界触发场景**：若 delegation_state 要塞 `exposed_artifacts_json` 的 artifact_ref 列表（每 ref 40 字节，13 仓库×3 引擎=39 个 artifact×40=1560 token）→ 单 child 就爆 1200 cap 且多个 artifact_ref 不能只用 1 个 preview。G18 没提 artifact_ref 压缩。

### T7（bubble_up 链路 token + 延迟成本）

**写入成本估算**：
- **ancestor 链长度**（5 层场景）：`ancestor_path='run-L1-orch/run-L3-exec-3/run-L4-rev-3'` = 3 个 run_id × 28 chars + 2 个 `/` = **≈86 chars**（S10 场景 L0 不是 run，故 ancestor_path 只到 L1..L4，4 层时 ~115 chars；T7 这条链 depth=2，3 runs，86 chars）。无溢出风险（TEXT 列）。
- **写入行数**（worst case）：
  - 1× `session_state_items(category='finding', priority=0)` at L4
  - 1× `session_state_item_events(mutation='bubble_up')` at source
  - Worker 派生：2× `session_delegations UPDATE blocker_json`（L3 exec-3, L1 orch，L2 已 archived 跳过）→ 2× `session_state_item_events(mutation='update')`
  - 1× UPSERT `session_state_items(category='anchor', item_key='alerts[0]')` at root session
  - 1× `session_state_item_events(mutation='insert')` for anchor
  - **总 7 行跨 3 表**
- **每行 payload**：~200–400 byte，总 ~2.5 KB
- **延迟**：7 行 inserts/updates，若一个事务 ~15 ms，若 worker 异步则 propagator latency ≈ 50 ms + 各 UPDATE 单独提交约 5 ms 每个 ≈ 30 ms → **总 <100 ms**，远低于场景 §3 T7 承诺的 500 ms。
- **token 成本**（LLM 侧）：**0**——整个 bubble_up 链路不触发任何 LLM 调用，UI 订阅 state_item_events 推送，L0 下一次真正 manifest 才会把新 alert 纳入 `session_anchor` zone（~80 token 的增量）。

**对比朴素方案**（若没 bubble_up 一等机制）：
- 朴素 = 每层 agent 各生成一次 "向父层汇报 critical finding" 的 LLM 响应：5 层 × 平均 800 token（input+output）= 4000 token，还需 5 次 LLM 调用 ≈ 5 × 2 s = 10 s 延迟。
- 节省 **100% token + ~100× 延迟**。这就是 G14 一等 bubble_up 的核心价值。

### ancestor_path 5 层下长度粗估

| 层级 | 路径示例 | 长度 |
| --- | --- | ---: |
| L1 | `run-L1-orch` | ~12 char |
| L2 | `run-L1-orch/run-L2-plan` | ~25 char |
| L3 | `run-L1-orch/run-L3-exec-2` | ~28 char |
| L4 | `run-L1-orch/run-L3-exec-2/run-L4-rev-2` | ~44 char |
| L5（假想 reviewer 再派 sub-triager） | + `/run-L5-trg-1` | ~58 char |
| 真实 UUID 版（每 run_id ~36 char） | L4 = 36*3+2 = **110 char** | TEXT 列无压力 |

**结论**：即便 10 层也 <500 char，TEXT 列足够；但 `idx_delegations_root_depth (root_run_id, depth, updated_at)` 是三列索引，不含 ancestor_path，按 ancestor_path 反向遍历仍需 string parse（F-notable-4）。

---

## §8 最终判断

- **本场景下 v0.2 是否可实现**：⚠️（主骨架✅；但 G20 + G21 两个新 gap 是 blocking，不修会影响正确性）
- **顺畅度**：⚠️ 有明显摩擦（retry_scope 判定规则、G18 大 n 矛盾、业务 key drill-down 粒度是三大摩擦源）
- **一句话总结**：v0.2 把 5 层 delegation 的**数据结构**（ancestor_path/root_run_id/session_delegations/bubble_up/retry_scope enum）基本搭齐，**token 模型**（delegation_state zone 恒定 O(active_children)）理论上健康，但 **契约完整度**在三个地方留白——`delegation_state` zone n≥8 的公式矛盾、`retry_scope` 没判定规则、业务 key→run_id 的投影粒度——这三处不补，S10 CISO 场景在生产环境会触发歧义实现并在 retry/drill-down 流程上出 bug。

---

## §附录 A · T6 drill-down 的具体 SQL 与 API 调用链

为让 F-notable-3 的别扭可视化，把 T6 实际跑的 3 次 DB query 写出来：

**API hop 1**：`GET /sessions/sess-vuln-annual-q1/delegations?root_run_id=run-L1-orch`（v0.2 L2729）
```sql
-- 底层走 idx_delegations_root_depth
SELECT delegation_id, parent_run_id, child_run_id, depth, status, phase,
       directive, last_summary_ref, last_summary_token_estimate,
       exposed_artifacts_json, blocker_json, updated_at
FROM session_delegations
WHERE session_id = 'sess-vuln-annual-q1'
  AND root_run_id = 'run-L1-orch'
ORDER BY depth ASC, updated_at DESC
LIMIT 50;
```
返回 ~7 行（T5 时刻：1 plan closed + 4 exec active + 1 review-2 active + 任何其他）。

**API hop 2**：client 侧过滤，仓库名 `payments-service` 要在每行的 `directive`/`exposed_artifacts_json` 里字符串匹配——v0.2 没给 JSON 索引字段，需 MatrixOne 本体支持 `JSON_CONTAINS` 或拉回客户端做 filter。本场景 13 仓库 × 4 exec = 52 值，可接受；扩到 500 仓库就要加 leaf 索引表（G22 新 gap 建议）。

**API hop 3**：`GET /chat/runs/run-L3-exec-2/delegation-summary`（v0.2 L2738）
```sql
-- 在 session_state_items 里按 last_summary_ref 定位
SELECT i.item_id, i.category, i.title, i.summary_text, i.payload_json, i.token_estimate
FROM session_state_items i
INNER JOIN session_delegations d ON d.last_summary_ref = i.item_id
WHERE d.child_run_id = 'run-L3-exec-2'
  AND i.status IN ('active','archived');
```

**API hop 4**（optional L4）：同样对 `run-L4-rev-2-batch1` 查 summary 看 triage 进度。

**合成回答**：server 拼 "exec-2 正在 triage batch-1，payments-service 已完成 SAST+SCA…" 一句话返回。整个链路 ≤50 ms（4 × ~10 ms index lookup），**0 LLM token**。

**问题**：如果用户问 "所有 tier-1 仓库扫到哪了"（多仓库同时），就得对每个仓库名跑一次 hop 2 filter，没 batch API。v0.2 L2729 的 delegation API 一次拉整树，但 hop 3/4 无 batch 形式。记为 F-minor-8（API batching 缺）。

---

## §附录 B · T8 structured retry 的状态机落地

**完整顺序**（带 A15 三级分流 + G14 retry_scope + G12 confidence）：

```text
用户原话 "把 executor-2 的数据库扫描重做一遍"
  │
  ├── Layer 1 structured_event（v0.2 L644）
  │     query: SELECT child_run_id, directive FROM session_delegations
  │            WHERE session_id=? AND status='running' AND directive LIKE '%group-2%'
  │     hit: run-L3-exec-2, confidence=1.0
  │
  ├── Layer 2 rule_extraction（v0.2 L646）
  │     regex: /重做|rerun|redo/ → action='retry'
  │     regex: /数据库|SQL|sqli/ → scope_hint='sqli'
  │     confidence=0.9
  │
  └── Layer 3 small_model（v0.2 L648）
        prompt: "解析 '数据库扫描' 对应 SAST rule set"
        output: "sast-rule-set:sqli-*"
        confidence=0.7

合成：suggested_next_action {
  id: sna_abc,
  target_run: run-L3-exec-2,
  action: retry,
  scope_hint: sqli,
  retry_scope: subtree,  -- 因 exec-2 有 active reviewer，建议 subtree (G21 建议补全)
  candidates: [
    {cand_id: cand_retry_node, confidence: 0.7},
    {cand_id: cand_retry_subtree, confidence: 0.9}  -- picked
  ],
  expires_at: now+7d  -- v0.2 L690 todo 默认 7d
}
status = accepted (user_explicit, confidence_at_accept=1.0 per G12 L669)
  │
  ↓ INSERT session_state_item_events(mutation='apply_suggestion', payload={...})
  ↓ executor: L1 agent 消费此事件
  │
  ├── INSERT agent_runs run-L3-exec-2-retry (retry_of=run-L3-exec-2, retry_scope=subtree)
  ├── UPDATE agent_runs run-L3-exec-2 SET status='superseded'
  ├── INSERT session_delegations (child_run_id=run-L3-exec-2-retry, delegation_id=dl-exec-2-retry)
  ├── UPDATE session_delegations WHERE child_run_id=run-L3-exec-2 SET status='superseded'
  ├── UPSERT session_state_items(category='delegation_state', item_key='delegation:dl-exec-2-retry')
  ├── UPDATE session_state_items(category='delegation_state', item_key='delegation:dl-exec-2') SET status='archived'
  ├── 【级联 subtree】 对所有 WHERE parent_run_id IN (run-L3-exec-2) AND status='running' 的 child run 重复上述 6 步
  │     → run-L4-rev-2 → superseded → 新 run-L4-rev-2-retry
  └── INSERT decision row: category='decision', payload={action:retry_subtree, reason:'user pointed out rule set miss'}

事务提交：12–14 写操作跨 4 表（agent_runs/session_delegations/session_state_items/session_state_item_events），MatrixOne 单事务 OK，但见 §6 组合 1 的注意。
```

**不走 LLM 的关键**：Layer 1 直接 DB match 成功 + Layer 2 rule 命中 + 只有 scope hint 走小模型。主 LLM 没被调用 → 避免 "hallucinate 成新任务"。v0.2 L637–656 的设计原意就是这个。

---

## §附录 C · bubble_up 事件在 5 层下的时序图（T7）

```text
t=0ms    L4 rev-3 reviewer finishes triage, finds 1 critical
t=+5ms   INSERT session_state_items(category='finding', item_id=it_cve,
             session_id=sess-vuln-annual-q1, priority=0,
             payload={severity:critical, repo:auth-gateway, rule:CVE-2022-22965-like})
t=+10ms  INSERT session_state_item_events(
             item_id=it_cve, mutation='bubble_up',
             payload={bubble_seq:1 (unclear scope, see F-minor-4),
                      severity:critical,
                      source_run_id:run-L4-rev-3,
                      original_item_id:it_cve,
                      bubble_target_scope:root_session,
                      summary:'Critical RCE in auth-gateway',
                      artifact_refs:[artifact_finding_pack]})
t=+15ms  【worker】subscribe mutation='bubble_up' events, see above row
t=+20ms  walk ancestor_path='run-L1-orch/run-L3-exec-3/run-L4-rev-3' backward:
            - UPDATE session_delegations WHERE child_run_id=run-L3-exec-3
                SET blocker_json=JSON_PATCH(blocker_json, {critical_findings_count:+1, bubble_ref:ev_xxx})
                → INSERT session_state_item_events(mutation='update', item_key='delegation:dl-exec-3')
            - (L2 planner 已 archived，跳过 or 仍写？ ambiguous F-minor-5)
            - UPDATE session_delegations WHERE child_run_id=run-L1-orch...
                wait, run-L1-orch IS root, not a child of anyone
                → skip the self; instead UPSERT root session anchor
t=+35ms   UPSERT session_state_items(
              session_id=sess-vuln-annual-q1, category='anchor', item_key='alerts[0]',
              priority=0, payload={severity:critical, repo, rule, artifact_ref})
t=+40ms   INSERT session_state_item_events(mutation='insert', category='anchor')
t=+50ms   SSE push to L0 UI: event type='bubble_up', payload summary
t=+80ms   UI renders red banner on L0

Total latency L4 → L0 banner ≈ 80ms (well within 500ms spec)
LLM token cost: 0
```

**若 bubble_seq 是 per-session**：2 个并发 critical 会 sequence 正确。**若 per-item**：bubble_seq 都是 1，UI 无法排序（F-minor-4 触发）。建议 v0.2 明确 per-session 作用域。

---

## §附录 D · manifest zone 完整对照（所有 6 关键节点 token 总表）

（从场景 §6 表直接复刻并加 cap 列）

| zone | L0 T1 | L1 T4 | L3 exec-2 T5 | L4 review T5 | L0 T6 drill | L1 T10 final | v0.2 cap / 说明 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| system_static | 1200 | 1200 | 1200 | 1200 | 0 | 1200 | stable prefix, cache-friendly |
| tool_schemas | 1100 | 900 | 2400 | 600 | 0 | 700 | pruned by active agent |
| skills | 600 | 600 | 500 | 400 | 0 | 400 | selected skills |
| session_anchor | 450 | 450 | 450 | 500 | 0 | 500 | "hard cap" 未给数字 |
| plan_todo | 0 | 500 | 800 | 0 | 0 | 200 | active phase + next |
| recent_tail | 300 | 700 | 1400 | 900 | 0 | 800 | 2–4 turns |
| summary | 0 | 350 | 600 | 0 | 0 | 2100 | latest L1b |
| retrieved_facts | 0 | 0 | 0 | 400 | 0 | 3800 | Top-K |
| tool_previews | 0 | 0 | 300 | 100 | 0 | 0 | short only |
| `delegation_state` | 0 | 800 | 180 | 0 | 0 | 400 | **G18 total 1500 hard cap** |
| safety_approvals | 0 | 0 | 0 | 0 | 0 | 0 | — |
| workspace | 200 | 200 | 200 | 200 | 0 | 200 | — |
| **合计** | **3850** | **5700** | **8030** | **4300** | **0** | **10300** | 远低于 200k |

**关键观察**：
1. L1 T4 `delegation_state=800` 实际是 4 child × 150 avg = 600；场景原表写 800 含 "plan summary roll-up" 的 200——这部分按 v0.2 应归到 `summary` zone（L2's summary ref），**场景计量有轻微混淆**，不影响结论。
2. L1 T10 final 10.3k 的 `retrieved_facts=3800` 是 30 条 finding 全文——若 finding 条数翻到 100 条（6600 token）会开始挤 `summary` zone；v0.2 没规定 finding zone 归属（它归在 `retrieved_facts` 或 `summary` 或新开一个 `findings` zone？）。记为 F-minor-9。
3. 所有层的 `workspace=200` 稳定——G11 resolved 后的 reachability_probe 通过 `workspace_state` 写入，5 层共享同一 workspace 不放大。

---

## §附录 E · 5 层 delegation 下性能与 observability 核对

**查询性能（假设 MatrixOne 索引正常）**：

| 场景 | SQL 访问 | 索引 | 预估耗时 |
| --- | --- | --- | ---: |
| T6 delegation tree load | `idx_delegations_root_depth` | ✅ covered | ~5 ms |
| T7 ancestor walk (string split) | 无索引（内存 parse） | ⚠️ | ~1 ms |
| T8 retry candidate match | `idx_delegations_session_status` | ✅ | ~5 ms |
| T10 reporter 读 4 summary | `session_state_items(session_id, category, status, priority)` | ✅ | ~10 ms |
| UI `session_delegations` 列表订阅 | `idx_delegations_session_status` | ✅ | ~5 ms/poll |
| artifact ACL check（G9）| 取 session.project_id + artifact.access_scope → tree walk | ⚠️（未 spec index）| unknown |

**observability 核对**：
- `context_manifests.reason` 枚举完整覆盖本场景 6 个回合：T1=initial_turn / T2-T4=delegation_poll / T5=delegation_poll（L3 → L4）/ T7 不产 manifest / T8 不产 manifest / T10=tree_structured_report。**✅ G1 覆盖充分**。
- `context_manifest_items.dropped_count` 在 T4 L1 本级 = 0（zone 皆未超），T10 reporter 需关注（finding 全文若超则 drop）。
- `agent_run_events` 基于 `run_counters.next_event_idx` 单 writer 契约（G15 L900–950）在 5 层并行下自然满足——每个 run 独立 counter，不会相互踩。

**F-minor-10（observability）**：v0.2 没规定 delegation tree 的"健康度 metric"（例如 critical bubble 数量、superseded 分支比例），监控系统要自己拼 SQL。G9/G14 各自有 counter 字段但没聚合 view。

---

## §附录 F · 设计层面总结（Summary of findings）

- **v0.2 把 G4/G14/G18/G9 四条 gap 集中封装的 delegation 协议栈，已经撑得住 5 层深度的主流程**：ancestor_path + root_run_id + session_delegations 的三元组让 T6 drill-down 能从递归 CTE 降级成单次 index lookup；bubble_up 一等事件让 T7 跨 5 层告警链无需 LLM；retry_scope enum 让 T8 结构化 retry 能 superseded 原树保留审计。
- **但三处契约不完整导致 S10 在生产上不完全 ready**：
  1. G20（new）：G18 公式 n≥8 矛盾；
  2. G21（new）：retry_scope 判定规则 + apply_suggestion payload 缺字段；
  3. G22（new）：业务 key drill-down 投影粒度。
- **一组需警惕的组合效应（§6 组合 4）**：G9 access_scope='delegation' 的定义是 parent↔descendants，而 T10 reporter 与 exec 是"同 root 的兄弟"而非 descendant，reporter 访问 exec artifact 按字面解释**不允许**。这是 v0.2 的真实漏洞（不只是实现细节），建议把 access_scope='delegation' 明确放宽为"same root_run_id tree anywhere"。
- **S10 的核心价值被 v0.2 兑现**：token 随深度 O(active_children) 而非 O(N^depth)，本场景 T4 紧张回合 L1 本级仅 5700 token（对比朴素 >416k），T7 bubble 0 token，T6 drill 0 token——G14+G18 的设计思路**方向完全正确**，补 G20/G21/G22 即可达到生产级。
