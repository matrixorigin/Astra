# S02 — SRE 阿强找 4 个月金融运维 session 里上个月那次 Kafka consumer lag 的修复记录

> Status: Draft v0.1
> DAUT: **A3, A6, A10**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：阿强，某持牌支付公司 SRE，负责 payment / order / clearing 三条核心链路的线上稳定性。astra
web agent 当「随身日志 + 排障副驾」用：每次处理告警、跑 SQL、改配置、写修复脚本都在同一个
session 里问答。不开新 session——他的习惯是「一个系统一个 session，方便回溯」。

**业务背景**：这个 session 从 2026-01-10 开起到今天 2026-05-06 已经 4 个月不停线，transcript +
tool_output 原始字节 ~10GB（pg_dump 片段、kafka lag metric 时序、内网 grafana 截图 OCR 文本、
MatrixOne 慢查询日志、若干 500MB 级 pcap）。today 阿强要处理一个新告警，但他想起上个月踩过
一个**一模一样性质**的坑——Kafka consumer lag 飙升导致下游数据错位。他想把当时**完整的排查
链路 + 修复 SQL** 抽出来作为模板，而不是从头再调查一遍。

**一句话目标**：在 10GB 老 session 里，**不 LLM 回放、不全量扫表**，用三级检索定位到上个月那
一次排查，拉出原始 SQL 修复脚本，生成一个可复用的批处理 check。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 本次交互总回合 | 10 回合 |
| Session 累计天数 | 117 天 |
| 累计 chunk 数（session_history_chunks） | ~68 万（`SELECT COUNT(*)` from session_history_chunks where session_id = ?） |
| 累计 tool_output 行数 | ~21 万 |
| 累计 session_artifacts | ~9.4 千 |
| 历史触发过 compaction 次数 | 37 次 |
| 原始 transcript + tool_output 字节 | ~10.3 GB |
| 本次交互 prompt token 峰值（T7） | ≤ 20k |

## 2. 被测 DAUT

S02 的压测点是「很老 + 很大 + 要找准一条细节」，三条假设同时吃压力。

| 编号 | 压测方式 |
| --- | --- |
| **A3** | 整轮交互**绝不允许**对 conversation_log / session_tool_outputs 做无界扫描。所有检索第一跳必须落在 `session_history_chunks` 的结构化索引上：`idx_history_type_created (session_id, chunk_type, created_at)` 做时间+chunk_type 有界扫描，上限 1000 行；`idx_history_session_created` 做纯时间窗切片。T2 的候选集规模 ≤ 50，T5 加 FTS 后 ≤ 5，T6 按 raw_ref 精确 load 1 个 artifact 或 conversation_log 切片。任何一步返回 > 2000 行必须中断并报 `RetrievalBoundExceeded`。 |
| **A10** | 三级检索**严格分层** + 每级有 SLA：(1) structured filters 走 session_history_chunks 列索引，目标 < 50ms；(2) 不够或模糊再上 full-text，目标 < 200ms；(3) 还不够再上 vector，目标 < 500ms 且**不做**频繁 UPDATE/DELETE（embedding 表用 append + 软删）。FTS 命中后**按 raw_ref 精确拉原始**，不再扫。**vector 过期（content_hash 漂移）必须降级**回 FTS，不允许用脏 embedding 返回错误 chunk。 |
| **A6** | 4 个月里 compaction 触发了 37 次，老的 `category='decision'` / `category='error_state'` / `chunk_type='decision'`/`chunk_type='error'` 行**必须**以 `status='active'` 或 `status='archived'` 形式保留，绝不能被压缩成 L1b 自然语言后丢结构。上个月那次 Kafka lag 的 root cause decision + 修复 SQL 的 artifact 指针必须活着——否则 T2 结构化过滤直接 miss，整个检索链断。 |

## 3. 会话时间线

10 个回合覆盖：T1 模糊请求 → T2 一级 structured 检索 → T3 候选摘要 → T4 用户澄清 → T5 加
FTS 二次检索（含 vector 降级） → T6 raw_ref 精确 load → T7 渲染到 retrieved_facts + 持久化
manifest → T8 用户追问生成 check 脚本 → T9 patch 写出 → T10 artifact 落库与 provenance 绑定。

每回合字段：**用户原话** / **agent 动作** / **具体查询（SQL 模板）** / **manifest zone 数字**
/ **LLM 看不到什么**。今天是 **2026-05-06（周三）**，「上个月」= 2026-04-06..2026-05-05。

### T1 · 09:12 阿强模糊提问

**用户原话**：「上个月我们查过一次 Kafka consumer lag 异常导致数据错位，你帮我写过修复脚本，
大概那周二或周三，是 payment 服务不是 order 服务。找一下完整排查记录和修复 SQL。」

**agent 动作**：ContextAssembler 识别到**显式历史请求**（触发词：「上个月」「查过」「写过」），
不走 recent_tail，直接进入检索规划。解析用户意图为 `RetrievalRequest { time_window: [2026-04-06,
2026-05-05], weekday_filter: [Tue, Wed], topic_hints: ["Kafka","consumer lag","数据错位"],
service_hints: ["payment"], artifact_hints: ["修复 SQL","脚本"] }`。写一条 `suggested_next_action`
说明「即将走三级检索，预算 4k token 给 retrieved_facts」。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1200 / `session_anchor` 400（金融运维 session
的目标+当前告警上下文）/ `plan_todo` 300（阿强长期活跃的 3 条 SRE checklist）/ `recent_tail` 600
（昨天的两轮） / `summary` 0。总 ≈ 3700。`reason='explicit_history_request'`。

**LLM 看不到**：10GB 全部历史，包括上个月那一轮对话本身——此轮 agent 只看得到「我要去检索」这
件事，还没读到任何老数据。

### T2 · 09:12 一级 structured 检索

**用户原话**：（无，agent 继续 T1 的 turn）

**agent 动作**：发起一级 structured 查询。**不**碰 conversation_log 全表，**不**读 session_tool_outputs
raw body。只走 `session_history_chunks` 的 `idx_history_type_created` 索引。

**具体查询**：

```sql
-- (1) 时间窗 + chunk_type 白名单，按 importance DESC, created_at ASC 取前 50
SELECT chunk_id, chunk_type, source_table, source_id, title, preview_text,
       raw_ref, content_hash, created_at, importance, token_estimate
FROM session_history_chunks
WHERE session_id = ?
  AND created_at BETWEEN '2026-04-06 00:00:00' AND '2026-05-05 23:59:59'
  AND chunk_type IN ('error','tool_output_preview','decision')
  AND DAYOFWEEK(created_at) IN (3, 4)          -- Tue=3, Wed=4（MySQL/MO 语义）
ORDER BY importance DESC, created_at ASC
LIMIT 50;
```

走 `idx_history_type_created (session_id, chunk_type, created_at)` + secondary
`idx_history_session_created` 复合裁剪，**返回 41 行**，耗时实测 38ms。41 行里归并出 **3 个时段
候选**（按 created_at clustering）：C1=2026-04-08 周三 下午（order 服务 DLQ 堵塞事件），C2=
2026-04-15 周三 深夜（payment 服务 consumer lag + 对账数据错位），C3=2026-04-22 周二 上午
（clearing 服务 rebalance 抖动）。

**manifest zone**：无新增——这一轮只是检索器内部动作，返回的 41 行**暂存 retrieval scratch**，不
直接进 prompt。token 开销：preview_text 平均 180 tokens × 41 = 7.4k，**仅在 retrieval layer 内部
参与 rerank**，不渲染给 LLM。`context_manifests.reason='retrieval_scan_only'`，
`dropped_count=38`（41-3 归并后丢掉）。

**LLM 看不到**：41 行 preview_text 原文，raw_ref 指向的任何 artifact body，其他时间窗内不属于
三类 chunk_type 的行（assistant_message / tool_call 等，数量级 ~5 万 / 月）。

### T3 · 09:13 agent 给 3 候选摘要请用户澄清

**用户原话**：（无）

**agent 动作**：把 3 个候选压成结构化摘要，进入 prompt 的 `retrieved_facts` zone；此时 **不**
load raw，仅用 chunk.title + 首行 preview_text。生成 assistant 消息问用户："找到 3 个候选时段，
哪一个是你说的 payment 服务那次？"

**候选摘要**（prompt 里实际渲染）：

```text
[C1] 2026-04-08 (Wed) 15:42  source=session_tool_outputs  chunk_type=error
     title: "order-svc consumer group lag > 30k, DLQ stuck"
     preview: "order-service 的 payment-event-topic consumer group lag 飙到 32k..."
[C2] 2026-04-15 (Wed) 23:17  source=session_artifacts      chunk_type=decision
     title: "payment-svc lag root cause = offset commit 风暴 + 双写幂等漏洞"
     preview: "确认 payment-service consumer lag 是因为 K8s node 重启..."
[C3] 2026-04-22 (Tue) 10:05  source=session_tool_outputs  chunk_type=error
     title: "clearing-svc consumer rebalance storm, 对账数据延迟"
     preview: "clearing-service consumer group 反复 rebalance..."
```

**manifest zone**：`retrieved_facts` 420（3 条候选摘要，含 provenance：chunk_id + raw_ref）/
`session_anchor` 400 / `plan_todo` 300 / `recent_tail` 700 / system/tools 2400。总 ≈ 4220。
`reason='clarification_ask_user'`，`dropped_count=38`。

**LLM 看不到**：41 行里其他 38 条被 rerank 降权的 chunk；3 个候选各自的 raw_ref 指向的原始
日志/artifact body。

### T4 · 09:14 阿强澄清

**用户原话**：「C2，payment 服务那次。order 和 clearing 不是，你别看错了。我要的是 payment
consumer 那次 offset 错位 + 对账对不上那个。」

**agent 动作**：记录一条 `anchor` 子项（`item_key='current_retrieval_target'`，payload 指向 C2 的
chunk_id + raw_ref），置信度 high。进入二级检索。

**manifest zone**：新增 200 tokens 到 `session_anchor`（锁定 C2）；其他维持。总 ≈ 4400。
`reason='user_clarification_received'`。

**LLM 看不到**：C1、C3 的详细信息（已从 retrieved_facts 里剔除）。

### T5 · 09:14 二级 FTS 检索 + vector 降级

**用户原话**：（无）

**agent 动作**：光锁定 C2 时段还不够——C2 clustering 里共 14 个 chunk（同一事件相关的
error/tool_output_preview/decision 混合）。agent 要精确定位**那一条 decision chunk**（记录 root
cause）+ **那一段 tool_output**（含修复 SQL）。进入 FTS。

**具体查询**：

```sql
-- (2a) 先在 C2 时段 + payment 关键词上做 FTS
SELECT c.chunk_id, c.chunk_type, c.source_table, c.source_id, c.raw_ref,
       c.content_hash, c.preview_text, c.importance,
       MATCH(c.preview_text) AGAINST ('+payment +(lag offset consumer)' IN BOOLEAN MODE) AS score
FROM session_history_chunks c
WHERE c.session_id = ?
  AND c.created_at BETWEEN '2026-04-15 00:00:00' AND '2026-04-16 06:00:00'
  AND c.chunk_type IN ('decision','tool_output_preview','error')
  AND MATCH(c.preview_text) AGAINST ('+payment +(lag offset consumer)' IN BOOLEAN MODE)
ORDER BY score DESC, c.importance DESC
LIMIT 10;
```

FTS 命中 **6 行**，耗时 110ms。其中 decision 类 1 行（chunk_id=`chk_2026_0415_decision_payment_
lag_root_cause`）、tool_output_preview 类 3 行（其中 1 行 title 含 "fix-payment-offset-realign.sql"）、
error 类 2 行。

**vector 降级分支**：agent 本想走 vector 再 rerank，但检索层先做 `content_hash` 校验——把 chunk
的 `content_hash` 和 vector 表里 `embedding_content_hash` 比对，发现 decision 那条 chunk 在 04-18
有过一次 retention-driven 元信息重写（importance bump），`content_hash` 已漂移但 vector 表是
04-15 batch 生成的，**5 行中 3 行 hash 不匹配**。检索层**直接降级**：放弃 vector 这一级，不做
rerank，仅用 FTS score + `importance` 列做排序。写一条 `retrieval_degrade` 事件到
`agent_events`（`event_type='retrieval.vector_stale'`，payload 含 stale chunk_id 列表）。

**归并结果**：1 条 decision chunk + 1 条 tool_output_preview chunk（含修复 SQL 所在 artifact 指
针）= 2 条 top chunk，锁定进入 T6。

**manifest zone**：依然无新增——FTS 结果和 C1-C2 摘要都在 retrieval scratch 里，尚未渲染。
`reason='fts_hit_with_vector_fallback'`，`dropped_count=4`。

**LLM 看不到**：FTS 丢弃的 4 条 chunk（包括 error 那 2 条，agent 判定为 root cause decision 的
上游依赖，交叉引用即可，不必单独进 prompt）；vector 降级的内部信号。

### T6 · 09:15 按 raw_ref 精确 load 原始 SQL 切片

**用户原话**：（无）

**agent 动作**：拿到 2 条 top chunk 的 `raw_ref`：

- `raw_ref_decision` = `session_artifacts://artifact:decision-20260415-payment-lag-root-cause`
- `raw_ref_sql` = `session_artifacts://artifact:fix-payment-offset-realign-20260415`

按 `raw_ref` 精确加载：

```sql
-- (3a) 拉 decision 原文
SELECT artifact_id, title, content_text, byte_size, content_hash
FROM session_artifacts
WHERE artifact_id = 'artifact:decision-20260415-payment-lag-root-cause';

-- (3b) 拉修复 SQL 脚本
SELECT artifact_id, title, content_text, byte_size, content_hash
FROM session_artifacts
WHERE artifact_id = 'artifact:fix-payment-offset-realign-20260415';
```

两次 PK lookup，耗时合计 12ms。load 出来：decision artifact ~18KB（约 4500 tokens 原文），修
复 SQL artifact ~62KB（~15k tokens 原文），含 3 段 SQL：(1) 诊断 offset 漂移的 `SELECT ... FROM
kafka_consumer_offsets WHERE group_id=...`，(2) payment_event 表对齐补偿脚本 `INSERT ... ON
DUPLICATE KEY UPDATE`，(3) 对账修复 `UPDATE payment_settlement SET status='recon_fixed' WHERE
...`。

**render 策略**（严格防 SQL 截断错位）：agent 使用 `render_mode='code_block_preserved'`，按 SQL
语句边界（以 `;` + 换行为切分点）切片，绝不在语句中间断；每段 SQL 单独包 fence，附头部
`-- provenance: artifact_id=fix-payment-offset-realign-20260415, section=N/3, source_hash=<hash>`。
如果任一段 SQL 超过 `tool_previews` zone 的 single-item 上限（默认 1500 tokens），**整段**放入
`tool_previews` 并在 `retrieved_facts` 里只留 anchor link + 首 3 行，**绝不做 token-level
truncation**。

**具体 chunks 排版**（T7 之前的内部 draft）：

```text
[retrieved_facts zone]
  - 决策摘要（render_mode=narrative，token_budget=600）
      "2026-04-15 22:40 告警：payment-service consumer group 在 K8s node
       重启后 offset commit 风暴导致 lag，下游对账数据错位。Root cause
       = 双写幂等键碰撞 + offset commit 未经 transaction outbox。"
      provenance: chunk_id=chk_2026_0415_decision_payment_lag_root_cause
      source_hash=c0f3...a81b
  - SQL 骨架索引（render_mode=code_block_preserved，token_budget=400）
      [1/3] 诊断脚本 title + 首行（anchor to tool_previews item #1）
      [2/3] 补偿脚本 title + 首行（anchor to tool_previews item #2）
      [3/3] 对账修复  title + 首行（anchor to tool_previews item #3）
[tool_previews zone]
  #1 诊断 SQL 完整 ~260 行（~1400 tokens）render_mode=code_block_preserved
  #2 补偿 SQL 完整 ~180 行（~900 tokens） render_mode=code_block_preserved
  #3 对账修复 SQL 完整 ~120 行（~650 tokens）render_mode=code_block_preserved
```

**manifest zone**：`retrieved_facts` 1000（决策叙述 600 + SQL 骨架索引 400）/ `tool_previews` 2950
（3 段 SQL 完整保留，含 provenance 头部）/ `session_anchor` 600（锁定 C2 + retrieval target）/
`plan_todo` 300 / `recent_tail` 700 / `summary` 380（4 月份 L1b 摘要，之前 compaction 产出的）/
system/tools 2400。**总 ≈ 8330**，`reason='history_retrieval_loaded'`，`dropped_count=0`。

**LLM 看不到**：session_artifacts 里其他 ~9.4k 个 artifact；decision artifact 里未被纳入的 ~3500
token 背景描述（agent 判定与本次提问不相关）；修复 SQL 原文里的注释块（~2k tokens 的解释文
字，agent 判定渲染 SQL 语句结构已足够）。

### T7 · 09:15 agent 回答阿强 + 持久化 manifest_items

**用户原话**：（无，agent 主动回复）

**agent 动作**：基于 T6 prompt 生成回答：简述 root cause + 贴出 3 段 SQL 概要 + 给出 artifact
链接让阿强在 UI 里直接展开全文。**同时**把本轮 retrieval 结果绑定到 manifest：

```sql
-- (4) 持久化 manifest_items，绑定本轮引用的 chunks/artifacts 的 source_hash
INSERT INTO context_manifest_items
  (manifest_id, session_id, item_order, zone, source_table, source_id,
   source_hash, included, token_estimate, budget_tokens, reason, render_mode)
VALUES
  (?, ?, 1, 'retrieved_facts', 'session_history_chunks',
   'chk_2026_0415_decision_payment_lag_root_cause', 'c0f3...a81b',
   TRUE, 600, 700, 'retrieval_top1', 'narrative'),
  (?, ?, 2, 'tool_previews',   'session_artifacts',
   'artifact:fix-payment-offset-realign-20260415',   '9b12...e4d7',
   TRUE, 2950, 3000, 'retrieval_top2_raw_load', 'code_block_preserved');
```

`context_manifests.total_estimated_tokens=8330`；`compaction_version` 继承当前。

**manifest zone**：无变化。`reason='response_emit'`。

**LLM 看不到**：manifest_items 的 DB 状态（是基础设施，LLM 永远看不到）。

### T8 · 09:18 阿强追问「再给我写个批处理 check 脚本」

**用户原话**：「OK，这次查到的 root cause 非常完整。你基于这次经验，再给我写个批处理 check
脚本——定期扫一下所有 payment-* topic 的 consumer group lag + offset commit 是否落 transaction
outbox，下次同类问题自动发告警，不要等到数据错位。」

**agent 动作**：识别出**派生式任务**。不重新检索——T6 load 的两条 artifact 已经在 manifest 里，
同一 session 内 prompt cache 命中（`prompt_cache_key` 匹配 T7 的 stable prefix）。plan_todo 追
加一条 todo：`"write-kafka-consumer-lag-batch-check"`，priority=high，依赖 `provenance_ref=
chk_2026_0415_decision_payment_lag_root_cause`（表明这条 todo 的设计理论来源）。

**manifest zone**：`retrieved_facts` 1000（未变）/ `tool_previews` 2950（未变，走 prompt cache）/
`plan_todo` 420（新增 1 条 todo 100 tokens）/ `session_anchor` 600 / `recent_tail` 1100（含 T7
回答 + T8 本问）/ `summary` 380 / system/tools 2400。**总 ≈ 8850**，`reason='follow_up_task'`，
`prompt_cache_key=<T7 的 hash>`。

**LLM 看不到**：除 T6 已渲染部分外的原始排查日志（~400 行中剩余 ~250 行），阿强历史上其他
check 脚本模板（没进检索，agent 判定不需要）。

### T9 · 09:19 agent 生成 check 脚本 patch

**用户原话**：（无）

**agent 动作**：按 T6 的 decision（root cause = offset commit 风暴 + transaction outbox 缺失）+
T6 的 SQL 结构（诊断脚本的查询形状）生成 patch。agent 明确把 patch 里 `-- Design rationale:`
注释段指向 `chunk_id=chk_2026_0415_decision_payment_lag_root_cause` + `artifact_id=fix-
payment-offset-realign-20260415`，作为**显式 provenance**写入脚本头部。patch ~260 行 bash +
SQL 混合，agent 通过 `tool=edge_shell`（workspace 是 edge）写到 `/ops/scripts/kafka-consumer-
lag-check.sh`。

**manifest zone**：`retrieved_facts` 1000（未变）/ `tool_previews` 3200（旧 2950 + 新生成脚本
preview 250）/ `plan_todo` 420 / `recent_tail` 1400（含 T8 问 + 本轮 agent 生成动作 streaming）
/ 其他未变。**总 ≈ 9420**，`reason='tool_execute'`。

**LLM 看不到**：edge workspace 上其他脚本；原始 decision artifact 里未引用的背景段。

### T10 · 09:20 artifact 落库 + manifest_items 绑定 T6 source_hash

**用户原话**：「挺好，我跑了一下没报错。把这个脚本存下来，命名规范里归到 `playbook/kafka/`，
下次 lag 告警 oncall 直接能调。」

**agent 动作**：把 T9 产出的 patch 落 `session_artifacts`（`artifact:playbook-kafka-consumer-lag-
check-20260506`），同时**显式**写 provenance 绑定：

```sql
-- (5) 新 artifact
INSERT INTO session_artifacts (artifact_id, session_id, title, content_text,
  byte_size, content_hash, created_at, ...)
VALUES ('artifact:playbook-kafka-consumer-lag-check-20260506', ?,
  'Kafka consumer lag periodic check playbook',
  <content>, 9840, '7e4a...b3c1', NOW(), ...);

-- (6) 写一条 chunk 索引（lazy index，实际由 async indexer 几秒后生成 vector）
INSERT INTO session_history_chunks (chunk_id, session_id, chunk_type,
  source_table, source_id, title, preview_text, raw_ref, content_hash,
  token_estimate, importance, created_at, indexed_at, ...)
VALUES ('chk_2026_0506_playbook_kafka_lag', ?, 'artifact_text',
  'session_artifacts', 'artifact:playbook-kafka-consumer-lag-check-20260506',
  'Kafka consumer lag periodic check playbook',
  '-- Design rationale: derived from 2026-04-15 payment-svc lag root cause...',
  'session_artifacts://artifact:playbook-kafka-consumer-lag-check-20260506',
  '7e4a...b3c1', 2460, 80, NOW(), NULL, ...);

-- (7) 在本轮 manifest_items 里绑定 T6 的 source_hash 作为 derivation 证据
INSERT INTO context_manifest_items
  (manifest_id, session_id, item_order, zone, source_table, source_id,
   source_hash, included, token_estimate, reason, render_mode)
VALUES
  (?, ?, 3, 'tool_previews', 'session_artifacts',
   'artifact:playbook-kafka-consumer-lag-check-20260506',
   '7e4a...b3c1', TRUE, 260, 'derived_artifact_emit', 'code_block_preserved'),
  -- 同轮 manifest 里保留 T6 的 source_hash，标注 derived_from
  (?, ?, 4, 'tool_previews', 'session_artifacts',
   'artifact:fix-payment-offset-realign-20260415',
   '9b12...e4d7', FALSE, 0, 'derived_from_reference', 'ref_only');
```

第二条 manifest item 的 `included=FALSE` 但**保留 source_hash**——用于后续审计或阿强下次问
「这个 playbook 是从哪个事件学出来的」时可追溯。

**manifest zone**：`tool_previews` 3460（+260 新 artifact preview）/ 其他维持。总 ≈ 9680，
`reason='artifact_commit'`。

**LLM 看不到**：chunks 表 async 生成 vector embedding 的过程（lazy index）；阿强未来 oncall
同事读这个 playbook 时的独立 session。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 10GB 原始历史，不能进任何 prompt | A3 + A9：session_history_chunks 是唯一入口；raw 永远走 raw_ref lazy load，只有精确命中才读 |
| 「上个月周二/三」时间语义 | A10 一级：structured 列索引 `idx_history_type_created`，时间窗 + `DAYOFWEEK()` + chunk_type 白名单 3 种，一次命中 41 行 |
| 「payment 不是 order」去歧义 | A10 二级：FTS on preview_text，BOOLEAN MODE `+payment +(lag offset consumer)`，按 FTS score + importance 排序 |
| vector 索引过期 | A10 降级：content_hash 校验不通过立即放弃 vector，写 retrieval.vector_stale 事件，仅用 FTS + importance 排序；**不允许**用脏 embedding |
| 4 个月 37 次 compaction 老 decision 不丢 | A6：chunk_type='decision' 和 session_state_items(category='decision') 在 compaction 流程里是白名单，永远保留 |
| raw SQL 不能 token-level 截断 | render_mode='code_block_preserved'：按 `;` + 换行语句边界切分；超过 zone single-item 上限整段移到 tool_previews；anchor link 保留 provenance |
| 派生 artifact 要绑定原始来源 | T10：新 artifact 的 manifest_items 同时写一条 `included=FALSE, reason='derived_from_reference'` 记录原 source_hash，审计可回溯 |
| 整轮 LLM turn 不能被检索拖慢 | SLA：structured < 50ms / FTS < 200ms / vector < 500ms；全链路 T2→T6 累计 < 200ms；T7 回答 streaming 期间才真正 LLM 调用 |

## 5. 预期 state projection 演化

**T2 结束（structured 检索完成）**：

| 投影 | rows touched | 说明 |
| --- | --- | --- |
| session_history_chunks | 41 读（索引扫描） | `idx_history_type_created` 命中，无回表 preview 是主键带 |
| conversation_log | 0 读 | 严禁此层被触碰 |
| session_tool_outputs | 0 读 | 严禁 |
| context_manifests | 1 写（header） | reason='retrieval_scan_only'，dropped_count=38 |
| context_manifest_items | 0 写 | 本轮无 LLM 调用，items 留给 T3 一起 flush |
| retrieval scratch buffer（内存） | 41 行 | 归并为 3 候选 |
| 候选去噪耗时 | 38ms |  |

**T6 结束（raw_ref load 完成，prompt 组装就绪）**：

| 投影 | rows touched | 说明 |
| --- | --- | --- |
| session_history_chunks | 41 + 6（T5 FTS） = 47 索引读 | 全部走索引，无全表扫描 |
| session_artifacts | 2 PK lookup | decision + 修复 SQL |
| context_manifest_items | 2 写（included=TRUE） | retrieved_facts 1 + tool_previews 1 |
| agent_events | 1 写 | retrieval.vector_stale 降级事件 |
| session_state_items（anchor 子项） | 1 写 | current_retrieval_target = C2 |
| prompt tokens（会话状态部分） | ~5330 | anchor 600 + plan 300 + recent 700 + summary 380 + retrieved 1000 + tool_previews 2950 - 600 重复 |
| 全链路累计耗时 | 160ms（structured 38 + FTS 110 + artifact load 12） | 全部 < SLA |

**T10 结束（派生 artifact 落库）**：

| 投影 | rows touched | 说明 |
| --- | --- | --- |
| session_artifacts | 1 insert | playbook-kafka-consumer-lag-check-20260506 |
| session_history_chunks | 1 insert（indexed_at=NULL） | lazy index，等 async batch |
| context_manifest_items | 2 写 | 新 artifact included=TRUE + 原 fix SQL included=FALSE/reason=derived_from_reference |
| session_state_items（todo_state） | 1 update | todo "write-kafka-consumer-lag-batch-check" → status=done |
| agent_run_events | 1 写 | event_type='artifact_committed' |

**关键不变量**：raw conversation_log / session_tool_outputs 在整轮 10 回合中**被读的次数 = 0**；
`session_history_chunks` 单轮累计被扫行数 = 47；所有读都走 covering index 或 PK lookup。

## 6. Token 节省估算

**朴素方案**（把 10GB 历史全塞）：

- 10GB 原始 UTF-8 文本 ≈ 10 × 10^9 bytes / ~4 bytes/token ≈ **2.5 × 10^9 tokens**
- **任何模型都塞不下**——Claude 3.5 Sonnet 200k，GPT-4-turbo 128k，Gemini 1.5 Pro 2M，即使
  2M 窗口也差 1250 倍。朴素方案**物理不可行**。

**「塞满 200k 窗口」方案**（随便裁剪到 200k 再送 LLM）：

- 200k tokens 约对应 4 个月里最近的 ~3 天原始对话 + 少量 tool_output。上个月那次 payment
  事件的时间点在 **3 周前**，早已滑出 200k 窗口 → LLM 根本看不到目标细节 → 用户被迫自己
  肉眼翻 session 历史或放弃
- 即便把 200k 换成「最重要的 200k」（靠人工挑），agent 也不知道哪些「最重要」，因为**没有
  检索索引就没有"重要"的定义**

**本设计方案**（实际 T7 prompt token 分解，按 zone）：

| zone | T7 tokens | 计算依据 |
| --- | --- | --- |
| system_static | 1200 | 常驻 |
| tool_schemas | 1200 | 剪枝后的工具子集（SRE 场景） |
| session_anchor | 600 | 目标 + current_retrieval_target=C2 锁定 |
| plan_todo | 300 | 3 条活跃 SRE checklist |
| recent_tail | 700 | 昨天两轮 + T1-T3 本次对话 |
| summary | 380 | 4 月份 L1b 摘要（老的 compaction 产物） |
| retrieved_facts | 1000 | 决策叙述 600 + SQL 骨架索引 400 |
| tool_previews | 2950 | 3 段 SQL 完整保留 + provenance 头 |
| **有效「会话状态」部分** | **≈ 5330** | anchor + plan_todo + recent_tail + summary + retrieved_facts + tool_previews |
| **prompt 总计** | **≈ 8330** |  |

**对比**：

| 方案 | 目标细节可达性 | prompt tokens | 备注 |
| --- | --- | --- | --- |
| 朴素（全塞） | 不适用 | 2.5B | 物理不可行 |
| 塞满 200k 窗口裁剪 | **miss**（目标在 3 周前，已滑出） | 200k | 技术上能跑，业务上失败 |
| 本设计（三级检索 + manifest） | **hit** | ~8.3k（T7）/ ~9.7k（T10 峰值） | 全链路检索耗时 160ms |

**计算式**：本设计 prompt ≈ 20k 以内（含 T10 派生生成），相比「物理不可行」的朴素方案，**关
键不是比例而是可行性**——从"不可能"到"~20k"。对「塞满 200k」方案，节省 (200000 − 9700) /
200000 ≈ **95.2%**，且把「目标细节 miss」改为「hit」。

**可复用性**：T8-T10 走 prompt cache（`prompt_cache_key` 匹配 T7 stable prefix），T6 的 3 段 SQL
以 manifest_items.source_hash 为键持续复用；下次阿强或 oncall 同事在**不同 session** 里检索
「kafka consumer lag playbook」时，T10 落库的新 chunk 会在 lazy index 完成后（预计 < 60s）进
入下一轮 structured 检索候选，**形成经验闭环**。

## 7. 潜在失败模式

**F1 · vector 索引过期未降级，返回错误 chunk**：若检索层不做 `content_hash` 校验，直接用
04-15 batch 生成的 vector（其中 3 行与 DB 当前 hash 不符）参与 rerank，rerank top-1 有概率落
到 order 或 clearing 的错别时段 chunk。下游 T6 按 raw_ref 拉到错 artifact，阿强收到 order 服务
那次的修复 SQL，跑到 payment 库上会造成二次事故。**可观测信号**：`agent_events` 没有
`retrieval.vector_stale` 事件但 content_hash 表里确有漂移记录；`context_manifest_items.source_hash`
与 `session_artifacts.content_hash` 不匹配。**缓解**：vector 检索路径硬编码一次 hash 比对步骤；
不通过即降级并写事件，永不静默。

**F2 · FTS 分词对中英混合不友好**：MatrixOne / MySQL 默认 ngram 对 "consumer lag" 这种英文
短语 + 中文上下文混排时，可能切成 "consum" "mer la" "ag" 这种碎片，BOOLEAN MODE 命中率骤
降。T5 FTS 返回 0 行，整个二级检索失败，被迫上 vector（见 F1）。**可观测信号**：FTS 查询
EXPLAIN 里 `rows` 估算远低于预期；C2 时段内 chunk_type=decision 行数 > 0 但 FTS 召回 = 0。
**缓解**：建索引时显式指定 `WITH PARSER ngram` 且 `ngram_token_size=2`；或预处理 preview_text
时把中英词拆开存一份到独立 `preview_fts_text` 列。

**F3 · raw_ref 指向的 artifact 已被 retention GC**：4 个月里如果 retention policy 不分 chunk
type（比如 30 天后把 session_artifacts 全部归档到冷存储并从热库删行），T6 的 PK lookup 命中
chunks 表但 `session_artifacts` 行不存在，raw load 失败。**可观测信号**：`session_history_chunks`
存在但 `session_artifacts WHERE artifact_id=?` 返回空；agent 回复里出现 "referenced artifact not
found, degrade to preview only"。**缓解**：decision / artifact_text / tool_output_preview 三类
chunk 绑定的 artifact 必须有**独立长保留策略**（至少与 chunks 表同寿命），或归档到冷存时在
session_artifacts 里保留一条 tombstone 行 + cold_storage_ref；当前设计文档未明确这一点（见
§8 第一条反馈）。

**F4 · importance 权重压过 recency 导致老但不相关结果置顶**：T2 ORDER BY `importance DESC,
created_at ASC` 若阿强历史上某次 order 服务事故的 importance 被人为拉到 100（SRE 手动
pin），会在候选集里置顶，哪怕它不是 payment 服务。T3 给用户看到的 3 候选中 C1 本质无关但
列第一位，可能误导用户选错。**可观测信号**：T3 候选集 importance 分布极不均匀（std > 30）；
用户 T4 需要明确说「不是 C1」。**缓解**：structured 检索阶段不要仅按 importance 单字段排
序，改用 `(importance * 0.4 + recency_score * 0.6)`，或 k-means 聚类后每类取 top-1。

**F5 · compaction 把 "payment 服务" 关键词从 chunk preview 中丢掉**：4 个月里 compaction 了
37 次，若某次 compaction 在生成 `chunk_type='decision'` 的 preview_text 时过度浓缩成「服务消
费位点异常 + 对账修复」而没写 "payment"，T5 FTS 用 `+payment` 过滤就 miss 到目标 chunk。
**可观测信号**：T5 FTS 返回 0 行但 decision chunk 在 C2 时段确实存在（structured 层能扫到）；
人工读 preview_text 发现被「服务级别脱敏」。**缓解**：compaction 生成 preview 时强制保留**服
务名 / 错误码 / 关键实体**的 NER 抽取结果；chunk_type='decision' / 'error' 的 preview 必须包含
`involved_services: ["payment"]` 这类结构化头。

**F6 · 检索耗时未设 SLA，整轮 turn 被拖慢**：若 structured / FTS / vector 都没有 per-stage
timeout，FTS 那一层在 10GB chunks 上跑 3 秒，用户在 T3 之前要等 3 秒才看到「找到 3 候
选」。SRE 在紧急告警场景下这 3 秒体感极差。**可观测信号**：`agent_events` 里某一 stage 的
elapsed_ms > 500；用户反馈"检索比直接肉眼翻 UI 还慢"。**缓解**：每级检索都要 per-stage
timeout（structured 50ms / FTS 200ms / vector 500ms），超时立即降级到上一级结果并 emit
degrade 事件；agent 的回答要明确标注「检索未完全完成，基于 N 条候选回答」。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §6 History Chunks and Retrieval Index — 覆盖了三级检索的描述，但**没有明确每级 SLA 数值
   约束**，也没有规定「每级必须能独立降级」的 state machine。S02 的 F6 就是这条空白直接带出的。
2. §Lazy Indexing — 规定了 chunks / FTS / vector 的 async 生成路径，但**没有说明 index 失效后
   的重建流程**。T5 的 vector 过期检测是本设计隐含需要的，但文档没写 content_hash 漂移该怎
   么排队重建、重建优先级怎么排、重建期间检索层怎么自动 skip。
3. §History Reconstruction Modes — 覆盖了 "LLM 不该回放 10GB" 的底线，但**没有规定
   raw_ref 指向的 artifact 的 retention 策略**。F3 的失败模式本质是 artifact GC 策略与 chunks
   索引寿命不一致，设计文档没有显式绑定。

**三条具体建议**：

- **补齐三级检索 SLA + 降级 state machine**：在 §6 末尾加一张小表：structured < 50ms / FTS
  < 200ms / vector < 500ms，全链路 < 1s；每级超时或结果为空必须降级到上一级而非静默失败；
  降级必写 `agent_events` 事件 `retrieval.<stage>_<reason>`（如 `retrieval.vector_stale` /
  `retrieval.fts_timeout`）。该事件用于审计和 F6 定位。
- **补齐 content_hash 失效→索引重建流程**：在 §Lazy Indexing 加一段：当 chunks 的
  content_hash 发生变化（importance bump / preview rewrite / compaction 带来的 re-preview），
  必须把对应 vector 行标记为 stale（embedding 表软删，append 新版本），并 enqueue 到
  re-embedding queue。检索层读 vector 时优先 JOIN chunks 表比对 hash；不一致直接跳过。规
  定一个重建 SLA（例如 stale 后 10 分钟内重新 embedding 完成），否则该 chunk 的 vector 检索
  能力在此期间视为不可用。
- **明确 raw_ref 指向 artifact 的 retention 绑定**：在 §6 或 §Compaction 里加一段：对
  `chunk_type IN ('decision','error','tool_output_preview','artifact_text')` 的 chunk，其
  `raw_ref` 指向的 session_artifacts 行**必须**至少与 chunk 同寿命；归档到冷存时在
  session_artifacts 里保留一条 tombstone 行，字段 `cold_storage_ref` 指向冷存地址，`status=
  'archived_cold'`；retrieval 层在 raw load 阶段如果读到 archived_cold，自动走冷存 fetch（可
  能耗时 > 1s）并给 agent 一个 `artifact_cold_fetch_required` 信号，让 agent 在 prompt 里告
  知用户「此 artifact 在冷存，拉取中」而非静默 miss。
