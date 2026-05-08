# S11 — 架构师小林找 3 个月前那个 MatrixOne schema 演进决策

> Status: Draft v0.1
> DAUT: **A3, A10, A12**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：小林，某数据基础设施公司首席架构师，负责一个线上运行的 MatrixOne 数据
平台。使用 astra web agent 的习惯与阿强（S02）相反：**按主题开 session**，一个主题聊透就
close，下次换主题开新 session。这是他的「session 即项目阶段」心智模型。

**业务背景**：过去 3 个月（2026-02-07..2026-05-05）小林围绕同一个数据平台开了 **8 个独立
session**，每个 session close 时都产出了一份 decision artifact：

| session_id | 主题 | 周期 | 关键产出 |
| --- | --- | --- | --- |
| s_1 | metadata schema 初版设计 | 02-07..02-14 | DDL + ER 图 |
| s_2 | 性能基线 & 索引调优 | 02-18..02-27 | benchmark 报告 + 索引规约 |
| s_3 | 分区策略（hash vs range） | 03-02..03-09 | decision："按 tenant_id hash" |
| s_4 | 多租户隔离：RBAC + row filter | 03-11..03-20 | policy artifact |
| **s_5** | **schema 演进策略：JSON 列 vs 结构化列** | **03-23..03-31** | **decision："pricing/metrics 表拒绝 JSON 列"** |
| s_6 | 备份恢复演练 | 04-03..04-10 | runbook |
| **s_7** | **自写个人 skill `matrixone-schema-guide` v1→v3** | **04-13..04-22** | **skill 3 个版本** |
| s_8 | 容量规划 & 跨云部署蓝图 | 04-25..05-05 | capacity 模型 |

今天 2026-05-06 小林**新开 session s_9**，第一句话就是：「我要给 pricing 表加个新列，好像
3 个月前某次聊过为什么不用 JSON 列？帮我找一下那次的理由，我快忘了。」

**一句话目标**：在**跨 8 个 session** 的历史里定位到 s_5 的 JSON 列决策，**不**重放任何旧
session，并在本轮 DDL 生成时自动激活 s_7 里小林自己写过的个人 skill。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 本次交互总回合 | 10 回合 |
| 被检索 session 总数 | 8（同 user_id 下） |
| 历史累计 chunks（跨 8 session 合计） | ~9.6 万 |
| 本 user 过去 90 天 `chunk_type='decision'` 行数 | ~320 |
| 历史累计 transcript 字节（跨 session 合计） | ~3.1 GB |
| 本次 prompt token 峰值（T9） | ≤ 10k |

## 2. 被测 DAUT

S11 的压测点是**跨 session 维度**——S02 已经覆盖了单 session 内的三级检索，这里要打的是
**同一 user 的多个 session 之间是否能通过同一套机制互通**。

| 编号 | 压测方式 |
| --- | --- |
| **A3** | 跨 session 检索**第一跳不允许**全表扫或对 `chunk_type`/`preview_text` 无界扫描。`session_history_chunks` 有 `user_id` 列，但现有索引 `idx_history_session_seq` / `idx_history_session_created` / `idx_history_type_created` 全部**以 `session_id` 为前导列**——**没有以 `user_id` 为前导的复合索引**。跨 session 查询 `WHERE user_id=? AND chunk_type='decision' AND created_at BETWEEN ? AND ?` 会退化为「对该 user 所有 session 各自扫一遍」或直接走 range scan on user_id alone，导致候选集 ~320 行按 user 一次性回表。本场景 T3 必须暴露这个 gap（见 §7 F1、§8 建议二）。 |
| **A10** | 三级检索在**跨 session 维度**上重走一遍：(1) structured `user_id + chunk_type + created_at` 三元组过滤 → ~320 候选；(2) FTS 命中 `+JSON +("pricing" OR "schema" OR "column")`，~6 行；(3) vector rerank top-3 取回 raw_ref。每一级必须携带 `source_session_id` 作为 provenance——render 时 LLM 必须能看到「这条决策来自 **s_5** 的 2026-03-28 那一轮」，否则用户无法信任跨 session 召回。 |
| **A12** | 个人 skill 跨 session **版本化 + 激活切指针**在本场景有两个压点：(a) 小林在 s_7 发布了 `matrixone-schema-guide` v1→v2→v3 三个版本（`user_skill_versions` 三行，各自 `content_hash` 不同），当前 `skills_registry.active_version` 指向 v3；(b) 在**新 session s_9** 里这个 skill 的**激活状态**是什么？设计文档 §9 规定 `skill_installations` 是 user 维度的（安装=可用），但「安装 ≠ 本 session 自动加载进 prompt」——本场景揭示「per-session activation」这一层语义缺失（见 §8 建议三）。 |

## 3. 会话时间线

10 回合覆盖：T1 模糊提问 → T2 session 9 本地 miss → T3 跨 session structured 检索 → T4 FTS
+ vector → T5 raw_ref load s_5 的 decision + provenance render → T6 用户确认 → T7 agent 检
测到 s_7 的个人 skill 未激活 → T8 agent 询问激活 → T9 用户同意，skill 注入 prompt，DDL 生
成 → T10 artifact 落库 + user-level memory 沉淀建议（揭示 gap）。

今天是 **2026-05-06（周三）**，session s_9 的 `started_at=09:30`，user_id=`u_lin`。

### T1 · 09:31 小林新开 session 提问

**用户原话**：「我要给 pricing 表加个新列，叫 `regional_markup_rules`，每个地区一套加价规
则。但我好像 3 个月前某次聊过为什么不用 JSON 列，帮我找一下那次的理由，我快忘了——不
然我又要重新论证一遍。」

**agent 动作**：ContextAssembler 识别为**显式跨 session 历史请求**（触发词：「3 个月前」「某
次聊过」「那次」+ 与 session 9 `started_at` 差值 > 当前 session 寿命）。解析为
`RetrievalRequest { scope: 'cross_session', user_id: 'u_lin', time_window: [2026-02-05,
2026-05-05], topic_hints: ['JSON column', 'pricing', 'schema evolution'], artifact_hints:
['decision'] }`。写 `session_state_items(scope='session', category='retrieval_intent')`。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1100 / `session_anchor` 250（session 9
目标：pricing 加列）/ `plan_todo` 0（新 session 暂无 plan）/ `recent_tail` 120（只有用户这一
句）/ `summary` 0。总 ≈ 2670，`reason='cross_session_retrieval_intent'`。

**LLM 看不到**：其他 7 个 session 的任何内容——session 9 的 prompt 里目前零跨 session 字节。

### T2 · 09:31 session 9 本地 context miss（正确行为）

**用户原话**：（无，agent 继续）

**agent 动作**：先在 session 9 内部 fallback 搜一次——符合「不要假设是跨 session」的安全
默认。查 `session_state_items WHERE session_id='s_9' AND category IN ('decision','summary')`，
命中 0 行（session 9 刚开 30 秒，正确）。也查 `session_history_chunks WHERE session_id='s_9'`，
命中 0 行。**正确地** miss——这里 scope='session' 的隔离是设计文档明文规定。

**具体查询**：

```sql
SELECT item_id, category, item_key, title, summary_text
FROM session_state_items
WHERE session_id = 's_9' AND scope = 'session'
  AND category IN ('decision','retrieval_cache','summary')
  AND status = 'active'
ORDER BY updated_at DESC LIMIT 20;
-- result: 0 rows
```

**manifest zone**：无变化。`reason='local_scope_miss_expected'`。

**LLM 看不到**：本步骤是 retrieval 内部探测，尚未进入 prompt。

### T3 · 09:32 跨 session structured 检索（揭示索引 gap）

**用户原话**：（无）

**agent 动作**：升级 scope 到 `cross_session`。在 `session_history_chunks` 上发第一级查询：

```sql
-- 跨 session structured filter：user_id + chunk_type + time range
SELECT chunk_id, session_id, chunk_type, source_table, source_id,
       title, preview_text, raw_ref, content_hash, created_at,
       importance, token_estimate
FROM session_history_chunks
WHERE user_id = 'u_lin'
  AND chunk_type = 'decision'
  AND created_at BETWEEN '2026-02-05 00:00:00' AND '2026-05-05 23:59:59'
ORDER BY importance DESC, created_at DESC
LIMIT 50;
-- returned: 31 rows（真实 decision chunks）
```

**索引行为**（关键压力点）：现有索引 `idx_history_type_created (session_id, chunk_type,
created_at)` **以 session_id 为前导**——本查询无 session_id 谓词，优化器选了 `idx_history_
source (source_table, source_id)` 做间接路径或直接退化为**按 user_id 做 range scan 再过滤**；
实测走 `user_id` 无索引 → 全表 range scan on session_history_chunks（该 user 名下 ~9.6 万行），
耗时 420ms，远超 structured SLA 50ms。**这就是 S11 揭示的 A3 gap**：跨 session 检索**缺
少** `(user_id, chunk_type, created_at)` 复合索引。agent 能拿到 31 条候选（结果正确），但成
本远超预算。写 `agent_events(event_type='retrieval.cross_session_slow_path',
payload={scanned_rows:96421, returned:31, elapsed_ms:420, missing_index:
'idx_history_user_type_created'})`。

**归并**：31 条候选按 `session_id` 聚合——s_3（分区策略）7 条、**s_5（schema 演进）12
条**、s_4（RBAC）5 条、s_8（容量规划）4 条、其他 3 条。按 `topic_hints` 先粗排：s_5 的
12 条里 7 条 title 含 "JSON"/"schema evolution"/"pricing"。

**manifest zone**：候选暂存 retrieval scratch，不渲染。`reason='cross_session_structured_scan'`。

**LLM 看不到**：31 行 preview_text 原文；其他非 decision chunk（assistant_message / tool_call
等，跨 8 session 合计 ~9.6 万行）。

### T4 · 09:32 FTS 二级过滤 + vector rerank

**用户原话**：（无）

**agent 动作**：把 T3 的 31 个候选用 FTS 再收敛。为避免再一次跨 user 全表扫，这一级
**带着 T3 的 chunk_id list 做 IN 过滤**——把 FTS 限制在已命中的 31 行里。

```sql
-- 二级：在 T3 候选上做 FTS（IN 过滤避免全表）
SELECT c.chunk_id, c.session_id, c.raw_ref, c.content_hash,
       c.preview_text, c.created_at,
       MATCH(c.preview_text)
         AGAINST ('+JSON +("pricing" "schema" "column" "evolution")' IN BOOLEAN MODE) AS score
FROM session_history_chunks c
WHERE c.chunk_id IN (<31 candidates>)
  AND MATCH(c.preview_text)
        AGAINST ('+JSON +("pricing" "schema" "column" "evolution")' IN BOOLEAN MODE)
ORDER BY score DESC, c.importance DESC
LIMIT 5;
-- returned: 3 rows, all from session_id='s_5'
```

FTS 命中 3 行全部来自 s_5：chunk_ids = `chk_s5_0328_decision_json_vs_columnar` (score 12.4) /
`chk_s5_0329_benchmark_json_path_pricing` (8.1) / `chk_s5_0330_summary_schema_evolution` (4.7)。
耗时 90ms，符合 FTS SLA 200ms。

**vector rerank**：对这 3 行各自 `chunk_id` 查 embedding 表，比对 `content_hash` vs
embedding 的 `indexed_content_hash`——3 行全部一致（s_5 close 时 async indexer 已批量生
成过，且之后没有 compaction 改写）。rerank top-1 稳定在 `chk_s5_0328_decision_json_vs_
columnar`，top-2 是 benchmark。

**manifest zone**：无新增。`reason='fts_plus_vector_ok'`。

### T5 · 09:33 raw_ref 跨 session load + provenance render

**用户原话**：（无）

**agent 动作**：按 top-2 的 raw_ref 精确加载。raw_ref 指向的是 `session_artifacts://
artifact:decision-s5-20260328-json-vs-columnar`——**跨 session load 一个 artifact 行**。
`session_artifacts` 不以 session_id 为权限边界（user_id 才是），PK lookup 12ms 命中。

```sql
SELECT artifact_id, session_id, title, content_text, byte_size, content_hash, created_at
FROM session_artifacts
WHERE artifact_id IN (
  'artifact:decision-s5-20260328-json-vs-columnar',
  'artifact:benchmark-s5-20260329-json-path-pricing'
) AND user_id = 'u_lin';
-- returned 2 rows, total 23KB content_text
```

**render 策略**：两份 artifact 各自压成 retrieved_facts zone 条目，**必须**携带
`source_session_id` 作为 provenance 字段。decision artifact 原文里有 8 条 Rust/MatrixOne
社区帖子讨论 + 2 条 benchmark 数据（JSON_EXTRACT 路径查询延迟 vs 结构化列 6x slower），
agent 判定：完整保留决策摘要 + 2 条 benchmark 数字；8 条讨论折叠为 "see artifact"。

```text
[retrieved_facts zone]
  - 决策摘要（render_mode=narrative，~600 tokens）
      "2026-03-28 session s_5 决策：pricing / metrics 类高频查询表
       拒绝 JSON 列。理由：(1) MatrixOne JSON_EXTRACT 路径查询相比
       结构化列 p99 延迟 6.2x；(2) JSON 列无法有效走向量/全文索引；
       (3) 审计上 schema drift 难回溯。采用方案：结构化列 + 维度
       扩展表 (tenant_id, region) 关联。"
      provenance: source_session_id=s_5, chunk_id=chk_s5_0328_decision_json_vs_columnar,
                  artifact_id=artifact:decision-s5-20260328-json-vs-columnar,
                  decision_date=2026-03-28, content_hash=a1f0...92bc
  - Benchmark 数字（render_mode=table，~150 tokens）
      provenance: source_session_id=s_5, chunk_id=chk_s5_0329_benchmark_json_path_pricing
```

**manifest zone**：`system_static` 1200 / `tool_schemas` 1100 / `session_anchor` 250 /
`recent_tail` 300 / `retrieved_facts` 780（600 决策 + 150 benchmark + 30 provenance header）
/ `summary` 0。总 ≈ 3630，`reason='cross_session_retrieval_hit'`，`dropped_count=29`（31-2）。

**LLM 看不到**：s_5 的其他 10 条 chunk；其他 7 个 session 的任何字节；s_5 里 decision 原
artifact 里的 8 条外部讨论原文。

### T6 · 09:34 用户确认 + 派生任务

**用户原话**：「对，就是这个。6.2x 那个数字我有印象。那现在基于这个决策，我要给 pricing 表
加一个 `regional_markup_rules` 列——按你刚才说的结构化方案，写 DDL。区域我们定为 NA /
EU / APAC 三个 enum。」

**agent 动作**：锁定决策为当前 turn 的依据，写 `session_state_items(scope='session',
category='anchor', item_key='active_decision_ref', payload_json={source_session_id:'s_5',
chunk_id:'chk_s5_0328_...', cited_at:'2026-05-06T09:33'})`。追加 todo `"draft-pricing-regional-
markup-ddl"`，priority=high。

**manifest zone**：`session_anchor` 450（+200 绑定 s_5 决策引用）/ `plan_todo` 80（1 条新
todo）/ `recent_tail` 450（含 T5 agent 回答 + T6 用户问）/ `retrieved_facts` 780（未变）/ 其
他维持。总 ≈ 3980，`reason='user_confirm_and_derive_task'`。

### T7 · 09:34 agent 发现 s_7 有未激活的个人 skill

**用户原话**：（无）

**agent 动作**：在生成 DDL 之前做一次 **skill 解析**。查 `user_skill_sources` + 按
`idx_user_skill_versions_active` 找当前 user 名下 status='active' 的 skill：

```sql
-- 查 user 已发布并保持 active 的所有个人 skill
SELECT s.skill_name, s.source_id, v.version_id, v.version, v.content_hash,
       v.summary_text, v.token_estimate
FROM user_skill_sources s
JOIN user_skill_versions v ON v.source_id = s.source_id AND v.status = 'active'
WHERE s.user_id = 'u_lin' AND s.status = 'active';
-- returned 1 row: skill_name='matrixone-schema-guide', version='v3',
--                 token_estimate=1800, source_id='usk_src_lin_mo_schema'
```

该 skill 在 s_7 发布，`skills_registry.owner_user_id='u_lin'`，`skill_installations(user_id=
'u_lin')` 有一行 `installed_at=2026-04-22`，即**全局可用**。

但 agent 进一步查 session 9 内部的激活状态：

```sql
SELECT item_id, payload_json FROM session_state_items
WHERE session_id='s_9' AND scope='session'
  AND category='active_skill' AND status='active';
-- returned 0 rows
```

session 9 本地**没有**激活任何个人 skill——与 S02 / S01 的惯例一致，skill 在新 session 默认
**不自动激活**（避免每次都把 ~1800 token 顶进 prompt）。这里的**语义空白**：设计文档
§9 规定 skill_installations 是 user 维度的「可用」，但**每 session 是否自动激活**并无明文
（见 §8 建议三）。

**manifest zone**：未变，本步仅内部解析。`reason='skill_resolver_scan'`。

### T8 · 09:35 agent 询问用户是否激活 skill

**用户原话**：（无，agent 主动提问）

**agent 动作**：生成一条 assistant 消息询问用户：

```text
检测到您在 2026-04-22 (session s_7) 发布过个人 skill
  `matrixone-schema-guide` v3（~1800 tokens）
内容覆盖：enum 列命名、分区键选择、向外扩展维度表的命名约定等。
当前 session s_9 未激活该 skill。是否本 session 激活？
  [a] 激活（用于本 session 全部后续 turn）
  [b] 仅本次 DDL 生成使用（one-shot，token 只花一次）
  [c] 不激活
```

写 `agent_events(event_type='skill.activation_prompt', payload={skill_name:
'matrixone-schema-guide', version:'v3', session_id:'s_9', suggested:'b'})`。`suggested='b'`
是 agent 的默认建议（one-shot 更省 token，且该 skill 主要跟 DDL 相关）。

**manifest zone**：`recent_tail` 700（+250 此问）/ 其他维持。总 ≈ 4230，`reason=
'skill_activation_ask'`。

**LLM 看不到**：skill 的 content_markdown 原文（尚未注入，只有 summary_text 和 token 估算
参与了 agent 的内部决策）。

### T9 · 09:36 用户同意激活 + DDL 生成

**用户原话**：「[a]，本 session 全程激活。这个 skill 就是我 3 个月前总结的，早该默认用了。」

**agent 动作**：
1. 写 `session_state_items(scope='session', category='active_skill', item_key='matrixone-schema-
guide', payload_json={version_id:'usv_v3', content_hash:'...', activated_at:now}, status=
'active')`。
2. 加载 `user_skill_versions.content_markdown`（~1800 tokens）进入 prompt 的 `skills` zone。
3. 基于决策摘要 + skill 内容 + 用户要求，生成 DDL：

```sql
-- 来自 skill matrixone-schema-guide v3 的命名约定 + s_5 决策
-- 方案：拒绝 JSON 列，结构化 + 维度扩展
ALTER TABLE pricing
  ADD COLUMN region ENUM('NA','EU','APAC') NOT NULL DEFAULT 'NA',
  ADD COLUMN regional_markup_bps INT NOT NULL DEFAULT 0
    COMMENT 'basis points; 100 = 1.00%';
CREATE INDEX idx_pricing_region_markup ON pricing (region, regional_markup_bps);

-- 若存在跨维度的细粒度规则，落维度表：
CREATE TABLE IF NOT EXISTS pricing_regional_rules (
  rule_id        VARCHAR(64) PRIMARY KEY,
  tenant_id      VARCHAR(64) NOT NULL,
  region         ENUM('NA','EU','APAC') NOT NULL,
  product_class  VARCHAR(64) NOT NULL,
  markup_bps     INT NOT NULL,
  effective_from TIMESTAMP NOT NULL,
  effective_to   TIMESTAMP NULL,
  created_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_prr_tenant_region (tenant_id, region, effective_from),
  INDEX idx_prr_region_class (region, product_class)
);
```

DDL 头部注释**显式**写两条 provenance：`-- Decision origin: session=s_5, chunk=chk_s5_
0328_decision_json_vs_columnar` 和 `-- Skill applied: matrixone-schema-guide v3
(hash=<v3_hash>)`。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1100 / `session_anchor` 450 / `plan_
todo` 80 / `recent_tail` 900 / `retrieved_facts` 780 / `skills` 1800（新增，skill v3 整段） /
`tool_previews` 650（生成的 DDL preview）/ `summary` 0。**总 ≈ 6960**，`reason=
'skill_activated_ddl_generated'`。

**LLM 看不到**：除 v3 外的 v1/v2（archive 在 user_skill_versions 里），其他 7 个 session 的原文。

### T10 · 09:38 artifact 落库 + user-level memory 建议（揭示 gap）

**用户原话**：「跑过了，DDL 没问题。存下来吧。另外——你刚才跨 session 找到 s_5 那个决
策的方式很好。我以后每次开新 session 都要重新找一遍吗？这个 JSON 列的决策，应该算我**整
个数据平台的通用约束**，不是 s_5 一个 session 的事。」

**agent 动作**：
1. 落 DDL artifact：`INSERT INTO session_artifacts (artifact_id='artifact:ddl-s9-20260506-
pricing-regional-markup', user_id='u_lin', session_id='s_9', content_text=<DDL>, byte_size=
2840, content_hash='7b3a...c4e1')`。
2. 写新 chunk：`session_history_chunks(chunk_type='artifact_text', source_table='session_
artifacts', source_id=<artifact_id>, raw_ref=..., indexed_at=NULL)` — lazy index。
3. 绑定 provenance chain 到 manifest_items：新 DDL artifact `included=TRUE`，
`artifact:decision-s5-20260328-json-vs-columnar` 和 `user_skill_versions/v3` 各写一条
`included=FALSE, reason='derived_from_reference'`（保留 source_hash 以便审计回溯）。

然后**面对用户的第二问**：agent 主动识别为「user-level memory 沉淀请求」，但发现**设计
文档的 `session_state_items.scope` 默认只有 `'session'`**——虽然 schema 允许 `VARCHAR(32)`，
但全文只定义了 session 级投影。agent 的回复必须**诚实暴露这个 gap**：

```text
您说得对。目前系统层面：
  - 可立即做：在 `session_state_items` 写一条 scope='user'（schema 支持）
    的 decision_anchor，item_key='mo-schema.reject-json-in-hot-tables'，
    payload 指向 s_5 chunk_id。下次您开新 session，ContextAssembler 在
    初始化阶段可以加载 scope='user' 的 user-level memory 作为 anchor。
  - 但 **风险**：当前设计文档没有明文定义 scope='user' 的加载策略（哪些
    category / 什么 token 预算 / 如何过期）。若我现在写入，行为是
    不确定的——可能被 GC，可能不加载。
  - 建议：我写一条 `suggested_user_memory_item` 到 session 9 的
    state，等设计文档明确后再由您或管理员批量沉淀。
请确认方案 A（激进：立即写 scope='user'）或 B（保守：仅 suggested）。
```

写 `session_state_items(scope='session', category='suggested_user_memory',
item_key='mo-schema.reject-json-in-hot-tables', status='pending',
payload_json={origin_session_id:'s_5', origin_chunk_id:'chk_s5_0328_...', proposed_scope:
'user', proposed_category:'durable_decision'})`。

**manifest zone**：`tool_previews` 900（+250 新 DDL artifact preview）/ `recent_tail` 1200 /
`retrieved_facts` 780（未变）/ 其他维持。总 ≈ 7580，`reason='artifact_commit_with_
user_memory_suggestion'`。

**LLM 看不到**：写入 scope='user' 行之后在 **其他未来 session** 的加载时机（属于未来事件，
本轮不决定）。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 跨 8 session ~9.6 万 chunks 一次 user 级扫描 | A3：`session_history_chunks` 有 user_id 列但无 `(user_id, chunk_type, created_at)` 复合索引，T3 查询必须退化为 user_id range scan + 列过滤；**这是设计文档的索引 gap** |
| FTS 如何不跨 user 泄漏 | 二级 FTS 加 `c.chunk_id IN (<T3 candidates>)` 语义上隐含 user 过滤（T3 已限制 user_id）；不允许单独的 global FTS 检索 |
| 跨 session raw_ref 的权限边界 | session_artifacts 行 PK 查，但**必须** JOIN / WHERE `user_id=?`——一个 user 的 artifact 不能被另一个 user 读 |
| provenance 必须含 source_session_id | 否则用户无法判断"哪个 session 的决策"；retrieved_facts zone 的 render header 强制要求 |
| 个人 skill 激活时机 | A12：skill_installations 表示「可用」，per-session activation 通过 `session_state_items(category='active_skill')` 显式写入；设计文档未明文该 category |
| user-level memory 沉淀缺口 | L3 durable memory 在 §State Layers 里列了"future memory tables"，但 `session_state_items.scope` 默认只设 session — 语义缺口 |
| token 预算变动 | skill 激活瞬间注入 1800 token，必须由 BudgetAllocator 提前留出 `skills` zone 预算并在其他 zone 自动 squeeze |

## 5. 预期 state projection 演化

**T3 结束（跨 session structured 扫描完成）**：

| 投影 | rows touched | 说明 |
| --- | --- | --- |
| session_history_chunks | 96421 range scan → 31 匹配 | **退化为 user_id 级 range scan**（索引 gap） |
| context_manifests | 1 写（header） | reason='cross_session_structured_scan' |
| context_manifest_items | 0 写 | 本阶段无 LLM turn |
| agent_events | 1 写 | retrieval.cross_session_slow_path，payload 含 scanned_rows |
| retrieval scratch buffer | 31 行 | 按 session_id 聚合为 5 簇 |

**T5 结束（s_5 raw_ref load 完成）**：

| 投影 | rows touched | 说明 |
| --- | --- | --- |
| session_history_chunks | 31 + 3（FTS in-set）= 34 | 全部走索引/IN 过滤 |
| session_artifacts | 2 PK lookup (user_id + artifact_id) | 跨 session load s_5 artifact |
| context_manifest_items | 2 写（included=TRUE，zone=retrieved_facts） | 含 source_session_id='s_5' 字段 |
| session_state_items | 1 写（anchor.current_retrieval_target） | scope='session' |
| agent_events | 1 写 | retrieval.cross_session_hit，provenance.session_id='s_5' |

**T9 结束（skill 激活 + DDL 生成）**：

| 投影 | rows touched | 说明 |
| --- | --- | --- |
| user_skill_sources / user_skill_versions | 1 + 1 读 | 按 idx_user_skill_versions_active |
| session_state_items | 1 写（active_skill） | scope='session', category='active_skill' |
| skills_registry | 1 读 | 验证 owner_user_id + active_version |
| context_manifest_items | 1 写（zone='skills', source=user_skill_versions/v3） | token_estimate=1800 |

**T10 结束（artifact + suggested user memory）**：

| 投影 | rows touched | 说明 |
| --- | --- | --- |
| session_artifacts | 1 insert | DDL artifact |
| session_history_chunks | 1 insert（indexed_at=NULL） | lazy index |
| context_manifest_items | 3 写 | 新 artifact + s_5 decision ref + skill v3 ref |
| session_state_items | 1 写（**pending** suggested_user_memory） | scope='session'，揭示 gap：无法直接写 scope='user' |

**关键不变量**：除 T3 的 96421 行 range scan 之外，其他所有跨 session 读都走索引或 PK；T9
skill 激活后，`session_state_items.scope='session'` 的写入隔离严格维持；**无** scope='user'
的写入发生（因为设计未定义）。

## 6. Token 节省估算

**朴素方案**（把 8 个 session 的全部 transcript 塞给 LLM）：

- 跨 8 session 合计 ~3.1GB UTF-8 文本 / ~4 bytes/token ≈ **7.75 × 10^8 tokens**
- 即使 Gemini 1.5 Pro 2M 窗口，也差 **~390 倍**。**物理不可行**。

**伪方案 A**（「人工翻旧 session」，小林自己搜）：

- 小林要在 web UI 里打开 s_1..s_8 每个 session 的 transcript，ctrl-F "JSON"——估算 15–25 分钟
  纯 UI 翻页时间。这不是 agent 的方案，是用户放弃 agent 的 fallback，**体验失败**。

**伪方案 B**（用 session 9 自然语言问并依赖 LLM 自己回忆）：

- 新 session s_9 的 LLM 根本不知道小林是谁、之前聊过什么；会基于通用知识给建议（可能
  正确但**无法引用小林自己的决策**，provenance 缺失）。用户信任度崩坏。

**本设计方案**（实际 T9 prompt token 分解，按 zone）：

| zone | T9 tokens | 计算依据 |
| --- | --- | --- |
| system_static | 1200 | 常驻 |
| tool_schemas | 1100 | 剪枝后的 DBA 工具子集 |
| session_anchor | 450 | session 9 目标 + s_5 决策引用 anchor |
| plan_todo | 80 | 1 条活跃 todo |
| recent_tail | 900 | T1..T9 本次对话 |
| summary | 0 | session 9 太短无 summary |
| retrieved_facts | 780 | s_5 决策 600 + benchmark 150 + provenance 30 |
| skills | 1800 | matrixone-schema-guide v3 全文 |
| tool_previews | 650 | DDL 首段 preview |
| **prompt 总计（T9）** | **≈ 6960** |  |

**对比表**：

| 方案 | 目标细节可达性 | prompt tokens | 用户体验 |
| --- | --- | --- | --- |
| 朴素（全塞 8 session） | 不适用 | 7.75 × 10^8 | 物理不可行 |
| 伪方案 A（人工翻 UI） | hit 但 15–25 分钟 | 0（不调 LLM） | **放弃 agent** |
| 伪方案 B（让 LLM 凭感觉） | **无 provenance** | ~3k | 信任崩坏 |
| 本设计（跨 session 三级检索 + skill 激活） | **hit + provenance** | ≈ 6960（T9） / ≈ 7580（T10 峰值） | 成功 |

**节省计算**：相对朴素方案节省 `(7.75e8 − 6960) / 7.75e8 ≈ 99.9991%`——但真正关键的不是
百分比，而是把**物理不可行**转为**可运行**。相对伪方案 B，新增 ~4k token 换来的是「引用
小林自己 3 个月前的决策 + 激活他自己写的 skill」，provenance 完整。

**可复用性**：T10 的 suggested_user_memory 若未来升级为 `scope='user'` 的真实沉淀，下次小
林开 session s_10 时 ContextAssembler 可以在 **anchor zone 直接加载**（预计 ~200 token），
彻底免除 T3 的 ~400ms 跨 session 扫描——形成「长期决策进入 session 初始 anchor」的闭环。

## 7. 潜在失败模式

**F1 · session_history_chunks 跨 session FTS 缺复合索引 → user 级全扫**：现有索引均以
session_id 为前导。T3 的 `WHERE user_id=? AND chunk_type='decision' AND created_at BETWEEN
? AND ?` 在该 user 名下 ~9.6 万行里 range scan，耗时 420ms。若该 user 长期重度使用 agent
（如 1 年后 chunks ~百万级），该路径会进一步恶化到秒级。**可观测信号**：`agent_events` 有
`retrieval.cross_session_slow_path`，`payload.scanned_rows / returned_rows > 1000`；Explain
看到 `key=NULL` 或仅 `idx_history_source` 间接走。**缓解**：补复合索引 `(user_id,
chunk_type, created_at)`（见 §8 建议二）。

**F2 · 个人 skill 版本跨 session 激活的 race**：小林在 s_7 结束后，若 s_9 激活的是 v3 ，而他
中途又在另一设备开了 s_10 且发布 v4，`skills_registry.active_version` 切到 v4 的瞬间， s_9 的
prompt 仍持有 v3 的 content（已渲染）。后续 turn 重新解析时若按「激活 = 跟随 registry 最
新」会突然切 content，prompt cache 全 miss。**可观测信号**：同 session 两个 turn 的
`skills` zone `source_hash` 不一致；prompt_cache_hit_rate 骤降。**缓解**：`session_state_items
(category='active_skill')` 激活时**固定 version_id**（快照语义），不跟 registry 自动 follow；
若用户要升级，显式写新的 `active_skill` 行（新 version_id）。

**F3 · 跨 session decision 召回精度：同 user 多项目混串**：假设小林除 MatrixOne 数据平台
外还有一个**独立的推荐系统项目**，也讨论过 JSON 列。T3 的 structured filter 只按 user_id +
chunk_type + time，会把推荐系统的 JSON 讨论也召回。T4 的 FTS `+JSON +("pricing" ...)` 能
大部分过滤掉，但边界情况（推荐系统也有一个叫 `item_pricing` 的表）会击穿。**可观测信
号**：T3 候选跨多个不相关的 session cluster；FTS top-1 与 top-2 score 差距 < 20%；用户 T6
纠错频率升高。**缓解**：`session_history_chunks` 增加 `project_id` 或 `tag` 列（或沿用
`session_state_items.category` 维度作为 project tag 下钻）；retrieval 加一级 project filter。

**F4 · session_state_items.scope 缺 'user' 定义 → T10 无处落地**：当前设计文档 §State
Layers 表的 L3 明写"future memory tables"，scope 字段 schema 支持但无文字定义。T10 的
suggested_user_memory 只能以 scope='session' pending 状态暂存；若用户跨月再开 session 希
望直接看到这条决策，当前 ContextAssembler 不会去读另一个 session 的 state_items（违反
scope 隔离）。**可观测信号**：用户反复跨 session 问同一个决策，agent 每次都走 T3 慢路径；
`suggested_user_memory` 在多个 session 里重复出现但从未 promote。**缓解**：§8 建议一，引入
`scope='user'` 的语义定义或新表 `user_memory_items`。

**F5 · user_preferences 与 decision memory 混用（语义不清）**：设计文档 §State Layers 把
`user_preferences` 归到 L3 durable memory。如果实现时把「拒绝 JSON 列」这种**技术决策**
写入 user_preferences（「用户偏好结构化列」），会与真正的偏好（主题色、时区、默认模型）
混成一张表。下次查询时难以分辨哪条是偏好、哪条是决策 memory。**可观测信号**：
`user_preferences` 表行数在"偏好 + 决策"两种语义之间语义分布失衡，category 列出现大量
`decision`/`engineering_rule` 取值。**缓解**：§8 建议一里把 decision memory 单独承载。

**F6 · 跨 session 检索的权限：多 org 场景**：小林同一个 user_id 可能在两个 org（employer +
personal）都有 session，scope 层用 user_id 做边界就会把 personal 的 JSON 列实验也召回给
employer 的工作 session。**可观测信号**：T3 返回的 session_id 跨 `org_id` 边界；审计日志有
`cross_org_retrieval_warning`。**缓解**：`session_history_chunks` / `session_artifacts` 增加
`org_id` 列；retrieval 必须同时过滤 `user_id + org_id`，以 org 为隔离硬边界。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §State Layers L3 durable memory — 写了"future memory tables"但未落地；本场景 T10 的
   sugessted_user_memory 没处可写。跨 session decision 这一重要 memory 类别的归属不清。
2. §History Chunks and Retrieval Index — 给了 4 个 index 但**全部以 session_id 为前导**；
   没有为「同 user 跨 session」设计索引路径，T3 的压测直接暴露。
3. §Personal Skills — 定义了 `user_skill_sources` / `user_skill_versions` 和 `skills_registry`，
   说明了 publish 流程，但**未定义 per-session activation 的显式语义**：installed 是否 ≠
   auto-activated？激活是 session scoped 还是 user scoped？T8 的询问/激活动作落在
   `session_state_items(category='active_skill')` 上是 reasonable 推断，但文档没写死。

**三条具体建议**：

- **引入 `scope='user'` 的 session_state_items（或新表 user_memory_items）**：明确定义两类
  scope。scope='user' 的 category 白名单包括 `durable_decision` / `engineering_rule` /
  `rejected_pattern` 等；载荷含 `origin_session_id` + `origin_chunk_id` 作为来源 provenance。
  ContextAssembler 初始化任何新 session 时，按 `(user_id, scope='user', status='active')`
  加载进 anchor zone，预算单独 ≤ 400 token。若列表长则用 `priority` 字段 rerank。这样 T10
  的 suggested 可以一键 promote 到真正的 user memory，下次新 session 免 T3 慢路径。
- **`session_history_chunks` 补复合索引 `(user_id, chunk_type, created_at)`**：现有 4 个索引
  全部以 session_id 前导，跨 session 查询无法利用。补一个 `idx_history_user_type_created`
  即可把 T3 从 420ms 降到 < 50ms。同时在检索层规约：跨 session 查询**必须**带
  `user_id` 谓词，缺失即报 `CrossSessionAuthMissing`；组合 `chunk_type` 过滤是 SLA 保证的
  前提。
- **明确个人 skill 的 session 激活语义**：在 §9 Personal Skills 补一段：`skill_installations`
  表示「该 user 在当前 workspace 可用该 skill」，但**不等于**每个新 session 自动加载进
  prompt；per-session 激活由 `session_state_items(scope='session', category='active_skill',
  item_key=<skill_name>, payload_json.version_id=<frozen>)` 显式承载；加载策略：new
  session 默认不激活，由 agent 按 topic hit 主动建议（如 T8），或用户设置 user-level
  preference `auto_activate_skills_on_topic_match=true` 启用自动激活。激活后 `version_id`
  固定到具体快照，不跟 registry `active_version` 自动切换（见 §7 F2）。
