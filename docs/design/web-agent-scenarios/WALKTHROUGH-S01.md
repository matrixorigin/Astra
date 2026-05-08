# WALKTHROUGH-S01 — Rust 电商后台 60 轮 axum 迁移

> Status: v0.2 运行推演 (基于 web-agent-session-state.md v0.2, 3040 行)
> Scenario: [S01-long-dev-marathon.md](S01-long-dev-marathon.md) §1–§3
> Baseline gaps: G1–G19 resolved (GAP-TRACKER.md)
> Mode: 端到端轨迹推演 · 非评审 · 以"真的跑一遍"视角

## §1 场景标识与 v0.2 映射

一句话场景：7 年 Rust 工程师用 astra web agent 驾驶员式协作，把 12-crate
电商后台从 actix-web 1.x 迁到 axum 0.8，3 天/60 回合/~400 工具调用/~80
artifact，每次写盘都要 diff 审。被测 DAUT: **A2 / A3 / A4 / A6 / A10**。

本场景命中的 v0.2 关键章节：

- `context_manifest_reason_types` 枚举 @ L1462–1529（本场景用 `initial_turn`
  / `post_compaction` / `history_recall_{structured,fts,vector}` /
  `large_tool_output_gated` / `final_delivery_summary`，6 种全用到）
- Compaction Invariants @ L2496–2594（T8/T38/T58 三次压缩，每次必过 8 条
  SQL assertion，尤其是 `plan_state.version` 禁 bump 与活跃 run 互斥）
- Retrieval State Machine SLA @ L1930–1987（T15 纯 structured <50ms；T31
  三级全开 <1s；T52 structured 命中不触发 FTS/vector）
- Delegation @ L1321–1426（本场景主线 60 轮**不派 sub-agent**，T60 交付
  由主 session 直接批量检索结构化事实合成，不走 `session_delegations`）
- Budget Policy v1 @ L2381–2406 + G10 small-window @ L2408–2440（本场景
  使用 Sonnet 4.5/200k 大窗口，未触发 `budget_v1_8k`）

## §2 运行前提

Schema 初值：`agent_sessions` 1 行、`agent_runs` 每回合起一行（60 行终态）、
`session_state_items` T1 预置 anchor+workspace_state+plan_state+2 skill_hint
共 5 行、`session_todos`/`session_todo_deps` T1 写入 12 条 P0/P1 + 依赖边、
`context_manifest_reason_types` 18 条 seed 已载入（enum FK 生效）、
`preview_template_registry` 对 `cargo` 无原生 entry → 走 fallback 400B preview
（⚠️ G8 未覆盖 cargo，见 §5）。`session_transcript_items` 空；
`session_history_chunks`/`session_artifacts` 空；`session_delegations`
始终空（本场景主线不派 sub-agent）；`agent_run_events.event_idx`
由 G15 单 writer 协议生成，本场景单用户单客户端，无并发 writer 冲突。

Workspace：`cloud` + Rust 1.83 + git 分支 `migrate/axum`（`workspace_state`
记录 reachability_probe/G11 初值 `ok`）。工具集：`edit`/`run_cargo`/
`grep`/`read`/`web_search`/`shell`。预期 context window：**Sonnet 4.5 /
200k**，用默认 Budget Policy（非 G10 小窗口），无硬 `plan_todo` 数字上限
（见 §4 friction F2）。预期每回合 prompt 5k–12k tokens 之间波动。

## §3 端到端运行轨迹（10 个代表回合）

### T1 · 开局对齐

- **用户**：「帮我把这个仓库从 actix-web 1.x 迁到 axum 0.8，12 个 crate，
  先别动代码，给我迁移 plan 和风险清单，我来对齐。」
- **服务端**：`agent_runs` 新建 run_1 (trigger=`user_message`)；
  `session_state_items` 插入 `anchor`/`workspace_state`/`plan_state(v1,
  12-phase)`/2 条 `skill_hint(rust-migration, axum-handler-analysis)`；
  `session_todos` 批量 upsert 12 条，`session_todo_deps` 17 条边；
  `context_manifests` 写 1 行 `reason='initial_turn'` (G1 enum L1503 命中)。
- **可见**：SSE `session.opened` / `plan.seeded(12_phases)` / `todo.batch_inserted(12)`。
  Manifest zone: `system_static=1200` / `tool_schemas=1800` / `skills=900`
  / `session_anchor=400` / `plan_todo=600` / `workspace=200`，
  `total_estimated_tokens=5100`, `dropped_count=0`, `tokenizer_id='claude_3_5'`,
  `budget_template_id=NULL`（default 模板无 id，⚠️ 见 F3）。LLM 看不到：
  8 万行仓库代码、skill 全文（只看摘要）。

### T8 · 首次 compaction 触发

- **用户**：「`order` crate 已 green，继续 `payment`，先扫 `actix_web::
  dev::Service` 用法看哪里映射不了。」
- **服务端**：turn 正常写入 3 条 `decision`（tokio::select! /
  FromRequestParts / thiserror 错误类型统一）+ 5 条 `active_file` +
  `plan_state.phase` 从 `order→done`/`payment→active`（同 turn 更新，
  非 compaction turn，`plan_state.version` 1→2 允许）。turn 结束
  `CompactionPolicy` 检测 `recent_tail` 累积 9200 > 8000 阈值触发 L1b：
  **先检查 active_runs=0** (G2 inv #4 L2553–2557) → 本 run 已结束 ✓；
  写 1 条 `summary(scope='session')` + 把 T2–T6 的 7 条 transcript_item
  标 `is_compacted=TRUE` (L1774)；**不动** plan_state/decision/active
  todo_state (G2 inv #2)；`plan_state.version` **不 bump** (G2 inv #3)。
  写第 2 条 `context_manifests` `reason='post_compaction'` (G1 L1506)，
  `compaction_version='v1.0'`、`dropped_count=7`。G2 SQL assertion 1/2/3/4/6
  全过 0；#6 返回 1 ✓。
- **可见**：SSE `compaction.started` / `compaction.done(summary_id=sum_1)`。
  Zones: `anchor=400` / `plan_todo=700` / `recent_tail=1800`（压缩后）/
  `summary=900` / `retrieved_facts=300` / `tool_previews=600`，total≈4700。
  LLM 看不到：7 条被标记 transcript 原文 (~11k tokens，仍在 conversation_log)。

### T15 · 回忆 T3 设计决策（纯 structured 检索）

- **用户**：「等下——我们 T3 是不是讲过 error 类型要走 `thiserror`？
  当时怎么约束 `From<sqlx::Error>` 的？」
- **服务端**：`RetrievalPolicy` 识别 "historical decision lookup" → 启动
  G3 state machine (L1949–1961)。**Stage 1 Structured** (L1938):
  `SELECT ... FROM session_state_items WHERE session_id=? AND
  category='decision' AND status='active' ORDER BY priority DESC,
  updated_at DESC LIMIT 5`，实测 18ms（target <50ms ✓），**直接命中**
  `decision:error-type-unified`，候选数 1，**不触发 FTS/vector**。
  manifest `reason='history_recall_structured'` (G1 L1507)，
  `dropped_count=0`，emit 0 degradation event。
- **可见**：SSE `retrieval.structured_hit(chunk=decision:error-type-unified,
  elapsed_ms=18)`；UI 侧 "引用 T8 决策" inline 折叠。Zones:
  `retrieved_facts` 300→540 (+240 decision payload)，其余不变，total≈4940。
  LLM 看不到：T3 原始 assistant_message 的 1800 tokens 思考过程（L2 audit 保留）。

### T24 · 新增 `settlement-v2`（plan 大改）

- **用户**：「临时决定——`settlement` 的 retry 拆 `settlement-v2` 单独承
  接，老那块先冻结，你把 plan 切开。」
- **服务端**：**非 compaction turn**，允许 plan 语义变更。
  `plan_state.version` 7→8 (G2 inv #3 只约束 compaction turn，此处合法)；
  `payload_json` 从 12 阶段扩成 14；8 条旧 `settlement` todo
  `status='active'→'paused'`；新 6 条 `settlement-v2` todo 插入
  `session_todos` + `session_todo_deps` 补 4 条依赖（依赖 `payment` 完成）；
  新增 1 条 `decision`（"为什么单独 crate：编译期隔离 + 热 rollback"）。
  `session_state_item_events` 批量写 9 条 mutation（1 update plan +
  8 update todo + 6 insert todo + 1 insert decision）。
- **可见**：SSE `plan.version_bumped(7→8)` / `todo.paused×8` /
  `todo.inserted×6` / `decision.created`。Zones: `plan_todo` **700→1100**
  （plan skeleton + 6 新 active + 6 paused title refs），`anchor=400`,
  `recent_tail=1800`, `summary=900`，total≈5100，`dropped_count=2`
  (2 条 done 且冷的老 todo)。**⚠️ plan_todo=1100 已超出"800 保持"的合理预
  期**——默认 Budget Policy 只说 "hard cap" (L2389) 但无数字，详 F2。
  LLM 看不到：8 条 paused todo 的 description (平均 300 tokens/条, ~2400)。

### T31 · 深度 bug：三级检索全开

- **用户**：「CI 挂了，看这 traceback：`SqlxError(PoolTimedOut)` 跟上次
  `inventory` 调通前那个 pool 配置报错一样，你翻回去看当时怎么调的。」
- **服务端**：G3 state machine 三级全开。**Stage 1 structured**
  `category='error_state' AND payload_json->>'$.kind'='pool_timeout'`
  → 0 行（T13 那条 error_state 已被 T8 compaction 归档至 `status='resolved'`
  ，L1a 归档行仍可查但默认 active filter 跳过）→ emit
  `retrieval.structured_empty` (L1940)。**Stage 2 FTS**
  `MATCH(preview_text) AGAINST('PoolTimedOut pool')` 对
  `session_history_chunks` 查，命中 4 条 chunk（含 T13 tool_output_preview），
  耗时 180ms (target <200ms ✓)，但 rerank 后 top-1 置信度 0.61 <
  阈值 0.75 → 继续升级。**Stage 3 vector**：1 次 embedding 查询 (G3 SLA
  "1 vector query per turn" L1942)，取 top-3，rerank(recency+importance)
  锁定 T13 chunk，耗 430ms；`content_hash` 与 `index_hash` 一致（L1982–
  1985 未触发 stale），raw_ref 加载 2300 tokens 原始 cargo 输出。
  manifest `reason='history_recall_vector'` (G1 L1509)。
- **可见**：SSE `retrieval.structured_empty` / `retrieval.fts_degraded` /
  `retrieval.vector_hit(distance=0.18)` / `retrieval.raw_loaded`。Zones:
  `retrieved_facts` **暴涨 3800** / `plan_todo=1100` / `recent_tail=1800`
  / `tool_previews=800`，total≈8700。`dropped_count=6`（vector 其余 top-K
  被剪）。**⚠️ T31 合计 8700，是本场景峰值第二，但 plan_todo 仍 1100 未
  收窄**——见 F2。

### T38 · 第二次 compaction + 归档陈旧 tool_ref

- **用户**：「OK bug 修完，继续 `notification`。你先把之前那些 cargo
  输出清理下，别占着。」
- **服务端**：compaction 触发前再查 `active_runs=0` (G2 inv #4) ✓。
  写新 `summary(scope='session', version=2)` 覆盖 T9–T30 叙述
  (token_estimate=1100)；**旧 summary v1 标 `status='archived'` 不删**
  (G2 inv #8 L2584–2590 ✓)。32 条旧 `tool_ref` (cargo check preview)
  批量 `status='active'→'archived'`、`token_estimate=0`；对应
  `session_artifacts` 32 个 `ref_status='compacted'`（G9 L1685–1692 的
  `status` + `referenced_by_manifest_count` 由 compaction 递减）。**关键**:
  这些 tool_ref 归档不影响 `session_transcript_items` 本身的行——
  transcript 里 `is_compacted=TRUE` 标记保持，行仍可 scroll 出来（L1793
  "UI can hide or show compacted/deleted rows based on mode"）。
  `plan_state.version` 不 bump (G2 inv #3 ✓)；所有 active decision/
  error_state 保留 (G2 inv #2 ✓)。
- **可见**：SSE `compaction.started(version=v1.1)` / `tool_ref.archived×32` /
  `summary.replaced(v1→v2)`。Zones: `summary=1100` / `plan_todo=1000`
  (settlement-v2 部分已 done，收窄) / `recent_tail=1600` / `retrieved_
  facts=500` / `tool_previews=400`，total≈5000，`dropped_count=32`。
  详细 token 账见 §7。LLM 看不到：32 条 archived tool_ref preview
  (~18k tokens) + summary v1 (~900 tokens)。

### T45 · 大型工具输出入库（cargo test 2.1 MB）

- **用户**：「跑全量 integration test 看是不是都绿了。」
- **服务端**：agent 调 `run_cargo cargo test --workspace`，输出 2.1 MB
  (~520k tokens)，含 47 失败。`session_tool_outputs` 写 1 行：
  `tool_name='cargo'`, `status='error'`, `byte_size=2_214_912`, 写
  `artifact_ref='artifact:cargo-test-T45'`, `content_hash='sha256:...'`,
  `token_estimate=480`。**⚠️ G8 preview_template 表 L1648–1656 无
  `cargo` 条目** → 走 L1621–1622 fallback 400 字符 preview,
  `preview_status='fallback'`（见 §5 新 gap 候选 G20）。新增 1 条
  `error_state`（"47 failing tests, missing tower::Service impl"）+
  1 条 `session_history_chunks(chunk_type='error')` 供后续检索。G9
  session_artifacts 新增 1 行，`retention_policy='default'`,
  `access_scope='delegation'`（即便本场景无 delegation，默认 scope 也合理）。
  manifest `reason='large_tool_output_gated'` (G1 L1510)。
- **可见**：SSE `tool.output.artifact_gated(artifact_id=..., bytes=2.1M)` /
  `error_state.inserted(cluster=47_tests)`。Zones: `tool_previews=480`
  (preview_text + artifact_ref 链接) / `session_anchor=520`（+120 error_state
  补丁） / `recent_tail=1800`，total≈5100，`dropped_count=1`，
  `drop_reason='exceeded_tool_preview_cap'`。LLM 看不到：原始 2.1 MB
  cargo 输出（用户可在 UI "expand artifact" 显式加载）。

### T52 · 老 decision 检索（纯 structured 命中）

- **用户**：「等下，我们之前为什么选了 `tokio::select!` 不用 `futures::
  select!`？记得有个原因跟 cancellation 有关。」
- **服务端**：G3 state machine。**Stage 1 structured** 直接
  `category='decision' AND status='active' AND (summary_text LIKE '%select%'
  OR payload_json->>'$.title' LIKE '%select%')`（注意：非 production 模
  式允许 summary_text 索引列 LIKE；production 禁 payload_json LIKE 过滤，
  这里假设 title 已投影到 summary_text 列 L1830 附近索引 ✓）实测 22ms
  (<50ms ✓)，命中 T8 写入的 `decision:runtime-tokio-select` 记录，
  payload_json rationale ("futures::select! 不保证 cancel-safety 在
  stream::StreamExt poll 之间语义; 无 biased mode") token_estimate=210。
  **不升级 FTS/vector** → 这是 A10 分层的价值。manifest `reason=
  'history_recall_structured'`，`dropped_count=0`。
- **可见**：SSE `retrieval.structured_hit(elapsed_ms=22)`；UI 引用 T8
  决策锚点。Zones: `retrieved_facts` 500→710 (+210)，其余不变，
  total≈5200。LLM 看不到：T8 的 3 种备选方案逐条比较的 3.5k tokens 思考过程。

### T58 · 第三次 compaction

- **用户**：「`notification` 和 `settlement-v2` 都过了，最后一步 workspace
  一起跑 release build，你先准备。」
- **服务端**：G2 inv #4 active_runs 检查 ✓。写 `summary v3`
  (token_estimate=1300) 覆盖 T31–T57 叙述；v2 标 `archived` 不删
  (G2 inv #8 ✓)。`plan_state.version` 10→11（⚠️ 注意：本 turn 同时做
  compaction + 切 phase `workspace_release`，**此处 G2 inv #3 有歧义**
  —— L2543–2551 写"provenance_event_id 为 compaction_event 时 version
  不能 bump"，但若 compaction 和 user turn 用不同 provenance_event_id,
  就不违反。实现需明确 turn 内两次事件分开。详 §5 新 gap 候选）。
  52 条 done todo 保留在 DB（供 T60 检索），但 3 条 active todo 进 prompt。
  G2 inv #1/2/5/6/7/8 全过。
- **可见**：SSE `compaction.done(v1.2)` / `plan.phase_switched(→release)`。
  Zones: `summary=1300` / `plan_todo=600`（3 active + 2 依赖）/
  `session_anchor=400` / `recent_tail=1600` / `retrieved_facts=400` /
  `tool_previews=300` / `workspace=200`，total≈4800, `dropped_count=47`。
  LLM 看不到：52 条 done todo description、T32–T57 详细 tool 序列。

### T60 · 交付总结

- **用户**：「都过了，给我出一份迁移交付报告，列所有 decision、遗留风险、
  回滚路径，直接贴 PR description。」
- **服务端**：**关键判断：是否派 sub-agent？** 按 G4 L1365–1368，子会话
  只在"user-visible tab / long-lived resume / separate permissions /
  cross-session reuse" 时才创建。T60 交付属于"主 session 内一次性聚合"，
  **不满足**任何派生条件 → **主 session 直接批量检索**，不启 delegation。
  SQL: `SELECT * FROM session_state_items WHERE session_id=?
  AND category IN ('decision','error_state','plan_state')
  AND (status='active' OR category='plan_state') ORDER BY
  category, updated_at`（走 `idx_state_items_session_zone` 类索引 ✓），
  一次性拉 18 decision + 3 active error + 14 plan phase 共 35 条；
  agent 拼 Markdown 报告写 `session_artifacts(artifact_ref='artifact:
  delivery-report-v1', retention_policy='project_long_term')`。
  manifest `reason='final_delivery_summary'` (G1 L1521)。
- **可见**：SSE `artifact.created(delivery-report-v1)` /
  `final.delivered`。Zones: `retrieved_facts` **破例扩到 4000**
  (默认 Budget Policy L2394 "Top-K facts" 未给数字上限，此处一次性加
  载全 decision 原文合理但超出常规 700) / `plan_todo=300` /
  `recent_tail=1400` / `session_anchor=600`，total≈7200。LLM 看不到：
  60 轮完整 transcript (15 MB)。

## §4 发现的别扭点（Frictions）

**F1 · compaction turn 边界与语义事件的 provenance 歧义**（notable）
- Ref: T58；v0.2 §Compaction Invariants L2543–2551
- Observation: T58 用户单轮消息既触发 compaction 又触发 phase 切换。
  G2 inv #3 用 `provenance_event_id=:compaction_event_id` 绑定禁 version
  bump，但同一 turn 若 agent 先写 phase 切换事件（provenance=user_turn）、
  再跑 compaction（provenance=compaction_event），SQL 不会报警。
  然而实现者极可能共用一个 event_id，误触发 assertion fail。
- Why: 正确性风险 + 实现歧义。Severity: `notable`。

**F2 · `plan_todo` zone 在大窗口 Budget Policy 下无硬数字上限**（notable）
- Ref: T24/T31 plan_todo=1100；§Budget Policy v1 L2389–2390
- Observation: 默认模板只写 "Always include; keep under a hard cap"，
  数字全无。G10 small-window 模板把 plan_todo 限到 400，而大窗口无对应
  数值。S01 用户预期 plan_todo ≤800 被打破（实测 700→1100→1000→600）。
- Why: token 预算不可预测 / UX 上用户感觉"plan 展开刷屏"。Severity: `notable`。

**F3 · default Budget Policy 无 `budget_template_id`**（minor）
- Ref: T1 manifest；L2436–2438 仅规定小窗口 `budget_v1_8k` 必填
- Observation: 大窗口场景下 `context_manifests.budget_template_id=NULL`
  是合规的，但 G1 reason + budget_template 联合聚合（"某 reason 在哪种
  预算下触发多少 drop"）就无法按模板分组。
- Why: 可观测性退化。Severity: `minor`（不阻塞 S01 跑通）。

**F4 · `cargo` 系工具在 preview_template_registry 缺席**（notable）
- Ref: T45 cargo test 2.1 MB；L1648–1656 baseline 无 cargo entry
- Observation: T45 fallback 到 400 字符，但 47 failing test 的 pattern
  ("tower::Service impl missing") 可能因 preview 截断丢失信号。
  `preview_status='fallback'` 留了痕迹但无人补模板。
- Why: 大工具输出检索信号损失，长期会放大 F5 的 error_state 漏抓。Severity: `notable`。

**F5 · T38 归档 tool_ref 后 `session_transcript_items` 的 UI 语义歧义**（minor）
- Ref: T38；L1774 `is_compacted` + L1793 "UI can hide or show"
- Observation: T38 归档的 32 tool_ref 对应 `session_transcript_items`
  可能 `is_compacted=TRUE`（若 T8 已标记）或 `FALSE`（如 T15–T30 新产生的
  preview）。归档 state_item 时**未规定**是否同步回改 transcript_item 的
  `is_compacted` 标记。两处数据源容易不一致。
- Why: UI scroll-up 展示"已归档"badge 会混乱。Severity: `minor`。

**F6 · T60 `retrieved_facts=4000` 破例扩展无契约依据**（notable）
- Ref: T60；Budget Policy L2394
- Observation: 默认模板对 retrieved_facts "Top-K by relevance" 未给数字。
  T60 合理需要一次性 4000 tokens 加载全部 decision，但这是"final_delivery
  _summary" reason 的隐性特权，v0.2 无 per-reason zone 豁免清单。
- Why: 实现者可能拒绝/截断，或滥用此特权于非交付 turn。Severity: `notable`。

## §5 发现的新 gap 候选

- **title**: Compaction turn 内多事件共用 provenance_event_id 的裁决规则缺失
- **triggered_by_turn**: T58
- **related_v0.2_section**: §Compaction Invariants L2543–2551 (G2 inv #3)
- **severity**: medium
- **rationale**: G2 assertion 按 `provenance_event_id` 匹配 compaction
  事件。若一次 user turn 同时触发 compaction + 语义状态变更（plan phase
  切换、decision 写入），实现若共用 event_id，正常语义变更会误命中
  assertion；若分发两个 event_id，G2 又不约束"user turn event 不能在
  compaction 运行期间 bump plan_state"——这不是理论威胁，T58 就是真实触发点。
- **proposed_fix**: 规定 compaction 必须拥有独立 `provenance_event_id`
  与 agent_run_events 绑定，且 compaction event 执行期间（start→end）
  任何 session_state_item_events 必须属于 compaction_event_id，禁止穿插。

- **title**: `preview_template_registry` 未覆盖 Rust 工具链（cargo/rustc/clippy）
- **triggered_by_turn**: T45
- **related_v0.2_section**: §Preview Template Registry L1648–1656
- **severity**: medium
- **rationale**: baseline 只列 `pg_dump`/`slow_query_analyzer`/`fetch_url`/
  `parse_pdf`/`llm_extract_findings`/`benchmark_slice`。Rust 系的
  cargo test/check/clippy 是本场景 50% 以上工具调用量，fallback 400B
  无法保住 "failing test name + file:line" 这种关键 first-class 信号，
  error_state 提取质量下降。S06 会用到 psql 类似但已覆盖，S01 Rust 侧
  是 blind spot。
- **proposed_fix**: 在 baseline 表加 `cargo`（max_preview=2000B, first_class_
  columns: `failing_tests[]`, `warning_count`, `crate_name`, chunk_type=
  `error`）、`rustc`/`clippy`（800B, `error_code`, `file`, `line`）。

- **title**: 默认大窗口 Budget Policy 的数字上限与 `budget_template_id` 必填性
- **triggered_by_turn**: T24 / T60
- **related_v0.2_section**: §Budget Policy v1 L2381–2406
- **severity**: low
- **rationale**: 目前只有 G10 `budget_v1_8k` 给了 zone 数字上限；大
  窗口默认策略全靠 "hard cap" 字面描述。S01 T24 plan_todo=1100 / T60
  retrieved_facts=4000 都超出读者的直觉期望，但无契约可查。
- **proposed_fix**: 新增 `budget_v1_200k` 命名模板（anchor=600/plan_todo
  =1200/recent_tail=2000/summary=1500/retrieved_facts=2000+final_override/
  previews=800），并强制所有 manifest 必填 `budget_template_id`，不允许 NULL。

## §6 Gap 组合效应检验

**组合 A · G2 + G1 + G17**（compaction 契约 × manifest reason 枚举 × hash 规范化）
- 运行时可能发生的坑：T8/T38/T58 三次 compaction 每次都要（1）查 active_runs
  （G2 #4）、（2）写 reason='post_compaction'（G1 L1506）、（3）对
  `summary` payload 算 content_hash（G17 L2163–2194 规定规范化）。
  若三者提交顺序错位（compaction_event 先写但 summary hash 后算，中间
  crash），manifest 行指向未写完的 summary_id，重启后 G2 #6 assert
  `post_compaction_manifest_count=1` 仍过，但 summary payload 缺失。
- v0.2 是否覆盖: **partial**。G2 规定 manifest 必写、G17 规定 hash 规范，
  但没有事务顺序契约。
- 建议: 在 §Compaction 增 "compaction_event 开始→summary upsert→
  manifest upsert" 需在单个 DB 事务，或用 outbox pattern 保证最终一致。

**组合 B · G3 + G10 + 新 gap "budget_v1_200k"**（三级检索 × 小窗口预算 × 大窗口预算）
- 运行时可能发生的坑：T31 三级全开加载 3800 tokens 到 retrieved_facts，
  大窗口下合理。但若同一场景用 8k 小窗口 (G10)，L2433–2435 规定
  vector 只在 ≥1000 retrieved 空间时才允许 — T31 `plan_todo=1100` 已
  占掉 G10 cap 400 的三倍，根本无法进入 vector，assistant 就必须降级
  回答 "未找到" (G3 L1946–1947)。v0.2 对"大窗口配置下 plan_todo 膨胀
  到 1100 是否合法"没有说法。
- v0.2 是否覆盖: **no**。G10 只约束小窗口，大窗口无数字上限。
- 建议: 落地 §5 第 3 条新 gap 的 `budget_v1_200k`，并写明大小窗口切换
  时哪些 zone 必须重新压缩（plan_todo 从 1100 收窄到 400 的策略）。

**组合 C · G2 + G9 + F5**（compaction 不物理删 × artifact 反向计数 × transcript 标记）
- 运行时可能发生的坑：T38 归档 32 tool_ref → `session_state_items.status=
  'archived'`（G2 #2 允许，tool_ref 不在白名单，合法）→ 触发 G9
  L1689 `referenced_by_state_items_count` 减 32 →
  `session_artifacts.retention_until` 重算可能提前到期 → 后续 T52 若
  检索触达这些 artifact 会返回 `status='expired'` 的 tombstone (L1735–1739)。
  但同时 `session_transcript_items` 的 `is_compacted` 标志**未被同步**
  （见 F5），UI scroll-up 展示旧 cargo 输出时，既不是"已归档"（transcript
  没这 flag），也不是"已过期"（artifact 已 tombstone）——三处元数据冲突。
- v0.2 是否覆盖: **no**。G2/G9/transcript 三者各自完备，但跨表联动未规定。
- 建议: 新增一条 "archival propagation rule"：state_item archival
  必须同 transcript_item.is_compacted 联动，artifact 的
  retention recompute 需以活跃 state_item + 活跃 transcript 双计数之
  **最大值**为准，避免 premature expiration。

**组合 D · G4 + G1 + G6 (user-scope) · T60 交付不派 sub-agent 的判定链**
- 运行时可能发生的坑：T60 用户要"交付报告"，直觉上像个"研究型 sub-task"，
  但 G4 L1365–1368 明确"子会话只在 user-visible tab / long-lived resume /
  separate permissions / cross-session reuse 时创建"。T60 四条全不满足 →
  主 session 自己跑 SQL 聚合 18 decision + 3 error + 14 phase，组装
  Markdown 写 artifact。G1 `final_delivery_summary` (L1521) 覆盖此 reason。
  唯一坑：若实现者对"何时派 delegation"的判定函数用启发式（如"聚合 >10
  条结构化事实时派 child"），会错误产生一个 child run，`session_delegations`
  多一行；后续 T60+ 用户若开第二个 session 想回看这份报告，**需要 G6
  scope='project'** 才能读跨 session artifact（retention=project_long_term
  L1705 提供了 artifact 层保留，但 decision 本身 scope='session' 默认不跨）。
- v0.2 是否覆盖: **partial**。G4 条件是描述性的，缺"决策树伪代码"；G1
  的 `final_delivery_summary` 只管 manifest reason，不约束是否派 child。
- 建议: 在 §Delegation 增 "Delegation Decision Algorithm"（伪码 5 行）；
  明确 final_delivery_summary 默认 parent-only，除非 session 本身 project-
  scoped 且用户显式要求跨 session 可读。

## §7 T38 compaction 前后 manifest token 账

T38 是本场景最密集的 "清理 + 继续" turn。前一轮 T37 正常工作回合基线：

| Zone | T37 (pre-compact baseline) | 计算 |
| --- | ---: | --- |
| system_static | 1200 | 固定 prefix |
| tool_schemas | 1800 | Rust+shell+file 工具 schema 剪枝后 |
| skills | 900 | rust-migration + axum-handler-analysis + cargo-test-triage |
| session_anchor | 400 | 迁移目标 + 活跃 error_state 摘要 |
| plan_todo | 1050 | 14 phases skeleton + 9 active + 4 paused title refs |
| recent_tail | 1800 | T34–T37 四轮 user/assistant + edit diff |
| summary | 900 | v1（T8 产物，已较陈旧） |
| retrieved_facts | 600 | T31 遗留的 PoolTimedOut 片段 + 2 条 decision |
| tool_previews | 1300 | 累计 32 条旧 cargo check preview（每条 ~40 tokens） |
| workspace | 200 | reachability_probe ok |
| **total** | **10150** | 已逼近但未超 "合理上限"（启发式 12k） |

T38 turn 执行（compaction）后的新基线：

| Zone | T38 (post-compact) | Δ | 机制 |
| --- | ---: | ---: | --- |
| system_static | 1200 | 0 | 不动 |
| tool_schemas | 1800 | 0 | 不动 |
| skills | 900 | 0 | 不动 |
| session_anchor | 400 | 0 | G2 #2 保护 anchor 行 |
| plan_todo | 1000 | -50 | settlement-v2 6 条 done 合并计数（title 保留 ref） |
| recent_tail | 1600 | -200 | 压缩后新 tail 窗口 |
| summary | 1100 | +200 | v2 覆盖 v1（v1 token_estimate 重估为 0，仍在 DB） |
| retrieved_facts | 500 | -100 | 旧 vector-hit chunk 自然失活（非 compaction 主动删） |
| tool_previews | 400 | **-900** | 32 条归档 tool_ref 的 token_estimate=0，只剩 8 条活跃 preview |
| workspace | 200 | 0 | 不动 |
| **total** | **9100** | **-1050** | 净省 ~10% |

关键账目：
- **未进 prompt 但留在 DB**：32 条 archived tool_ref 原始 preview
  (~18000 tokens)、summary v1 (~900 tokens)、T9–T30 期间 ~80 条
  transcript_item（item_seq 保留）、T13 pool_timeout 原始 cargo 输出 raw_ref。
- **G2 验证**：assertion #1–#8 同步跑，forbidden_state_mutations=0，
  forbidden_plan_version_bumps=0，active_runs=0，
  archived_todos_without_provenance=0，post_compaction_manifest_count=1，
  durable_facts_without_provenance=0，deleted_summaries=0 ✓。
- **G9 反向引用**：32 个 artifact 的 `referenced_by_manifest_count`
  从各自 1~3 减至 0，进入 `expiring` 候选池 (T-7 GC preflight)；
  但 retention_policy='default' 且 created_at 才 2–3 天，不会立即 GC。

## §8 最终判断

- **本场景下 v0.2 是否可实现**: ✅（主线可跑通，无 blocking gap）
- **顺畅度**: ⚠️ 有 6 处 notable/minor friction（F1–F6），其中 F1/F2/F4
  在实现阶段必须显式决策，否则会埋 bug；F3/F5/F6 不影响跑通但降低可观测性。
- **新 gap 候选**: 3 条中 2 条 medium / 1 条 low，均为**完善性**缺陷，
  不阻塞 60 轮马拉松跑完；建议按顺序补 §5 第 2 条（cargo preview template）
  → 第 1 条（compaction provenance 契约）→ 第 3 条（大窗口预算模板）。
- **一句话总结**：v0.2 在 S01 这条"长会话 + 3 次 compaction + 3 次
  三级检索 + 1 次大工具输出 + 主 session 自聚合交付"主轴上机制完整，
  G1/G2/G3/G4 四条核心契约都能被真实压到并产生可验证的 SQL/event 痕迹；
  但**大窗口 Budget Policy 的数字化缺失**和 **compaction 与语义事件的
  provenance 边界**是两个需要在 Phase 3–4 落地前收紧的实现契约。
实现优先级：cargo preview template（T45 信号损失）> compaction
provenance 契约（T58 正确性）> 大窗口 budget_v1_200k（T24/T60 观测性）。
