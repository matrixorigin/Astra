# WALKTHROUGH-S08 — DBA 老陈的 MySQL→MatrixOne 迁前审计

> Status: v0.2 运行推演 (基于 web-agent-session-state.md v0.2, 3040 行)
> Scenario: [S08-giant-tool-output.md](S08-giant-tool-output.md) §1–§3
> Baseline gaps: G1–G19 resolved (GAP-TRACKER.md)
> Mode: 端到端轨迹推演 · 非评审 · 以"真的跑一遍"视角

## §1 场景标识与 v0.2 映射

一句话场景：DBA 老陈驱动 astra web agent 做迁前审计，一天内同步扫 pg_dump
3GB schema、1000 个 SQL 文件、800MB slowlog，累计 ~4GB tool 输出，prompt
永远不允许任一原始 raw 进入；次日还要亚秒级回查「第 3 条慢查询为什么慢」
与「昨天那个 FK 报错在哪个文件」。被测 DAUT: **A3 / A9 / A10**。

本场景命中的 v0.2 关键章节:

- Preview Template Registry @ L1615–1669（G8），pg_dump baseline `max_preview=
  1200 bytes` / `artifact_text` chunk / `content_hash` first-class column；
  slow_query_analyzer baseline `max_preview=1600 bytes` / `tool_output_preview`
- Artifact Retention & Access Scope @ L1671–1749（G9），`status` ∈ active/
  expiring/archived_cold/expired，T-7 GC 活跃扫描，`referenced_by_manifest_
  count` 三计数器，presigned URL 下载契约 L1741–1742
- Content Hash Normalization @ L2163–2194（G17），pg_dump_v1 剔除 dump
  timestamp/server version/session id；slow_query_v1 时间戳分桶
- Retrieval SLA State Machine @ L1930–1987（G3），Structured <50ms→FTS
  <200ms→Vector <500ms→Raw <250ms（hot）/ cold 允许超
- session_tool_outputs schema @ L1560–1610（含 `idx_tool_outputs_tool_created`
  L1590、`idx_tool_outputs_status_created` L1592、`normalize_version` L1568）

## §2 运行前提

Schema 初值：`agent_sessions` 1 行（`project_id='audit.migrate.mysql.q3'`、
`project_retention_policy='project_long_term'` G9 L1697–1699）；`agent_runs`
每回合 1 行，终态 12 行；`preview_template_registry` 已 seed `pg_dump` /
`slow_query_analyzer` 2 条基线（G8 L1647–1656），`sql_compat_scan` ⚠️ 未列
baseline → fallback 400B（见 §4 F4 / §5）；`context_manifest_reason_types`
enum 已 seed，本场景用 `initial_turn` / `normal_turn` / `large_tool_output_
gated` L1510 / `history_recall_structured` L1507 / `history_recall_fts`
L1508 / `final_delivery_summary` L1521 共 6 种。`session_transcript_items`
/ `session_tool_outputs` / `session_artifacts` / `session_history_chunks`
均空；`session_delegations` 始终空（单 session 审计，不派子 agent）。

Workspace：cloud（DB 凭据仅 cloud 可用，edge 工具被 skill selector 过滤，
`workspace_state.reachability_probe='ok'` G11），工具集：`pg_dump_schema` /
`pg_schema_structurize` / `sql_compat_scan` / `slow_query_analyzer` /
`slow_query_explain` / `render_migration_risk_report` / `fts_scan` +
renderer。预期 context window：**Sonnet 4.5 / 200k** 大窗口，默认 Budget
Policy（L2381–2406），`tool_previews` 目标带 500–1500 tokens L593。

## §3 端到端运行轨迹（12 回合）

### T1 · 09:10 「做迁前审计」

- **用户**：「下季度主库 MySQL 迁 MatrixOne，迁前做审计：schema 整库导一遍、
  1000+ 业务 SQL 脚本扫不兼容、近 7 天 slowlog 出 TOP N，完了出份报告。」
- **服务端**：`agent_runs` 新建 run_1 (trigger=`user_message`)；
  `session_state_items` 插 `anchor`（MySQL→MatrixOne 迁前审计）/
  `workspace_state(cloud)` / `plan_state v0`；`context_manifests` 1 行
  `reason='initial_turn'` (G1 L1504)，`total_estimated_tokens≈3240`。
- **可见**：SSE `session.opened`。Zones: `system_static=1200` /
  `tool_schemas=1800`（7 类 schema，edge 系被裁）/ `session_anchor=240` /
  `plan_todo=0` / `tool_previews=0`。LLM 看不到：老陈的 Oracle→MySQL 旧伤。

### T2 · 09:15 agent 拟计划 + 5 条 todo

- **用户**：（隐式同意）
- **服务端**：`plan_state` v0→v1 (phase='audit_v1')；`session_todos` 批量
  upsert 5 条（pgdump / scan_1000 / slowlog / report / decision_log），
  均 `status='active'`, `source='agent'`；`session_todo_deps` 4 条；新增
  `decision(key='audit.scope.v1')`；`context_manifests`
  `reason='normal_turn'` L1505, `dropped_count=0`。
- **可见**：SSE `plan.seeded(5)` / `todo.batch_inserted(5)`。Zones：
  `plan_todo=320` / `session_anchor=260` / `recent_tail=420`，总 ≈4000。

### T3 · 09:20 pg_dump schema-only 3GB ⚠️ 关键验证

- **用户**：「先跑 pg_dump，schema only，整库。」
- **服务端**：`pg_dump_schema` tool runner 流式写 OSS（runner→OSS 直连，
  不过 API server RSS）；240s 完成，`byte_size=3,221,225,472`。
  `session_artifacts` 写 1 行：`artifact_id='artifact:pgdump-prod-20260601-
  schema'`, `artifact_kind='tool_output_raw'`, `byte_size=3221225472`,
  `normalize_version='pg_dump_v1'` (G17 L2184 — 剔除 `-- Dumped on ...`/
  `-- Dumped by pg_dump version 16.1`/session ids), `content_hash=sha256
  (normalized)`, `retention_policy='project_long_term'` L1705,
  `retention_until=now()+90d`, `access_scope='delegation'` L1718, `status=
  'active'` L1710, `referenced_by_manifest_count=1`（T3 manifest 引用）。
  `session_tool_outputs` 写 1 行：`tool_name='pg_dump_schema'`,
  `preview_template_version='v1'` (G8 L1567), `normalize_version=
  'pg_dump_v1'` L1568, `status='ok'`, `byte_size=3221225472`,
  `preview_text` **800 字符**（G8 baseline `max_preview=1200 bytes` L1651
  留余量）= 头部 SET 语句 + CREATE SCHEMA + 尾部统计 "...truncated at 800
  chars / 3221225472 bytes, stats: tables=482 views=37 FK=1204 indexes=3890;
  full in artifact:pgdump-prod-20260601-schema"。
- **可见**：SSE `tool.result(preview_only)` / `artifact.created`。Zones：
  `tool_previews=240` / `plan_todo=320` / `recent_tail=600`，总 ≈4160。
  `context_manifests.reason='large_tool_output_gated'` (G1 L1510 / default
  zone=tool_previews ✓)。LLM 看不到：3GB CREATE TABLE/INDEX 全文、全部 FK
  定义、全部 view 定义 — 仅在 OSS 对象里。

### T4 · 09:25 pg_schema_structurize 派生 8MB JSON

- **用户**：（隐式）agent 要做结构抽取供后续报告使用。
- **服务端**：`pg_schema_structurize` 在 tool runner 侧**直接消费**
  `artifact:pgdump-prod-20260601-schema`（跳过 API server），输出 8MB 结
  构化 JSON。`session_artifacts` 第 2 行：`artifact:pgdump-prod-20260601-
  struct-json`, `byte_size=8388608`, `derived_from_artifact_id='artifact:
  pgdump-prod-20260601-schema'` (G9 L1688)，`normalize_version=
  'pgstruct_v1'`（⚠️ 非 G17 baseline → 见 §4 F3）。raw dump 的
  `referenced_by_manifest_count` 因派生链不变（G9 #4 L1728 "GC 派生前先判
  raw"）。`session_tool_outputs` 再 1 行 preview 900 字符（表数/索引直方图/
  FK 跨 schema 比例/高风险模式 ENUM/AUTO_INCREMENT/触发器）。
- **可见**：SSE `tool.result`。Zones: `tool_previews=520`（240+280 累加）
  / 其余基本不变，总 ≈4460。**不变量**：context builder SELECT 带
  `WHERE byte_size < 16384` 兜底，超 16KB 的 tool 输出强制走 preview 分支。

### T5 · 09:40 1000 SQL 扫描 ⚠️ N+1 关键验证

- **用户**：「repo 下 `/apps/*/sql/*.sql` 全扫一遍，MySQL 方言、用 MatrixOne 语法。」
- **服务端**：`sql_compat_scan` fan-out 1000 次（runner 内并行），返回 1 聚
  合 + 1000 per-file。**写入策略**：`session_tool_outputs` 批量 insert
  **1001 行**，走单事务 + 多值 `INSERT INTO ... VALUES (...), (...)` 拆成
  10 批 × 100 行。**⚠️ v0.2 未强制 batch**（见 §4 F1）—— 若实现者按 per-
  file 一次 insert，1000 × (~3ms RT) ≈ 3s 拖爆 T5 延迟。per-file 行
  `preview_text` 80 字符 `"path/payments.sql: 3 issues [ROW_FORMAT,
  UTF8MB3, FT_PARSER]"`, `status` ∈ `ok`/`warn`/`fail`（⚠️ 枚举需
  CHECK 约束 §7 F5）；聚合行 `preview_text` 1200 字符 "scanned=1000
  fail=137 warn=214 ok=649 top_types: ROW_FORMAT_COMPRESSED=82 ..."。
  per-file 详报合并写 1 个 artifact `artifact:sql-scan-detail-20260601`
  `byte_size≈56MB`，`normalize_version='sqlscan_v1'`（⚠️ G17 未列）。
- **可见**：SSE `tool.result(rows=1001)` / `artifact.created`。Zones:
  `tool_previews=480`（**只引用聚合行** preview，per-file 不进 prompt
  — 若 1000×80≈20k tokens 进 prompt 直接爆）/ `plan_todo=360` / `recent_
  tail=900`，总 ≈4500。LLM 看不到：56MB detail JSON、1000 条 per-file。

### T6 · 09:50 「就列出不兼容的」⚠️ structured filter 关键验证

- **用户**：「agent 废话少点，就列出 fail 的文件名 + 不兼容点，别给我贴全文。」
- **服务端**：agent 不调工具，发 **Tier-1 structured filter**：`SELECT
  output_id, title, preview_text FROM session_tool_outputs WHERE session_
  id=? AND tool_name='sql_compat_scan' AND status='fail' ORDER BY created_
  at ASC`，命中 `idx_tool_outputs_status_created` (L1592 `session_id,
  status, created_at`) — 注意 `idx_tool_outputs_tool_created` L1590 是
  (tool_name, created_at) 不含 status，会 scan 1001 行再过滤；**优先
  status 复合索引**。返回 137 行 ~11KB。SLA: elapsed_ms ≈12 (<50ms G3
  L1940 ✓)。`session_state_items` 插 `tool_ref(key='scan.fail.cursor.v1')`
  记住 137 数 + 检索 hash。
- **可见**：SSE `retrieval.structured_hit(rows=137, ms=12)`；UI 列 137 行
  卡片。Zones: `tool_previews=1100`（渲染前 20 条 + 类型直方图，硬撞
  1500 cap 前主动降档）/ `plan_todo=360`，总 ≈4800。`context_manifests.
  reason='history_recall_structured'` (G1 L1507 default zone=retrieved_
  facts — ⚠️ 但此处走 tool_previews，reason/zone 不一致，见 §4 F2)。

### T7 · 10:15 slowlog 800MB

- **用户**：「近 7 天 slowlog 拉下来，TOP 50 慢查询。」
- **服务端**：`slow_query_analyzer` 流 OSS 消费 `byte_size=838,860,800`，
  runner 内完成解析/fingerprint/TOP50 digest。2 artifacts: (a) raw log
  `artifact:slowlog-raw-20260525-20260601` `byte_size=838860800`,
  `normalize_version=NULL`（⚠️ 有意不规范化——日志本就时间序列，但 G17
  未显式允许 NULL，见 §5 新 gap）, `retention_policy='default'`,
  `retention_until=now()+30d`；(b) `artifact:slowlog-top50-20260601`
  `byte_size=412000`, `derived_from_artifact_id='slowlog-raw-...'` (G9
  L1688), `normalize_version='slow_query_v1'` (G17 L2185)。`session_tool_
  outputs` 1 行 preview 1000 字符 (G8 baseline 1600B L1652 留余量): TOP10
  digest 每条 ~80 字符 + 全局指标 + "full in artifact:slowlog-top50-20260601"。
- **可见**：SSE `tool.result`. Zones: `tool_previews` 已到 **1100(T6)+280
  (T7)=1380**，逼近 1500 上限。context builder 做 LRU 降级：T5 聚合 →
  240 / T6 → 580 / T7 = 660，最终 **1480**。`plan_todo=380`（T7 前置 todo
  done）/ `recent_tail=1000`，总 ≈5100。LLM 看不到：800MB raw、412KB
  digest 全文、T5 1000 per-file、T6 另 117 行。

### T8 · 10:40 「第 3 条慢查询为啥慢」⚠️ 按需 chunk 加载

- **用户**：「第 3 条那个 join 慢的，explain 给我看看，为啥慢。」
- **服务端**：**A10 三级检索**：(1) Tier-1 structured `WHERE session_id=?
  AND tool_name='slow_query_analyzer'` 命中 `idx_tool_outputs_tool_
  created` L1590, 拿到 T7 聚合行 `artifact_ref='slowlog-top50-20260601'`；
  (2) 按 `digest_rank=3` 读 digest artifact 的对应 chunk（412KB 可整块
  拉，含 10 条采样 + EXPLAIN plan + 涉及表/索引）；(3) **按需 byte-range
  GET** raw slowlog artifact 的 offset 区间 ~1.8MB（从 digest metadata 里
  的 `source_offsets`），绝**不**加载 800MB。走 G3 Tier-4 Raw load
  `<250ms` hot L1943。runner 侧解析为 JSON ~22KB → runtime 渲染 preview
  800 字符 (G8 slow_query_analyzer baseline 1600B 复用)。`session_tool_
  outputs` 写 1 行 `tool_name='slow_query_explain'`，`event_id`=T8 turn
  event, `parent_output_id`=T7 聚合 output_id（延续链，⚠️ v0.2 schema
  L1560–1593 **无 parent_output_id 字段** —— 见 §5 新 gap）。
- **可见**：SSE `retrieval.structured_hit`/`raw_load(ms=186)`. Zones:
  LRU 再降级，T8 explain preview=400 / T7 聚合=180 / 其余保留，总 ≈5000。
  LLM 看不到：800MB 的 99.78%、TOP50 JSON 另 49 条。

### T9 · 11:10 「昨天 FK 报错在哪个文件」⚠️ FTS 关键验证

- **用户**：「等下——上午扫脚本的时候有个 foreign key 报错，是哪个文件来着？」
- **服务端**：`NextActionExtractor` 识别历史回查。(1) Tier-1 试 `WHERE
  tool_name='sql_compat_scan' AND status IN ('fail','warn')` 返回 137+214=
  351 行，**无法靠 status 精确定位** → emit `retrieval.structured_
  ambiguous` (G3 L1940 的 `bound_exceeded` 变体) → (2) **Tier-2 FTS**：
  `MATCH(preview_text) AGAINST ('+foreign +key' IN BOOLEAN MODE)`, SLA
  <200ms G3 L1941，返回 4 行 elapsed_ms≈78。⚠️ `session_tool_outputs.
  preview_text` 字段 FTS index v0.2 **未声明**（见 §4 F2 / §5），本场景
  依赖实现层补；G8 L1639–1642 的 `fts_field_weights` 是 template registry
  级别，不直接翻译成 DDL。append-only 写入不造成 bloat ✓。**不触发
  Tier-3 vector**（精确关键词无需语义近似 — 按 G10 L2433–2435 小窗口
  规则精神，本场景大窗口亦遵循"能便宜就便宜"）。
- **可见**：SSE `retrieval.fts_hit(rows=4, ms=78)`。Zones: `tool_previews`
  T8 状态 + 360 tokens（4 条 FTS 命中），总 ≈5120。`context_manifests.
  reason='history_recall_fts'` (G1 L1508) ✓。LLM 看不到：倒排索引、56MB
  scan_detail 任何字节。

### T10 · 11:40 合成迁移风险报告 ⚠️ 30 artifact_ref zone token

- **用户**：「上面这些齐了，出份迁移风险报告：schema + SQL 兼容性 + slowlog
  TOP 风险，分级。」
- **服务端**：`render_migration_risk_report`, input = 3 主 artifact_ref
  (pgdump-struct-json / sql-scan-detail / slowlog-top50) + **T8 explain
  派生 + T5 per-file 指向的 27 个高危文件 artifact 位置** = 30 `artifact_
  ref`。runner 在 cloud 侧批量 GET artifact 合成 markdown ~180KB →
  `session_artifacts` 写 `artifact:migration-risk-report-20260601-v1`,
  `byte_size=184320`, `derived_from_artifact_id=NULL` (多源派生无单一
  parent — ⚠️ G9 L1688 单值字段限制，见 §5), `provenance_refs` 需另存
  （v0.2 无此列 → §5 新 gap）。`session_tool_outputs` 写 1 行 preview 800
  字符。**关键**：30 个 `artifact_ref` 全部**只在 tool_previews zone 以
  ref 形式**渲染 → 每个 ref ~30 tokens × 30 = 900 tokens。超 `tool_
  previews` 1500 cap 风险 → context builder **分组聚合**：3 主 ref +
  "27 个 high-risk file refs 折叠为 1 个 group ref + inline count"，实际
  渲染 ~180 tokens。⚠️ v0.2 无"ref 聚合渲染"契约（见 §5 新 gap）。
- **可见**：SSE `artifact.created(report-v1)`. Zones: `tool_previews=580`
  (T8=180 + T10 报告=400) / `retrieved_facts=440`（2 decision） / `plan_
  todo=280`，总 ≈5000。`context_manifests.reason='final_delivery_
  summary'` (G1 L1521)。

### T11 · 11:55 下载报告

- **用户**：「把报告下下来，我发群里。」
- **服务端**：UI 调 `GET /api/artifacts/:id/download` → 返回 **302** 到
  OSS presigned URL TTL 5min（G9 L1741–1742 "Large artifact downloads
  should return a presigned object-store URL. The API server signs and
  audits; it should not proxy multi-GB object bytes."）。浏览器直连 OSS
  下 180KB。API server RSS 占用**文件数据 0 字节**。`agent_events` 1 条
  `artifact_downloaded` audit。`session_tool_outputs` 无新增。
- **可见**：浏览器下载完成。Zones: 与 T10 基本一致 + `recent_tail +120`
  记录下载事件，总 ≈5100。LLM 看不到：presigned URL 签名。

### T12 · 次日 09:00 刷新页面 ⚠️ 绝不重拉 3GB 关键验证

- **用户**：（无交互，浏览器重开 session 窗口）
- **服务端**：Web display history 模式（§MatrixOne Load Model L366 Warm
  tier）：(1) `SELECT item_seq, item_type, role, preview_text, payload_ref
  FROM session_transcript_items WHERE session_id=? AND is_deleted=FALSE
  ORDER BY item_seq DESC LIMIT 50`; (2) `SELECT output_id, tool_name,
  status, byte_size, preview_text, artifact_ref FROM session_tool_outputs
  WHERE session_id=? AND output_id IN (?)`, 命中 `idx_tool_outputs_
  session_created` L1589。两查询合计 ~3ms, 读 ~50 行 × 800B preview =
  40KB 传输。3GB pg_dump / 800MB slowlog / 56MB scan / 180KB 报告
  **一个字节不读**，OSS 零请求。`context_manifests` **无新增行**（不进
  LLM），`agent_events` 1 条 `session_opened_for_display`。**G9 T-7 活跃
  扫描在此验证**：3GB pg_dump `retention_until=now-89d+1d`（90d 策略，
  次日仍 ≈89d 剩余），未触发扫描；但若 90d 后老陈再刷新，G9 L1724 "At
  T-7 days mark expiring" + L1725–1726 重算 `referenced_by_manifest_count
  FROM context_manifest_items` — T3/T4/T7/T10 manifest 均仍引用，
  counter ≥4，L1727 "extend retention or migrate to cold storage" 生效，
  延期或迁冷存而非直接删。
- **可见**：first-paint <500ms（40KB 传输 + UI 渲染 200ms）。LLM 本回合
  完全不被调用。

## §4 发现的别扭点（Frictions）

**F1 · `session_tool_outputs` 批量写入契约缺失（blocking）**
- Ref: T5；v0.2 §4 Tool Result and Artifact References L1560–1610
- Observation: schema 定义 PK = `output_id`，无文档说明"fan-out 工具结果
  必须批量 insert"。T5 若实现者循环单 insert，MatrixOne 网络 RT ×1000
  直接把 T5 延迟拖到 3–5s。v0.2 L1886 只对 `session_history_chunks` 说
  "Vector embeddings should reference `chunk_id` and be batch inserted"，
  对 tool_outputs 未强制。
- Why: UX 明显卡顿 + DB connection pool 风险 + 正确性亦可能（部分 insert
  失败后只回滚了一部分 per-file，聚合 preview 却已写入 → state 不一致）。
- Severity: `blocking`（本场景 T5 就是主压测点）。

**F2 · `preview_text` FTS index v0.2 未声明 DDL（notable）**
- Ref: T9；v0.2 §4 session_tool_outputs L1571 `preview_text TEXT NULL`
- Observation: G3 L1941 "FTS over indexed preview text" 契约仅描述行为
  SLA，未声明"在 session_tool_outputs.preview_text 上建 FULLTEXT index"
  或"另建一张 text search table"。G8 L1639–1642 的 `fts_field_weights`
  是 template metadata，不翻译为 DDL。实现者可能漏建 → T9 退化为全表
  scan 351 行，虽可接受但"真正 800MB 索引表"时会失败。
- Severity: `notable`。

**F3 · `normalize_version` 基线未覆盖 `pg_schema_structurize` / `sql_
compat_scan` (notable)**
- Ref: T4/T5；v0.2 G17 L2180–2189 baseline 表
- Observation: baseline 列 `pg_dump` / `slow_query_analyzer` / `fetch_
  url` / `parse_pdf` / `SKILL.md`，本场景两个派生工具（`pg_schema_
  structurize` 产 8MB JSON / `sql_compat_scan` 产 56MB JSON）未列入 →
  实现者自由命名 `normalize_version='pgstruct_v1'` / `sqlscan_v1` 可运
  行，但**跨部署对齐性丢失**：A 环境 sqlscan_v1 = 剔时间戳，B 环境 sql
  scan_v1 = 不剔 → content_hash 差异 → 跨环境 dedup 失败。
- Severity: `notable`（本场景不阻塞，长线生态需补）。

**F4 · `sql_compat_scan` 无 preview_template baseline（notable）**
- Ref: T5；v0.2 G8 L1648–1656 baseline 表
- Observation: baseline 仅 `pg_dump` / `slow_query_analyzer` / `fetch_
  url` / `parse_pdf` / `llm_extract_findings` / `benchmark_slice` 6 条。
  S08 主压测工具 `sql_compat_scan` 未列 → fallback 400B L1622。per-file
  80 字符还够，但**聚合行 1200 字符**实际超 fallback 3 倍，L1622 规则
  会截断到 400 → 聚合 preview 丢失 TopN 统计。
- Severity: `notable`。

**F5 · `history_recall_structured` reason 的 default zone 与本场景实际
zone 不匹配（minor）**
- Ref: T6；v0.2 G1 L1507 `history_recall_structured → default zone=
  retrieved_facts`
- Observation: T6 返回 137 条 tool_output preview 应渲染到 `tool_previews`
  zone（它们本来就是 tool 输出），而不是 `retrieved_facts`（后者专给
  decision/finding 类）。L1524–1527 虽说"Reason selection must reflect
  the highest-cost or most-specific behavior"，但**reason-zone 不是硬约束
  仅 default**，实现上易混淆。
- Severity: `minor`（功能跑通，可观测性轻度退化）。

**F6 · 跨 manifest 的 `artifact_ref` counter 写入时序未契约化（notable）**
- Ref: T3/T4/T12；v0.2 G9 L1689 `referenced_by_manifest_count`
- Observation: L1725 "Recompute manifest/state/citation counters from
  `context_manifest_items`"—— 但这是**GC 前 recompute**，日常路径 counter
  如何增减未写。T3 manifest 引用 pgdump raw → counter +1？何时 +1？
  transaction 边界？context_manifests 回滚时 counter 是否回退？实现者
  需自行定协议，易和 T12 display-mode (不新建 manifest) 口径不一致。
- Severity: `notable`。

## §5 发现的新 gap 候选

- **title**: Tool output 派生链 `parent_output_id` + 多源 `provenance_refs`
  字段缺失
- **triggered_by_turn**: T8 / T10
- **related_v0.2_section**: §4 Tool Result and Artifact References L1560–
  1610；G9 Retention L1688 `derived_from_artifact_id`（单值）
- **severity**: medium
- **rationale**: T8 `slow_query_explain` 的 output 应链回 T7 `slow_query_
  analyzer` output（"第 3 条慢查询的 explain"），v0.2 schema 无 `parent_
  output_id` 字段，派生链只能靠 preview 文本约定。T10 迁移报告多源派生
  (pgdump_struct + sql_scan + slowlog_top50 三个 artifact)，但 `session_
  artifacts.derived_from_artifact_id` 是**单值 VARCHAR** L1688，无法存 3
  个源 → 溯源链断。v0.2 报告类工具普遍多源派生，这不是 S08 独有。
- **proposed_fix**: (a) `session_tool_outputs` 新增 `parent_output_id
  VARCHAR(128) NULL` + `idx_tool_outputs_parent(parent_output_id,
  created_at)`；(b) `session_artifacts` 新增 `provenance_refs_json JSON
  NULL`（或另开 `session_artifact_provenance` 子表 `(artifact_id,
  source_artifact_id, relation_type)`），与 `derived_from_artifact_id`
  并存 —— 单值用于"单一派生"快速反查，JSON 用于"多源合成"完整溯源。

- **title**: `normalize_version=NULL` 的合法性与 hash 契约冲突
- **triggered_by_turn**: T7 slowlog raw artifact
- **related_v0.2_section**: G17 L2165–2192
- **severity**: low→medium
- **rationale**: G17 L2167–2172 "Every tool output ... that writes
  `content_hash` must declare a `normalize_version`" — 但 slowlog raw
  本来就是时间序列，规范化会破坏完整性，业务要求 hash 全量 raw。此时
  `normalize_version` 是 `raw_v1`（恒等）还是 `NULL`？schema L1568 允许
  NULL，G17 又要求必填 → 文档与 schema 矛盾。
- **proposed_fix**: G17 补一行 "Tools that intentionally hash raw bytes
  without normalization MUST declare `normalize_version='raw_v1'` (identity
  transformation)"，禁止 NULL；或明确 schema NOT NULL 并为 baseline 表补
  `raw_v1` 行。

## §6 Gap 组合效应检验

**组合 A · G8 + G9 + G17**：preview template × retention × content hash
- 运行时场景：T3 pg_dump 3GB 写入时同时触发 G8（preview_template_version=
  'v1'）/ G17（normalize_version='pg_dump_v1'）/ G9（retention_until=now+
  90d, status='active'）。90 天后 G9 T-7 触发 expiring，L1725 重算
  counter — 但若同时 G8 升级 preview_template 到 v2（更大 max_preview），
  G17 升级 pg_dump_v2（不同 normalize 规则），旧 artifact 的
  `preview_text` 是按 v1 生成、`content_hash` 按 pg_dump_v1 计算，新
  manifest 用 v2 预期 → counter 非零但 hash 不 match 新规则 → GC 决策
  边界不清（延期？重 hash？）。
- v0.2 是否覆盖：**partial**。G9 L1727 "extend retention or migrate to
  cold storage" 未规定升级路径；G17 L2190 "`normalize_version` must
  bump" 仅管新写入，不管旧 artifact 重算。
- 如果 no，建议：加"跨版本兼容性条款" — 旧 artifact 保留旧版本号与旧
  hash，context builder 对 version mismatch 降级渲染为"historical artifact
  produced under pg_dump_v1; current active version pg_dump_v2"；GC 一律
  尊重 counter，不做隐式重 hash。

**组合 B · G3 + G8 + G9**：retrieval SLA × preview template × expired artifact
- 运行时场景：T12 次日刷新 → 100 天后再刷新。G3 Tier-1 structured 命中
  T3 行（index 仍有），拿到 `artifact_ref`，但 G9 L1712–1713 raw 已
  `archived_cold` 或 `expired` → Tier-4 raw load 走 cold storage
  `<250ms` hot 不成立（L1943 "cold storage may exceed this"）。context
  builder L1733–1739 "historical artifact; raw no longer available;
  summary preserved"。但 G8 preview_template 对 expired 行没特别约束 —
  preview_text 本身还在 session_tool_outputs (L1571)，应该可读 → 体验上
  老陈点开会看到 preview 但点"下载原文"失败，需明确 UI 降级文案。
- v0.2 是否覆盖：**partial**。G9 context builder 降级行为定义了，但 G3
  retrieval 事件类型 L1940 没列 `retrieval.raw_expired`（只有 `raw_
  missing` / `raw_cold_fetch_required`）— 细分粒度不够。
- 建议：G3 事件表增 `retrieval.raw_expired` 事件，区分"冷存可取"与
  "已过期不可取"，UI 按事件类型选文案。

**组合 C · G1 + G8 + F5**：reason enum × preview template × zone 分配
- 运行时场景：T6 reason='history_recall_structured' default zone=
  `retrieved_facts` (G1 L1507)，但实际渲染在 `tool_previews` zone（137
  行全是 tool_output preview）。G8 L1650 pg_dump default chunk_type=
  `artifact_text`，slow_query_analyzer=`tool_output_preview`，不同 tool
  渲染到的 zone 不一致 — context builder 如何 route？v0.2 未规定"按
  reason default zone 还是按 template chunk_type 路由"的冲突消解规则。
- v0.2 是否覆盖：**no**。G1 default zone 是 "reason → zone"，G8 是 "tool
  → chunk_type"，两套映射并存但没有 precedence。
- 建议：明确规则 — "tool output class 输出（tool_previews zone）永远按
  template chunk_type 路由，reason default zone 仅用于非 tool 类 item（
  summary/decision/finding）"；或在 context builder spec 里加决策树。

## §7 Token 实际 walkthrough — T3 pg_dump 3GB

最紧张的是 T3（单次 tool 输出 3GB 写入），但 prompt 侧 T7 最挤（1500
cap 逼近）。本节双列呈现：

**T3 byte_size 与 zone 分配**：

| 项 | 值 | 来源 |
| --- | ---: | --- |
| `session_artifacts.byte_size` | 3,221,225,472 | raw dump 真实字节 |
| `session_tool_outputs.byte_size` | 3,221,225,472 | L1584 反映 raw 大小 |
| `session_tool_outputs.preview_text` 长度 | 800 字符 ≈ 800B | G8 baseline max_preview=1200B L1651，留余量 |
| `session_tool_outputs.token_estimate` | 210 | preview + metadata |
| `content_hash` 输入大小 | 3GB normalized | G17 `sha256(pg_dump_v1(raw))` L2171 |
| `referenced_by_manifest_count` | 1 | T3 manifest tool_previews 引用 |

**T3 manifest zones**（总 `context_manifests.total_estimated_tokens≈4160`）：

| Zone | Cap / 实际 | 说明 |
| ---: | ---: | --- |
| `system_static` | 1200 | 常驻，L2387 |
| `tool_schemas` | 1800 | 7 类 tool，L2388 剪枝后 |
| `session_anchor` | 240 | L2389 hard cap |
| `plan_todo` | 320 | 4 active todo，L2390 "active plan + next actionable" |
| `tool_previews` | **240** | **1 条 pg_dump preview** 800B ≈ 240 tokens（3:1 char/token for SQL）；cap 500–1500 L593 大窗口 |
| `recent_tail` | 600 | T1–T3 三轮 |
| `retrieved_facts` | 0 | 无历史检索 |
| `delegation_state` | 0 | 无 delegation |
| **总** | **4400** | overhead 包含 JSON 字段分隔，实测 ≈4160 |

**关键验证结论**：

1. `tool_previews=240` 远低于 L593 cap 1500，**即使 preview 做到 1200B
   max 也只 360 tokens**，单轮无超限压力；压力在 T5–T10 累加。
2. T3 朴素方案会尝试把 3GB raw 放 `tool_previews` → 10 亿 tokens，预算
   差 ~24 万倍，**v0.2 `byte_size<16KB` 兜底路由是必需硬约束**（S08 §4
   指出）而非 best-effort —— 建议显式加到 schema L1560 comment。
3. 90 天后 T-7 扫描：G9 L1724 mark expiring → L1725 recompute counter
   FROM `context_manifest_items WHERE source_table='session_tool_outputs'
   AND source_id=T3.output_id`，T3/T4/T7/T10 四个 manifest 命中 →
   counter=4 → L1727 extend or migrate cold。**活跃扫描生效 ✓**，但依
   赖 manifest_items 行在 90 天后仍存在（v0.2 未规定 manifest GC 策略，
   若 manifests 早于 artifacts 被清理，counter 失真）。

## §8 最终判断

- **本场景下 v0.2 是否可实现**：⚠️ 可实现但**有 1 blocking + 4 notable
  friction 需补**（F1 batch insert 契约必须先定，否则 T5 延迟爆）。
- **顺畅度**：⚠️ 有明显摩擦。核心三件事（preview/artifact/retrieval）
  机制全到位，但"工具级契约（template baseline、normalize baseline、
  parent/provenance 链路）"覆盖不足，工程师实施会反复查表补洞。
- **一句话总结**：v0.2 对"giant tool output"的骨架（preview + artifact_
  ref + content_hash normalize + 三级检索 + presigned download）全部对
  味，但 S08 暴露的**批量写入协议、FTS DDL 契约、派生/多源溯源、normalize_
  version=NULL 合法性** 4 个细节需 v0.3 补足；否则实现者落地时会在正确
  路径上踩到可规避的坑，但不至于方向性翻车。
