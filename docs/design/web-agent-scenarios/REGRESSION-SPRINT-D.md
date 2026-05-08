# Regression Verification — Sprint D (G20–G27)

> Verifier: regression sub-agent (quality reviewer, strict mode)
> Date: 2026-05-07
> Scope: 8 条 critical+high gap 回归 + false-positive 风险筛查 + 跨 Sprint 总核验
> Target doc: `docs/design/web-agent-session-state.md` v0.3 @ 3653 行 (Sprint D +613 行)

## Verdict Summary

| Gap | Severity | Verdict | Patch 区间 | 关键证据 |
| --- | --- | --- | --- | --- |
| **G20** Sibling artifact ACL | critical | ✅ `fully_resolved` | L2155–L2229 | 定义替换 + 访问矩阵 + grants 表 |
| **G21** Delegation budget formula | critical | ✅ `fully_resolved` | L2968–L3011 | 公式重写 + 7 × 验算表 + overflow |
| **G22** retry_scope propagation | critical | ✅ `fully_resolved` | L1346–L1397 | 6 级选择规则 + payload 扩展 |
| **G23** Tool output batch insert | critical | ✅ `fully_resolved` | L1858–L1932 | batch 表 + 1000 行 <500ms 契约 |
| **G24** Cold-start hydration | high | ✅ `fully_resolved` | L441–L498 | replay_required 三标志 + client/server 规则 |
| **G25** Device lease SSE parity | high | ✅ `fully_resolved` | L560–L630 | lease_events 表 + CAS + SDK 清理 |
| **G26** Manifest reason enum | high | ✅ `fully_resolved` | L1718–L1764 | 9 新 reason + turn_intent + 未知回退 |
| **G27** Tool baseline + raw_ref | high | ✅ `fully_resolved` | L1990–L2097 | runner/scheme registry + 18 模板 |

**8/8 `fully_resolved`。零 `needs_rework`。零新 false-positive（未出现 Sprint B 的 G19 模式）。**

---

## G21 数学公式验算（关键审查）

### 旧公式（v0.2 L2450 附近已明确 DEPRECATE，见 L2963–L2966）

```
per_child_budget = min(1200, max(200, floor(1500 / active_children)))
```

当 `active_children ≥ 8` 时 `floor=200 × 8 = 1600 > 1500`——两约束不可同时成立。

### 新公式（v0.3 L2972–L2987）

```
budget_total           = 1500
min_child_floor        = 200
max_rendered_children  = floor(1500 / 200) = 7
candidate_children     = active non-terminal 按 (blocker severity, status, updated_at, priority) 排序
rendered_children      = take first max_rendered_children
overflow_children      = 剩余 -> overflow summary row
per_child_budget       = max(200, floor(1500 / count(rendered_children)))
```

### 手工验算

| n (active children) | rendered | per_child | rendered_total | ≤ cap (1500) | overflow | 合理 |
| ---: | ---: | ---: | ---: | :---: | ---: | :---: |
| 1 | 1 | 1500 | 1500 | ✅ | 0 | ✅ (单 child 吃满 zone) |
| 3 | 3 | 500 | 1500 | ✅ | 0 | ✅ |
| 5 | 5 | 300 | 1500 | ✅ | 0 | ✅ |
| 7 | 7 | 214 | 1498 | ✅ | 0 | ✅ (2 token slack 属 floor 舍入正常) |
| 8 | 7 | 214 | 1498 | ✅ | 1 | ✅ (overflow 压缩为 "1 more…") |
| 10 | 7 | 214 | 1498 | ✅ | 3 | ✅ |
| 15 | 7 | 214 | 1498 | ✅ | 8 | ✅ |

**结论**：新公式在全部 7 个边界值均成立，no 矛盾。n=1 仍能利用 full budget，n≥8 通过 top-7 裁剪 + overflow row 保持 hard cap。

### 边界复检：blocker 借用路径（L3020–L3025）

场景：某 child 激活 blocker，想临时用 `2 × per_child_budget`。

- n=3, blocker child wants 1000：剩余 500 分给 2 child = 250 each（> floor 200）→ `1000 + 2×250 = 1500` ✅
- n=7, blocker child wants 428 (2×214)：剩余 1072 / 6 ≈ 178 < floor 200 → 规则触发"lower-priority 移 overflow"，变成 5 rendered@214=1070，总计 `428 + 1070 = 1498` ≤ cap ✅
- 文字明确：「zone still stays under `budget_total=1500`」（L3022–L3023）— 不允许 zone 突破 cap，与 n≥8 的基础公式一致。

**Blocker borrow 路径无新矛盾。**

### G21 Verdict: `fully_resolved`，公式逻辑自洽，overflow 语义清楚。

---

## G20 ACL 定义变更核验（关键审查）

### Sprint B 的 G19 false-positive 教训

Sprint B 曾声明 G19 resolved 而无 GAP-FIX 锚点 → Sprint C 纠正。本轮 G20 的核心检验点：**改的是 access_scope 的"定义"还是"只补字段"？**

### v0.2 原文（L2110, 仍保留，向后兼容）

```sql
ALTER TABLE session_artifacts ADD COLUMN access_scope VARCHAR(32) NOT NULL DEFAULT 'delegation';
```

配字面定义（v0.2 原 §Artifact Access Scope 的狭义「parent + descendants」）。

### v0.3 新增（L2146–L2153，**原文修改**）

```text
- `delegation`: DEPRECATED in v0.3 as an ambiguous alias. Existing rows map to
  `same_root_tree` unless a migration explicitly narrows them to
  `delegation_direct`.
- `delegation_direct`: owning run, direct parent runs, and descendants ...
- `same_root_tree`: any run/session under the same `root_run_id` can load raw
  bytes when the artifact is exposed by the owner or parent orchestrator.
- `user`: all sessions owned by the same user ...
```

- 旧 scope 值被**显式 DEPRECATE**（不是留着不动）
- 新增 `delegation_direct`（原狭义语义拆出）+ `same_root_tree`（兄弟语义）两个新 enum 值
- 迁移策略：existing row → `same_root_tree`（默认宽松，向后兼容读路径）

### v0.3 §Delegation Tree Artifact ACL（L2155–L2229，新增）

1. **Additive schema**：`owner_run_id`, `owner_delegation_id`, `root_run_id` 3 列 + 2 索引（`idx_artifacts_root_scope`, `idx_artifacts_owner_run`）— L2166–L2170
2. `session_delegations.sibling_exposed_artifacts_json` — L2172
3. `session_artifact_grants` 独立审计表 — L2174–L2192（grant_scope/source_run_id/target_run_id/expires_at 全）
4. **访问矩阵**（L2197–L2202）6 列：owner / 直父 / 子孙 / 同 root 兄弟 / root orchestrator / 不同 root
5. Grant 规则 4 条（L2204–L2218）：兄弟读必须满足 `root_run_id 相等 + (scope=same_root_tree OR grant row OR 父 orchestrator 在 sibling_exposed_artifacts_json 列出)`；ancestor_path 只作 hint 不做 sole authority；grant 追加审计不物理删；raw 下载与 context render 走同一 ACL 检查
6. 三条 Examples（L2220–L2228）对应 S09 FE/BE/DBA 兄弟、S10 executor-2→reviewer-3→reporter、不同用户不同 root deny

### 三个 sibling 场景回应核查

| 场景 | 旧 v0.2 结论 | 新 v0.3 结论 | 证据 |
| --- | --- | --- | --- |
| S09 T6/T7 FE↔BE sibling access migration SQL | ❌ (delegation 定义只允 parent + descendants) | ✅ 通过 `same_root_tree` OR 父 orchestrator 在 `sibling_exposed_artifacts_json` 列出 | L2197–L2218 |
| S09 T12 DBA→BE 跨兄弟 | ❌ | ✅ 同上 | L2222–L2223 |
| S10 executor-2 的 reviewer-3 finding → 兄弟 reporter | ❌ | ✅ 需 orchestrator bubble_up 中写 grant 或设 same_root_tree | L2224–L2225 |
| 不同 root 用户 deny | (未明确) | ✅ 访问矩阵明示 deny | L2201 "Different root" 列 |

### 判断：**改的是"定义"+ "补字段"双管齐下**。不是单纯补字段掩盖。

### G20 Verdict: `fully_resolved`，兄弟 ACL 语义闭环，审计路径 (grants) 完整。

### G20 residual

- `session_artifacts.access_scope DEFAULT 'delegation'` 默认值保留 (L2110) — 新插入仍默认废弃值。**建议 Phase 4 实现期补 DEFAULT 迁移**或校验层挡掉。minor，不阻塞。
- 未在 ALTER 段给出 `session_artifact_grants.grant_scope` 的 enum 值清单（只说 VARCHAR(32)）。建议在实现 contract test 中固化 `{same_root_tree, delegation_direct, explicit}` 三值。

---

## G22. retry_scope Selection and Propagation

### 承诺清单（from GAP-TRACKER proposed_fix + response patch_summary）

1. G14 加 "retry_scope 选择规则" — ✅ L1353–L1368 六级决策
2. `apply_suggestion` payload 补 `retry_scope` — ✅ L1323 payload sample; L1379 retry-specific payload 含 retry_scope/target_run_id/target_delegation_id/scope_source/retry_reason
3. 所有 retry 事件携带 scope — ✅ L1387–L1392 四个载体逐条列出：`session_state_item_events(mutation='apply_suggestion')`、`agent_runs.retry_scope`、`agent_run_events` retry 调度、audit UI 比较链接
4. Unknown scope 硬失败 — ✅ L1394–L1395

### v0.3 证据

选择优先级（L1353–L1368）：
1. 用户显式词语
2. suggestion payload 已携带的 retry_scope
3. 目标 run 有 active 子 delegation / blocker descendant / stateful 子 tool → `subtree`
4. 无子 delegation + idempotent/superseded → `node`
5. 多兄弟同父 + 用户提 group/wave/batch/all → `siblings`
6. 默认 `node` + `scope_source='default_node'`

### S10 §4 F-blocking-2 场景回应

S10 T8 "重做 executor-2 的数据库扫描"：
- 用户显式提到单一 child run → Rule 1 match `node` (or Rule 3 若该 child 仍有 reviewer 活着 → `subtree`)
- `scope_source='explicit_user'` 或 `'inferred_active_child'` 明确
- `apply_suggestion` payload 把 retry_scope 传入下游，不再靠 server 二次猜

### 交叉一致性（G14/G15）

- `agent_runs.retry_scope VARCHAR(32) NOT NULL DEFAULT 'node'` (L1058, G15 表 schema) — 已预铺 ✅
- `session_state_item_events.mutation` 枚举 L1292–L1299 含 `apply_suggestion` — G14 一致 ✅
- 新增 payload 字段不破坏 G14 v0.2 既有 `{suggested_next_action_id, chosen_candidate_id, source, confidence_at_accept}` 兼容性（追加 optional 字段）✅

### G22 Verdict: `fully_resolved`

### G22 residual

- 未给出 `scope_source` 的 enum 清单（`explicit_user` / `suggestion_payload` / `inferred_active_child` / `default_node` 等建议字面）。建议在 Phase 4 contract test 固化。
- Rule 1 "用户显式词语" 的关键词映射表未给出（中/英文都可能出现）；这属于 NLU 实现层。

---

## G23. session_tool_outputs 批量写入契约

### 承诺清单

1. `session_tool_output_batches` 表 — ✅ L1866–L1893 完整 DDL
2. batch_id/batch_seq/batch_row_idx 索引 — ✅ 3 个索引（uq_tool_output_batch_seq, idx_tool_output_batches_session, idx_tool_output_batches_run_status）
3. 100–500 行 / 500 行硬上限 / 16MiB 硬上限 — ✅ L1895–L1902
4. Begin/commit 协议 — ✅ L1904–L1913（伪代码）
5. 失败回滚 + 非 completed batch 不入 context — ✅ L1915–L1921
6. 1000 行 <500ms 性能契约 — ✅ L1923–L1930

### S08 §4 F-blocking 场景回应

S08 扫 1000 SQL 文件 → 2 个 batch (每 batch 500 行) → 一次 transaction bulk insert。`expected_row_count=500` 在 batch 表先标 `writing`，完成更新 `completed`。S08 §5 `parent_output_id` 需求在 G27 补（L2078）。

### 交叉一致性（G15 event_idx）

- `session_tool_outputs` 与 `agent_run_events` 是**两张表**，各自独立：
  - agent_run_events 的 `event_idx` 由 `run_counters` 单 writer 协议保序（G15 L1075–L1086）
  - session_tool_outputs 的 `batch_id/batch_seq/batch_row_idx` 由 batch 表独立协议
- 两个 bulk insert 在同一 turn 内先后独立 commit，无事务交织
- **无冲突** ✅

### 交叉一致性（G17 normalize_version）

- G23 batch 写入的 rows 仍受 G17 `content_hash + normalize_version` 契约约束
- G27 L2067 明确 NULL = raw_v1，兼容 G23 bulk insert 旧实现

### G23 Verdict: `fully_resolved`

### G23 residual

- batch `status` 枚举值 (`pending/writing/completed/failed`) 未显式列出，从 DDL L1875 `DEFAULT 'pending'` 和 L1911 `status='completed'`、L1919 `status='failed'` 反推；建议在实现端加 CHECK constraint。
- `aggregation_complete=false` 的 preview 隐含但未在 preview_template_registry schema 显式。minor。

---

## G24. Cold-Start Hydration

### 承诺清单

1. `replay_required`, `transcript_replay_required`, `run_event_replay_required` 3 个标志加入 state 响应 — ✅ L462–L471
2. Cold-start 请求语义 `known_state_revision=0` / `client_cache_empty=true` — ✅ L452
3. Transcript API 先拉历史 / stream 从 last_index=-1 或 0 全量 replay — ✅ L477–L484
4. IndexedDB commits 完成**后**才 advance local watermark — ✅ L485–L487
5. Warm client 仍走 delta 路径 — ✅ L488

### 交叉一致性（G19 watermark atomicity）

- G19 L634–L684 定义 IndexedDB transaction 协议 (event rows + watermark 一事务)
- G24 L485–L487 明确："Only after IndexedDB commits the replayed event rows and transcript pages may it advance local `run_event_high_watermark` / `transcript_high_watermark`."
- **明确对齐** ✅
- G24 L483–L484 "cold client must never call `stream?last_index=<server hwm>`" ← 防止跳过 replay → 与 G19 gap_detected abort 语义一致

### S04 §5 / S03 §3 T5 场景回应

- S04 T09 双 tab 冷启：新 tab 拿到 `replay_required=true` → stream from 0 / transcript from 0，IndexedDB 逐事务应用；兄弟 tab 走 warm 路径。多 tab BroadcastChannel watermark（G19 L673–L679）前提下两 tab 状态最终一致 ✅
- S03 T5 iPad 首登：`known_state_revision=0&client_cache_empty=true` → server 返回三个 `replay_required=true` → client 先 transcript 拉最新页回退 + run stream from 0 — 不会跳过历史 ✅

### G24 Verdict: `fully_resolved`

### G24 residual

- L481–L484 支持两种 stream 语法 (`last_index=-1` vs `from_index=0&inclusive=true`)，未指定 primary。建议在 Phase 2 API contract 选其一作为 canonical。minor。
- Transcript "page from `after_seq=0` or latest first then backwards" 给了两种策略 (L477–L480)，UX 行为不同；建议实现期统一。minor。

---

## G25. Device Lease End Event Parity

### 承诺清单

1. `session_device_lease_events` 表 — ✅ L572–L587
2. active→revoked 与 active→expired 终态 — ✅ L593–L595
3. 对称 SSE payload — ✅ L600–L603 表格
4. CAS 扫描单 emitter — ✅ L612–L620
5. SDK 清理行为 (IndexedDB/localStorage/sessionStorage/memory) — ✅ L622–L628
6. 归一化事件 `device_lease_ended` 可选并存 — ✅ L605–L607

### S03 §5 候选 B 场景回应

S03 T8 Chromebook 公用设备：
- 24h 过期扫描 → CAS update L612–L615 原子 → INSERT `session_device_lease_events(event_type='expired')` → broadcast `device_lease_expired`（与 revoke 同 payload shape）
- SDK 在 `device_lease_expired` 收到后：关流、清 IndexedDB 等、显示 re-auth 页 → 零信任设备残留数据消失 ✅

### 交叉一致性（G13 session_device_leases）

- G13 session_device_leases 的 `status` 转换 (active → revoked/expired) 通过 G25 CAS 协议落盘
- G13 已有的 `/sessions/{id}/device/revoke` 路径产生 `device_revoked` 事件；G25 补上 passive expiry 的 parity
- G25 `session_device_lease_events.event_type` 字段虽未列 enum 值，但表格 L600–L603 明示 `{device_revoked, device_lease_expired}` 两值

### G25 Verdict: `fully_resolved`

### G25 residual

- `event_type` 与 `reason` 两字段语义关系未显式（event_type='expired' 时 reason 填什么？时限？idle？）——建议 Phase 2 枚举化。minor。
- 重复 expiry 扫描已通过 CAS `WHERE lease_id=? AND status='active' AND expires_at<=now` 防御（L614–L615），符合预期。

---

## G26. Manifest Reason Enumeration + turn_intent

### 承诺清单

1. 9 个新 reason: `ambiguity_clarification`, `execute_after_clarification`, `user_memory_promote/archive/revise/loaded_on_init`, `progressive_loading`, `intent_driven_preview_expand`, `other` — ✅ L1723–L1733 全表
2. `cross_skill_alignment` 作为 canonical 保留 — ✅ L1735–L1736 显式说明
3. Unknown 回退 `other` + 告警 + 保留 `reason_original` — ✅ L1738–L1746
4. `turn_intent VARCHAR(64)` 新字段 + 6 个 seed 值 — ✅ L1748–L1762
5. `budget_override` 审计字段 (zone, base_cap, override_cap, actual_tokens, borrowed_from) — ✅ L1761–L1762

### 场景回应核查（5 个场景）

| 场景 | 需求 reason | v0.3 落点 |
| --- | --- | --- |
| S05 §5 G22 "继续" 歧义 | `ambiguity_clarification` | ✅ L1725 |
| S11 §5 #1 user memory 生命周期 | `user_memory_promote/archive/revise/loaded_on_init` | ✅ L1727–L1730 |
| S13 §5 G22 skill alignment | `cross_skill_alignment` (v0.2 已有) + `skill_alignment` turn_intent | ✅ L1735 + L1759 |
| S14 §5 #2 progressive_loading | `progressive_loading` (budget class) | ✅ L1731 |
| S12 §5 benchmark_comparison intent | `intent_driven_preview_expand` reason + `benchmark_comparison` turn_intent | ✅ L1732 + L1755 |

### 交叉一致性（Sprint A–C 16 枚举）

v0.2 L1691–L1711 原 18 seed (sprint review 数成 16 见 S01 §2)：`initial_turn`, `normal_turn`, `post_compaction`, `history_recall_{structured|fts|vector}`, `large_tool_output_gated`, `plan_subtree_query`, `tree_structured_report`, `workspace_switch`, `approval_resume`, `cross_session_recall`, `delegation_poll`, `partial_blocker_review`, `delegation_aggregate`, `cross_skill_alignment`, `skill_quality_review`, `final_delivery_summary`.

Sprint D 新 9：`ambiguity_clarification`, `execute_after_clarification`, `user_memory_promote`, `user_memory_archive`, `user_memory_revise`, `user_memory_loaded_on_init`, `progressive_loading`, `intent_driven_preview_expand`, `other`.

**无命名冲突，无语义重叠**（`cross_skill_alignment` 作为 reason 保留，`skill_alignment` 作 turn_intent，清楚分离）✅

### Budget 交叉 (G10 / G21)

- `turn_intent=benchmark_comparison` 可将 `tool_previews` 提到 2500 tokens，从 `recent_tail` 借——与 G10 small-window budget rules (L2944–2951 recent_tail 1600 floor) 理论上可冲突
- 但 L1755 声明 "while respecting the `recent_tail` floor" ← 明确保留 floor
- `budget_override` 审计字段写明 `borrowed_from` 供追溯
- G21 overflow 报告入 context_manifest_items — G26 feeds G21 progressive_loading 预算 reporting (response L1107 明示)

### G26 Verdict: `fully_resolved`

### G26 residual

- `turn_intent` 仅在 context_manifests 字段列表提到但未明确 DDL ADD COLUMN 语句；建议 Phase 3 补 ALTER TABLE。minor。
- `agent_events.event_type='manifest.reason_unknown'` 告警类型未加入事件 event_type 总表；建议 implementation clarification 补齐。

---

## G27. Tool Baseline, Raw Ref, Runner Registration

### 承诺清单

1. `tool_runner_registry` 表 — ✅ L1999–L2009
2. `raw_ref_scheme_registry` 表 — ✅ L2011–L2019
3. Canonical raw_ref format `<scheme>://<namespace>/<id>@<content_hash>` — ✅ L2024–L2026
4. 6 个 scheme baseline — ✅ L2030–L2037 (artifact/conversation_log/object_store/s3/cold_storage/blob)
5. 18 个 baseline templates (包含 Rust tools) — ✅ L2044–L2063 表格
6. `normalize_version=NULL` → `raw_v1` 兼容 — ✅ L2067–L2068
7. `raw_v1` 恒等变换语义 — ✅ L2069–L2071
8. `parent_output_id` + `session_artifact_provenance` 多源派生 — ✅ L2078–L2090
9. Runner 必须 registration 后才能写 session_tool_outputs — ✅ L1994–L1996

### 场景回应核查

| 场景 | 需求 | v0.3 落点 |
| --- | --- | --- |
| S01 cargo/rustc/clippy | 三个 tool baseline | ✅ L2056–L2058 |
| S02 raw_ref canonical scheme | 注册表 + 至少 3 scheme | ✅ 6 个 scheme 表格 L2030–L2037 |
| S08 sql_compat_scan / pg_schema_structurize / slow_query_analyzer | 三个 tool baseline + `parent_output_id` 派生 | ✅ L2047–L2049 + L2078 |
| S08 normalize_version=NULL 冲突 | 规定语义 | ✅ `NULL → raw_v1` L2067–L2068 |
| S08 slowlog 有意不规范化 | raw_v1 恒等 | ✅ L2069–L2071 明言 "required choice for tools such as raw slowlog capture where normalization would destroy forensic value" |

### 交叉一致性（G8 preview template table 格式）

G8 v0.2 baseline table (L1968–L1976) 4 列：`tool | max preview | first-class columns | chunk type`

G27 v0.3 baseline table (L2044–L2063) 5 列：`tool | max preview | first-class columns | chunk type | normalize`

**新表是 G8 的 superset** — 列顺序保持 (4 列相同) + 末尾加 `normalize`。G8 原 6 条 (pg_dump, slow_query_analyzer, fetch_url, parse_pdf, llm_extract_findings, benchmark_slice) 全部重复且扩展 normalize 列 ✅；不破坏既有格式。

### 交叉一致性（G9 ACL + G20 sibling ACL）

- L2039–L2040 "Resolvers must perform the G9 access check before loading raw bytes. String parsing alone never grants access." → G20 sibling grant 检查同样适用 ✅
- response `feeds` 链：G27 → G20 raw_ref resolver access_check（response L1146）

### 交叉一致性（G17 content hash normalization）

- G17 原文 (L2163–L2194) 约束 content_hash + normalize_version 双比对
- G27 L2067 "NULL is deprecated"、L2068 "legacy NULL reads as raw_v1" → 向后兼容，无契约破坏 ✅

### G27 Verdict: `fully_resolved`

### G27 residual

- `tool_runner_registry.status` 枚举未列；`raw_ref_scheme_registry.access_check` 字段值空间未明。建议 Phase 6 补具体值。minor。
- 18 baseline templates 中 `skill_diff` 与 S13 的 "skill alignment" 流程关联未在此处给出 payload 细节，需 G16 实现期对齐。minor。
- `raw_ref` 的 `@<content_hash>` 是必填还是可选 (格式 L2025 作 mandatory；对 `blob://sha256/abc` 这种 hash-only 语义有冗余) — 实现层自明，无歧义影响。

---

## 跨 Sprint 总核验

### G1–G29 全景状态（Sprint A+B+C+D 汇总）

| 状态 | 数量 | Gap |
| --- | --- | --- |
| ✅ fully_resolved | 27 | G1–G27 |
| ⭕ open (medium, 按计划延后) | 2 | G28 (cancel mutation), G29 (checkpoint extra) |

### 已解 27 条按 Sprint 分组

| Sprint | Gap | Severity | Patch 区间 |
| --- | --- | --- | --- |
| A (critical) | G2 / G4 / G15 | 3 critical | L1030–L1197 / L1198 附近 delegation / L2496+ compaction |
| B (high) | G1 / G3 / G5 / G6 / G7 / G8 / G9 / G18 | 8 high | 分散 |
| C (medium) | G10 / G11 / G12 / G13 / G14 / G16 / G17 / G19 | 8 medium | 分散 |
| D (critical+high) | G20 / G21 / G22 / G23 / G24 / G25 / G26 / G27 | 4 critical + 4 high | L441–L3011 |

### 跨 Sprint 字段一致性检查

| 字段 | 定义位置 | 一致性 |
| --- | --- | --- |
| `retry_scope` | G15 L1058 (agent_runs DDL) / G14 L1329 (contract) / G22 L1370–L1395 (selection + propagation) | ✅ 三处同一 enum `{node, subtree, siblings}`，default `'node'` |
| `session_state_item_events.mutation` | G14 L1290–L1299 (8 values) | ✅ 未被 Sprint D 扩展（cancel 属 G28 延期，不在本轮范围） |
| `context_manifest_reason_types.reason` | G1 L1653–L1660 (DDL) / L1691–L1711 (18 seed) / G26 L1723–L1733 (+9 Sprint D) | ✅ 总 27 + `other` |
| `access_scope` | G9 L2110 (DDL default 'delegation') / G20 L2143–L2153 (deprecation + 新值) | ⚠️ 新插入默认值仍 'delegation' (deprecated)；建议 Phase 4 校验层拒绝或迁移 DEFAULT |
| `run_event_high_watermark` | G13 state response L399–L402 / G19 L647 session_watermarks / G24 L464–L467 cold-start replay | ✅ 三处语义一致，server hwm ≠ local applied |
| `normalize_version` | G17 (v0.2) / G27 L2067 (NULL→raw_v1) | ✅ 向后兼容 |
| `preview_template` 表格列 | G8 L1968–L1976 (4 列) / G27 L2044–L2063 (5 列 superset) | ✅ superset 扩展 |
| `batch_id/batch_seq` | 仅 G23 L1866+ (独立 namespace，与 G15 event_idx 无交叉) | ✅ 无冲突 |

**零矛盾**。

### 是否引入新 false-positive（Sprint B G19 模式）

依照 Sprint B 事故：声明 resolved 但无 GAP-FIX 锚点、承诺清单未落地。

本轮检查：
- `grep 'GAP-FIX: G2[0-7]'` → 8 × 2 = 16 个锚点全部命中（`<!-- GAP-FIX: G20 -->` / `<!-- /GAP-FIX: G20 -->` ... G27）
- GAP-TRACKER 每条 `resolution_commit` 字段均给 v0.3 + 具体行号区间
- 承诺清单逐条与文档证据比对，未发现"声称做了但找不到文字"
- G21 数学公式亲手 Python 验算 7 个 n 值全对
- G20 定义变更文本真实存在 (L2146–L2153)，不是只补字段

**结论：零新 false-positive**。

### v0.3 是否可发布

**可以发布为 v0.3 Draft。**

支撑证据：
1. Sprint A–D 共 27 条 gap 100% resolved，覆盖 14 压测场景提出的全部 critical/high
2. 文档从 v0.2 的 3040 行扩到 3653 行（+20%），新增均为 schema/契约/状态机/矩阵/公式/DDL，不是 filler
3. 交叉 Sprint 字段一致性检查无冲突
4. 向后兼容：原 schema 无删除/类型变更，新值/新列均 additive；旧 scope 值 DEPRECATE 而非删除
5. `## Changelog v0.3` 节 L3633–L3653 完整列出 8 条 Sprint D 闭环条目
6. 数学公式（G21）亲手验算通过
7. 定义层面更改（G20）文字落实

### 仍需进入 contract test 兜底的事项（承接 REGRESSION-SPRINT-C §建议）

Sprint C 曾列 8 条必补 contract test；Sprint D 补完后可删减如下：

| Sprint C 原条目 | Sprint D 是否强化 | 结论 |
| --- | --- | --- |
| 1. Compaction Invariants 8 条 (G2) | 未触 | **保留** |
| 2. run_counters 单 writer (G15) | 未触 | **保留** |
| 3. IndexedDB 原子性 (G19) | G24 cold-start 触达 | **保留** 但可合并 cold-start E2E 用例 |
| 4. Delegation tree bubble_up (G14) | G22 retry_scope 强化 | **保留** + 扩展 retry 路径 |
| 5. Approval state machine (G7) | 未触 | **保留** |
| 6. 小窗口 budget (G10) | G26 turn_intent=benchmark_comparison 覆盖 borrow 路径 | **保留** + 加 turn_intent 路径 |
| 7. Content hash normalization (G17) | G27 NULL→raw_v1 规则需测 | **加强**：必须验 legacy NULL read-path |
| 8. Device fingerprint 反回退 (G13) | G25 parity 需测 | **保留** + 加 passive expiry SSE 端到端 |

Sprint D 自身新增 contract test 项（承接 §6 建议）：

1. `cross-tree artifact access` (G20) — 兄弟读 + different-root deny + grant revoke audit (response test_plan 已列)
2. `delegation budget property test n∈{1,3,5,7,8,10,15}` (G21) — 包括 blocker borrow 路径 + overflow row
3. `retry_scope propagation` (G22) — 6 级选择规则每路径一个 case + unknown scope hard fail
4. `1000 session_tool_outputs bulk insert <500ms` (G23) — 性能契约 + partial batch rollback + context ignore incomplete
5. `cold-start hydration` (G24) — 空 IndexedDB replay + warm cache delta-path 不触发 replay + state API replay flag
6. `passive lease expiry SSE parity` (G25) — 扫描 emit 唯一 + SDK 清理 IndexedDB
7. `manifest reason enum completeness` (G26) — 生产 reason 在 enum 中 + 未知降级 other + 告警事件
8. `tool runner registration pre-condition` (G27) — 未注册 runner 拒写 + raw_ref parser golden tests + Rust/sql_compat baseline + legacy NULL→raw_v1

**总计：11 条 contract test 必补项**（原 8 条精简到 7 条仍保留 + Sprint D 新 8 条，有部分合并为 11 条）

### G28 / G29 延期结论

- G28 (`session_state_item_events.mutation` 补 `cancel`) — medium，S05 T3 触发 40 次/天 审计 JSON filter，不阻塞设计发布；Phase 1 contract test 兜底 + 后续 Sprint 合入 mutation enum DDL CHECK constraint
- G29 (checkpoint_v1.extra 子步断点) — medium，S04 T11 极端 pod kill 场景；Phase 1 contract test 兜底 + 设计层保留"extra LONGTEXT"不阻塞

**两者按 Sprint D 范围约定保留 open，不影响 v0.3 发布判断。**

---

## 建议

### 发布判断

**✅ v0.3 Draft 可发布，并可进入 Phase 1 实现期**。

理由：
1. 27/29 gap resolved（全部 critical + high），剩余 2 条 medium 已明确延期策略
2. 零 false-positive，零跨 Sprint 矛盾
3. 数学公式手工验算通过，定义层面更改落实
4. Changelog + GAP-FIX 锚点完整

### Phase 1 开工前清单（<1 天）

1. `session_artifacts.access_scope` DEFAULT 从 `'delegation'` 迁到 `'same_root_tree'` 或加入写入层校验器
2. `context_manifests` ALTER TABLE 加 `turn_intent VARCHAR(64) NULL` DDL（G26 现只描述字段，未给 DDL）
3. 补 `session_device_lease_events.event_type` / `scope_source` / `tool_runner_registry.status` / `raw_ref_scheme_registry.access_check` 几处 enum 值清单（已在 residual 列出）
4. 把 G14 `mutation` 枚举 8 值写成 SQL CHECK constraint（Sprint C 遗漏项）

### Contract Test 兜底必补项（11 条见上）

优先级：
- **P0**: G21 budget property test / G23 1000-row <500ms 性能 / G22 retry_scope hard fail / G20 sibling access integration
- **P1**: G24 cold-start E2E / G25 passive expiry SSE / G27 runner registration / G19 IndexedDB 原子性（合并 G24）
- **P2**: 保留 Sprint C 原 G2/G7/G13/G15 四条 invariants 测试

### 后续 Sprint E 候选

- G28 (cancel mutation) + G29 (checkpoint extra) 打包进 Sprint E，预计 0.5 天
- 若 Phase 1 实现期发现新 gap（G30+），走标准 walkthrough → gap tracker → patch → regression 循环

---

## 附：Sprint D Patch 锚点验证

```text
$ grep -n 'GAP-FIX: G2[0-7]' docs/design/web-agent-session-state.md
441:  <!-- GAP-FIX: G24 -->
498:  <!-- /GAP-FIX: G24 -->
560:  <!-- GAP-FIX: G25 -->
630:  <!-- /GAP-FIX: G25 -->
1346: <!-- GAP-FIX: G22 -->
1397: <!-- /GAP-FIX: G22 -->
1718: <!-- GAP-FIX: G26 -->
1764: <!-- /GAP-FIX: G26 -->
1858: <!-- GAP-FIX: G23 -->
1932: <!-- /GAP-FIX: G23 -->
1990: <!-- GAP-FIX: G27 -->
2097: <!-- /GAP-FIX: G27 -->
2155: <!-- GAP-FIX: G20 -->
2229: <!-- /GAP-FIX: G20 -->
2968: <!-- GAP-FIX: G21 -->
3011: <!-- /GAP-FIX: G21 -->
```

**8 条 gap × 2 锚点 = 16 个锚点全部命中**。

---

## 结语

Sprint D 是 web-agent-session-state v0.1 → v0.3 设计 review 循环的收官轮。从 **14 场景压测 → 29 gap 识别 → Sprint A/B/C/D 四轮 patch → 四轮 regression** 全闭环。

本轮未出现 Sprint B G19 式的 false-positive，说明：
- 设计 agent 吸取了 Sprint B 的教训（所有 resolution_commit 都给具体 L 区间）
- Walkthrough-driven gap discovery（14 × §5） 比 gap review 更能发现运行期陷阱（4 critical 均在此阶段暴露）
- 数学公式类 gap（G21）必须手工验算，不能只看 LGTM

**进入 Phase 1 实现期的前置条件已满足**。
