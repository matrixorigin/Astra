# Regression Verification — Sprint B (G1 / G3 / G5 / G6 / G7 / G8 / G9 / G18 / G19)

> Verifier: regression sub-agent (quality reviewer, strict mode)
> Date: 2026-05-07
> Target doc: `docs/design/web-agent-session-state.md` v0.2 @ 2560 行
> Baseline: `REGRESSION-SPRINT-A.md`（G2/G4/G15 均 `partially_resolved`）
>
> **Verdict summary**:
> - Sprint B resolved 声明：**G1=`fully_resolved`** / **G3=`fully_resolved`** /
>   **G5=`fully_resolved`** / **G6=`partially_resolved`** /
>   **G7=`fully_resolved`** / **G8=`fully_resolved`** /
>   **G9=`fully_resolved`** / **G18=`fully_resolved`**
> - **G19** 在 GAP-TRACKER 中仍为 `open`（无 patch、无 GAP-FIX 标记）；
>   题面中"顺手做的 G19 也 resolved"的说法**不成立**
> - Sprint A residual 回收情况：
>   - **G2 可从 `partially_resolved` → `resolved`**（见下方详析）
>   - **G4 可从 `partially_resolved` → `resolved`**
>   - **G15 维持 `partially_resolved`**（r1 clock-skew 已解，但 r2 原子性 = G19 仍未落地；r4 payload schema 未补）

---

## 前置：v0.2 GAP-FIX 锚点清单（grep 佐证）

```text
L  735 <!-- GAP-FIX: G15 -->        L  882 <!-- /GAP-FIX: G15 -->
L  973 <!-- GAP-FIX: G7  -->        L 1075 <!-- /GAP-FIX: G7  -->
L 1077 <!-- GAP-FIX: G4  -->        L 1182 <!-- /GAP-FIX: G4  -->
L 1216 <!-- GAP-FIX: G1  -->        L 1281 <!-- /GAP-FIX: G1  -->
L 1367 <!-- GAP-FIX: G8  -->        L 1421 <!-- /GAP-FIX: G8  -->
L 1423 <!-- GAP-FIX: G9  -->        L 1500 <!-- /GAP-FIX: G9  -->
L 1681 <!-- GAP-FIX: G3  -->        L 1738 <!-- /GAP-FIX: G3  -->
L 1893 <!-- GAP-FIX: G5  -->        L 1946 <!-- /GAP-FIX: G5  -->
L 1948 <!-- GAP-FIX: G6  -->        L 1999 <!-- /GAP-FIX: G6  -->
L 2054 <!-- GAP-FIX: G18 -->        L 2083 <!-- /GAP-FIX: G18 -->
L 2104 <!-- GAP-FIX: G2  -->        L 2204 <!-- /GAP-FIX: G2  -->
```

**G19 在文档中不存在 `<!-- GAP-FIX: G19 -->` 锚点**。grep 无命中。以此作为
本回归最显著的空白。

---

## G1. `context_manifests.reason` 未枚举化

### 承诺清单（from GAP-TRACKER `patch_summary`）

1. 新增 `context_manifest_reason_types` lookup 表
2. 指定 Rust-side validation（MatrixOne 无法强制 FK 时）
3. 种子数据覆盖 lifecycle / compaction / retrieval / tool gating / plan tree /
   workspace / approval / cross-session recall / delegation / final delivery

### 场景反馈回应

**S01 §8 建议 2**（feedback L10–16）：manifest reason 需覆盖 `initial_turn` /
`post_compaction_turn` / `history_recall_structured|fts|vector` /
`large_tool_output_gated` / `final_delivery_summary`。

**S06 §8 建议 4**（feedback L29–33）：需新增 `plan_subtree_query`（T10）/
`tree_structured_report`（T35）枚举值。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| lookup 表 DDL | L1218–1226 `context_manifest_reason_types` | ✅ |
| manifest items 表（独立于 reason 枚举） | L1228–1246 | ✅（超前加入 `context_manifest_items`，正是 zone 追踪所需） |
| Rust-side 验证声明 | L1249–1252 明确 "Free-form reason strings are not allowed in production." | ✅ |
| `initial_turn` / `post_compaction` | L1258 / L1260 | ✅ |
| `history_recall_{structured,fts,vector}` | L1261–1263 | ✅ |
| `large_tool_output_gated` | L1264 | ✅ |
| `plan_subtree_query` / `tree_structured_report` | L1265 / L1266 | ✅ |
| `workspace_switch` / `approval_resume` / `cross_session_recall` | L1267–1269 | ✅ |
| **bonus**: `delegation_poll` / `partial_blocker_review` / `delegation_aggregate` | L1270–1272 | ✅ 超集，被 G18 的 `partial_blocker_review` 反向消费（L2079） |
| `final_delivery_summary` | L1273 | ✅ |
| reason selection 语义 | L1275–1279 "highest-cost or most-specific" | ✅ |

**Cross-gap consumption**：
- G2 SQL #6（L2178–2182）消费 `post_compaction`（枚举命中）
- G18 L2079 消费 `partial_blocker_review`（枚举命中）
- G5 L1910–1911 明确要求 manifest 区分 `plan_subtree_query` 与
  `tree_structured_report`（枚举命中）
- G6 新增 `cross_session_recall`（枚举命中）

### Verdict: `fully_resolved`

proposed_fix 4 条全部落地，`triggered_by` 场景反馈 100% 回应，并为 G2/G5/G6/G18
下游消费枚举留好路径。

### Residual gaps
- **无** — GAP-TRACKER 状态可由 `resolved` 保持

---

## G3. Retrieval SLA state machine

### 承诺清单

1. Staged SLA 表（structured <50ms / FTS <200ms / vector <500ms / 全链 <1s）
2. Hard caps + `retrieval.<stage>_<reason>` 事件
3. 每轮 1 次 vector 查询 / stale vector hash skip + reindex
4. Retrieval event payload contract

### 场景反馈回应

**S01 §8 建议 3**（feedback L32–34）：每轮 retrieval 预算表，1 次 vector。

**S02 §8 建议 1**（feedback L16–19）：三级 SLA + 降级 state machine，每级
必须独立降级，写 `retrieval.<stage>_<reason>` 事件。

**S02 §8 建议 2**（feedback L20–25）：content_hash 漂移时 stale skip + 排队
重嵌入。

**S02 §8 建议 3**（feedback L26–32）：`raw_ref` artifact retention 绑定（→ G9）。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| 4 级 SLA 表（structured/fts/vector/raw） | L1689–1694 | ✅ |
| `structured <50ms / fts <200ms / vector <500ms` | L1691–1693 每行 Target SLA 列 | ✅ |
| 全链 `<1s` 排除 cold fetch | L1696–1697 | ✅ |
| hard cap（1000 scanned / 50 candidate / 100 / 20 等） | L1691–1693 右列 | ✅ |
| `retrieval.structured_empty/timeout/bound_exceeded` | L1691 | ✅ |
| `retrieval.fts_empty/timeout` | L1692 | ✅ |
| `retrieval.vector_empty/timeout/stale` | L1693 | ✅ |
| `retrieval.raw_missing/raw_cold_fetch_required` | L1694 | ✅（`raw_cold_fetch_required` 正是 S02 §8-3 的 signal） |
| 每轮 1 次 vector | L1693 "1 vector query per turn" | ✅ |
| state machine 伪码 | L1702–1712 | ✅ |
| retrieval event payload JSON schema | L1716–1731 | ✅（含 `stage/reason/candidate_count/elapsed_ms/timeout_ms/query_hash/chunk_id/content_hash/index_hash/fallback_stage`） |
| Vector stale → skip + reindex | L1733–1736 | ✅（要求 emit `retrieval.vector_stale`，skip 该行，enqueue re-embedding） |
| "old detail not found → 直说而非编造" | L1697–1698 | ✅（S01 §8 建议 3 的引申） |

### Verdict: `fully_resolved`

proposed_fix 全部落地，S01/S02 §8 对应 G3 的 3 条建议全部回应（S02 建议 3 的
artifact retention 由 G9 覆盖）。

### Residual gaps
- **无** — GAP-TRACKER 可保持 `resolved`
- **Cross-gap observation（不影响 verdict）**：retrieval event payload
  `source_session_id` 字段（L1721）承载 G6 `cross_session_recall` 用途；
  该字段与 G6 §Cross-Session Scope 协同正确（见 G6 分析）。

---

## G5. Plan Tree Rendering Policy

### 承诺清单

1. `session_todos` 增 depth / path / summary / provenance 字段 + parent/status 索引
2. Rendering 4 规则（祖先链 / current subtasks / 兄弟 / explicit subtree API）
3. 2 段 SQL（祖先链 / pending subtree）
4. Manifest reason 增 `plan_subtree_query` / `tree_structured_report`

### 场景反馈回应（S06 §8）

S06 §8 建议 1（feedback L5–12 + L37–40）：4 条渲染规则必须明示。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| `session_todos.depth / path / summary_text` | L1857–1861 | ✅ |
| `provenance_event_id` | L1866 | ✅ |
| `idx_todos_parent_status`（parent_id + status）| L1874 | ✅（命中 S06 §8-1 子树查询） |
| `idx_todos_backlog_pool`（为 G6 共用） | L1875 | ✅（跨 gap 复用） |
| Rule 1 — 祖先链渲染 | L1903–1904 | ✅ |
| Rule 2 — current section 非 archived subtask | L1905–1906 | ✅ |
| Rule 3 — 兄弟 section 仅 title | L1907 | ✅ |
| Rule 4 — cross-subtree 走 explicit API | L1908–1909 | ✅（API `GET /sessions/{id}/plan/subtree?root=` 在 L2283） |
| Rule 5 — manifest 区分两 reason | L1910–1911 | ✅（枚举值已在 G1 落地） |
| 祖先链 SQL | L1915–1929（recursive CTE） | ✅ |
| pending subtree SQL | L1933–1941 | ✅ |
| materialized path（深树优化） | L1943–1944 | ✅ |

**Cross-gap sanity**：S06 反馈还要求 compaction 保护 delegation_state active
（属 G2，已覆盖）与 manifest reason 枚举（属 G1，已覆盖）。G5 与 G4 的
delegation tree 是两棵独立树：G5 渲染的是 `session_todos`（plan 骨架），
与 `session_delegations.ancestor_path`（delegation 执行树）不混淆。

### Verdict: `fully_resolved`

proposed_fix 4 条全部落地，S06 §8-1 的全部 4 条渲染规则均有显式 bullet 与 SQL。

### Residual gaps
- **无** — GAP-TRACKER 可保持 `resolved`

---

## G6. 跨 session scope (`scope='user'`)

### 承诺清单

1. `session_state_items.scope` 扩枚举 `session/user/project/workspace`
2. user-scope category 白名单 `durable_decision/engineering_rule/rejected_pattern`
3. 载荷含 `origin_session_id` + `origin_chunk_id` provenance
4. `idx_history_user_type_created` 跨 session 检索索引
5. ContextAssembler 新 session 预算 ≤400 token
6. `session_todos.backlog_pool_id` + `status='backlog'`

### 场景反馈回应

**S05 §8 建议 1**（feedback L27–30）：backlog_pool_id + status='backlog' 第三态。

**S11 §8 建议 1**（feedback L16–21）：`scope='user'` + 白名单 + provenance +
400 token 预算。

**S11 §8 建议 2**（feedback L22–26）：`idx_history_user_type_created` 补齐 +
跨 session 查询必须带 `user_id` 谓词。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| scope enum 文字 session/user/project/workspace | L1952–1957 | ✅ |
| `session_state_items.scope` 列（default='session'） | L894 | ✅ |
| `idx_state_user_scope_category`（user/scope/category/status/priority） | L919 | ✅ |
| `origin_session_id` / `origin_chunk_id` / `origin_state_item_id` | L908–910 | ✅（真实列而非 JSON 内部字段） |
| `idx_state_origin_session` | L920 | ✅ |
| user-scope 白名单 `durable_decision / engineering_rule / rejected_pattern` | L1964–1968 | ✅ |
| category 主列表同步 | L964–966 | ✅ |
| namespace key 约定（`user:<user_id>` 等） | L1959–1962 | ✅ |
| 400 token 预算 + 加载 SQL | L1976–1989 | ✅ |
| `session_todos.backlog_pool_id` | L1855 | ✅ |
| `origin_session_id` on session_todos | L1856 | ✅ |
| `idx_todos_backlog_pool` | L1875 | ✅ |
| `idx_history_user_type_created` on session_history_chunks | L1610 | ✅ |
| 跨 session 检索要求 `user_id` 否则 auth error | L1995–1997 | ✅ |
| backlog status/attachement 语义 | L1991–1993 | ✅ |

### v0.2 内部一致性观察

1. **backlog `status='backlog'` 与 compaction invariant #5 的交互未显式**：
   G2 invariant #5（L2124–2125）只说 "completed subtasks may be archived"，
   没说 `status='backlog'` 的 todo 如何被 compaction 对待。若一个 session 里
   backlog 数目很多，compaction 是否允许把它们 archive？**未契约化**，需 Sprint C
   补 1 段说明或合并到 G14。
2. **user-scope item 的 `session_id` 存 `user:<user_id>` 字符串**：L1959–1962。
   但 L890 `session_state_items` schema 里 `session_id VARCHAR(128) NOT NULL`
   是自由列。UNIQUE `(session_id, scope, category, item_key)` L916 依赖
   `session_id` 做主键区分，这条 UPSERT 唯一性仍然成立（`user:xxx` 与 `sess_yyy`
   不会冲突）。**没问题**，但依赖命名约定（magic string），**建议补一条**在
   §Cross-Session Scope 下明说"写入端必须使用该 prefix，否则索引退化"。
3. **`scope='user'` 的 `idx_state_user_scope_category` 是 `(user_id, scope,
   category, status, priority)`**（L919）。`status='active'` 是点谓词，`priority`
   做 ORDER BY，命中索引。与 L1978–1985 的加载 SQL（WHERE user_id + scope='user'
   + status='active' + category IN (...) ORDER BY priority DESC, updated_at DESC）
   对齐。**`updated_at` 未在索引列**，会触发 filesort；在 ≤400 token 限制下影响
   可忽略，但需 Sprint C 实现期观测。

### Verdict: `partially_resolved`

proposed_fix 全部落地，S05/S11 §8 反馈全部回应。但两点**文档层面的不明确**
未阻塞实现，仍应在 GAP-TRACKER 记录为 partial：

- backlog 与 compaction archive 的交互
- namespace key (`user:<user_id>`) 命名约定未在 §Cross-Session Scope 前置
  "必须"语句（目前只说"stores the namespace owner key"，未要求 prefix 格式）

### Residual gaps

| ID | 描述 | 处理建议 |
| --- | --- | --- |
| G6-r1 | backlog todo 在 compaction invariant 下的处理未写 | 合并到 G14 或 Sprint C 补 compaction invariant #9 |
| G6-r2 | namespace key magic string `user:/project:/workspace:` 未做成常量或 enum | v0.2 内部编辑：在 §Cross-Session Scope 第 2 段加强语气 "MUST use this prefix; otherwise idx_state_user_scope_category will miss." |

（这两条都属于"表述松"，不降低场景覆盖率；可保留 `resolved` 或降级
`partially_resolved` — 建议后者，但可由设计 agent 选择）

---

## G7. Approval conditions + external notifications

### 承诺清单

1. `session_approval_conditions` 表
2. `session_external_notifications` 表
3. approval_state payload contract
4. 状态机 `pending_approvers → approved → pending_requester_confirm → running → completed`
5. `notification_dispatched` / `notification_acknowledged` / `requester_confirm` /
   `edge_timeout` run events
6. `expires_at_server` + `ttl_seconds` display 语义
7. `waiting_for_edge` 300s 超时转 failed/waiting_for_user

### 场景反馈回应（S07 §8）

建议 1（feedback L23–29）：conditions 独立子表（approval_item_id / condition_type /
condition_spec_json / check_trigger / added_by / added_at）。

建议 2（feedback L30–34）：`waiting_for_edge` 超时转 failed 策略 + per-run
configurable。

建议 3（feedback L35–40）：External Notification Adapter 首节 + `notification_dispatched`
/ `notification_acknowledged` 事件类型 + idempotency key。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| `session_approval_conditions` 表 | L980–1000 | ✅ |
| — 字段 `approval_item_id/condition_type/condition_spec_json/check_trigger/added_by/added_at` | L982/986/987/988/990/991 | ✅（7/7） |
| — `expires_at_server` 字段 | L992 | ✅（超集） |
| — 索引 `(condition_type, check_trigger, status, expires_at_server)` | L998 | ✅（S07 §8-1 "按 condition_type + check_trigger 查询" 直接命中） |
| `session_external_notifications` 表 | L1002–1024 | ✅ |
| — `idempotency_key` 唯一键 | L1021 `uq_notification_idempotency` | ✅ |
| — `external_ref` / `payload_hash` / `delivered_at` / `acknowledged_at` | L1013–1016 | ✅ |
| approval state item payload schema | L1029–1041 | ✅（含 `required_approvers / approvals / condition_refs / evidence / linked_approval_ref / expires_at_server / ttl_seconds`） |
| 状态机 | L1045–1055 | ✅（pending_approvers → approved → pending_requester_confirm → running → completed；+ rejected / approval_expired / approval_retracted） |
| Requester confirm 单独 run event | L1057–1059 | ✅ |
| External notification adapter 协议 | L1061–1069 | ✅ |
| Idempotency 语义 | L1066–1067 | ✅ |
| `expires_at_server` vs UI `ttl_seconds` | L1068–1069 | ✅ **Sprint A G15-r1 顺带解决** |
| `waiting_for_edge` 300s 超时 + per-run policy | L1071–1073 `checkpoint_json.extra.waiting_for_edge_policy` | ✅ |
| 新事件类型 `approval_expired/retracted/request/decision/condition_modified/requester_confirm/notification_dispatched/notification_acknowledged/edge_timeout/run_resumed_after_restart` | L862–873 | ✅（一等事件列表扩充至 10 条） |

### v0.2 内部一致性

1. **G7 与 G15 的 run event 列表统一**：L862–873 是 G15 `<!-- GAP-FIX: G15 -->`
   范围内（L735–882）的同一列表；Sprint A 的 3 个事件 + Sprint B 的 7 个事件
   全部合并，无重名。✅
2. **`session_approval_conditions` 未使用 FK 指向 `session_state_items`**：
   `approval_item_id` 是 VARCHAR(128) 自由列，语义上应等于
   `session_state_items.item_id`（where category='approval_state'）。
   **未声明**；但同文档其他表也用 advisory logical FK，风格一致。✅
3. **`expires_at_server` 使用 `TIMESTAMP NULL`**：L992。与 `UNIQUE/idempotency`
   无冲突。
4. **Payload contract JSON 的 `ttl_seconds` 与 DB `expires_at_server`** 是双源：
   UI 用 `ttl_seconds` 显示（可以是客户端倒计时相对值），服务端用
   `expires_at_server` 判实际过期 — L1068–1069 显式声明。**正确解决
   G15-r1 "clock skew"**。✅

### Verdict: `fully_resolved`

proposed_fix 7 条全部落地。S07 §8 全部 3 条建议都有对应 schema + protocol + 事件。
同时顺带解决 Sprint A **G15-r1（expires_at_server/ttl_seconds）**。

### Residual gaps
- **无** — GAP-TRACKER 可保持 `resolved`
- **Cross-gap resolve**: G15-r1 在 G7 里被解决。REGRESSION-SPRINT-A §G15
  residual 表可以划掉 r1。

---

## G8. Preview Template Registry

### 承诺清单

1. `preview_template_registry` 表
2. `session_tool_outputs` 扩 preview template/version + normalize_version +
   preview status/token/byte caps + first-class columns
3. 基线模板 pg_dump / slow_query_analyzer / fetch_url / parse_pdf /
   llm_extract_findings / benchmark_slice
4. `finding / benchmark / citation` payload contract

### 场景反馈回应

**S08 §8 建议 1**（feedback L5–12）：按 `tool_name` 分级 preview 长度。

**S08 §8 建议 3**（feedback L23–29）：`content_hash` 规范化 + `normalize_version` —
这是 G17（尚 open）的范围，但 G8 在 schema 侧埋了 `normalize_version` 字段
（L1320 / L1355）作为 hook，属边际贡献。

**S12 §8 建议 1**（feedback L5–13）：Preview Template Registry（`version` /
`first_class_columns` / `fts_field_weights` / `max_preview_bytes`）。

**S12 §8 建议 2**（feedback L14–21）：`finding` / `benchmark` / `citation` 作为
一等结构化 chunk category，compaction 白名单 — 与 G2 invariant #2 联动。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| `preview_template_registry` 表 | L1347–1361 | ✅ |
| — (tool_name, version) 主键 | L1359 | ✅ |
| — `first_class_columns_json` / `fts_field_weights_json` / `max_preview_bytes` / `normalize_version` / `schema_json` | L1351–1356 | ✅ |
| `session_tool_outputs` 扩列 `preview_template_version` / `normalize_version` / `preview_status` / `preview_token_estimate` / `max_preview_bytes` | L1319 / L1320 / L1325 / L1324 / L1326 | ✅ |
| 一等提升列 `artifact_kind/row_count/error_count/duration_ms/http_status/content_type/relevance_score` | L1329–1335 | ✅（覆盖 S12 反馈的 FTS field 提升） |
| 验证行为（无模板 400 字符兜底 + `preview_status='fallback'`） | L1372–1374 | ✅ |
| YAML 样例 | L1378–1397 | ✅ |
| 基线模板 pg_dump | L1403 | ✅ |
| slow_query_analyzer | L1404 | ✅ |
| fetch_url | L1405 | ✅ |
| parse_pdf | L1406 | ✅ |
| llm_extract_findings（chunk_type=finding） | L1407 | ✅ |
| benchmark_slice（chunk_type=benchmark） | L1408 | ✅ |
| `finding` payload `{claim, evidence_refs[], confidence, source_artifact_ref}` | L1412 | ✅ |
| `benchmark` payload `{metric, value, unit, baseline, method, source_artifact_ref}` | L1413 | ✅ |
| `citation` payload `{source_artifact_ref, locator, quote_hash, summary}` | L1414 | ✅ |
| "compaction 保护 + Sprint B 用 insert/update 做跨 agent 提升，tree bubble_up 仍属 G14" | L1417–1419 | ✅ 明确切分 |

**Sprint A G2-r1 顺带解决**：G2 白名单包含 `finding/benchmark/citation`
（L2116–2117 / L2146），Sprint A 回归批评"category 抢跑但 payload 未 spec"。
G8 在 L1410–1414 补齐 payload 字段。**G2-r1 由 G8 覆盖**。

**Sprint A G2-r2 独立核对**：`session_history_chunks.chunk_type` 需同步
`finding/benchmark/citation`。现在 L1615–1630 的 chunk_type 列表：
`user_message / assistant_message / tool_call / tool_output_preview /
artifact_text / file_snapshot / decision / error / plan_change / todo_change /
summary / finding / benchmark / citation` — **三类齐全**。G2-r2 resolved.

### v0.2 内部一致性

1. **`session_tool_outputs.preview_text TEXT NULL` 无硬长度约束**：L1323。
   模板里 `max_preview_bytes` 是应用层校验（L1372 "runtime validates
   output previews against the active template"）。DB 层没硬约束，这是
   Development Rule 可接受（TEXT 比 VARCHAR 更灵活），但 runtime 必须强制。
   ✅（设计上清楚）
2. **`fts_field_weights_json` 是 JSON 列**：L1354。Development Rules 禁止
   **filter** JSON 列。但这里是读配置表，runtime 加载后用，不做 WHERE
   谓词。✅
3. **G8 模板引入的 "chunk_type" 与 G2 chunk_type 白名单冲突？**
   - G8 template 默认 `default_chunk_type VARCHAR(64) NOT NULL DEFAULT 'tool_output_preview'`（L1352）
   - G2 compaction invariant #2 只保护 `session_state_items.category`，不直接
     依赖 chunk_type
   - G2-r2 后 `chunk_type` 列表扩展 `finding/benchmark/citation` 与 G8
     baseline 模板 `llm_extract_findings → finding` / `benchmark_slice →
     benchmark` 对应一致。✅

### Verdict: `fully_resolved`

proposed_fix 全部落地。S08/S12 §8 反馈 100% 覆盖。同时**顺带解决
Sprint A G2-r1（finding/benchmark/citation payload schema）和 G2-r2
（chunk_type 同步）**。

### Residual gaps
- **无** — GAP-TRACKER 可保持 `resolved`
- **Cross-gap resolve**: G2-r1 + G2-r2 都在 G8 里被解决。

---

## G9. Artifact retention 双向感知

### 承诺清单

1. `session_artifacts` 增 `retention_policy` / `retention_until` / `status` /
   `access_scope` / `project_id` / `referenced_by_{manifest,state_items,citation}_count`
2. `agent_sessions` 增 `project_id` / `project_retention_policy`
3. T-7 GC preflight 扫描 + cold-storage/tombstone + expired degradation
4. `private/delegation/user` 三级 access scope
5. presigned URL 下载策略

### 场景反馈回应

**S08 §8 建议 2**（feedback L13–22）：`retention_until` + `status` +
`referenced_by_manifest_count` + T-7 扫描 + expired → "historical, raw no longer
available" 占位。

**S12 §8 建议 3**（feedback L22–29 + L44–50）：`retention_policy`（default/
project_long_term/permanent）+ `agent_sessions.project_id` + GC 反向查询
`referenced_by_state_items_count` + `referenced_by_citation_count`。

**S09 §8 建议 2**（feedback L34–40，属 G4 residual r4）：`private/delegation/
user` 三级 access scope。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| `retention_policy` (default/project_long_term/permanent) | L1435 + L1453–1457 | ✅ |
| `retention_until` | L1436 | ✅ |
| `status` (active/expiring/archived_cold/expired) | L1437 + L1459–1464 | ✅（四态齐） |
| `cold_storage_ref` | L1438 | ✅（联动 S02 §8-3 要求） |
| `derived_from_artifact_id` | L1439 | ✅ |
| `referenced_by_manifest_count` | L1440 | ✅ |
| `referenced_by_state_items_count` | L1441 | ✅ |
| `referenced_by_citation_count` | L1442 | ✅ |
| `access_scope` (private/delegation/user) | L1434 + L1466–1471 | ✅ **G4-r4 resolved** |
| `agent_sessions.project_id` + `project_retention_policy` | L1448–1449 | ✅ |
| idx_artifacts_retention / idx_artifacts_project / idx_artifacts_derived | L1443–1445 | ✅ |
| idx_sessions_project | L1450 | ✅ |
| T-7 GC preflight 步骤化 | L1473–1480（5 步 list） | ✅ |
| Expired artifact context degradation | L1482–1490 | ✅ "historical artifact; raw no longer available; summary preserved" 与 S08 §8-2 的文字高度一致 |
| Presigned URL | L1492–1493 | ✅（API 在 L2293–2294 `GET /artifacts/{id}/download-url`） |
| Cross-gap 说明（citation 依赖 G8 / delegation 依赖 G4/G18 / G14 bubble_up 未定义） | L1495–1498 | ✅ 清楚切分 |

### v0.2 内部一致性

1. **G9 与 G6 的 `access_scope` 关系**：G6 定义 `session_state_items.scope`
   为 `session/user/project/workspace`；G9 定义 `session_artifacts.access_scope`
   为 `private/delegation/user`。**两套 scope 命名不完全一致**：
   - G6 的 `session` ↔ G9 的 `private`（语义相同，命名不同）
   - G6 的 `user` ↔ G9 的 `user` ✅
   - G6 的 `project/workspace` 在 G9 没对应，而 G9 的 `delegation`（跨父子 run
     可见）在 G6 没对应。
   这是 artifact 与 state item 语义差异导致（artifact 有 delegation tree 作为
   共享单位，state item 以 session/user/project 为单位），**合理但命名分裂**。
   建议 Sprint C 在文档里补一张对照表。不阻塞实现。
2. **ALTER TABLE 而非 CREATE**：L1433–1450 是"additive extension"，假设
   `session_artifacts` 已存在。L1427–1429 显式注明"If the existing table
   already has equivalent columns, keep them and map the names explicitly in the
   migration." ✅ 向后兼容。
3. **`status='archived_cold'` 与 G3 `retrieval.raw_cold_fetch_required`**：
   G3 的 raw load stage 明确信号 `retrieval.raw_cold_fetch_required`
   （L1694）。G9 `status='archived_cold'` + `cold_storage_ref` 是其后端
   实现。**联动闭环**，与 S02 §8-3 要求一致。✅

### Verdict: `fully_resolved`

proposed_fix 全部落地，S08/S12 §8 反馈 100% 覆盖。**顺带解决
Sprint A G4-r4（artifact ACL `private/delegation/user`）**。

### Residual gaps
- **无** — GAP-TRACKER 可保持 `resolved`
- **Cross-gap resolve**: G4-r4 在 G9 里被解决。
- **Cross-gap note for Sprint C**: G6 vs G9 的 scope 命名分裂应在 Sprint C
  对齐（不开新 gap，可作为 G6 response 追加说明）。

---

## G18. Delegation context zone 动态预算公式

### 承诺清单

1. `delegation_state` 作为一等 zone 加入 zone 表
2. 公式：`per_child_budget = min(1200, max(200, floor(1500 / active_children)))`
   （note: response JSON 里承认把 proposed_fix 的"min(1500/active_children, floor=200)"
    修正为 capped min+max 形式）
3. Blocker → 2× 扩展从 `recent_tail` 借
4. Summary 超限 fallback 到 `title + artifact_ref`
5. 用 `last_summary_token_estimate` 预校验

### 场景反馈回应（S09 §8）

建议 3（feedback L41–48）：`per_child_budget = min(1500/active_children,
hard_floor=200)`，blocker 时子临时扩 2× 从 `recent_tail` 借。

REGRESSION-SPRINT-A §G4 residual r1 同源。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| `delegation_state` zone 入 Budget Policy v1 表 | L2037 "Delegation state \| Include bounded child-agent summaries; never raw child transcripts." | ✅（与 anchor/plan_todo/recent_tail/summary/retrieved/tool_previews/safety 并列） |
| 1500 token 总帽 | L2058–2059 | ✅ |
| 公式 `per_child_budget = min(1200, max(200, floor(1500 / active_children)))` | L2062–2064 | ✅（更严谨：上限 1200 防单 child 吃掉全部预算，下限 200 防过小） |
| 公式与 proposed_fix 简写等价性说明 | L2066–2068 "same hard floor: no active child gets less than 200 while bounded by 1500" | ✅ |
| `active_children=0` 跳过 zone | L2068 | ✅ |
| Pre-check `last_summary_token_estimate` | L2072–2073 | ✅（G4 字段终于被 policy 消费） |
| Summary 超限 fallback `title + phase + status + blocker + artifact_ref` | L2074–2076 + `context_manifest_items` 记录 dropped | ✅ |
| Blocker → 2× 扩，从 recent_tail 借 | L2077–2079 + manifest `reason='partial_blocker_review'` | ✅ |
| 绝不展子 transcript | L2080–2081 | ✅ |
| 子细节走 drill-down API | L2081 `GET /chat/runs/{run_id}/delegation-summary` | ✅ |

### v0.2 内部一致性

1. **公式修正**：proposed_fix 的 `min(1500/active_children, floor=200)`
   字面意义上是"取小于等于 min(...)，且 floor 不小于 200"，工程上需要
   capped min+max。v0.2 L2063 给出 `min(1200, max(200, floor(1500/active_children)))`
   是**正确**修正，并在 L2066–2068 保留 tracker 简写并解释等价。✅
2. **1200 上限防失衡**：当 `active_children=1` 时，`floor(1500/1)=1500`，
   被 `min(1200, ·)` 限到 1200；留 300 token 给 blocker 扩展或其他 child
   入队的弹性。合理。✅
3. **2× 扩借 recent_tail**：L2077–2079 说"临时 2× per_child_budget，从
   recent_tail 借"。但 Budget Policy v1 L2038 "Last 2-4 turns or budgeted
   recent messages" 没明确 recent_tail 的预算下限。若多个 child 同时
   blocker，recent_tail 可能被榨干。**未明确冲突解决顺序**：当 recent_tail
   被借完，继续借谁？或仅首 blocker 能借？**contract 未闭环**。建议
   Sprint C 补 1 段"借用优先级与最低 recent_tail 保底"。
4. **与 G1 枚举 `delegation_poll / partial_blocker_review / delegation_aggregate`
   联动**：manifest reason 三值都指向 `delegation_state` zone（L1270–1272），
   其中 `partial_blocker_review` 正是 G18 blocker 扩展时的 reason
   （L2079）。✅
5. **与 G4 `session_delegations.last_summary_token_estimate`
   （INT NOT NULL DEFAULT 0, L1098）联动**：G4 schema 放字段，G18 policy
   消费字段。Sprint A 回归 §G4 第 122 行明确批评"字段有但无 policy 用法"
   已被 G18 解决。✅ **Sprint A G4 residual "policy 留白"资产化**。

### Verdict: `fully_resolved`

proposed_fix 全部落地，Sprint A G4-r1 被 G18 覆盖，公式从 proposed_fix 的
直觉简写正确化为 capped min+max 形式。

### Residual gaps

| ID | 描述 | 处理建议 |
| --- | --- | --- |
| G18-r1（轻） | 多 child 同时 blocker 时 recent_tail 借用的优先级未定义 | Sprint C 实现时通过单元测试或补一行文档即可，不开新 gap |

可保留 `resolved`，r1 属细节。

---

## G19. Web 客户端 watermark + event apply 原子性契约

### 承诺清单

1. IndexedDB `run_event_high_watermark` 与 events 行同一 transaction 写入
2. client gap 检测：连续 event_idx 跳号 > 1 → reset + re-replay from last_ok_idx
3. 多 tab 共享 watermark via BroadcastChannel 或 SharedWorker

### 场景反馈回应（S04 §8 建议 3）

REGRESSION-SPRINT-A §G15 residual r2 同源。

### v0.2 落地证据

| 承诺条目 | 证据 | 状态 |
| --- | --- | --- |
| IndexedDB transaction 原子性 | **未找到** | ❌ |
| client gap 检测（event_idx 跳号 reset） | **未找到** | ❌ |
| 多 tab BroadcastChannel/SharedWorker | **未找到** | ❌ |
| §Web Client Cache 是否有新段落 | L373–432 基本未改动；L427–429 仅说 "Stream events update IndexedDB as canonical rows arrive. On reconnect, browser sends last seen run event index and transcript watermark." 无 transaction 语义、无 gap 检测、无多 tab 协议 | ❌ |
| L869–880（G15 内 SSE 协议段）"share watermarks through the web cache" 仍是 Sprint A 的一句话原文 | L879–880 | ❌ 未扩展 |

**grep 全文搜索**：
- `IndexedDB transaction` → 无命中
- `BroadcastChannel` / `SharedWorker` → 无命中
- `gap detection` / `last_ok_idx` / `re-replay` → 无命中
- `<!-- GAP-FIX: G19 -->` → 无命中

**GAP-TRACKER 自身状态**：L594–607 G19 条目 `status: open` / `response: -` /
`resolution_commit: -`。**设计 agent 没有做 G19**。

### Verdict: `needs_rework`（题面说"顺手做的 G19 也 resolved"**失实**）

题面与实物矛盾。GAP-TRACKER.md L605 明确 `status: open`，文档无 GAP-FIX 锚点，
§Web Client Cache 章节无 Sprint B 编辑痕迹。

### Residual gaps

| ID | 描述 | 处理建议 |
| --- | --- | --- |
| G19-full | 整个 G19 proposed_fix（3 点）未落地 | **必须回到 Sprint B 末尾或并入 Sprint C**；不要在 GAP-TRACKER 标 resolved |

---

## 交叉一致性检查

### Sprint A residual 是否已解

| Residual | Sprint B 覆盖点 | 判决 |
| --- | --- | --- |
| **G2-r1** (`finding/benchmark/citation` payload schema) | G8 L1410–1414 给出 3 类完整 payload schema | ✅ **resolved** |
| **G2-r2** (`session_history_chunks.chunk_type` 未同步 `finding/benchmark/citation`) | L1615–1630 chunk_type 列表已含三类 | ✅ **resolved** |
| **G2-r3** (`session_state_item_events.mutation` 未显式枚举) | 仍是 `VARCHAR(32)` L932，G2 SQL 只 match `replace/archive/delete` 与 `delete` 子集 | ❌ **未解**；GAP-TRACKER 自身将其合并到 G14 处理，属正常延期 |
| **G2-r4** (summary 双表写入路径未闭环) | 未改动；§Compaction L2085–2102 旧文字仍保留，未明说 "state item summary 为权威，conversation_log 为 narrative 回放源" | ❌ **未解** |
| **G2-r5** (manifest `run_id` NULL 时 invariant #6 误判) | G2 SQL #6 L2178–2182 已改为 `(run_id = :compaction_run_id) OR (run_id IS NULL AND :compaction_run_id IS NULL)`；并加 comment L2176–2177 | ✅ **resolved** |
| **G4-r1** (delegation zone 动态预算公式) | G18 全节 L2054–2083 | ✅ **resolved** |
| **G4-r2** (session_state_items ↔ session_delegations UPSERT 同步) | §Projection Sync Contract L1167–1180 同事务 + `item_key='delegation:<delegation_id>'` 约定 | ✅ **resolved** |
| **G4-r3** (state_item mutation 枚举) | 未解（合并 G14） | ❌ **未解**（同 G2-r3） |
| **G4-r4** (artifact ACL `private/delegation/user`) | G9 L1466–1471 | ✅ **resolved** |
| **G4-r5** (`superseded` 状态 / `retry_scope`) | 未解（合并 G14） | ❌ **未解**（合并 G14 属正常延期） |
| **G15-r1** (`expires_at_server` / `ttl_seconds` / clock-skew 协议) | G7 L1038–1039 JSON + L1068–1069 "UI may display ttl_seconds, but execution checks expires_at_server" | ✅ **resolved** |
| **G15-r2** (IndexedDB watermark 原子性 → G19) | **未解**。G19 整条 open | ❌ |
| **G15-r3** (`run_counters.generation` vs `run_generation` 命名) | L784 已改为 `run_generation BIGINT NOT NULL DEFAULT 1` | ✅ **resolved**（Sprint A 内部编辑） |
| **G15-r4** (approval 事件 payload schema) | 事件名在 L862–873 列了 10 个，但**单一事件的 payload schema 仍未给**；G7 给了 approval state item payload contract（L1029–1041），但事件 payload（如 `approval_expired.payload`）未列 | ❌ **部分未解**（事件 payload schema 缺） |
| **G15-r5** (`auth_sessions` 多设备 lease 与 G13 device fingerprint 关联) | 合并 G13 处理，G13 仍 open | ❌ **未解**（合并 G13 属正常延期） |

#### 小结

- **完全被 Sprint B 顺带解决的 Sprint A residual**：G2-r1, G2-r2, G2-r5,
  G4-r1, G4-r2, G4-r4, G15-r1, G15-r3（共 8 条）
- **Sprint B 未碰、合并到 G14/G13 处理**：G2-r3, G4-r3, G4-r5, G15-r5（共 4 条）
- **Sprint B **应该**解决但没有**：G2-r4（summary 双表契约），G15-r2（→ G19
  integrity），G15-r4（事件 payload schema）

### Sprint B 内部一致性

#### (1) G6 (`scope='user'`) 与 G2/G4 原有 state_item 字段兼容性

**G2** invariant #2（L2116–2117）保护 `plan_state/decision/finding/benchmark/
citation` + active `todo_state/error_state/delegation_state`。G6 的 user-scope
category 是 `durable_decision/engineering_rule/rejected_pattern`，**不与 G2
白名单重叠**。G6 user memory 与 compaction 正交，不会被 compaction 错误 archive。
✅ 兼容。

**G4** delegation_state 的 `session_id` 是真实 UI session id（L1087）；G6 user-
scope item 的 `session_id` 是 `user:<user_id>` namespace key（L1959–1962）。
两者共用 `session_state_items` 表但属不同 scope，UNIQUE `(session_id, scope,
category, item_key)` 保护互不冲突（`sess_xxx / delegation_state` vs
`user:yyy / durable_decision`）。✅ 兼容。

**潜在坑**：`idx_state_session_category (session_id, category, status, priority)`
L917 以 `session_id` 前导，若 runtime 按 UI session id 查"该 session 所有 state
items"时，**user-scope 的 item 不会出现**（因为 session_id 存 `user:...`）。
这是 G6 的预期行为（new session 通过 `idx_state_user_scope_category` 单独
加载），但**实现时容易误以为"列 session 所有 active state"应该包含 user
memory**。建议 Sprint C 在文档 §Session State Projection 加一行说明：
"查询单 UI session 的 state 必须同时扫 `session_id=<sess_id>` 与
`user_id=<user_id> AND scope='user' AND status='active'` 两组索引"。⚠️
**非阻塞**，记为文档留白。

#### (2) G3 (retrieval SLA) 与 G10 (small-window budget，仍 open) 字段冲突

G3 retrieval event payload（L1716–1731）含 `timeout_ms`、`elapsed_ms`、
`candidate_count`。G10 proposed_fix 要求 manifest 写 `tokenizer_id`。二者字段
**独立**，无冲突。但 G10 涉及的 `budget_v1_8k` 模板和 G3 的 `vector`
"1 query per turn" 帽在 8k 窗口下可能被迫降为 0。建议 G10 resolve 时协调：
"small-window profile 禁用 vector retrieval"。**非冲突，属依赖关系**。

#### (3) G7 approval 事件 vs G15 `approval_expired/retracted` 事件类型

G15 Sprint A 第一轮就预铺了 `approval_expired / approval_retracted`（L864–865）。
G7 Sprint B 扩充至 10 条（L862–873）。**列表在同一个 GAP-FIX: G15 范围内
（L735–882）**，属 G7 patch 时复用 G15 的既有列表位置。命名一致，无重复。
✅ 兼容。

**但**：G15 Sprint A 事件 payload schema 从未给出（见 Sprint A residual G15-r4），
G7 给了 approval state item payload（L1029–1041）但**事件 payload**
（`approval_expired.payload_json` 应该有什么）仍未列。**G15-r4 仍部分未解**。

#### (4) G8 preview template registry 与 G17 `content_hash` 规范化

G17 proposed_fix 要求每 tool 声明 `normalize_version`。G8 在 schema 里放了
`session_tool_outputs.normalize_version VARCHAR(16)` L1320 + `preview_template_
registry.normalize_version VARCHAR(16) NOT NULL DEFAULT 'v1'` L1355。**G8
埋了 hook 但未定义规范化规则**，属"schema 预铺"策略。

GAP-TRACKER G8 response 的 `does_not_resolve` 明确声明 "G17 content hash
normalization details beyond normalize_version hook"（L347）。**兼容且诚实
切分**，G17 仍 open 但落地路径清晰。✅

#### (5) v0.2 新增 SQL 建表与 Sprint A 建表字段命名冲突扫描

运行以下 grep 搜索 Sprint B 新增表名是否与 Sprint A 已有表冲突：

```bash
grep -E 'CREATE TABLE.*(session_approval_conditions|session_external_notifications|session_delegations|context_manifest_reason_types|context_manifest_items|preview_template_registry)' docs/design/web-agent-session-state.md
```

新增 6 张表：
- `session_approval_conditions` L980（新名，无冲突）
- `session_external_notifications` L1002（新名，无冲突）
- `session_delegations` L1084（新名，无冲突；Sprint A 保留的 `session_state_items(category='delegation_state')` 继续作为 projection）
- `context_manifest_reason_types` L1218（新名，无冲突）
- `context_manifest_items` L1228（新名；Sprint A 的 `context_manifests` 主表在 L1190，无冲突，属 1:N 关系）
- `preview_template_registry` L1347（新名，无冲突）

**ALTER TABLE 冲击扫描**：
- `session_artifacts` 新加 10 列（L1433–1445）+ 3 索引，Sprint A 未建此表
  ，属 "additive extension to existing table"，L1427–1429 注释"假设表存在"。
  **对仓库现有 session_artifacts 是否兼容**：设计文档未展示 Sprint A 的
  session_artifacts DDL，属预先 implicit schema，ALTER 写法合理。
- `agent_sessions` 新加 3 列（project_id / project_retention_policy +
  索引）L1448–1450。Sprint A 未重定义 agent_sessions，属 additive。✅

**字段命名冲突**：
- `session_artifacts.access_scope` (G9) vs `session_state_items.scope` (G6) —
  语义不同，命名已区分（`access_scope` vs `scope`）。✅
- `session_approval_conditions.session_id` + `session_external_notifications.session_id`
  与 `session_state_items.session_id` 一致用 VARCHAR(128)。✅
- `session_delegations.ancestor_path` (L1092) vs `agent_runs.ancestor_path`
  (L744)：两处都 TEXT，同义字段。Sprint A 回归 §G4 曾批评 `agent_runs.
  ancestor_path` 允许 NULL 而 `session_delegations.ancestor_path` NOT NULL。
  ⚠️ **此处 v0.2 未修**：agent_runs.ancestor_path L744 仍 `TEXT NULL`；
  session_delegations.ancestor_path L1092 仍 `TEXT NOT NULL`。
  Sprint A 回归建议"规定 root_run_id = run_id self-ref 保 NOT NULL"未采纳。
  属 G4 residual（未升级）的未处理延续。

#### (6) `mutation` 枚举未显式（跨 G2/G4/G14）

L932 `mutation VARCHAR(32) NOT NULL`。
- G2 SQL #1 match `'replace', 'archive', 'delete'`（L2144）
- G2 SQL #8 match `'delete'`（L2198）
- G4 projection sync contract（L1167–1180）未显式 mutation 值
- G14（未 resolved）proposed_fix 要求扩至 `{insert, update, replace, archive,
  bubble_up, apply_suggestion}`

**未显式枚举是 Sprint A 识别的 G2-r3 / G4-r3 残留**，v0.2 仍未解，合并到
G14 处理。✅ 与 Sprint A 回归建议一致，无恶化。

#### (7) 新枚举一致性

G1 reason 枚举（L1258–1273）+ G7 事件类型（L862–873）+ G6 scope（L1952–1957）
+ G9 status/access_scope（L1459–1471）之间**无重名碰撞**。命名空间独立。✅

### 新发现的 issue（是否要开 G20+）

| 候选 | 原因 | 建议 |
| --- | --- | --- |
| **G20**: G6 backlog todo 与 compaction archive 的交互 | G6-r1 | 合并到 G14（compaction extension）或 G6 response 追加，**不必开新 gap** |
| **G21**: G6 namespace key magic string 规约 | G6-r2 | 属 v0.2 内部编辑问题，不开 gap |
| **G22**: G18 多 child 同时 blocker 时 recent_tail 借用优先级 | G18-r1 | 实现期单元测试覆盖，不开 gap |
| **G23**: G9 `access_scope` 命名与 G6 `scope` 命名分裂 | Sprint B 内部一致性观察 (1) | v0.2 内部编辑补对照表，不开 gap |
| **G24**: `agent_runs.ancestor_path` NULL vs `session_delegations.ancestor_path` NOT NULL | Sprint A 回归 §G4 提过，Sprint B 未修 | 合并到 G4 residual 的"未升级项"中，v0.2 内部编辑修复即可 |
| **G25**: G15 run event 数组中 10 个事件的 payload_json schema 未定义 | Sprint A G15-r4 延续 | 实现期 contract test 兜底，**可考虑正式开 G20（新）**以确保每事件 payload 被显式约定 |

**判决**：建议**不开 G20+**；G19 已是 medium 级事实 open gap，应该先把 G19
真正落地。新增发现的 6 条都可通过"v0.2 内部编辑"、"合并 G13/G14"或"实现期
contract test"消化，不必新开 gap。但若希望形式化约束事件 payload（G25
候选），可考虑正式开 G20。

---

## GAP-TRACKER 状态更新建议

### 升级到 `resolved`

| Gap | 当前 | 建议 | 理由 |
| --- | --- | --- | --- |
| **G2** | partially_resolved | **resolved** | r1 (payload schema) ✅ G8；r2 (chunk_type) ✅ v0.2 内部修；r5 (manifest run_id NULL) ✅ L2176–2182。剩 r3/r4 合并 G14 与不重要 narrative split，可 GAP-TRACKER 补 `residual_notes` 后 close |
| **G4** | partially_resolved | **resolved** | r1 (zone budget) ✅ G18；r2 (UPSERT sync) ✅ §Projection Sync Contract；r4 (artifact ACL) ✅ G9。剩 r3/r5 合并 G14，属正常延期 |
| **G1** | resolved | **resolved** | 保持 |
| **G3** | resolved | **resolved** | 保持 |
| **G5** | resolved | **resolved** | 保持 |
| **G6** | resolved | **resolved**（或 partially_resolved） | 两条 r1/r2 属文档表述层面，不阻塞实现；设计 agent 自行选择 |
| **G7** | resolved | **resolved** | 保持；顺带 G15-r1 resolved |
| **G8** | resolved | **resolved** | 保持；顺带 G2-r1/r2 resolved |
| **G9** | resolved | **resolved** | 保持；顺带 G4-r4 resolved |
| **G18** | resolved | **resolved** | 保持；顺带 G4-r1 resolved |

### 维持 `partially_resolved` 或降级

| Gap | 当前 | 建议 | 理由 |
| --- | --- | --- | --- |
| **G15** | partially_resolved | **维持 partially_resolved** | r1 ✅ G7；r3 ✅ v0.2 内部。但 **r2 = G19 未落地**；r4 事件 payload schema 未补；r5 合并 G13。r2 + r4 是主证据 |
| **G19** | open | **维持 open** | 题面声称"顺手做了"，**实物未做**。不得擅升为 resolved |

### 新 gap

**建议不开 G20+**。发现的 6 条细节均可通过：
- 合并到现有 open gap（G13/G14/G17）
- v0.2 文档内部编辑（namespace key、scope 命名对照表、ancestor_path 可空性）
- 实现期 contract/integration test 兜底（recent_tail 借用优先级、事件 payload schema）

消化。**若要形式化约束事件 payload schema**（G25 候选），可开 G20；否则保持
19 条 gap 稳定。

---

## 综合建议

### Sprint B 整体质量评估

**Sprint B 在声称的 8 条 gap（G1/G3/G5/G6/G7/G8/G9/G18）上质量极高**：
- 所有 proposed_fix 子项均有具体 schema + 文字落地
- 几乎每条 gap 都顺带解决一至多条 Sprint A residual
- 跨 gap consumes/feeds 在 GAP-TRACKER response 里显式声明，与 v0.2 文档
  交叉引用自洽
- 枚举/索引/约束命名没有冲突
- 向后兼容（ALTER TABLE 对 Sprint A 的 session_artifacts / agent_sessions
  表无破坏）

**唯一明显缺陷**：G19 未落地。题面说"顺手做了"，实物完全没有。
这是 Sprint B 过程可信度问题，必须在 Sprint C 首先补上（或在 Sprint B
最后加一个小 patch）。

**次级缺陷**：
- G15-r4（10 个事件的 payload schema）未在 G7 patch 时顺手完成
- G2-r4（summary 双表权威）未在 G2/G8 patch 时完成
- G6 两条文档表述松（backlog/compaction 交互、namespace key 约束语气）

### 推荐 Sprint C 优先级排序

按阻塞程度 × 依赖关系：

1. **G19**（补 Sprint B 未完成项，最高优先；会是 Phase 2 实现的硬阻塞）
2. **G14**（bubble_up + retry_scope + mutation 枚举；会解决 G2-r3/r4/r5 + G4-r3/r5；是 multi-agent 产品化的剩余一块）
3. **G13**（device fingerprint + session_lease；会解决 G15-r5；与 G19 互补）
4. **G11**（workspace reachability_probe；阻塞 edge/cloud 切换）
5. **G10**（small-window budget template；私有化部署需要；与 G3/G18 协调）
6. **G17**（content_hash 规范化；G8 已埋 hook，规则化即可）
7. **G12**（next-action confidence 阈值；相对独立，可并行）
8. **G16**（personal skill per-session 激活；与 G6 关联但不阻塞）

**并行化建议**：Sprint C 可分两轨：
- 轨 A（durability 收尾）：G19 + G14 + G13
- 轨 B（工具生态）：G11 + G10 + G17 + G12 + G16

两轨在 G13 ↔ G14 处有交叉（`auth_sessions` 多设备 vs `retry_of` 生命周期），
需要一次合稿同步。

### 对 REGRESSION-SPRINT-A.md 的修订建议

在 Sprint A 报告末尾追加一节"Sprint B update"，记录：

- G2 r1/r2/r5 已解（G8 + v0.2 内部）
- G4 r1/r2/r4 已解（G18 + Projection Sync Contract + G9）
- G15 r1/r3 已解（G7 + v0.2 内部）
- G15 r2 → G19（**仍 open**，题面声称已做但实际未做）
- G2 r3/r4 / G4 r3/r5 / G15 r4/r5 合并到 G13/G14 / 实现期 contract test，
  属正常延期
