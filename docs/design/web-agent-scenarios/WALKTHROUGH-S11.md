# WALKTHROUGH S11 — 跨 session 记忆：3 个月前 MatrixOne JSON 列决策

> Author perspective: 一位有经验的工程师按 v0.2 设计**真实实现并运行 S11 场景**的视角；
> 非评审、非重写场景。每个回合都要回答四问：机制可达 / 体验顺畅 / 别扭点 / token 效率。
>
> Source scenario: `S11-cross-session-memory.md` v0.1 §1–§3（只读时间线）
> Source design: `web-agent-session-state.md` v0.2（19 gap 已 patch）
> Source tracker: `GAP-TRACKER.md`（G1/G3/G6/G9/G12/G16 resolution commits）

## §1 场景标识与 v0.2 映射

**一句话场景**：用户小林 2026-05-06 新开 session s_9，要求 agent 在**跨 8 个旧 session**
的 history 里定位到 s_5（2026-03-28）的"pricing 表拒绝 JSON 列"决策，并在 DDL 生成时自
动激活他自己在 s_7 发布的个人 skill `matrixone-schema-guide` v3，最后把决策沉淀为
user-scope 的 durable memory 供未来所有新 session 自动加载。

**被测 DAUT**：A3（跨 session 结构化检索索引路径）/ A10（三级检索在跨 session 维度的
provenance 传导）/ A12（个人 skill 跨 session 版本化 + per-session activation + frozen
version_id）。

**v0.2 关键章节与行号**：
- G6 Cross-Session Scope and User Memory @ L2302–2353（scope='user' 白名单
  `durable_decision/engineering_rule/rejected_pattern`、provenance 列 origin_session_id +
  origin_chunk_id、anchor zone ≤400 token 加载预算、"跨 session 检索必须带 user_id"授权铁律）。
- G3 Retrieval State Machine @ L1930–1987（三级 SLA: structured 50ms / FTS 200ms /
  vector 500ms；event payload 含 `source_session_id`；vector stale 降级路径）。
- `session_history_chunks` DDL @ L1833–1862，**新索引 `idx_history_user_type_created
  (user_id, chunk_type, created_at, session_id)` @ L1859**——S11 T3 核心命中点。
- G16 Personal Skill Activation @ L2120–2161（frozen version_id、新 session 默认不激活、
  suggested_next_action 路径、`skill_installations.scope` 扩展 L2101–2103）。
- G12 Next-Action Confidence State Machine @ L659–698（confidence 0.5–0.8 band 触发 ask-user；
  ask-user fatigue 策略 L693–696）。

## §2 运行前提

- **schema 初始状态**：`agent_sessions` 含 s_1..s_8（status='closed'）与 s_9（status='active',
  started_at=2026-05-06T09:30）；`session_history_chunks` 跨 8 session 合计 ~9.6 万行，其中
  `chunk_type='decision'` 且 90 天窗口内 ~320 行；`session_state_items` 在 s_1..s_8 均产出过
  scope='session' 决策行，但**尚无 scope='user' 行**（系统此前从未 promote 过，T10 第一次）。
- **user_skill_versions**：s_7 期间产出 v1/v2/v3 三行（status 分别为 superseded/superseded/
  active），`skills_registry.active_version='usv_v3'`，`skill_installations(user_id='u_lin',
  scope='user', workspace_id=NULL, session_id=NULL)` 一行；v0.2 L2101–2103 确认
  `skill_installations` 已扩 `scope` + `workspace_id` 列 + `idx_skill_install_scope` 索引。
- **workspace**：cloud 单 workspace（`ws_mo_platform`），无 edge fallback；user_id='u_lin'
  单 org 绑定（`org_id='org_database_infra'`，`auto_activate_on_topic_match=false`）。
- **context window**：使用 `budget_v1_medium_128k`（非 8k 小窗模板；cross-session recall
  要求 `retrieved_facts` + `skills` 双 zone 同时存在，8k 会直接 overflow — §7 末段对比）。
- **预期 prompt 峰值**：T9 ≈ 6960 tokens；T10 ≈ 7580 tokens，远低于 128k 上限。
- **未激活 trace**：`session_state_items(session_id='s_9', category='active_skill')` 初始 0 行。

## §3 端到端运行轨迹

### T1 · 09:31 小林发首句（显式跨 session 意图）

**User 原话**：「我要给 pricing 表加 `regional_markup_rules`，每个地区一套加价规则。
但我好像 3 个月前某次聊过为什么不用 JSON 列，帮我找一下那次的理由。」

**Server behavior**：ContextAssembler 解析触发词（"3 个月前"+"某次聊过"+"那次"；时间
差值 > s_9 寿命 30 秒）→ 构造 `RetrievalRequest{scope:'cross_session', user_id:'u_lin',
time_window:[2026-02-05, 2026-05-05], topic_hints:['JSON','pricing','schema evolution'],
artifact_hints:['decision']}`；写 `session_state_items(scope='session', category=
'retrieval_intent', item_key='t1_cross_session', status='active')` @ s_9；RunLoop 插入
`agent_events(event_type='retrieval.intent_parsed', payload={triggers:['3月前','某次','那次'],
scope:'cross_session'})`。

**User sees**：SSE `session.turn_started`(seq=1) → UI 顶部出现"检索中…"spinner；manifest
zone：`system_static` 1200 / `tool_schemas` 1100 / `session_anchor` 250 / `plan_todo` 0 /
`recent_tail` 120 / `summary` 0 = **2670 tokens**；`context_manifests.reason=
'cross_session_retrieval_intent'`（G1 枚举已含 `cross_session_recall` 近亲，实际 intent 阶段
对应 `retrieval_intent` — 见 §5 新 gap）。

### T2 · 09:31 session 9 本地 context miss（正确隔离）

**User 原话**：（无；agent 内部流）

**Server behavior**：按"先 local 后 cross"的安全默认，先查 scope='session' 本地。
```sql
SELECT item_id, category FROM session_state_items
WHERE session_id='s_9' AND scope='session'
  AND category IN ('decision','summary','retrieval_cache')
  AND status='active' ORDER BY updated_at DESC LIMIT 20;
-- 0 rows (s_9 刚开 30 秒)
SELECT chunk_id FROM session_history_chunks WHERE session_id='s_9' LIMIT 1;
-- 0 rows
```
写 `agent_events(event_type='retrieval.local_scope_empty', payload={scope:'session',
session_id:'s_9'})`。正确 miss——G6 L2308 "scope='session' visible only inside one session"
明文保证隔离。

**User sees**：无 SSE（内部 trace 只落 DB）；manifest 无变化；<20ms 用户无感知。
`reason='local_scope_miss_expected'`。

### T3 · 09:32 跨 session structured 检索（G6+G3+L1859 新索引联动）

**User 原话**：（无）

**Server behavior**：升级 scope='cross_session'，发射 structured 查询走 L1859 新索引
`idx_history_user_type_created (user_id, chunk_type, created_at, session_id)`：
```sql
-- EXPLAIN key=idx_history_user_type_created, rows≈320, type=range
SELECT chunk_id, session_id, chunk_type, source_table, source_id,
       title, preview_text, raw_ref, content_hash, created_at, importance
FROM session_history_chunks
WHERE user_id = 'u_lin'
  AND chunk_type = 'decision'
  AND created_at BETWEEN '2026-02-05 00:00:00' AND '2026-05-05 23:59:59'
ORDER BY importance DESC, created_at DESC
LIMIT 50;
-- returned 31 rows in 28ms (meet G3 L1940 structured SLA <50ms)
```
候选按 session_id 聚合：s_5(12) / s_3(7) / s_4(5) / s_8(4) / 其他(3)。写
`agent_events(event_type='retrieval.structured_ok', payload={stage:'structured',
reason:'cross_session_recall', user_id:'u_lin', scanned_rows:320, returned:31,
elapsed_ms:28, index_used:'idx_history_user_type_created'})`。

**User sees**：仍是 spinner（未 render）；审计 trace 清晰。**对比 S11 §3 v0.1 原设计**：
场景文本描述"420ms 退化 scan"——**v0.2 已补 L1859 索引**，实际 28ms 走快路径。场景
v0.1 §7 F1 在 v0.2 上**不再触发**；场景 §8 建议二已 resolved 为 GAP-TRACKER L263。

### T4 · 09:32 FTS 二级 + vector rerank（跨 session 命中 s_5）

**User 原话**：（无）

**Server behavior**：FTS 带 T3 candidate IN filter 做隔离收敛（避免第二次 user 级扫描）。
```sql
SELECT c.chunk_id, c.session_id, c.raw_ref, c.content_hash, c.preview_text,
       MATCH(c.preview_text) AGAINST(
         '+JSON +("pricing" "schema" "column" "evolution")' IN BOOLEAN MODE) AS score
FROM session_history_chunks c
WHERE c.chunk_id IN (<31 candidates from T3>)
  AND MATCH(c.preview_text) AGAINST(
         '+JSON +("pricing" "schema" "column" "evolution")' IN BOOLEAN MODE)
ORDER BY score DESC, c.importance DESC LIMIT 5;
-- returned 3 rows, elapsed 92ms: chk_s5_0328(score=12.4) /
--   chk_s5_0329_benchmark(8.1) / chk_s5_0330_summary(4.7) ALL session='s_5'
```
Vector rerank：对 3 candidate 查 embedding 表，比对 `session_history_chunks.content_hash`
vs vector 行的 `indexed_content_hash`（G3 L1982–1985）。3 行全通过 → top-1 稳定锁定
`chk_s5_0328_decision_json_vs_columnar`。写 `agent_events(stage='fts', elapsed_ms=92,
source_session_id='s_5', candidate_count=3)` + `agent_events(stage='vector', elapsed_ms=180,
source_session_id='s_5', candidate_count=3, vector_stale_skipped=0)`。三级合计 28+92+180=
300ms，远低于 G3 总 SLA 1s。

**User sees**：仍是 spinner；trace 表中 3 条 event 按 stage 排布。

### T5 · 09:33 跨 session raw_ref load + retrieved_facts zone 渲染

**User 原话**：（无）

**Server behavior**：按 top-2 raw_ref 精确 PK load 到 `session_artifacts`——关键：
```sql
-- G6 L2349 铁律：必须带 user_id 作为授权边界
SELECT artifact_id, session_id, title, content_text, content_hash, created_at
FROM session_artifacts
WHERE artifact_id IN ('artifact:decision-s5-20260328-json-vs-columnar',
                      'artifact:benchmark-s5-20260329-json-path-pricing')
  AND user_id = 'u_lin';  -- 非 session_id 边界
-- returned 2 rows, total 23KB content_text, PK lookup 12ms
```
Render 到 `retrieved_facts` zone：decision narrative 600 token + benchmark table 150 token +
provenance header 30 token = 780 token。每条必含 `source_session_id='s_5'` 字段，否则 LLM
无法区分"这是 s_5 的决策还是 s_9 本地的"。写 `context_manifest_items` 2 条
（included=TRUE, zone='retrieved_facts', source_session_id='s_5'）+ 29 条
（included=FALSE, drop_reason='fts_lower_score'，保留 raw_ref 供 rehydrate）。

**User sees**：SSE `context_manifest.updated`(seq=3)+`assistant.message_chunk`(agent 回复中)；
UI `retrieved_facts` panel 出现 2 张卡片（🔗 source: s_5, 2026-03-28）。manifest：
system 1200/tool 1100/anchor 250/tail 300/**facts 780**/summary 0 = **3630**，
dropped_count=29，`reason='cross_session_recall'`（G1 L66 枚举）。

### T6 · 09:34 用户确认 + 派生 DDL 任务

**User 原话**：「对就是这个。6.2x 那个数字我有印象。那现在基于这个决策我要给 pricing
加个 `regional_markup_rules` 列——结构化方案，写 DDL。区域定为 NA/EU/APAC 三个 enum。」

**Server behavior**：(1) 写 `session_state_items(scope='session', category='anchor',
item_key='active_decision_ref', payload_json={source_session_id:'s_5', chunk_id:
'chk_s5_0328_decision_json_vs_columnar', cited_at:'2026-05-06T09:34', evidence_x:6.2},
status='active')` 锁定决策为当前 turn 依据；(2) 追加 todo 行 `session_todos(item_key=
'draft-pricing-regional-markup-ddl', priority=80, status='pending')`；(3) 写
`agent_events(event_type='decision.citation_confirmed', payload={cited_session:'s_5'})`。

**User sees**：SSE `todo.added` → UI todo 面板多 1 条 high-priority；manifest：anchor
**450**(+200，含决策 ref payload 摘要) / plan_todo 80(+80，新 todo 行) / tail 450 /
facts 780 / 其他维持 = **3980**；`reason='user_confirm_and_derive_task'`（G1 枚举里对应
`workflow_continuation` 或自定义；见 §5）。用户记得 6.2x 数字→说明 T5 provenance 保真度
达标；对 agent 信任度↑。

### T7 · 09:34 SkillResolver 发现 s_7 的 personal skill 未激活

**User 原话**：（无；内部 DDL-pre-generate hook 触发）

**Server behavior**：按 G16 L2117 的"skill selection uses the same selector, filters by
user ownership, installation, workspace bindings, and token budget"：
```sql
-- Step 1: 找 user 所有可用 skill
SELECT s.skill_name, s.source_id, v.version_id, v.version, v.content_hash,
       v.summary_text, v.token_estimate
FROM user_skill_sources s
JOIN user_skill_versions v ON v.source_id=s.source_id AND v.status='active'
JOIN skill_installations i ON i.skill_name=s.skill_name AND i.user_id=s.user_id
WHERE s.user_id='u_lin' AND s.status='active'
  AND i.scope IN ('user','workspace') AND i.status='active';
-- 1 row: matrixone-schema-guide v3 (usv_v3), token_estimate=1800
-- Step 2: 查 session 9 本地激活状态
SELECT item_id FROM session_state_items
WHERE session_id='s_9' AND scope='session' AND category='active_skill'
  AND status='active';
-- 0 rows → 未激活
```
由于 `auto_activate_on_topic_match=false`（§2 前提），G16 L2147–2149 规定走
suggested_next_action 路径；agent 不直接注入 1800 token，而是进 T8 ask-user。

**User sees**：无 UI 变化（内部 resolver scan）；`agent_events(event_type=
'skill.resolver_scan', payload={matched:1, active_in_session:0, suggest_activation:true,
topic_match_score:0.72, triggering_turn:'T6'})`。

### T8 · 09:35 agent 生成 skill 激活询问（G12 confidence 0.72 落 band）

**User 原话**：（无，agent 主动问）

**Server behavior**：构造 `suggested_next_action` 行：`source='rule'`（topic-match 规则），
`confidence=0.72`（落 G12 L670 的 0.5–0.8 band），`kind='skill_activation'`，
`payload_json={skill_name:'matrixone-schema-guide', version_id:'usv_v3', content_hash:'...',
token_estimate:1800, activation_modes:['session_scoped','one_shot','none']}`，
`expires_at=now+1h`（G12 L691 'hint' 过期）。0.72 落 band → **必须** ask-user，不 auto-accept。

Agent 生成 assistant 消息：
```
我注意到您在 2026-04-22 (session s_7) 发布过个人 skill
`matrixone-schema-guide` v3（~1800 tokens），覆盖 enum 命名、分区
键选择、维度表命名约定。本 session 未激活，是否激活？
  [a] 本 session 全程激活（每 turn +1800）
  [b] 仅本次 DDL 生成使用 one-shot
  [c] 不激活
```
写 `agent_events(event_type='skill.activation_prompt', payload={suggested:'a',
suggestion_id:'sna_skill_s9_t8'})`。

**User sees**：SSE `suggestion.pending`(seq=5) → UI 在 agent message 下方渲染 3 选项按钮卡
片；manifest：tail **700**(+250 ask-user msg) / 其他维持 = **4230**；`reason=
'skill_activation_ask'`（属 G1 枚举里 `ask_user_clarification` 子类）。

### T9 · 09:36 用户选 [a] + DDL 生成（skill zone 注入 frozen v3）

**User 原话**：「[a]，本 session 全程激活。这个 skill 就是我 3 个月前总结的，早该默认用。」

**Server behavior**：
(1) 响应 suggestion：`apply_suggestion` event 引用 `sna_skill_s9_t8.id`；
(2) 写 `session_state_items(scope='session', category='active_skill', item_key=
'matrixone-schema-guide', payload_json={source_id:'usk_src_lin_mo_schema', version_id:
'usv_v3', content_hash:'sha256:<v3>', activation_source:'user_explicit', activated_at:
'2026-05-06T09:36'}, status='active')`——**G16 L2145 规则：version_id 此刻 frozen**；
(3) 从 `user_skill_versions(version_id='usv_v3')` 加载 `content_markdown` 1800 token 注入
prompt 的 `skills` zone；(4) LLM 调用生成 DDL：

```sql
-- Decision origin: session=s_5, chunk=chk_s5_0328_decision_json_vs_columnar
-- Skill applied: matrixone-schema-guide v3 (hash=7b3a..c4e1)
ALTER TABLE pricing
  ADD COLUMN region ENUM('NA','EU','APAC') NOT NULL DEFAULT 'NA',
  ADD COLUMN regional_markup_bps INT NOT NULL DEFAULT 0
    COMMENT 'basis points; 100=1.00%';
CREATE INDEX idx_pricing_region_markup ON pricing (region, regional_markup_bps);

CREATE TABLE IF NOT EXISTS pricing_regional_rules (
  rule_id        VARCHAR(64) PRIMARY KEY,
  tenant_id      VARCHAR(64) NOT NULL,
  region         ENUM('NA','EU','APAC') NOT NULL,
  product_class  VARCHAR(64) NOT NULL,
  markup_bps     INT NOT NULL,
  effective_from TIMESTAMP NOT NULL,
  effective_to   TIMESTAMP NULL,
  INDEX idx_prr_tenant_region (tenant_id, region, effective_from)
);
```

**User sees**：SSE `skill.activated`(seq=7)+`tool_preview.updated`(seq=8)；UI skill 徽章点
亮；DDL 渲染在代码块；manifest 达峰值 **6960**（系 1200/工具 1100/anchor 450/todo 80/
tail 900/facts 780/**skills 1800**/preview 650/summary 0）；`reason=
'skill_activated_ddl_generated'`。**G16 frozen 效果**：若小林此刻在另一设备把 skill 升到 v4
（`skills_registry.active_version='usv_v4'`），s_9 仍持 v3——这正是场景 v0.1 §7 F2 的本质
防线，v0.2 L2145 已覆盖。

### T10 · 09:38 artifact 落库 + user memory promote（G6 **首次真实写入 scope='user'**）

**User 原话**：「跑过了 DDL 没问题，存下来。另外——你跨 session 找到 s_5 那个决策的
方式很好。我以后每次开新 session 都要重新找一遍吗？这个 JSON 列的决策应该算我**整个
数据平台的通用约束**，不是 s_5 一个 session 的事。」

**Server behavior**：
(1) `INSERT INTO session_artifacts(artifact_id='artifact:ddl-s9-20260506-pricing-regional-
markup', user_id='u_lin', session_id='s_9', content_text=<DDL>, byte_size=2840,
content_hash='7b3a...c4e1', access_scope='user', retention_policy='pinned_by_citation')`（G9
L1681–1694 新字段）；
(2) `INSERT INTO session_history_chunks(chunk_type='artifact_text', source_table=
'session_artifacts', source_id=<artifact_id>, indexed_at=NULL)` — lazy index；
(3) `context_manifest_items` 3 条（新 DDL/s_5 decision ref/skill v3 ref）；
(4) **关键 G6 写入**——识别用户话语为 durable_decision promote：

```sql
-- G6 L2302-2353 明文允许：scope='user' + category='durable_decision'
-- L2313-2316: session_id 列存 namespace key 'user:u_lin'，原 session 在 origin_session_id
-- L2324-2326: provenance 必须在真实列（origin_session_id / origin_chunk_id）
INSERT INTO session_state_items
  (item_id, user_id, session_id, origin_session_id, origin_chunk_id,
   scope, category, item_key, title, summary_text, payload_json,
   priority, status, created_at, updated_at)
VALUES
  ('ssi_usr_mo_reject_json', 'u_lin', 'user:u_lin', 's_5', 'chk_s5_0328_...',
   'user', 'durable_decision', 'mo-schema.reject-json-in-hot-tables',
   'Reject JSON columns on pricing/metrics-class hot tables',
   'MatrixOne JSON_EXTRACT p99 6.2x vs structured; use dimension table (tenant_id, region).',
   JSON_OBJECT('ref_artifact','artifact:decision-s5-20260328-json-vs-columnar',
               'decision_date','2026-03-28','evidence_benchmark_x',6.2,
               'applies_to_tables',JSON_ARRAY('pricing','metrics')),
   80, 'active', NOW(), NOW());
```

**对比 S11 场景 v0.1 §8**：原场景建议一"引入 scope='user'"——v0.2 L2302–2353 **已完全
resolved**。T10 原文"诚实暴露 gap"的 assistant reply **不再需要**；agent 可直接 promote，
回复改为："已沉淀为 user memory `mo-schema.reject-json-in-hot-tables`，下次您开新
session ContextAssembler 会自动加载到 anchor zone（预算 ≤400 token，G6 L2341）。"

**User sees**：SSE `artifact.persisted`(seq=9)+`user_memory.promoted`(seq=10)；UI 提示
"✨ 已沉淀为跨 session 记忆 (user scope, durable_decision)"；manifest：preview **900**
(+250 新 DDL preview) / tail **1200** / facts 780 / skills 1800 / 其他维持 = **7580**；
`reason='artifact_commit_with_user_memory_promote'`（G1 枚举需补，见 §5 第 1 条新 gap）。

## §4 发现的别扭点（Frictions）

**F1 · local→cross_session 升级的触发判定未枚举化**
- **Ref**: T1/T2 切换点 / v0.2 §Cross-Session Scope L2349–2351
- **Observation**: G6 L2349–2351 是授权铁律（"必须带 user_id + chunk_type 谓词"），但**触
  发升级 cross-session 的条件**（显式触发词？local miss？UI toggle？）在 v0.2 未定义。S11
  T1 依赖"3 个月前"+"某次"关键词，实现层只能 hard-code regex 或小模型分类器。
- **Why friction**: 跨实现版本行为不一致；保守触发会漏召回，激进触发会把每个提问都当
  cross_session 滥扫（即便有 L1859 索引，扫 320 行+render 也是 token 浪费）。
- **Severity**: notable

**F2 · T3 候选 31 行的 session-diversity rerank 策略未规定**
- **Ref**: T3 / §Retrieval State Machine L1932–1987
- **Observation**: L1859 索引按 `(user_id, chunk_type, created_at, session_id)` 排序；T3 拿
  到 31 行后按 `importance DESC` 排，但 v0.2 未规定"同 importance 时如何在 session clusters
  之间平衡"。S11 候选簇 s_5(12)/s_3(7)/s_4(5)/s_8(4)/其他(3)，若 importance 平分，FTS 前
  agent 可能随机选 clusters，召回精度下降。
- **Why friction**: 正确性边界模糊；无明确 session-diversity penalty/boost 规则；易回归。
- **Severity**: minor

**F3 · skill 激活 ask-user 选项缺 token cost 标注**
- **Ref**: T8 / G12 L659–698 + G16 L2120–2161
- **Observation**: agent 问"[a] session 激活 / [b] one-shot / [c] 不激活"，但 v0.2 未规定
  UI 或 prompt 文本**必须标注每选项 token 成本**（[a]+1800/turn × N turn；[b]+1800 仅 1
  turn）。成本敏感用户（尤其 `budget_v1_8k` 小窗部署）会踩坑。
- **Why friction**: UX 不透明；与 G10 小窗协同时 [a] 可能直接 overflow；缺失显性标注
  用户无法做知情决策。
- **Severity**: notable

**F4 · T10 user memory promote 越过 G12 confidence 状态机**
- **Ref**: T10 / G6 L2302–2353 + G12 L659–698
- **Observation**: 小林说"应该算整个平台的通用约束"→ agent 直接写 scope='user'；但这是一个
  **写入**动作，理论 confidence ≈ 0.6–0.7（"应该算"是模糊表达），按 G12 L670 应走 ask-user
  suggested_next_action 而非 auto-accept。v0.2 G6 未规定 promote 前是否必走 confidence band。
- **Why friction**: 两个 gap（G6+G12）虽各自 resolved，联动契约缺失；可能导致误 promote，
  用户反悔时需额外 archive 流程。
- **Severity**: notable

**F5 · session_history_chunks 无 workspace_id/org_id → 跨 org 召回风险**
- **Ref**: T3 + T4 / §session_history_chunks DDL L1833–1862
- **Observation**: 若小林同一 user_id='u_lin' 同时绑定 `org_datalab`（雇主）与
  `org_personal`，G6 的 user_id 铁律会把两个 org 的 decision 都召回。v0.2 DDL 无 org_id/
  workspace_id 列；G6 L2310–2311 虽定义 scope='workspace'，但**仅面向 state_items**，不涉
  及 history_chunks 物理分区。
- **Why friction**: 合规/隐私风险；多 org 是常见 SaaS 模式，硬隔离缺失。
- **Severity**: notable（若 multi-org 是产品方向则升 high）

**F6 · T4 vector 命中后 scratch buffer 的 ACL 未明**
- **Ref**: T4 / G3 L1982–1987
- **Observation**: vector stale 机制 L1982–1985 规定 content_hash 不符时跳过+重建；但"跨
  session vector 表是否按 user_id 分 shard 或行级 ACL"未明文。若 vector 表全局扁平且 lookup
  只按 chunk_id，跨用户 chunk_id 冲撞时权限检查需在 rerank 后端显式追加。
- **Severity**: minor

**F7 · T10 promoted user_memory 不回填原 session 的锚点**
- **Ref**: T10 / G6 L2324–2326
- **Observation**: promote 后 `ssi_usr_mo_reject_json.origin_session_id='s_5'`，但 s_5 自身
  的 `session_state_items` 没有反向 flag 标记"此决策已 promote 到 user memory"。未来若用户
  想查"哪些 s_5 决策已晋升"需反向扫 origin_session_id。
- **Why friction**: 审计不对称；缺反向索引字段；实现成本不大但语义需补。
- **Severity**: minor

## §5 发现的新 gap 候选

- **title**: G1 reason 枚举缺 `user_memory_promote` / `user_memory_archive` / `user_memory_revise`
- **triggered_by_turn**: T10
- **related_v0.2_section**: §Context Manifest Reason Enum（G1 @ GAP-TRACKER L45–76）+
  §Cross-Session Scope and User Memory L2302–2353
- **severity**: low
- **rationale**: T10 的 manifest 行为是"artifact commit + 同 turn user memory promote"，既
  非 `large_tool_output_gated` 也非 `cross_session_recall`——两个枚举都不够精确。没有
  first-class reason，后续 SQL 审计（`SELECT reason, COUNT(*) FROM context_manifests GROUP
  BY reason`）会落入 `other`，让 user memory 相关事件不可观测。
- **proposed_fix**: G1 reason 枚举追加 `user_memory_promote` / `user_memory_archive` /
  `user_memory_revise` / `user_memory_loaded_on_init`（新 session 初始化时加载的对称事件）；
  总计 4 个细分，对应 G6 的写/归档/修订/读路径。

- **title**: G6 user memory promote 与 G12 confidence 状态机的握手契约缺
- **triggered_by_turn**: T10（结合 T8 对比）
- **related_v0.2_section**: §Cross-Session Scope and User Memory L2318–2326 + §Next-Action
  Confidence L659–698
- **severity**: medium
- **rationale**: G6 定义 scope='user' 写入机制，G12 定义 confidence band；二者未在"user 说
  '沉淀下来'时 agent 应 auto-accept 还是 ask-user"上握手。T8 skill 激活走了 ask-user（因
  `suggested_next_action` 明文要求），T10 memory promote 直接写入——**对称性破坏**。若用
  户原话含"也许"/"应该"/"大概"，promote 准确率会下降。
- **proposed_fix**: G6 子节补一段 promote 流程：所有 scope='user' 写入默认经 G12
  `suggested_next_action(kind='user_memory_promote')`；仅当 confidence ≥0.8 或
  source='structured_event'（如用户点击 "Pin as user memory" 按钮）才 auto-accept。

## §6 Gap 组合效应检验

**组合 1：G3 + G6 + G1（跨 session 检索的事件审计）**
- **运行时可能发生的坑**：T3 走新索引 `idx_history_user_type_created` 命中 31 行 28ms，
  emit `retrieval.structured_ok`（G3 L1964-1980 event payload 含 source_session_id）。但
  G1 reason 枚举在 cross-session 命中时该写 `history_recall_structured` 还是
  `cross_session_recall`？GAP-TRACKER L52 两者都列——S11 T3 的 `context_manifests.reason`
  到底落谁？语义重叠会让聚合 SQL `GROUP BY reason` 把同类请求切两半，P95/SLA 指标失真。
- **v0.2 是否覆盖**: partial（枚举齐全但二选一规则未写）
- **如果 no，建议**: 规定跨 session 首次召回优先写 `cross_session_recall`（与 scope 语义对
  齐），`history_recall_{structured|fts|vector}` 仅在 session 内召回使用；或在 SQL 层通过
  `event_type` 副字段区分，避免单一 reason 字段承担双重语义。

**组合 2：G6 + G16（user memory 与 active_skill 的 anchor 重叠）**
- **运行时可能发生的坑**：T10 写 scope='user' `durable_decision`；下次新 session s_10 开
  时，ContextAssembler 按 G6 L2328–2338 加载进 anchor zone（≤400 token）；若 s_10 又触发
  `matrixone-schema-guide` skill auto-activation（假设小林此后开了 auto_activate），prompt
  会出现**两份重叠语义**：user memory summary ("reject JSON on hot tables") + skill v3 里
  同款建议段落（skill 写 markdown 时已包含 3 个月前的决策原文）。
- **v0.2 是否覆盖**: no（dedup 策略缺）
- **如果 no，建议**: SkillResolver 与 UserMemoryLoader 在 assembler 层共享 dedup pass
  （按 `origin_chunk_id` 或 semantic fingerprint / content_hash 前 128bit）；优先保留 user
  memory（更紧凑），skill 内同段落折叠为 `see user memory: mo-schema.reject-json-in-hot-tables`
  指针，释放 100–200 tokens。

**组合 3：G6 + G9（user memory 跨 session 引用 artifact 的反向计数）**
- **运行时可能发生的坑**：T10 user memory `payload_json.ref_artifact` 指向
  `artifact:decision-s5-20260328-...`；G9 L1692 Artifact Retention 允许 90 天 TTL；若
  user memory 永久有效但 artifact 被 GC，下次新 session 加载 memory 进 anchor → 用户点
  provenance 链接 → 404。G9 L1690 `referenced_by_state_items_count` 虽有计数，但当
  scope='user' state_item 跨 session 引用 artifact 时，计数更新逻辑常按 session_id 聚合，
  cross-scope 场景未必触发。
- **v0.2 是否覆盖**: no（cross-scope reverse counter 未规定）
- **如果 no，建议**: G9 扩展——scope='user' 的 state_item 引用 artifact 时必须设
  `retention_policy='pinned_by_user_memory'`，GC 绕过；或双计数器
  `referenced_by_user_memory_count` 独立维护（触发器/事务保证原子写入）。

**组合 4：G12 + G16（ask-user fatigue 与 skill activation 的交互）**
- **运行时可能发生的坑**：G12 L693–696 规定"同 session 1 小时内 3 次 clarification →
  降低 auto-accept 阈值"；T8 skill 激活本身就是 ask-user。若用户在 1 小时内还遇到另外 2
  个 clarification（比如 T6 前 agent 问过 region 是否 NA/EU/APAC、T7 前问过是否要维度表
  还是列扩展），那么 T8 的 skill 激活 ask 是第 3 次——按 G12 规则会降阈值，但 G16 没说
  skill activation 是否也享受阈值降低。
- **v0.2 是否覆盖**: partial（G12 覆盖通用 clarification，G16 未反向声明是否参与 fatigue）
- **如果 no，建议**: G16 明确声明 skill_activation 参与 fatigue 计数；若降阈值生效，0.72
  confidence 在新阈值（0.7）下可 auto-accept skill_session_scoped。

## §7 Token 实际 walkthrough — T4 跨 session chunk 命中时 retrieved_facts zone 账

S11 中最"紧张"的**不是** T9（已 6960，距 128k 很远），而是 T5→T9 窗口里
`retrieved_facts` zone 的 780 token 组成的 provenance 完整性与 zone cap 关系。
v0.2 §Budget Policy v1 @ L2381 定义 medium 窗口 retrieved_facts cap `max_fraction=0.15`
→ 0.15×128k = 19.2k tokens，780 远低于 cap，单纯不 overflow。真正要看的是**跨 session
provenance 账的细分**与 G3 L1970 `source_session_id` 契约的落实。

| 条目 | tokens | 计算依据 |
| --- | --- | --- |
| decision narrative | 600 | 2026-03-28 决策摘要：3 条理由（JSON_EXTRACT p99 6.2x / 向量全文索引不可用 / schema drift 审计难）+ 1 句方案（结构化 + 维度扩展表）；G8 preview template `chunk_type='decision'` 上限 1000 保守取 600 |
| benchmark table | 150 | 2 行数字（JSON_EXTRACT p99=62ms/CPU180% vs structured p99=10ms/CPU35%）；table render_mode token 密度比 narrative 高 ~25% |
| provenance header | 30 | `source_session_id=s_5` + `chunk_id=chk_s5_0328_decision_json_vs_columnar` + `decision_date=2026-03-28` + `content_hash[:8]=a1f0..92bc` |
| **zone 合计** | **780** | G3 L1970 contract 要求 `source_session_id` 必须可见 |

**完整 render 展开**：
```text
[retrieved_facts zone · 780 tokens · cross_session_recall · entries=2 · dropped=29]
  ┌─ Entry#1 (narrative, 600 t) ──────────────────────────────────
  │  "2026-03-28 session s_5 决策：pricing / metrics 类高频查询表
  │   拒绝 JSON 列。理由：(1) MatrixOne JSON_EXTRACT 路径查询相比
  │   结构化列 p99 延迟 6.2x；(2) JSON 列无法有效走向量/全文索引；
  │   (3) 审计上 schema drift 难回溯。方案：结构化列 + 维度扩展
  │   表 (tenant_id, region) 关联。"
  │  provenance: source_session_id=s_5,
  │              chunk_id=chk_s5_0328_decision_json_vs_columnar,
  │              artifact_id=artifact:decision-s5-20260328-json-vs-columnar,
  │              decision_date=2026-03-28,
  │              content_hash=a1f0..92bc
  ├─ Entry#2 (table, 150 t) ──────────────────────────────────────
  │  | 路径            | p99 latency | p99 CPU |
  │  | JSON_EXTRACT    | 62ms        | 180%    |
  │  | structured col  | 10ms        | 35%     |
  │  provenance: source_session_id=s_5,
  │              chunk_id=chk_s5_0329_benchmark_json_path_pricing
  └─ Zone header (30 t): "retrieved_facts · cross_session_recall ·
                          budget 19200 · used 780 · entries 2 · dropped 29"
```

**dropped_count=29 的后续语义**：T3 的 31 candidates 中 29 条 preview_text 未 render，
以 `context_manifest_items(included=FALSE, drop_reason='fts_lower_score', raw_ref=...)`
审计表形式保留。G9 L1689 `referenced_by_manifest_count` **不**对这 29 条递增（只对
included=TRUE 递增）。若 T6 后用户追问"还有其他 benchmark 吗"，agent 可 O(1) 从
dropped 列表 rehydrate，无需重走 T3→T4 全链。

**与 G10 `budget_v1_8k` 小窗对比**：同 780 token 在 8k 窗口下占 9.75%，接近小窗
retrieved_facts cap ~10%；若再叠加 T9 的 1800 token skill zone，必然 overflow→drop
cascade，skill 被 squeeze 或 decision summary 降级到 200 tokens、benchmark 砍 1 行。这解
释了 §2 为何强制选 medium template——S11 这类 cross-session recall + personal skill 双重叠
加的工作流，**必须**走 budget 降级版本才能在小窗环境运行。G10 的降级矩阵建议在 S11
复现版本中补一列 `cross_session_recall × skills_activated` 组合的紧凑渲染策略。

## §8 最终判断

- **本场景下 v0.2 是否可实现**：✅ 可实现。G6（L2302–2353）/ G3（L1930–1987）/ G16
  （L2120–2161）/ L1859 新索引四处联动，T1–T10 全链路在 v0.2 文字范围内可落地；S11 场景
  v0.1 §8 提出的三条建议（user memory scope / 复合索引 / skill activation 语义）**已全部
  resolved** 为 GAP-TRACKER 的 G6+G16。
- **顺畅度**：⚠️ 有明显摩擦。主要摩擦集中在 local→cross_session 升级条件（F1）、ask-user
  选项缺 token cost 标注（F3）、T10 直接 promote 越过 G12 confidence 状态机（F4 + §5 第 2
  条新 gap）、user memory + skill anchor 重叠无 dedup（§6 组合 2）——均为 UX/契约细节，
  不阻塞实现但工业化前需补。
- **一句话总结**：v0.2 把 S11 从"物理不可行"（朴素 7.75×10^8 tokens）或"agent 失灵"（LLM
  凭感觉无 provenance）降到 6960 tokens 可运行、跨 session 决策可引用可沉淀的工程态；核
  心机制全通，剩余 2 个新 gap（G1 枚举细化 + G6×G12 握手契约）皆为 low/medium，再补一
  轮即工业级化。
