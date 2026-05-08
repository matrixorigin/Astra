# WALKTHROUGH-S02 — 4 个月 10GB Session 里挖出上个月的 Kafka lag 修复 SQL

> Status: Draft v0.1（运行推演）
> Scenario: [S02-huge-session-old-bug.md](./S02-huge-session-old-bug.md) §1–§3
> Design: [../web-agent-session-state.md](../web-agent-session-state.md) v0.2
> Gap reference: [GAP-TRACKER.md](./GAP-TRACKER.md) G1/G2/G3/G8/G9/G10/G17

---

## §1 场景标识与 v0.2 映射

**一句话场景**：SRE 阿强在开了 117 天、累计 ~68 万 chunk / ~10.3 GB 原始的老 session 里，凭"上个月周二/三，payment 不是 order，修复 SQL"三条线索，用三级检索把 2026-04-15 那次 payment-svc Kafka consumer lag 的 decision + 修复 SQL artifact 拉回来，并派生新的 check playbook。

**被测 DAUT**：A3（禁止无界扫 conversation_log / session_tool_outputs），A6（4 个月 37 次 compaction 后 decision/error 结构不丢），A10（structured→FTS→vector 三级严格分层 + 每级 SLA + vector stale 降级）。

**最吃压的 v0.2 章节/字段**：

- §Retrieval State Machine @ L1930–1987（G3）：三级 SLA 50/200/500 ms + vector stale 降级 + `retrieval.*` 事件 payload 合约
- §Compaction Invariants @ L2496–2596（G2）：Invariant 2/8 —— `decision` / active `error_state` 不得 replace/archive；summary 只 append
- §History Chunks and Retrieval Index @ L1826–1928：`idx_history_type_created (session_id, chunk_type, created_at)` + raw_ref + content_hash
- §Artifact Retention and Access Scope @ L1671–1749（G9）：`retention_policy` / `status='expired'` / tombstone 渲染规则
- §Content Hash Normalization Contract @ L2165–2193（G17）：vector stale 检测必须 `(content_hash, normalize_version)` 双比对
- §Context Manifest Reason Enum @ L1500–1527（G1）：`history_recall_{structured|fts|vector}` 是本场景唯一合法 reason 集
- §Budget Policy v1 @ L2381–2440（G10）：本场景走 **默认 large-window policy**（不是 budget_v1_8k），`tokenizer_id` / `budget_template_id` 必写

---

## §2 运行前提

**Schema 初值假设**：

- `agent_sessions`：1 行存在，`session_id=sess_payment_ops_2026`，`project_id=payment-ops`，`project_retention_policy=project_long_term`，`created_at='2026-01-10'`。
- `agent_runs`：今天 T1 新开一条 `run_id=run_s02_0506`，`status='running'`，`owner_pod_id=pod-42`，`run_generation=1`。历史上已 close 了 ~2.8k 条 run。
- `session_history_chunks`：~68 万行存在，其中 `chunk_type='decision'` ~450 行、`'tool_output_preview'` ~21 万行、`'error'` ~3.8k 行，`idx_history_type_created` / `idx_history_session_created` 都已 hot（pg buffer pool 覆盖最近 30 天 + decision 全量）。
- `session_artifacts`：~9400 行。关键目标两条存活：`artifact:decision-20260415-payment-lag-root-cause`（`retention_policy='project_long_term'`, `status='active'`, `content_hash='c0f3…a81b'`, `normalize_version='v1'`, byte_size≈18 KB），`artifact:fix-payment-offset-realign-20260415`（同 policy，`content_hash='9b12…e4d7'`, byte_size≈62 KB）。
- Vector 表（`history_chunk_embeddings`）：04-15 batch 产出的 embedding 在 04-18 因 importance bump 导致 chunks 侧 `content_hash` 改写而漂移，vector 侧 `embedding_content_hash` 未更新；重建 queue 滞留 ~140 条 stale 行。
- `context_manifest_reason_types`：seed 了 18 条 canonical reason（L1500–1521），含 `history_recall_structured/fts/vector`、`normal_turn`、`large_tool_output_gated`。

**Workspace & Tools**：hybrid（cloud relay + payment-ops edge bridge）；tool 集合 = `edge_shell` / `mo_sql` / `kafka_admin` / `fetch_url` / `artifact_read` / `retrieval_probe`。edge bridge `reachability_probe.last_ok_at` 1 分钟内，视为 online。

**Context window budget**：假设部署模型 Claude-Sonnet-4.5 200k，**走默认 large-window policy**（非 `budget_v1_8k`）。`tokenizer_id='anthropic_claude_v1'`，`budget_template_id='budget_v1_200k'`（默认模板未在 §G10 表中显式列，但 §Budget Policy v1 L2385–2396 定义了 zone targets；这里按 system 1200 / tool_schemas 1200 / anchor≤800 / plan_todo≤500 / recent_tail≤1200 / summary≤800 / retrieved_facts≤4000 / tool_previews≤4000 / safety_approvals≤300 假设）。T7 峰值预算 ≈ 10k，远低于 200k 窗口。

### §2 补充：tool_schemas zone 裁剪依据

本 session 的 `project_id=payment-ops` 对应 SRE 工具子集（按 G8 preview_template_registry 选 active tool schemas）：

| tool_name | schema tokens | 是否在 T7 manifest | 裁剪理由 |
| --- | ---: | --- | --- |
| `edge_shell` | 220 | ✅ | T9 需要 |
| `mo_sql` | 180 | ✅ | 历史上 payment DBA 相关 |
| `kafka_admin` | 200 | ✅ | 场景主题 |
| `fetch_url` | 140 | ✅ | root cause 参考文档 |
| `artifact_read` | 90 | ✅ | UI 展开 artifact |
| `retrieval_probe` | 160 | ✅ | 三级检索自身探针 |
| `approval` | 120 | ✅ | 写生产脚本可能触发 |
| `git_commit` / `browser_test` / `image_edit` | — | ❌ | 非 payment-ops 域，L2388 "Prune by active agent, permissions, skill/tool relevance" |

合计 1110 tokens，向上取整到 1200 留余量。假设未经裁剪则 tool 库 ~40 tool × 180 tokens avg = 7200，压缩 83%。

### §2 补充：budget 假设与 `budget_template_id` 缺口

场景走 200k 窗口大模型，但 v0.2 §G10 只 seed 了 `budget_v1_8k`（L2410）。大窗口的模板 id **没有 canonical 名称**，本推演假设 `budget_template_id='budget_v1_default'` 但此值未 seed。→ 这一点是 §4 F1 的远亲：再次说明 enum / template registry 的补全价值。实现时建议 `context_manifests.budget_template_id` NULL 表示"fall through 到 §Budget Policy v1 的非小窗口目标"。

---

## §3 端到端运行轨迹

> 今天 2026-05-06 (周三)。「上个月」= 2026-04-06…2026-05-05。

### T1 · 09:12:04 — 阿强模糊提问

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 原话："上个月我们查过一次 Kafka consumer lag 异常导致数据错位，你帮我写过修复脚本，大概那周二或周三，是 payment 服务不是 order 服务。找一下完整排查记录和修复 SQL。" | ContextAssembler per-turn flow（L2355–2379）：(1) authn user_id，(2) load `session_state_items`（anchor / plan_todo / active errors），(3) **跳过** recent_tail 里 10GB 全量，(4) 解析为 `RetrievalRequest{ time_window:[2026-04-06, 2026-05-05], weekday_filter:[2,3], topic_hints:["Kafka","consumer lag","数据错位"], service_hints:["payment"], artifact_hints:["SQL","脚本"] }`，(5) 决定本轮 **context_manifests.reason = `history_recall_structured`**（L1507，G1 合法 reason）。写 `conversation_log` user_message 行 + 发 SSE `run.turn.started`。 | UI 侧立即回显用户消息 + "正在检索历史记录…" spinner；SSE 心跳 15s 一次（§Run Event Ordering L ~1085）。`total_estimated_tokens` 尚未算，manifest 行在 T2-T6 之后一并 flush。 |

**manifest zone 数字（T1 结束，LLM 尚未调用）**：`system_static` 1200 / `tool_schemas` 1200（SRE tool 子集） / `session_anchor` 520（金融运维长期目标 + 当前 P1 告警）/ `plan_todo` 280（3 条活跃 SRE checklist）/ `recent_tail` 620（昨天两轮 + 本轮 user_message）/ `summary` 400（4 月份 L1b）/ `retrieved_facts` 0 / `tool_previews` 0。总 ≈ 4220。

**LLM 看不到**：10GB 原始；本轮 retrieval 尚未发。

### T2 · 09:12:04 — 一级 structured 检索（40ms）

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| — | Stage 1（G3 L1940 `<50ms` / 1000 scanned / 50 candidates hard cap）。SQL: `SELECT chunk_id, chunk_type, source_id, raw_ref, content_hash, preview_text, importance FROM session_history_chunks WHERE session_id=? AND created_at BETWEEN '2026-04-06' AND '2026-05-05' AND chunk_type IN ('decision','tool_output_preview','error') AND DAYOFWEEK(created_at) IN (3,4) ORDER BY importance DESC, created_at ASC LIMIT 50;` 走 `idx_history_type_created`（L1858），覆盖索引无回表。返回 **41 行**，耗时 38ms，在 SLA 内。41 行 re-cluster by `created_at` → 3 个 event cluster C1/C2/C3（4-08 order / 4-15 payment / 4-22 clearing）。**不发 `retrieval.structured_empty`，不发 `retrieval.bound_exceeded`**。 | 无可见变化；前端仍是 spinner。后台 `agent_events` 写一条 `retrieval.structured_ok`（payload: `candidate_count:41, elapsed_ms:38, query_hash:sha256:…`，对应 G3 L1963–1980 的 payload 合约）。 |

**manifest zone**：T2 尚未组 prompt（三级检索完成后再一次性 render），41 行 preview 留在 retrieval scratch 内存中，**不**进 manifest。`context_manifests` 此时仍未写行（与场景原文「T2 写 1 行 header」不同——见 §4 F1）。

**LLM 看不到**：41 行 preview；其他 ~5 万条 assistant_message / tool_call chunk。

### T3 · 09:12:05 — 3 候选摘要 → 请用户澄清

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| — | Assembler 判定候选仍模糊（C1/C2/C3 跨三条服务线），**决定 ask_user**。把 3 个 cluster 各取 top-1 `importance` chunk 的 `(title, first-line preview, chunk_id, raw_ref)` 组成 3 条 facts 注入 `retrieved_facts` zone；reason = `history_recall_structured`（G1 合法值，L1507）。**本轮有 LLM 调用** —— assembler 让 LLM 生成 "找到 3 候选，请选" 的自然语言包装。写 `context_manifests` 1 行 + `context_manifest_items` 3 行（`zone='retrieved_facts'`, `included=TRUE`, `source_table='session_history_chunks'`, `source_hash=<chunk.content_hash>`, `token_estimate=140 each`）。`dropped_count=38`。 | SSE `run.turn.assistant_message_delta` 流式输出："找到 3 个候选时段——[C1] 2026-04-08 order-svc DLQ / [C2] 2026-04-15 payment-svc offset 风暴 / [C3] 2026-04-22 clearing-svc rebalance。哪一个是你说的 payment 那次？" UI 侧 3 个候选渲染为可点卡片（title + 时间 + chunk_id 锚点）。 |

**manifest zone（T3 LLM 调用时）**：system 1200 / tools 1200 / anchor 520 / plan_todo 280 / recent_tail 720（+ T1 user_msg）/ summary 400 / retrieved_facts 420（3×140）/ tool_previews 0。**总 ≈ 4740**，`reason='history_recall_structured'`。

**LLM 看不到**：41 行里其他 38 条降权 chunk；各 cluster 的 raw artifact body。

### T4 · 09:14 — 阿强澄清

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 原话："C2，payment 服务那次。order 和 clearing 不是，你别看错了。我要的是 payment consumer 那次 offset 错位 + 对账对不上那个。" | 写新 `session_state_items` 行：`scope='session'`, `category='anchor'`, `item_key='current_retrieval_target'`, `payload_json={cluster_id:'C2', chunk_id:'chk_2026_0415_decision_payment_lag_root_cause', raw_ref:'session_artifacts://artifact:decision-20260415-payment-lag-root-cause'}`, `status='active'`, `priority=100`。**进入 T5（Stage 2 FTS）** 而不是走 vector——`content_hash` 校验将在 T5 内部做。 | UI 显示用户选择被记录（anchor badge：C2 locked）。尚无新内容流出。 |

**manifest zone**：`session_anchor` 520 → 720（+200 tokens：current_retrieval_target 锁定）。其余维持。**总 ≈ 4940**，reason 仍 `history_recall_structured`（T4 未调用 LLM，只是 state 更新；不写新 manifest）。

**LLM 看不到**：C1/C3 从 retrieved_facts 里剔除（由 T5 覆盖写入）。

### T5 · 09:14:12 — 二级 FTS + vector stale 降级

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| — | Stage 2 FTS（G3 L1941 `<200ms` / 100 candidates）：`SELECT chunk_id, chunk_type, raw_ref, content_hash, preview_text, MATCH(preview_text) AGAINST ('+payment +(lag offset consumer)' IN BOOLEAN MODE) AS score FROM session_history_chunks WHERE session_id=? AND created_at BETWEEN '2026-04-15 00:00:00' AND '2026-04-16 06:00:00' AND chunk_type IN ('decision','tool_output_preview','error') AND MATCH(...) ORDER BY score DESC, importance DESC LIMIT 10;`。走 FTS index on `preview_text`（L1885）+ `idx_history_session_created` 时间裁剪。返回 **6 行**，耗时 110ms。写 `agent_events` `retrieval.fts_ok`。**Stage 3 本应走 vector rerank**——assembler 先做 G17 双字段 stale 校验（L2192：`compare both content_hash and normalize_version`）：JOIN `history_chunk_embeddings ON chunk_id` 发现 6 行中 **3 行** `embedding_content_hash != chunks.content_hash`（04-18 importance bump 漂移），**整组判定 vector stale**。按 G3 L1958 state machine：`retrieval.vector_stale` 事件 + enqueue re-embedding + `fallback_stage='fts'`。归并 top-2：`chk_2026_0415_decision_payment_lag_root_cause` + `chk_2026_0415_tool_preview_fix_payment_offset_realign_sql`（含 artifact 指针 `artifact:fix-payment-offset-realign-20260415`）。 | 后台 `agent_events` 写两条：`retrieval.fts_ok`（elapsed_ms:110, candidate_count:6）+ `retrieval.vector_stale`（候选 3 chunk_ids, fallback_stage:'fts', index_hash != content_hash）。UI 无变化（仍在 spinner，已累计 ~150ms，人感知不到）。 |

**manifest zone**：T5 尚未 prompt LLM，只更新 retrieval scratch。

**LLM 看不到**：FTS 丢弃的 4 条；vector 降级细节；其他 error cluster 日志。

### T6 · 09:14:15 — raw_ref 精确 load 原始 SQL

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| — | Stage 4 Raw Load（G3 L1943 `<250ms` hot）。两次 PK lookup（§1680 `session_artifacts`）：`SELECT artifact_id, title, content_text, byte_size, content_hash, status, retention_until FROM session_artifacts WHERE artifact_id IN ('artifact:decision-20260415-payment-lag-root-cause','artifact:fix-payment-offset-realign-20260415')`。两条都 `status='active'`（G9 L1710），hot 存；合计 12ms。Decision artifact ~18 KB ≈ 4500 tokens；SQL artifact ~62 KB ≈ 15k tokens（3 段：诊断 / 补偿 / 对账修复）。Render 策略：`render_mode='code_block_preserved'`（L1487 允许的 render_mode 取值空间；虽然 v0.2 没列举完整，但与 G8 preview template `code_block` 兼容）。**按 `;` + 换行切片，绝不 token-level truncate**；每段头加 `-- provenance: artifact_id=..., section=N/3, source_hash=<content_hash>`。**G9 + G17 双校验**：load 后比对 `session_artifacts.content_hash` 和 `session_history_chunks.content_hash`（二者应一致），一致才允许注入 prompt。 | 后台 `agent_events` `retrieval.raw_load_ok`（elapsed_ms:12, selected_refs:2）。UI 仍 spinner。累计检索耗时：38 + 110 + 12 = **160ms < 1s**（G3 end-to-end 目标）。 |

**manifest zone（T6 完成，T7 LLM 调用前准备）**：

- `retrieved_facts` 1000：决策叙述 narrative 600（含 provenance header）+ SQL 骨架索引 400（3 段 anchor link 到 tool_previews）
- `tool_previews` 2950：3 段 SQL 完整保留（诊断 1400 + 补偿 900 + 对账修复 650），每段 `render_mode='code_block_preserved'`
- `session_anchor` 720 / `plan_todo` 280 / `recent_tail` 850（+T3 assistant + T4 user）/ `summary` 400 / system 1200 / tools 1200
- **总 ≈ 9600**，`reason='history_recall_fts'`（T5 是关键击中级，按 G1 L1523–1527 "reflect the highest-cost or most-specific behavior"）

**LLM 看不到**：其他 ~9400 artifact；decision artifact 里未进 prompt 的 ~3500 token 背景段；SQL 注释块 ~2000 token。

### T7 · 09:15:00 — agent 回答 + 持久化 manifest_items

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| — | LLM 调用（Claude Sonnet 4.5，prompt ≈ 9600 tokens）。Streaming 输出：root cause 叙述 + 3 段 SQL concise preview + artifact 深链。调用结束后 **一次事务**写 `context_manifests` 1 行（`total_estimated_tokens=9600`, `tokenizer_id='anthropic_claude_v1'`, `budget_template_id='budget_v1_200k'`, `reason='history_recall_fts'`, `dropped_count=4` FTS 落选 + 38 structured 落选 = 42，`compaction_version='v37'` 继承）+ `context_manifest_items` **5 行**：(1) anchor 720 included / (2) retrieved_facts decision narrative 600 included `source_hash='c0f3…a81b'` / (3) retrieved_facts SQL skeleton 400 included / (4) tool_previews 3 段合计 2950 included `source_hash='9b12…e4d7'` / (5) recent_tail 850 included。写 `conversation_log` assistant_message 行 + streaming 结束事件。 | SSE `run.turn.assistant_message_delta` 一连串片段："2026-04-15 22:40 payment-svc consumer group 在 K8s node 重启后 offset commit 风暴导致 lag…root cause = 双写幂等键碰撞 + offset commit 未经 transaction outbox。修复脚本分 3 段——[代码块 1] 诊断 offset 漂移 / [代码块 2] payment_event 表对齐补偿 / [代码块 3] payment_settlement 对账修复。完整文件在 artifact:fix-payment-offset-realign-20260415"。UI 把 artifact_id 渲染为可点击链接，点击后调 `GET /artifacts/{id}` 走 presigned URL（G9 L1741–1742）。 |

**LLM 看不到**：`context_manifest_items` 的 DB 持久化状态；其他 9398 条 artifact。

### T8 · 09:18 — 派生任务「写批处理 check 脚本」

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 原话："OK 非常完整。你基于这次经验，再给我写个批处理 check 脚本——定期扫 payment-* topic consumer group lag + offset commit 是否落 transaction outbox，下次自动告警。" | ContextAssembler 识别为 **派生式 follow-up**（同 run 内）：无新检索。T6 的 retrieved_facts + tool_previews 已在 manifest，`prompt_cache_key` 匹配 T7 stable prefix → prompt cache 命中。`session_todos` insert 1 行：`todo_id='todo_write_kafka_lag_check'`, `priority='high'`, `plan_id` 继承，`origin_chunk_id='chk_2026_0415_decision_payment_lag_root_cause'`（G6 L2324–2326 provenance 列）。reason = `normal_turn`（L1505）。 | UI 立即回显用户问题 + "生成 check 脚本中…" spinner；plan panel 侧栏加一条新 todo。 |

**manifest zone（T8 LLM 调用时）**：retrieved_facts 1000（未变，cache）/ tool_previews 2950（未变，cache）/ plan_todo 380（+100 新 todo）/ session_anchor 720 / recent_tail 1120（+T7 回答头部 + T8 user）/ summary 400 / system 1200 / tools 1200。**总 ≈ 8970**，`reason='normal_turn'`，`prompt_cache_key=<T7 hash>`，`dropped_count=0`。

**LLM 看不到**：未被纳入的 decision 背景段；其他 check 模板。

### T9 · 09:19 — agent 生成 check 脚本 patch

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| — | LLM 继续流式输出 check 脚本 patch（bash + SQL 混合 ~260 行）。patch 头部显式写 `-- Design rationale: derived from chunk_id=chk_2026_0415_decision_payment_lag_root_cause, artifact_id=fix-payment-offset-realign-20260415`。assembler 在 tool_call 发出前检查 edge bridge reachability（G11）：`payment-ops` bridge `reachability_probe.last_ok_at` < 1min → online。调用 `tool=edge_shell`，cmd=`cat > /ops/scripts/kafka-consumer-lag-check.sh <<EOF … EOF`。tool_output 捕获 exit_code=0。同步写 `session_tool_outputs` 行 + preview（按 G8 preview_template_registry 的 `edge_shell` baseline 模板，preview ≤ 400 bytes 或首/末 200 字节）。 | SSE streaming patch 内容；UI 代码块渲染；tool_call bubble 展开 "edge_shell: /ops/scripts/kafka-consumer-lag-check.sh (exit 0)"。 |

**manifest zone**：retrieved_facts 1000（未变）/ tool_previews 3200（+250 新脚本 preview）/ plan_todo 380 / recent_tail 1400（+T8 assistant stream + T9 tool_output_preview）/ 其他未变。**总 ≈ 9620**，`reason='large_tool_output_gated'`（G1 L1510，因为生成 + preview 属于 tool artifact 写入）。

**LLM 看不到**：edge workspace 其他脚本；decision artifact 未引用段。

### T10 · 09:20 — artifact 落库 + manifest_items 绑定 T6 source_hash

| User 动作 | Server behavior | User sees |
| --- | --- | --- |
| 原话："挺好，我跑了没报错。存下来，归到 `playbook/kafka/`，下次 oncall 直接调。" | 写 `session_artifacts` 1 insert：`artifact_id='artifact:playbook-kafka-consumer-lag-check-20260506'`, `title='Kafka consumer lag periodic check playbook'`, `byte_size=9840`, `content_hash='7e4a…b3c1'`, `normalize_version='v1'`, `retention_policy='project_long_term'`, `status='active'`, `access_scope='delegation'`（G9 L1682，团队成员可用）, `derived_from_artifact_id='artifact:fix-payment-offset-realign-20260415'`（G9 L1688 derived chain）。写 `session_history_chunks` 1 insert：`chunk_type='artifact_text'`, `indexed_at=NULL`（lazy index，L627 async 生成 FTS + vector）, `importance=80`。写 `context_manifest_items` **2 行**：(新 artifact, zone='tool_previews', included=TRUE, reason='derived_artifact_emit', source_hash='7e4a…b3c1') + (原 SQL artifact, zone='tool_previews', **included=FALSE**, reason='derived_from_reference', source_hash='9b12…e4d7', token_estimate=0, budget_tokens=0) —— 第二行保留 source_hash 做审计回溯（"playbook 从哪个事件学出来的"）。`session_state_items` 把 todo `write-kafka-consumer-lag-batch-check` update 为 `status='done'`, `provenance_event_id=<run_event_idx>`。`agent_run_events` 写 `artifact_committed`。 | UI artifact panel 新增卡片 + "已归档到 playbook/kafka/"；plan panel 把 todo 打钩。可见完成。 |

**manifest zone（T10 LLM 调用 / 回复"好了"）**：tool_previews 3460（+260 新 artifact preview，旧 fix SQL included=FALSE 不计 token）/ 其他维持。**总 ≈ 9880**，`reason='large_tool_output_gated'`（同类别，artifact commit turn）。

**LLM 看不到**：async vector index 生成过程；未来 oncall 同事独立 session。

### §3.x T1–T10 DB 写入累计（§3 收尾）

| Turn | conversation_log | session_history_chunks | session_artifacts | context_manifests | context_manifest_items | session_state_items | agent_events | agent_run_events |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| T1 | +1 | 0 | 0 | 0（延到 T3 flush） | 0 | 0 | 0 | +1 |
| T2 | 0 | 0 写 / 41 读 | 0 | 0 | 0 | 0 | +1 structured_ok | 0 |
| T3 | +1 | 0 | 0 | +1 (history_recall_structured) | +3 | 0 | 0 | +2 |
| T4 | +1 | 0 | 0 | 0 | 0 | +1 (anchor) | 0 | +1 |
| T5 | 0 | 6 读 / JOIN 6 embeddings | 0 | 0 | 0 | 0 | +2 (fts_ok / vector_stale) | 0 |
| T6 | 0 | 0 | 2 PK 读 | 0 | 0 | 0 | +1 raw_load_ok | 0 |
| T7 | +1 | 0 | 0 | +1 (history_recall_fts, total=9600) | +5 | 0 | 0 | +2 |
| T8 | +1 | 0 | 0 | +1 (normal_turn, prompt_cache hit) | +1 | +1 (todo) | 0 | +1 |
| T9 | +1 | 0 | 0 | +1 (large_tool_output_gated) | +1 | 0 | 0 | +3 |
| T10 | +2 | +1 (indexed_at=NULL) | +1 playbook | +1 (large_tool_output_gated) | +2 (new + derived_from ref_only) | +1 (todo done) | 0 | +2 (artifact_committed) |
| **Σ** | **7** | **1 写 / 47 读** | **1 写 / 2 读** | **5** | **13** | **3** | **7** | **12** |

**DAUT 自检**：

1. **A3**：`conversation_log` 全程 append-only（7 行，user/assistant 成对）；`session_tool_outputs` **读 = 0**（T9 只写 edge_shell 的 preview，不触碰老行）。`session_history_chunks` 总被访问 47 次，**全部走 covering index 或 PK**，无全表扫描。✅
2. **A6**：本场景未新触发 compaction，但若 T11 触发 session-level compaction —— 按 G2 Invariant 4（L2553）先查 `agent_runs.status='running'` 存在（run_s02_0506），**必须延后**；且该 session 内 `chunk_type IN ('decision','error')` 行数 ≈ 4250，Invariant 2 全部白名单保护，永不 replace/archive。✅
3. **A10**：三级耗时 38 + 110 + 12 = 160ms <<1s；structured 41 ≤ 50 cap，FTS 6 ≤ 100 cap，vector stale 直接 skip 未消耗 "1 次 vector 查询每轮" 预算（G3 L1942）——**可选进入但被 stale 守卫拒绝**，符合合约。✅

**成功 vs 失败分支一览**（本场景若某一环崩掉）：

| 分支 | 触发条件 | 结果 | 用户感知 |
| --- | --- | --- | --- |
| **Happy**（本推演） | 三级 SLA 达标 + raw active | T7 完整 3 段 SQL | ~8s 完成 |
| FTS 0 命中（F2） | 中英分词切碎 | 降级 structured 粗结果 + ask_user 补词 | 多 1 轮澄清 |
| Vector stale 漏判（F3 / 组合 B） | payload 漏 normalize_version | 返回脏 embedding | **二次事故**：拿 order 修复 SQL 跑 payment 库 |
| Artifact expired（组合 A） | retention='default' + 35d | G9 L1735 tombstone 渲染 | "historical; raw unavailable; summary only" |
| Edge bridge offline（T9） | reachability_probe > 60s | edge_shell 从 tool_schemas 剔除 | "edge 不可达" + waiting_for_edge |

---

## §4 发现的别扭点（Frictions）

### F1 · 场景 T2 的 `context_manifests` 空写与 reason 枚举不吻合 —— notable

- **Ref**: S02 §3 T2 / v0.2 §Context Manifest Reason Enum L1500–1527（G1）
- **Observation**: 场景原文 T2 说「本轮只是检索器内部动作，不进 prompt」，但又写「`context_manifests.reason='retrieval_scan_only'`，`dropped_count=38`」。问题：(a) `retrieval_scan_only` 不在 G1 canonical enum（L1500 只有 `history_recall_{structured|fts|vector}`）；(b) 本轮**没有** LLM 调用，而 `context_manifests` 的主键索引是 `(session_id, turn_id)`（L1457），写一行只能挂在某个 turn_id 上——要么强制给一个 "phantom turn"，要么延迟到 T3 LLM 真调用时合并写。场景 §5 又把 T2 写成 "1 写 header"——两处口径冲突。
- **Why it's friction**: 实现者会纠结「是否允许 manifest row without run_id/turn_id」。v0.2 §2498 Invariant 6 里的 `context_manifests.run_id IS NULL` 场景只为 session-level compaction 预留，不包含 retrieval-internal scratch。要么场景原文改（retrieval scratch 不入 manifest，只进 agent_events），要么 v0.2 加一条"pre-LLM retrieval snapshot manifest"子类型。
- **Severity**: `notable`（不 blocking，但契约漂移会让 G3 的"每轮 retrieval 预算最多 1 次 vector 查询"这条语义难以审计）

### F2 · FTS 中英混合分词未合约化 —— notable

- **Ref**: S02 §3 T5 + §7 F2 / v0.2 §1885「Full-text index should target preview_text」
- **Observation**: v0.2 L1885 仅说「FTS index 建在 preview_text 上」，**未指定 parser / token_size / stopword**。MatrixOne / MySQL 默认 ngram 对 "consumer lag" + 中文上下文极可能把 "consumer" 切成 "con"/"ons"/"nsu"... BOOLEAN MODE `+payment +(lag offset consumer)` 可能 0 命中，强制走 vector，但 vector 已 stale（F1 降级路径），整个 T5 崩溃。场景 §7 已点名这是 F2，但 v0.2 没在 schema 或 preview template 里落地对策。
- **Why it's friction**: T5 是整个场景的**决定性击中级**。如果 FTS 0 命中，阿强等不到 T7 回答，agent 回复"未找到"，用户体验从 hit 降为 miss。
- **Severity**: `notable`（不 blocking 但与 G3 承诺的「FTS 是二级主力」预期不符）

### F3 · vector stale 检测只靠 content_hash，未显式比对 normalize_version —— notable

- **Ref**: S02 §3 T5 / v0.2 §G3 L1982 + G17 L2192
- **Observation**: G3 retrieval event payload（L1976–1978）只有 `content_hash` 和 `index_hash`，但 G17 L2191–2192 明确要求 "Deduplication, vector stale detection, artifact reuse, and skill version audit **must compare both `content_hash` and `normalize_version`**"。场景 T5 只说"3 行 hash 不匹配"，未说 normalize_version 比对，也未在 `retrieval.vector_stale` payload 里带 `normalize_version`。
- **Why it's friction**: 如果 normalize rule 从 v1 升级到 v2（比如 `slow_query_v1 → slow_query_v2`），所有老 embedding 的 `content_hash` 还是老规则算出来的，光比 content_hash 会漏判整批；G17 的语义被 G3 的 payload 合约「部分吃掉」。实现者要么扩 G3 payload，要么依赖侧写逻辑 —— 后者易漂移。
- **Severity**: `notable`（影响场景 F1 失败模式「vector 索引过期未降级」的精确度）

### F4 · `render_mode='code_block_preserved'` 未列入 v0.2 枚举 —— minor

- **Ref**: S02 §3 T6 / v0.2 §1487 `render_mode VARCHAR(64)` + §Preview Template Registry（G8 L1600 附近）
- **Observation**: `context_manifest_items.render_mode` 是 VARCHAR(64) 无枚举约束。场景大量使用 `code_block_preserved` / `narrative` / `ref_only`，这些字符串 v0.2 没有权威列表。G8 的 preview_template 有 `preview_kind` 但与 render_mode 不是同一字段。
- **Why it's friction**: 不同实现可能写 `code_block` / `codeblock` / `preserve_code` 三种拼法，SQL 聚合"按 render_mode 统计 token 浪费"会跑偏。T6 承诺「SQL 不被 token-level truncate」依赖这个枚举被严格校验。
- **Severity**: `minor`（加一张 lookup 表即可，类似 G1 处理 reason）

### F5 · raw_ref URL scheme `session_artifacts://artifact:...` 未规范 —— minor

- **Ref**: S02 §3 T6 raw_ref 字符串 / v0.2 §1888「Raw content is addressed by raw_ref and content_hash; it can point back to conversation_log, session_artifacts, object storage, or a compressed blob」
- **Observation**: v0.2 L1888–1890 只说 "raw_ref 可以指 conversation_log / session_artifacts / object storage / blob"，**没有定义 URL scheme**。场景用 `session_artifacts://artifact:decision-...`，另一个 agent 可能用 `sa:decision-...` 或 `/sessions/X/artifacts/Y`。raw load 代码要做 scheme dispatch，没有规范就是 if-else 地狱。
- **Why it's friction**: 跨 session / 跨 workspace delegation artifact 取回（G9 access_scope='delegation'）依赖 raw_ref 可被 parse，scheme 漂移会让 G4 的 delegation drill-down 偶发 404。
- **Severity**: `minor`（scheme 很好补；但补之前整个 artifact 引用链就脆）

### F6 · T10 `included=FALSE, reason='derived_from_reference'` 的审计查询路径不明 —— minor

- **Ref**: S02 §3 T10 / v0.2 §1474–1492 `context_manifest_items`
- **Observation**: T10 第二条 manifest_item 保留原 fix SQL artifact 的 `source_hash` 但 `included=FALSE`，语义是"派生证据留痕"。v0.2 schema 允许这种行（`included BOOLEAN`，没有 check 约束），但**没有**在任何地方写明 "如何查某个 artifact 是哪个派生 artifact 的源头"——没有反向索引 `(source_table, source_id, included=FALSE, reason LIKE 'derived_from%')`。G9 只定义了 `derived_from_artifact_id` 列（L1688），但 `context_manifest_items` 里的 derived_from 记录是另一条线，二者未 JOIN 契约化。
- **Why it's friction**: 未来阿强问"这个 playbook 从哪个事件派生的"——查 `derived_from_artifact_id` 只能得到一个；想看"完整证据链"需要扫 `context_manifest_items WHERE session_id=? AND reason='derived_from_reference'`，但这条 reason 不在 G1 manifest-level enum 里（item-level reason 是自由字符串，容易漂）。
- **Severity**: `minor`（但影响 G9 审计闭环的体感完整性）

### F7 · T4 anchor update 未写 `context_manifests`，但 agent 显然"感知"到了 —— minor

- **Ref**: S02 §3 T4 / v0.2 §2355–2379
- **Observation**: T4 只是用户澄清 + 写一条 `session_state_items.anchor`，**没有** LLM 调用，也没写 manifest。但 T5 的三级检索明确用到了 T4 的 anchor payload（cluster_id=C2）。语义上 anchor 是 session_state 的一部分，T5 进 LLM 时会被 `session_anchor` zone 带进去——但 T4-to-T5 之间没有 turn boundary，agent 怎么"知道"现在要进 T5？场景原文写「进入二级检索」——这是 server-driven 自动触发，但 v0.2 里没有"澄清收到自动续检索"的显式 trigger。
- **Why it's friction**: 实现者要判断"user 澄清 = 自动续前一轮 retrieval plan"还是"澄清 = 新 turn，要重新 plan"。前者需要把 retrieval_plan 挂在 `agent_runs` 或 `session_state_items`，后者成本翻倍。v0.2 L~580 有 `ContextAssembler` 流程但未规范 "clarification continuation" 语义。
- **Severity**: `minor`（G12 next-action confidence 部分覆盖，但不直接对应）

### F8 · LLM 回答不得"发明"summary 里没的 provenance —— notable

- **Ref**: S02 §3 T7 / v0.2 §G3 L1946–1947 "If all stages fail or time out, the assistant must say the old detail was not found instead of inventing continuity from a summary"
- **Observation**: v0.2 这条 "not found" 规则只针对**检索全部失败**的场景。场景 T6 是 happy path（raw load 成功），但 T7 LLM 生成 "2026-04-15 22:40 payment-svc consumer group 在 K8s node 重启后 offset commit 风暴…" 的叙述——**其中时间 "22:40" 是 decision artifact 原文里的，但 "K8s node 重启" 是 agent 推断的**（原 decision 可能只写"某次节点事件"）。G9 L1731–1738 定义了 expired artifact 的"不能发明细节"但没有 active artifact 的"不能过度推断"规则。
- **Why it's friction**: LLM 基于 600-token narrative 压缩后再给用户自然语言回复，容易把相关但非原文的技术黑话（"K8s node 重启"）当作事实陈述。G8 preview_template 只管 preview 的生成，不管 LLM 下游 render 的保真度。T7 `assistant_message` 本身没有反向 provenance 校验。
- **Severity**: `notable`（会影响阿强对 agent 的信任 —— 一旦发现"你说的 K8s 重启原文里没有"，会怀疑其他 3 段 SQL 也是编的）

### F9 · `session_state_items(category='anchor')` 白名单未显式 —— minor

- **Ref**: S02 §3 T4 anchor 写入 / v0.2 §Cross-Session Scope L2318–2322 + §Session State Projection L1075
- **Observation**: T4 写 `session_state_items(scope='session', category='anchor', item_key='current_retrieval_target')`。G6 的 user-scope 白名单（`durable_decision`/`engineering_rule`/`rejected_pattern`）明确，但 **session-scope 的 category 无白名单**。场景自创了 `anchor` 作为 category 名，合规但非 canonical。G2 L2508 提到 `plan_state`/`decision`/`finding`/`benchmark`/`citation`/`todo_state`/`error_state`/`delegation_state`/`summary` 九大 category，**没有 `anchor`**。
- **Why it's friction**: T4 写完这条后，compaction 规则（G2 Invariant 2）按什么对待？是白名单保护（不得 replace/archive）还是自由清理？场景假设它 "就留着"，但 v0.2 没说。
- **Severity**: `minor`（与 F4/F5 同类 —— category enum 缺失，易漂；补一个 canonical enum 即可）

---

## §5 发现的新 gap 候选

### GapCand-S02-01. `render_mode` 枚举 + preview_template 联动合约缺

- **title**: `context_manifest_items.render_mode` 未枚举化，导致 T6 的 "SQL 不截断" 承诺无 schema 保证
- **triggered_by_turn**: T6
- **related_v0.2_section**: §3 Context Manifests L1487 / §Preview Template Registry（G8）
- **severity**: medium
- **rationale**: 与 G1 处理 `context_manifests.reason` 思路一致 —— VARCHAR free-form 让"按 render 类型统计 token 浪费 / 定位 render 失败"不可能。另外 render_mode 与 G8 的 preview_template 是两条字段线，没 JOIN 契约；"preserved code block 的 token budget 如何与 preview_template.max_preview_bytes 映射"无合约。
- **proposed_fix**: 新增 `context_item_render_modes` lookup 表，seed `narrative` / `code_block_preserved` / `ref_only` / `table` / `tree` / `raw_verbatim`；preview_template.yaml 声明 `allowed_render_modes[]` 做 cross-check。

### GapCand-S02-02. raw_ref URL scheme 规范缺

- **title**: `raw_ref` 字符串格式无 canonical scheme，跨 session / delegation load 会漂
- **triggered_by_turn**: T6
- **related_v0.2_section**: §6 History Chunks L1888 / §Artifact Retention G9 L1671
- **severity**: low
- **rationale**: 场景 T6 的 `session_artifacts://artifact:...` 是合理直觉但非标准。G9 已定义 `access_scope`（private/delegation/user），跨 scope load 时 raw_ref 的权威 parser 要知道 "protocol → backing store"。没有 scheme 约束，delegation drill-down 或冷存归档后 raw load 会 404。
- **proposed_fix**: §6 新增"raw_ref 规范"子节：`<scheme>://<namespace>:<id>[@<content_hash>]`，scheme ∈ `{session_artifact, conversation_log, object_store, cold_storage, blob}`；每个 scheme 绑定一个 resolver 函数签名。

### GapCand-S02-03. vector stale 事件 payload 未带 `normalize_version` 导致 G3/G17 契约裂缝

- **title**: `retrieval.vector_stale` payload 仅带 content_hash，漏了 G17 要求的 normalize_version
- **triggered_by_turn**: T5
- **related_v0.2_section**: §Retrieval State Machine L1963–1980（G3）/ §Content Hash Normalization Contract L2165–2193（G17）
- **severity**: medium
- **rationale**: G17 L2192 明说 "must compare **both** content_hash and normalize_version"，但 G3 L1963 的 payload 合约只有 `content_hash` + `index_hash`，没有 `normalize_version`。normalize rule 升级时（v1→v2）vector stale 检测会系统性漏判，T5 降级不触发，返回脏 embedding，命中 F1 失败模式。
- **proposed_fix**: G3 payload 加 `normalize_version` + `index_normalize_version`；stale 检测 SQL：`JOIN embeddings e ON e.chunk_id=c.chunk_id WHERE e.index_content_hash != c.content_hash OR e.index_normalize_version != c.normalize_version`。

### GapCand-S02-04. `session_state_items.category` canonical enum 缺 (session-scope)

- **title**: session-scope 的 `category` 无 canonical 白名单，`anchor`/`current_retrieval_target` 类自创语义无处归档
- **triggered_by_turn**: T4
- **related_v0.2_section**: §Session State Projection L1075 / §Cross-Session Scope L2318–2322 / §Compaction Invariants L2508
- **severity**: low
- **rationale**: G6 覆盖了 user-scope 白名单（3 条），G2 列举了 9 大 category 作为 compaction 白名单，但 **session-scope 的开放 category 列表（含场景 T4 用的 `anchor`）** 从未合约化。下游 compaction、审计查询、UI 分组都依赖 category 取值可枚举。
- **proposed_fix**: 新增 `session_state_item_categories` lookup 表 + 初始 seed（`plan_state` / `decision` / `finding` / `benchmark` / `citation` / `todo_state` / `error_state` / `delegation_state` / `summary` / `anchor` / `workspace_state` / `active_skill` / `durable_decision` / `engineering_rule` / `rejected_pattern`）；写入时按 scope × category 组合校验（user-scope 仅允许 G6 的 3 条，session-scope 允许其他）。

---

## §6 Gap 组合效应检验

### 组合 A · G3 + G9 —— 检索命中的 artifact 在 retention 边界过期

- **组合**: G3（三级检索 + raw load）+ G9（artifact retention `expired` 态）
- **运行时可能发生的坑**: T5 FTS 命中 `chk_2026_0415_decision_payment_lag_root_cause`，但 T6 raw load 发现 `session_artifacts.status='expired'`（假设该行 `retention_policy='default'`，30 天后归档到冷存，tombstone 保留 `cold_storage_ref`）。G9 L1731–1738 规定 "render: 'historical artifact; raw no longer available; summary preserved'"——但 T6 场景里期望阿强得到**完整 3 段 SQL**，tombstone 只保 summary 是致命降级。阿强会看到"找到了但拉不回来"。
- **v0.2 是否覆盖**: **partial**。G9 L1724–1728 规定 GC preflight T-7 天会扫 `referenced_by_manifest_count`，但扫的是**被动计数器**；如果这个 session 从未被 T6 引用过该 artifact（只在 `session_history_chunks` 有 chunk 行而 `session_artifacts` 未被 manifest 引用），counter=0，GC 会照归档。场景假设 `retention_policy='project_long_term'` 所以安全，但 v0.2 **未强制** "chunks 指向的 artifact 必须继承 chunks 的 retention 寿命"——这与场景 §8 建议 3 重合。
- **如果 partial，建议**: 补一条 invariant："若 `session_history_chunks.chunk_type IN ('decision','error','artifact_text','tool_output_preview')` 且 `raw_ref` 指向 `session_artifacts`，该 artifact 的 `retention_until` 不得早于 chunk 所在 session 的 `project_retention_policy` 允许的最早归档日；触发 GC 前须 JOIN chunks 表做这一校验。"（场景 §8 建议 3 已口头提过，但未落 G9 schema 条款）

### 组合 B · G3 + G2 —— compaction 后 chunk content_hash 漂移 vs vector stale 降级

- **组合**: G3（vector stale 降级）+ G2（compaction invariants：decision 不得 replace/archive，但 summary 可 append）
- **运行时可能发生的坑**: 4 个月 37 次 compaction 里，假设第 18 次 compaction 虽然遵守 G2 Invariant 2（decision 不 replace），但**改写了** decision chunk 的 `preview_text`（比如加"related services: payment"头）——这属于"projection refresh"还是"destructive rewrite"？G2 L2506 只说 "raw rows 不被物理重写"，preview_text 重写 → `content_hash` 漂移 → 现有 vector 行集体失效。T5 场景里"04-18 importance bump 导致 content_hash 漂移"就是这条路径的一个实例。若 re-embedding queue 滞后（scene 假设 140 条 stale），T5 的 vector stale 降级会覆盖整批决策 chunk，强制走 FTS；但如果 FTS 本身也受中英分词影响（F2）—— agent 两级都失败，只剩 structured 一级的粗匹配。
- **v0.2 是否覆盖**: **partial**。G3 L1982–1985 覆盖了 stale 检测 + re-embedding enqueue，但**没有 re-embedding SLA**（场景 §8 建议 2 要求"stale 后 10 min 内完成"）。G2 L2496 的 invariant 清单没有显式讨论"preview_text 重写算不算 destructive"。
- **如果 partial，建议**: (a) G2 加 Invariant 9："compaction 可重写 preview_text / importance，但必须把旧 preview 快照到 `session_state_item_events` 里做审计，`content_hash` bump 时同步触发 re-embedding enqueue"。(b) G3 加 re-embedding SLA（比如 stale ≤ 10 min rebuilt，超时该 chunk 的 vector 能力视为长期不可用，下次检索直接跳过）。

### 组合 C · G3 + FTS 中英混合分词 —— T5 FTS 可能 0 命中，vector 又已 stale

- **组合**: G3（FTS stage）+ 隐含的 FTS parser 策略（v0.2 未合约化）
- **运行时可能发生的坑**: T5 查询 `'+payment +(lag offset consumer)'`。若 preview_text = "2026-04-15 22:40 payment-svc 消费组位点风暴导致下游对账错位"（混合中英 + 技术黑话），MatrixOne 默认 ngram_token_size=2 会把 "payment" 切成 "pa|ay|ym|me|en|nt"，BOOLEAN MODE `+payment` 找的是完整 token —— 命中率骤降。FTS 返回 0 行 → state machine 按 G3 L1957 走 vector；但 vector 已 stale（场景假设 3/6 行漂移）→ 按 L1958 fallback 回 FTS 结果，FTS 结果是 0 → 回 structured 的 41 行粗匹配 → 用户得到 3 个 cluster 但**不知道 C2 里哪一条是 decision chunk**，T6 没有精确 raw_ref。整个三级检索被"中英混合分词"一条线击穿。
- **v0.2 是否覆盖**: **no**。L1885 只说 "FTS index 建在 preview_text"，没规定 parser；没有 "FTS 0 命中且 vector stale 时如何保底"的策略（目前 state machine 只 fallback 到上一级，最终停留在 structured 粗匹配）。
- **如果 no，建议**: (a) §6 加 FTS parser 合约：MatrixOne 默认使用 `WITH PARSER ngram NGRAM_TOKEN_SIZE=2`，preview_text 写入时预处理为 "中英分段 + 关键实体 NER" 双列（`preview_text` + `preview_fts_tokens`），FTS 打后者。(b) G3 state machine 加一条："若 FTS + vector 都失败，且 structured 候选 > 5，assistant 必须主动 ask_user 做进一步筛选（不可静默返回 top-1）"——这一条场景 T3 刚好已经做了（ask_user 3 cluster），相当于把隐式策略显式化。

### 组合 D · G1 + G3 —— retrieval-scratch 无 LLM 调用时的 manifest 写入时机

- **组合**: G1（context_manifests.reason enum）+ G3（retrieval state machine 每级 persist manifest_items + retrieval events）
- **运行时可能发生的坑**: T2 是纯 retrieval-internal stage（无 LLM 调用），但 G3 L1959 state machine 规定 "persist context_manifest_items + retrieval events"。G1 enum（L1500–1521）只列了 `history_recall_{structured|fts|vector}` 这种面向 LLM-turn 的 reason，**没有** retrieval-scratch / pre-LLM-flush 的类别。实现者会左右为难：(a) 按 G3 要求在 T2 就写 manifest，但 reason 只能强行选 `history_recall_structured` —— 但该 turn 并未真正 render 给 LLM，`included=TRUE/FALSE` 语义都勉强；(b) 延迟到 T3 LLM 调用时合并写，但那样 T2 的 structured retrieval 细节（elapsed_ms、候选 41 行）就只在 `agent_events` 里，`context_manifests` 看不到，审计"本轮三级检索成本"需要 JOIN agent_events —— 多一层间接。
- **v0.2 是否覆盖**: **no**。G1 enum 固定 18 条 canonical reason，没有 `retrieval_scan_only` / `retrieval_prefetch` 这类 pre-LLM 中间态；G3 state machine 写 "persist manifest_items" 但不说写到哪个 manifest（T2 自己的 turnless manifest？T3 的？）。
- **如果 no，建议**: (a) G3 明确 "T2/T5 的 scratch 结果**只**写 `agent_events`（payload 带 candidate_count / elapsed_ms），不写 manifest"；`context_manifests` 只在 LLM 真调用的 turn 写；相应 `context_manifest_items` 的 `reason` 改为描述"这条 item 为何被选/丢"而非描述 turn 类型；(b) 或反之：G1 enum 加 `retrieval_prefetch` reason_class='retrieval_internal', default_zone=NULL，明确允许 turnless manifest 行，但必须带 non-null `request_id` 做 join。本场景倾向 (a)，因为它与场景原文"T2 不进 prompt"更贴。

---

## §7 Token 实际 walkthrough（T7 最紧张轮）

> T7 是本场景 prompt token 预算最紧的一轮：T6 刚加载完 decision artifact + 3 段 SQL，T7 是第一次真正 LLM 调用组装完整 context。

### Zone-by-zone 计算

| Zone | T7 实际 tokens | 计算依据 / v0.2 cap |
| --- | ---: | --- |
| `system_static` | 1200 | 产品级 + safety 常驻 prompt（L1533）；v0.2 默认 policy 未设硬 cap，实测 1200 |
| `tool_schemas` | 1200 | SRE workspace 7 tool（edge_shell/mo_sql/kafka_admin/fetch_url/artifact_read/retrieval_probe/approval）schema pruning 后（L1534，G10 默认不强约束） |
| `session_anchor` | 720 | 金融运维长期 session 目标（320）+ 当前 P1 告警上下文（200）+ T4 写的 `current_retrieval_target=C2` 锁定（200）；v0.2 L1536「Always include; hard cap」但 cap 数值未 v1 固化 |
| `plan_todo` | 280 | 3 条活跃 SRE checklist 各 ~90 tokens（L1537） |
| `recent_tail` | 850 | 昨天 2 轮（420）+ T1 user 问（180）+ T3 assistant 3-cluster 回复（250）= 850（L1538 L2392 "last 2–4 turns"） |
| `summary` | 400 | 4 月份 L1b narrative summary 1 条（L1539 + L2393）；来自第 37 次 compaction |
| `retrieved_facts` | 1000 | 决策 narrative 600（含 provenance header）+ SQL 骨架索引 400（3 段 anchor link），L1540 + G3 L1959 |
| `tool_previews` | 2950 | 3 段 SQL 完整保留：诊断 1400 + 补偿 900 + 对账修复 650；`render_mode='code_block_preserved'`（F4 友情提示：此 render_mode 未 v0.2 合约化）；每段头 `-- provenance:` 约 30 tokens × 3 = 90 含在 2950 内 |
| `safety_approvals` | 0 | 无 pending approval |
| `workspace` | 0 | 未切换 workspace |
| `delegation_state` | 0 | 无子 agent（G18 active_children=0，zone omitted） |
| **子总计** | **8600** | zone 合计 |
| Reserved output | 1000 | §2400 "Reserve output tokens first" |
| Safety buffer | 200 | §2401 "Reserve a safety buffer for tool calls" |
| **Prompt-facing 总计** | **~9600** | 场景 §6 表格写 8330，本推演更严格算进 reserved → ~9600 |

### 对照 v0.2 §Budget Policy v1

- v0.2 L2381–2396 **未给 large-window 的数字 cap**，只给 "zone targets"；`budget_v1_8k`（L2410–2438）是小窗口专属。这意味着 **本场景的 200k 部署没有 canonical budget template ID** —— `budget_template_id` 字段（L1450）按 G10 要求必写，但写什么？`budget_v1_200k` 不在 §G10 表中。→ **这是一个隐性 friction**，与 F1 同类（enum 缺失）。
- 假设写 `budget_v1_default`，则 9600 / 200000 = 4.8% 窗口占用，完全不紧张。对比朴素全塞方案：10GB ÷ 4 bytes/token ≈ 2.5B tokens，物理不可行。
- `dropped_count=42`：T2 structured 候选 41 - T3 show 3 = 38 dropped；T5 FTS 返回 6 - top 2 selected = 4 dropped。
- `compaction_version='v37'` 继承，`prompt_cache_key` 基于 stable_prefix_hash（system + tools + session_anchor 部分）算。

### 溢出风险

- **无溢出**：9600 << 200000。甚至 T10 峰值 9880 仍 < 5% 窗口。
- **若切到小窗口** `budget_v1_8k`（cap 8000 prompt + 500 output + 200 safety = 8700 上限）：T7 的 tool_previews 2950 会**立即超** L2423 的 500 cap。G10 L2430 "tool schemas and retrieved facts are aggressively pruned first"，但 3 段 SQL 无法压缩（render_mode='code_block_preserved' 禁止 token-level truncate）→ 必须改为 "retrieved_facts 只放 decision narrative 600 + artifact deep link，tool_previews 只放 1 段 SQL"，阿强需要点 UI link 展开其他两段 —— 体验下降但可用。
- **结论**：大窗口模型下完全顺畅；小窗口部署时需要额外的 "SQL 展开链接"降级策略，当前 v0.2 未规范这种 render-time fallback。

### T7 每 zone 项明细（逐行 token 账）

> 把上表里每个 zone 拆到具体 `context_manifest_items` 行 —— 验证 §4 F4 的 render_mode 影响。

**`retrieved_facts` zone — 1000 tokens 拆分**：

| item_order | source_table | source_id | render_mode | token_estimate | budget_tokens | reason（item-level）|
| ---: | --- | --- | --- | ---: | ---: | --- |
| 1 | session_history_chunks | chk_2026_0415_decision_payment_lag_root_cause | `narrative` | 600 | 700 | retrieval_top1_narrative |
| 2 | session_history_chunks | chk_2026_0415_tool_preview_fix_sql | `code_block_preserved` (anchor-only) | 400 | 500 | retrieval_top2_skeleton |

`narrative` item 是 agent 对 decision artifact 的自然语言压缩（agent 读 4500-token 原文后 generate 600-token 精华）；`code_block_preserved` 在 retrieved_facts 里只放"骨架 anchor"（每段 SQL 的 title + 首行 + `see tool_previews[#N]` 指针），完整 SQL 下沉到 tool_previews。

**`tool_previews` zone — 2950 tokens 拆分**：

| item_order | source_id | render_mode | token_estimate | 内容 |
| ---: | --- | --- | ---: | --- |
| 1 | artifact:fix-payment-offset-realign-20260415#section=1 | `code_block_preserved` | 1400 | 诊断 offset 漂移 SQL + provenance header |
| 2 | artifact:fix-payment-offset-realign-20260415#section=2 | `code_block_preserved` | 900 | payment_event 对齐补偿 SQL |
| 3 | artifact:fix-payment-offset-realign-20260415#section=3 | `code_block_preserved` | 650 | payment_settlement 对账修复 SQL |

**关键不变量 — 语句边界切分**：3 段 SQL 合计 2950 tokens，原 artifact 15k tokens。agent 按 `;` + 换行语句边界切片 → 3 段各为 "完整语句序列" 而非 "token 切片"，绝不在 SQL 中间断开。3 段各自在 `tool_previews.max_single_item_cap`（假设默认 1500 tokens）内，诊断段最接近 cap（1400/1500 = 93%）。**provenance 保留**：每段 header 写 `-- provenance: artifact_id=fix-payment-offset-realign-20260415, section=N/3, source_hash=9b12…e4d7`，约 30 tokens × 3 = 90 tokens 包含在 2950 内。→ T7 `retrieved_facts ≤ 3k`、`tool_previews ≤ 4k` 两条 budget 上限都满足。

### 跨回合峰值对比

| Turn | 合计 prompt tokens | dominant reason | 是否触发 prompt_cache |
| ---: | ---: | --- | --- |
| T3 | 4740 | history_recall_structured | No（首次 manifest） |
| T7 | 9600 | history_recall_fts | Partial（stable prefix cache T3 部分） |
| T8 | 8970 | normal_turn | Yes（stable prefix = T7 system + tools + anchor + retrieved_facts + tool_previews） |
| T9 | 9620 | large_tool_output_gated | Yes（继续 T7 前缀） |
| **T10 峰值** | **9880** | large_tool_output_gated | Yes |

峰值 9880 / 200000 = **4.94%** 窗口占用。T8–T10 走 prompt cache → 计费上 ~60% stable prefix 进 cached pricing，**实际付费 token 约 4000** per turn（cache hit 片段不重复计费）。

### 若切到 `budget_v1_8k` 小窗口部署的崩溃模式

假设把模型替换为本地 8k 窗口小模型（`budget_v1_8k`，§G10 L2416–2426）：

- 硬 cap：anchor 200 / plan_todo 400 / recent_tail 2000 / summary 500 / **retrieved_facts 1000 / tool_previews 500** / system+tools 3400 / output 500 / safety 200 = 8700。
- T7 组 prompt：`retrieved_facts` 本来 1000 ✅；但 `tool_previews` 2950 >> 500 cap **立即超**。
- `code_block_preserved` 禁止 token-level truncate → SQL 不能切碎 → 唯一路径是"把 3 段 SQL 从 prompt 里移出去，改成 `render_mode='ref_only'`，UI 侧让用户点链接展开"。此时 `tool_previews` 变 0，`retrieved_facts` 仍 1000（含 SQL skeleton + artifact deep-link）。
- 但是 v0.2 **未规范** render-time fallback（"当 zone cap 不够放完整 code block 时，自动降级为 ref_only 并在 context 里加一段 'code deferred to UI'"）。实现者要自己决定怎么做 —— 大概率写出三种不同行为。→ 这一点隐含在 F4 里。

---

## §8 最终判断

### 机械可达性（Mechanical feasibility）

- **三级检索 SLA**：structured 38ms（G3 <50ms ✅）+ FTS 110ms（<200ms ✅）+ raw load 12ms（<250ms hot ✅），端到端 160ms << 1s 目标。**10GB 数据规模下可达**，前提是 `idx_history_type_created` 和 preview_text FTS index 覆盖完整；`session_history_chunks` 在 MatrixOne / MySQL 合理硬件配置下 68 万行 + decision/error 索引 hot 完全可喂。
- **Schema / 契约 / API 允许性**：T1–T10 所有写入路径（`context_manifests` enum reason ✅；`context_manifest_items.source_hash` ✅；`session_artifacts.derived_from_artifact_id` ✅；`session_state_items.scope='session'` anchor ✅；`agent_events` retrieval.* ✅）都有 v0.2 schema 合约支撑，**无 schema-level 阻塞**。
- **边界情况**：vector stale 降级（G3 L1982–1985）+ FTS 二次命中（G3 L1941）+ raw_ref hot load（G3 L1943）三条路径组合即本场景 T5→T6 的 happy path。

### 顺畅度（UX smooth）

- 用户从 T1 提问到 T7 回答预计 ~8 秒（含 LLM streaming），其中检索占 160ms，LLM 生成占主体 —— **体感远优于"肉眼翻 4 个月 session 历史"**。T8–T10 走 prompt_cache，follow-up 响应 ~3 秒。
- 澄清环节 T3→T4（3 cluster ask_user）是**主动优秀设计**而非摩擦：与其返回 top-1 猜错，不如让用户选。阿强作为 SRE 对这种交互友好。
- UI 层面：artifact 深链（T7）+ plan_todo 同步（T8）+ artifact 归档 badge（T10）都依赖 SSE event 链完整，多 tab 下依赖 G19 watermark 原子性。

### 综合评分

- **本场景下 v0.2 是否可实现**: ⚠️ **大体可行**，核心路径机械可达 + SLA 达标；存在 3 处契约细节缝隙（F1 manifest 写入时机 / F2 FTS parser / F3 vector stale payload 漏 normalize_version）和 2 处 minor 不规范（F4 render_mode / F5 raw_ref scheme）
- **顺畅度**: ⚠️ **有明显摩擦但不致命** —— 摩擦集中在"同一概念在不同 gap patch 里有不同字段表达"（例：derived_from 链路在 G9 的 `derived_from_artifact_id` 列 vs `context_manifest_items.reason='derived_from_reference'` 未统一），影响审计体感
- **是否发现新 gap**: **是**，3 条 medium 级新 gap 候选见 §5（GapCand-S02-01 render_mode enum / GapCand-S02-02 raw_ref scheme / GapCand-S02-03 vector stale payload 带 normalize_version）
- **一句话总结**: 10GB session 的三级检索在 v0.2 的机械契约下能跑到 ~160ms 内拉回目标 artifact 且 prompt < 10k / <5% 窗口，核心路径完全可达；但 retrieval manifest 写入时机、FTS parser 策略、vector stale 与 G17 normalize_version 联动、raw_ref scheme 规范这 4 处细节没有落到 v0.2 schema / enum 里，实现时会出现"跑得起来但团队各写各的"的工程化漂移，建议按 §5 的 3 条新 gap 候选做一轮补丁即可收口 S02。


### 提交元信息

- 推演基准: `web-agent-session-state.md` v0.2（3040 行，19 条 gap 全部 resolved）
- 场景: `S02-huge-session-old-bug.md` v0.1 Draft §1–§3
- 推演范围: T1–T10 全部 10 回合，含 vector stale 降级 / code_block_preserved 保真 / derived artifact provenance 三条关键路径
- 未覆盖: §8 反馈已读但未 prime，GAP-TRACKER.md 仅读 G3/G9/G2/G1/G10/G17 相关条目
