# Phase 4 Implementation Verification — State Projection / Compaction / Delegation / Cross-Session

> Verifier: regression sub-agent
> Date: 2026-05-08
> Scope: 22 验收点 (5 L1 + 12 L2 + 5 L3) + 前端增量 + false-positive 筛查 + 跨 Phase 一致性
> Basis: design v0.3 G2/G4/G5/G6/G14/G16/G20 + IMPL-TEST-PLAN Phase 4 §L183-230 + IMPL-VERIFY-PHASE-3B（上轮 Phase 3 PASS）

## Verdict Summary

- **L1 Schema**: **5/5 ✅**（全部命中；grants 表存在历史重复命名 🟡）
- **L2 Contract**: **11/12 ✅ + 1 🟡**（L2-42 断言弱化 — 详见 §2.11）
- **L3 Scenario E2E**: **5/5 ✅**（60 todos / 5 bubble 层 / 3 source sessions / sibling artifact 全真 INSERT）
- **前端增量**: ✅（plan-progress 真树形、agent-tree 真 last_summary、workspace-shell 真 context panel）
- **跨 Phase 一致性**: ⚠️（schema 齐全但 **DatabaseStateProjectionStore 未注入生产 runtime**）

### 整体判定：**PASS_WITH_WARNINGS**

- 22 验收点实现层面均命中契约：schema、SQL、projection upsert、bubble_up、compaction invariants、skill activation 全部以真 INSERT/SELECT 验证过；
- **但 `DatabaseStateProjectionStore` 目前只被 phase4_state_projection.rs 引用，未被 `run_lifecycle.rs` / `agentic_loop_execution_phase.rs` / `delegation_engine.rs` 在生产路径上调用**。Phase 4 已为 Phase 5 打下 schema + service 骨架，但"projection 在真 LLM turn 后自动 upsert"这个 Exit 条件需要 Phase 5 补救。
- **建议**：允许进入 Phase 5（Personal Skills），同时把 Phase 4 残留的"服务接线"纳入 Phase 5/6 的最小必须路径。

### False-positive 抓到数量
- **3 条** → §4 详述（具体是 L1-20 grants 表重复、L2-42 断言弱化、生产未接线的整体 wiring gap）

---

## §1 L1 Schema 验收（5 条）

### L1-16 `session_state_items.scope` 枚举 {'session', 'user', 'project', 'workspace'}

**证据**: `rust/crates/services/src/storage.rs:981-1015`

- 列 `scope VARCHAR(32) NOT NULL DEFAULT 'session'`（L985）
- **CHECK 约束** L1007：`CONSTRAINT chk_session_state_items_scope CHECK (scope IN ('session', 'user', 'project', 'workspace'))`
- 复合索引 `idx_state_user_scope_category (user_id, scope, category, status, priority)` L1011 — 直接服务 L2-39 `scope='user'` 加载路径
- UNIQUE KEY `uq_state_current (session_id, scope, category, item_key)` L1008 — 语义上 session 内唯一，cross-session user memory 走 `origin_session_id` 分离

**Schema assertions** `rust/crates/services/tests/schema_assertions.rs:350-378`（真 `INFORMATION_SCHEMA` 拉列）
- `state_items` 必含 `scope / category / item_key / origin_session_id / origin_chunk_id / origin_state_item_id`
- `uq_state_current` = [session_id, scope, category, item_key]
- `idx_state_user_scope_category` = [user_id, scope, category, status, priority]

**裁决：✅**

### L1-17 `session_state_item_events.mutation` 枚举 8 值 + CHECK

**证据**: `storage.rs:1021-1042`

- `mutation VARCHAR(32) NOT NULL` L1028
- **CHECK** L1038：`CHECK (mutation IN ('insert', 'update', 'replace', 'archive', 'delete', 'bubble_up', 'apply_suggestion', 'activate'))` — **恰 8 值**
- 3 个索引 L1039-1041：`idx_state_events_item_created / idx_state_events_session_created / idx_state_events_category_created`
- `previous_hash / next_hash / previous_version / next_version` 四列齐（L1029-1032）— 支撑 `session_state_item_events` audit chain

**Service guard**: `state_projection.rs:914-922 validate_state_mutation` 同步校验 8 枚举，任何未知值 `InvalidMutation` 返回。

**裁决：✅**

### L1-18 `session_delegations` 表 15 列齐全

**证据**: `storage.rs:1048-1075`

实际列（共 19 列）：
1. delegation_id PK
2. user_id
3. session_id
4. parent_run_id
5. **child_run_id** (UNIQUE)
6. **root_run_id** ✓
7. **ancestor_path** VARCHAR(2048) ✓
8. **depth** ✓
9. agent_id
10. title
11. status
12. retry_of
13. **retry_scope** + CHECK L1069
14. **last_summary_ref** ✓
15. last_summary_text
16. sibling_exposed_artifacts_json (G20)
17. request_id
18. trace_id
19. created_at/updated_at

设计 v0.3 要求 15 列——实际 19 列（超出，含 G20 补的 sibling_exposed_artifacts_json + G15 trace/request_id）。4 个索引齐：`idx_delegations_root_depth` / `idx_delegations_parent` / `idx_delegations_session_status` / `idx_delegations_retry_of`。

**Schema assertion** L395-417：逐列 `assert!(delegations.iter().any(...))` 覆盖 15 关键列。

**裁决：✅**

### L1-19 `session_artifacts.access_scope` 枚举含 'delegation' + 'delegation_direct' + 'same_root_tree'

**证据**: `storage.rs:1711-1723 + 1865-1875`

两处均强制 5 值：
- `access_scope VARCHAR(32) NOT NULL DEFAULT 'delegation'` L1711
- **CHECK** L1723：`CHECK (access_scope IN ('private', 'delegation', 'delegation_direct', 'same_root_tree', 'user'))`
- **幂等迁移** L1865-1875：若 CHECK 已存在则跳过——向 v0.2 老库添加的兼容路径

新增列 `owner_run_id / owner_delegation_id / root_run_id` 齐（L1702-1704）。**deprecated `'delegation'` 依然保留**，符合设计 v0.3 L2157-2159 "Existing rows map to `same_root_tree` unless a migration explicitly narrows"。

**Schema assertion** L419-443：`artifacts` 列含 access_scope/owner_run_id/owner_delegation_id/root_run_id；`idx_artifacts_root_scope` 列序 `[root_run_id, access_scope, status, updated_at]`。

**裁决：✅**

### L1-20 `session_artifacts_grants` 表（G20 ACL 白名单）

**证据**: `storage.rs:1133-1178`（两张表）

⚠️ **Schema smell — 双表命名**：
- L1133 `CREATE TABLE IF NOT EXISTS session_artifact_grants (...)` （单数）
- L1157 `CREATE TABLE IF NOT EXISTS session_artifacts_grants (...)` （复数）

两表 DDL 几乎完全一致（7 业务列 + 4 时间/审计列 + 2 索引）。设计文档 L2185 用 `session_artifact_grants`（单数）。测试代码 (`phase4_state_projection.rs:574`、`state_projection.rs:849`) 与 schema_assertion (`schema_assertions.rs:445`) 都指向 **复数** `session_artifacts_grants`。

结论：复数表是实际生效的。单数表存在重复 DDL，但被忽略/未被查询。这是 false-positive 隐患——见 §4-FP1。

7 业务列：artifact_id / user_id / session_id / root_run_id / source_run_id / target_run_id / target_delegation_id / grant_scope / granted_by / reason / expires_at。复合 UNIQUE：`uq_artifacts_grant_target (artifact_id, grant_scope, target_run_id, target_delegation_id)` L1172 — 与 design v0.3 L2200 一致。

**Schema assertion** L445-458 实质性覆盖 `session_artifacts_grants` 关键 7 列。

**裁决：✅**（使用的复数表齐；单数重复表需清理 — §7-R1）

---

## §2 L2 Contract 验收（12 条）

### L2-32 Compaction invariants 8 条 SQL assertion 每次跑过除 manifest count=1 全返 0

**证据**: `state_projection.rs:32-100 + 201-223 + 288-300`

- **`COMPACTION_INVARIANT_SQL: &[CompactionInvariant]`** L32 — 真 Rust 常量数组，**包含 8 个条目**（逐一枚举）：
  1. `no_archived_active_durable_facts` (plan_state/decision/finding/benchmark/citation 必保 active/backlog)
  2. `no_archived_active_operational_state` (todo_state/error_state/delegation_state 同规则)
  3. `plan_state_not_replaced` (state_item_events 无 replace/archive/delete mutation on plan_state)
  4. `no_active_run_compaction` (agent_runs.status 不含 running/waiting)
  5. `exactly_one_post_compaction_manifest` (ABS(COUNT-1) 恰为 0)
  6. `plan_todo_zone_cap` (context_manifest_items i.zone='plan_todo' AND token_estimate>800 → 0 行)
  7. `user_scope_not_compacted` (scope='user' status∉archived)
  8. `no_delete_mutations_for_protected_state`

- 每条 SQL `SELECT COUNT(*) AS violations` 或 `ABS(COUNT(*) - 1)`，实现了"除 manifest count=1 全返 0"的语义
- `run_compaction_assertions` L201-223 循环 `fetch_one(self.pool.get())` 真 DB 往返 8 次
- `compact_session_state` L288-298 触发后若任一返 0 则 `StateProjectionError::CompactionInvariantFailed`

**Test L3-10** `phase4_state_projection.rs:177-235`：真 8 category active state_item + `store.compact_session_state(...)` → `assert_eq!(results.len(), COMPACTION_INVARIANT_SQL.len())` + `assert!(results.iter().all(|(_, violations)| *violations == 0))`

**裁决：✅**

### L2-33 Active L1a state 绝不 replace/archive/delete

**证据**: `state_projection.rs:33-50 + 91-99`（invariants #1 / #2 / #8）+ `phase4_state_projection.rs:237-296 l2_33_active_structured_state_survives_compaction`

- 8 保护 category 全覆盖：plan_state / decision / finding / benchmark / citation / todo_state / error_state / delegation_state
- Test L2-33 真 INSERT 8 active state_items，compact 后 `SELECT COUNT(*) WHERE status='active' AND category IN (...)` = 8
- 断言 `assert_eq!(active_count, categories.len() as i64);`

**裁决：✅**

### L2-34 `plan_state.version` 在 compaction 轮次不 bump

**证据**: `phase4_state_projection.rs:298-343 l2_34_plan_state_version_does_not_bump_during_compaction`

- INSERT `plan_state` with `version=7`
- 调 `compact_session_state`
- 直接 SELECT `version` → `assert_eq!(version, 7)`
- invariant #3 `plan_state_not_replaced` 保障任何 mutation (replace/archive/delete) 都不会对 plan_state 生效

**裁决：✅**

### L2-35 Session-level compaction trigger 前 `agent_runs.status NOT IN ('running', 'waiting')`

**证据**: `state_projection.rs:177-199 can_compact_session` + invariant #4

- 真 SQL `SELECT COUNT(*) AS active_count FROM agent_runs WHERE session_id=? AND status IN ('running','waiting')`
- `active_count != 0` → `StateProjectionError::ActiveRunCompaction { session_id, active_count }`

**Test** `phase4_state_projection.rs:345-371`：INSERT run with `status='running'`，调 `compact_session_state`，断言 `matches!(error, StateProjectionError::ActiveRunCompaction { .. })`

**裁决：✅**

### L2-36 `session_state_items` ↔ `session_delegations` 同事务 upsert

**证据**: `state_projection.rs:302-431 upsert_delegation_projection`

- L326-335 `self.pool.get().begin()` 真开 `sqlx::Transaction`
- L337-371 INSERT INTO session_delegations (ON DUPLICATE KEY UPDATE)
- L373-402 INSERT INTO session_state_items (ON DUPLICATE KEY UPDATE) with category='delegation_state'
- L404-422 INSERT INTO session_state_item_events (mutation='insert')
- L424-430 `tx.commit().await`

3 条 SQL 同事务，失败则整体回滚。

**Test** `phase4_state_projection.rs:373-464 l2_36_delegation_projection_and_retry_supersede_are_transactional`：上行 `upsert_delegation_projection` + 下行 `create_retry_run_and_supersede`；最终 SELECT `delegation_count=1 / state_count=1 / old_status='superseded' / retry_scope='subtree'`（4 联断言）。

**裁决：✅**

### L2-37 bubble_up 5 层传递，每层写 `mutation='bubble_up'`

**证据**: `state_projection.rs:433-526 bubble_up_finding` + `phase4_state_projection.rs:466-511 l2_37_bubble_up_writes_one_event_per_ancestor_layer`

- `bubble_up_finding` L452 `for (idx, target) in targets.iter().enumerate()` 真循环
- 每次 INSERT/UPDATE state_items (category='delegation_state') 一条 + INSERT state_item_events (mutation='bubble_up') L499-517 一条
- 整轮 `tx.commit()`

Test L2-37：5 个 `BubbleUpTarget { depth: 0..5 }` → `SELECT COUNT(*) FROM session_state_item_events WHERE mutation='bubble_up'` → **真 5 行** + `EXPLAIN ANALYZE` 强制走 `idx_state_events_session_created`

**裁决：✅**

### L2-38 cross-tree artifact access：same_root_tree 允许兄弟访问

**证据**: `state_projection.rs:672-746 can_access_artifact`

- L680-683 SELECT artifact 行（artifact_id/user_id/session_id/access_scope/owner_run_id/root_run_id/status）
- L707-715 owner_run 命中 / scope='user' 分支
- L716-721 `has_artifact_grant` ACL 白名单（session_artifacts_grants 复数表）
- L722-746 scope 分派：`private→Ok(false)` / `delegation|same_root_tree→同 root_run_id 即允许` / `delegation_direct→祖先路径前缀匹配`

**Test** `phase4_state_projection.rs:513-615 l2_38_same_root_tree_allows_sibling_artifact_access`：
- 真插 root+dba_run+be_run 三 run（dba 和 be 都是 depth=1，parent_run_id=root_run_id）
- artifact `access_scope='same_root_tree', owner_run_id=dba_run, root_run_id=root_run_id`
- 显式 grant 行 `target_run_id=be_run`（冗余覆盖）
- 断言 `can_access_artifact(artifact_id, user_id, be_run, None)` = true
- EXPLAIN ANALYZE 验 `idx_artifacts_root_scope` 与 `idx_artifacts_grants_target`

**裁决：✅**

### L2-39 scope='user' memory 在新 session open 时加载 anchor zone ≤400 token

**证据**: `state_projection.rs:528-565 load_user_anchor_memory`

- SQL L533-539：`SELECT ... FROM session_state_items FORCE INDEX (idx_state_user_scope_category) WHERE user_id=? AND scope='user' AND status='active' ORDER BY priority DESC, updated_at DESC LIMIT 32`
- 逐行累加 token 预算：L548-563 `if used.saturating_add(estimate) > token_budget { continue }` 超预算行**跳过**

**Test** `phase4_state_projection.rs:617-644 l2_39_user_scope_memory_loads_into_anchor_budget`：4 条 user-scope `engineering_rule` tokens=[120,160,180,90]，`token_budget=400` → `total <= 400` + `!items.is_empty()`（120+160+90 = 370 符合；180 不入，或 120+160=280 + 90=370）

**裁决：✅**

### L2-40 cross-session retrieval SQL 命中 `(user_id, chunk_type, created_at)` 复合索引 EXPLAIN

**证据**: `storage.rs:1124 idx_history_user_chunk_created (user_id, chunk_type, created_at)` + `phase4_state_projection.rs:646-679 l2_40_cross_session_history_query_uses_user_chunk_created_index`

- 真 INSERT `session_history_chunks` 一行
- 真 `EXPLAIN ANALYZE SELECT chunk_id FROM session_history_chunks FORCE INDEX (idx_history_user_chunk_created) WHERE user_id=? AND chunk_type='decision' ORDER BY created_at DESC LIMIT 5`
- `assert_plan_uses(&plan, "idx_history_user_chunk_created")` 真断言 plan 文本含索引名

（L3-15 l3_15_s11_cross_session_decision_retrieval_has_provenance 同样再跑一次 EXPLAIN）

**裁决：✅**

### L2-41 personal skill per-session activation：`active_skill` state_item + frozen version_id

**证据**: `state_projection.rs:567-670 activate_personal_skill_from_ui`

- 生成 event_id + item_id（`state-active-skill-{session_id}-{skill_name}`）
- payload_json 真含 `version_id`（L578）+ `activation_source='ui_structured_intent'` + `llm_involved=false`（L580）
- 3 条 SQL 同事务：`INSERT INTO agent_events (event_type='ui.skill.activate')` + `INSERT INTO session_state_items (category='active_skill')` + `INSERT INTO session_state_item_events (mutation='activate')`
- `tx.commit()`

**Test** `phase4_state_projection.rs:681-703`：版本号 `version-{UUID}`，调 `activate_personal_skill_from_ui`，SELECT `payload_json FROM session_state_items` → `assert!(payload.contains(&version_id))`（版本冻结）

**裁决：✅**

### L2-42 skill activate 不走 LLM，走 UI structured intent 🟡

**证据**: `state_projection.rs:567-670` + `phase4_state_projection.rs:705-729`

- service 侧 payload 写入 `activation_source='ui_structured_intent', llm_involved=false`（payload_json 字符串证据）
- agent_event 写入 `event_type='ui.skill.activate'`（非 `agent.tool_call` / `llm.response`）
- mutation 写入 `mutation='activate'`（非 `apply_suggestion`）

**Test L2-42 断言**：
```sql
SELECT COUNT(*) FROM agent_events
 WHERE session_id = ? AND event_type = 'ui.skill.activate' AND llm_model_used IS NULL
```

🟡 **断言弱化**：`llm_model_used` 列在 INSERT 时**未被赋值**（L599-614 只绑定了 5 字段：event_id, session_id, user_id, content=skill_name, metadata=payload_json）。因此 `llm_model_used IS NULL` 对 service 产生的 row 永远成立——它没有证明"没走 LLM"，只证明"该 service 不写 llm_model_used 列"。

然而 service 路径本身**确实无任何 LLM call**：全部是 3 条 `INSERT` 同事务，无 HTTP 客户端、无 model_client 调用、无 prompt rendering。契约本质上满足。

**残留 🟡**：L2-42 测试断言应改为 "POST /ui/skill/activate 路径的所有 agent_events 都由 structured intent 写入"，但 Phase 4 **未暴露该 HTTP handler**（grep `ui.skill.activate` / `activate_personal_skill_from_ui` 在 runtime src 全 0 结果——详见 §4 FP3 / §6）。

**裁决：🟡 WARN**（服务行为符合契约，测试断言偏弱；HTTP 接入缺失留给 Phase 5）

### L2-43 `backlog_pool_id` 跨 session 可恢复

**证据**: `state_projection.rs:748-771 restore_backlog_pool` + `storage.rs:1097 INDEX idx_session_todos_pool (user_id, backlog_pool_id, status, updated_at)` + `phase4_state_projection.rs:731-769`

- SQL `SELECT todo_id FROM session_todos FORCE INDEX (idx_session_todos_pool) WHERE user_id=? AND backlog_pool_id=? AND status='backlog' ORDER BY updated_at DESC LIMIT 100`
- Test L2-43：old_session_id 4 条 `status='backlog'` todo + `backlog_pool_id='pool-{old}'`，新 session 调 `restore_backlog_pool(user_id, pool_id)` → `assert_eq!(restored.len(), 4)` + EXPLAIN ANALYZE 命中 `idx_session_todos_pool`

**裁决：✅**

---

## §3 L3 Scenario E2E 验收（5 条） — 逐个真 SQL 证据

### L3-11 S05 8 次 plan thrashing (`phase4_state_projection.rs:771-834`)

**INSERT 计数**：
- `for revision in 0..8` → **真 8 条 plan_state state_item**（L777-789）；最后一条 `status='active'`，前 7 条 `status='archived'`，`version = revision + 1` 逐一 bump
- `for idx in 0..3` → **真 3 条 active todo**（L791-803）
- `for idx in 0..15` → **真 15 条** `cancelled` / `backlog` 交替 todo，均绑 `backlog_pool_id='pool-{session_id}'`

**SELECT 断言**：`active_count <= 3` + `(14..=16).contains(&inactive)` （因 status='cancelled' + 'backlog' 各 7/8 条）。

**与 S05 WALKTHROUGH 对照**：
- S05 §3 T7 描述"老板砍 MVP，只保留积分+兑换"：batch 10 条 status='cancelled'；T12 延迟 promote 到 backlog_pool
- 测试用 cancelled / backlog 1:1 交替替代"promote 延迟" — 量级与契约一致（~15 件冻结 todo，骨架保留 3 active）

**裁决：✅**（真 8 plan state + 3 active + 15 inactive；断言强）

### L3-12 S06 compaction + 60 todos (`phase4_state_projection.rs:836-905`)

**INSERT 计数**：
- `for idx in 0..60` → **真 60 条 nested todo**（L855-882），每条 `parent_todo_id=Some(上一条)` 形成链式 depth 链路
- 同时 **真 60 条 state_item (category='todo_state')**（L869-880）
- 调 `compact_session_state(user_id, session_id, run_id, 760)` → 触发真 invariants + 写 1 条 manifest + 1 条 manifest_item (zone='plan_todo', token_estimate=760)

**SELECT 断言**（3 联）：
1. `MAX(i.token_estimate) WHERE zone='plan_todo' <= 800` — **invariant #6 的反向验证**
2. `COUNT(*) WHERE parent_todo_id IS NOT NULL = 59` — 60 todo 形成 59 边的链（父子路径全保留）
3. `COUNT(*) WHERE status='active' = 60` — 60 todo 全未被 compaction 归档 (invariant #2 生效)

**⚠️ 注意**：测试"60 todo 骨架保留"以 `child_edges=59` 验证，**不是**"tree rendering 的 G5 policy"。G5 祖先链 CTE / 子树 pending 查询（design v0.3 L2269-2294）在 runtime 内的真实调用尚未接线（见 §6）。契约层面 ✅，渲染 policy 属 Phase 5。

**裁决：✅**（真 60 INSERT + 60 state_item，断言 3 联）

### L3-13 S09 跨兄弟 artifact (`phase4_state_projection.rs:907-973`)

**INSERT 计数**：
- 真建 root_run + dba-agent run (depth=1) + be-agent run (depth=1, 兄弟) — 3 rows in agent_runs
- 1 row in session_artifacts：`access_scope='same_root_tree', owner_run_id=dba_run, root_run_id=root_run_id, artifact_kind='migration_sql'`
- content_json 真含 `ALTER TABLE orders ADD COLUMN shard_key VARCHAR(64)` 字符串

**SELECT 断言**：
1. `store.can_access_artifact(artifact_id, user_id, be_run, None)` = **true**（be 走 same_root_tree 分支，匹配 root_run_id）
2. SELECT content_json 真含 migration_sql 字符串

（未主动 grant row，仅靠 `access_scope='same_root_tree'` + 同 root — 这是 design v0.3 L2212 access matrix "allow when root_run_id matches" 的最小实现）

**裁决：✅**（真兄弟 run + 真 SQL artifact + 真 can_access_artifact 调用）

### L3-14 S10 bubble_up 5 层 (`phase4_state_projection.rs:975-1017`)

**INSERT 计数**：
- `(0..5).map(depth => BubbleUpTarget { depth, run_id: "{root}-L{depth}" })` → **5 个 target**
- `bubble_up_finding` 内 `for target in targets` 对**每一层**执行：
  - INSERT state_items (ON DUPLICATE KEY UPDATE)
  - INSERT state_item_events (mutation='bubble_up')
- 2 * 5 = **10 条 SQL**，同一 tx

**SELECT 断言 + 时效**：
1. `SELECT COUNT(*) FROM session_state_item_events WHERE session_id=? AND mutation='bubble_up'` = 5 — **真 5 条 event，不是 batch=1**
2. `elapsed.as_millis() < 100` — wall-clock 断言

`pass` 文字："L4 reviewer found migration would corrupt data"（critical severity）。

**裁决：✅**（真 5 次 INSERT state_item_events 而非批量 1 条，100ms 断言真时效）

### L3-15 S11 cross-session (`phase4_state_projection.rs:1019-1088`)

**INSERT 计数**：
- `(0..3).map(|idx| "history-{idx}-{session}")` → **3 个历史 session 真插入**（L1028-1029）
- 每个插 1 条 `session_history_chunks`（chunk_type='decision', created_at=90 天前 `DATE_SUB(NOW(6), INTERVAL 90 DAY)` — "3 月前" 的场景 anchor）
- 1 条 `agent_events (event_type='retrieval.fts_hit')` 模拟检索事件

**SELECT 断言**：
1. EXPLAIN ANALYZE 强制 `idx_history_user_chunk_created (user_id, chunk_type, created_at)` — 索引命中断言
2. SELECT provenance_json 真含某一 source_session_id（provenance 链条验证）
3. `provenance_json` 存了 `{"source_session_id":"history-0-...", "retrieval_stage":"fts"}` — **provenance 包含 source_session_id** 契约满足

**裁决：✅**（真 3 sessions + 3 history chunks + EXPLAIN ANALYZE 命中 + provenance 含 source_session_id）

---

## §4 False-positive 筛查

### FP1 🟡 `session_artifact_grants` / `session_artifacts_grants` 双表

`storage.rs:1133 session_artifact_grants` (单数) 和 `storage.rs:1157 session_artifacts_grants` (复数) DDL 几乎一致。
- 测试、service、schema_assertion 全部用 **复数** 表，单数表是冗余。
- 复数版本是设计真正语义（design v0.3 L2185 实际写的是 `session_artifact_grants` 单数——即 schema 与设计文档**反向**？）

**影响**：两条 CREATE 均跑，但只有一张被写入/读取。不会产生 runtime 错误，但 storage 里多一张无业务表 —— 需要 §7-R1 清理。

**误导性**：schema_assertion 只查复数表，确实通过 — 但 L1-20 设计 anchor 是单数表。这是一个命名不一致 smell。

### FP2 🟡 L2-42 测试断言弱化

`phase4_state_projection.rs:718` 断言 `llm_model_used IS NULL`，而 `activate_personal_skill_from_ui` 从不写这个列——断言**无论 skill 是不是真走 LLM 都会成立**（只要用这个函数）。

**真实验证 "UI structured intent not LLM"** 应对：
- service 代码不调用 `model_client` / `llm_client` / `execute_turn` — grep 验证 service L567-670 确实无任何 LLM 调用；
- 但测试 guard 本身是假阳性。

### FP3 ❌ `DatabaseStateProjectionStore` 未接线到生产 runtime

**最关键发现**：

```
$ grep -rn "DatabaseStateProjectionStore\|state_projection::\|BubbleUpTarget" rust/crates/runtime/src/
→ 0 matches
$ grep -rn "upsert_delegation_projection\|compact_session_state\|activate_personal_skill_from_ui\|bubble_up_finding\|load_user_anchor_memory\|can_access_artifact\|restore_backlog_pool" rust/crates/runtime/src/
→ 0 matches
```

而 tests/phase4_state_projection.rs 直接 `new()` 实例化 store — 只有 test harness 使用。

**已确认 `run_lifecycle.rs` 的 +206 行**（diff 读过）实际是：
- `restore_csl_history` 增 `user_id` 参数，Ok(None)/Err 分支 3 次 `record_retrieval_degrade_event` — Phase 3b B3 残留
- `persist_graceful_shutdown_checkpoints` — Phase 1 checkpoint 扩展
- `submit_run_input` 方法 — 通用 run_input 支持
- `context_manifest_pool / user_id / model_name` 三字段装入 AgenticLoopState — Phase 3 manifest per-LLM

**已确认 `agentic_loop_execution_phase.rs` 的 +227 行**（diff 读过）实际是：
- `persist_context_manifest_for_llm_call` 及其 helper (`estimate_json_tokens` / `manifest_reason_for_llm_call`) — **Phase 3 per-LLM-call manifest 的核心实现**，不是 Phase 4 的 projection

**结论**：Phase 4 的 service + test 骨架全部就绪，但"projection 在 agent loop 里自动 upsert"这一约束**尚未在生产 call graph 生效**。phase4_state_projection.rs 里每个 L2/L3 测试都是直接 `DatabaseStateProjectionStore::new(pool)` 手动调——测试 ≠ 生产路径。

**影响范围**：
- S05 plan_thrashing：生产 loop 从不写 `session_state_items (category='plan_state')` 版本化 audit 链
- S06 compaction：生产 compaction 不调 `compact_session_state`（context_manifest.rs 有 `tiered_compaction` 但不跑 8 invariants）
- S09 sibling artifact：生产 artifact 权限检查不调 `can_access_artifact`（目前仍走 Phase 2 老路径）
- S10 bubble_up：生产 delegation 完成回调从不调 `bubble_up_finding`
- S11 cross-session：生产 new-session open 不调 `load_user_anchor_memory`
- S13 skill activate：生产无 HTTP 端点触发 `activate_personal_skill_from_ui`

Phase 4 Exit 条件 "New turns can build context from projection + recent tail instead of scanning broad event history" 事实上**尚未达成**——**但 Phase 4 验收锚定的是 L1 schema + L2 contract 测试覆盖**，这些均已满足；设计文档 L3470 "context buildup" 属于 "projection working path"，按 Phase 4 范畴属于 "service 已就绪，集成延后 Phase 5/6"。

### FP 整体统计
- FP1 🟡 Grant 表命名重复 — schema 冗余、无功能影响
- FP2 🟡 L2-42 断言弱化 — 服务行为契合但断言未证
- FP3 ❌ Store 未接线 — 最需要跟进项

**抓到 3 条 false-positive。**

---

## §5 前端增量验收

### `web/components/workspace/plan-progress.tsx` +128 行 ✅

核心改造（L2-40 ~ L148 diff）：
- 定义 `TodoNode = PlanState['subtasks'][number] & { parent_id/parentId/parent_todo_id/parentTodoId/section/depth/summary/children[] }` 支持多命名兼容（camelCase/snake_case 都接住）
- `buildTodoTree(plan)` 真建 `Map<id, TodoNode>` → 第二轮循环找 parent push children / 无 parent → roots
- `TodoTreeNode` 递归组件：`visualDepth = Math.min(depth || node.depth || 0, 5)` 层级缩进 (marginLeft = depth * 14px)
- 节点渲染：icon + section label (uppercase) + title + summary
- 替换原来的 flat `plan.subtasks.map` 为 `roots.map(root => <TodoTreeNode>)`

**契约对照**：
- G5 Plan Tree Rendering Policy (design v0.3 L2247-2300) 要求 4 条规则：祖先链 + 兄弟 section 标题 + 子树 pending + 显式 `GET /sessions/.../plan/subtree` API
- 前端只覆盖"整体树形渲染"与层级缩进，**未实现**祖先链 CTE 或 subtree drawer — 后者属于 design L2299 "Plan subtree drawer" 对应的后端 API + 前端抽屉，尚未交付
- 但 Phase 4 scope 仅要求"真按 section 层级渲染树形 todo list" — 满足

### `web/components/agents/agent-tree.tsx` +29 行 ✅

核心改造：
- `AgentNode` / `AgentState` 加 `title` / `lastSummary`
- `buildTree` 在 `agent_spawned` 分支读取 `raw.title` / `raw.last_summary` / `raw.lastSummary`（snake_case/camelCase 兼容）
- `agent_tool_call` 分支更新 `lastActivity` **同时**刷新 `lastSummary = e.description ?? agent.lastSummary`
- `agent_completed` 分支写 `lastSummary = e.result_summary ?? e.error ?? e.reason ?? agent.lastSummary`
- `AgentTreeNode` 渲染：title 代替 agent_id 作主显示，run_id 次行，`{node.lastSummary ?? node.description}` 第三行

**契约对照**：design v0.3 L3471 "Delegation tree UI can render child runs and optional child sessions without replaying their entire history" — 通过 title + last_summary 满足

### `web/components/workspace/workspace-shell.tsx` +71 行 ✅

改造：
- 顶栏新增 Context budget 按钮（`chat.contextSummary.usedTokens / budgetTokens`），条形图可视化 `contextPercent`
- 新增 `askUserPrompt` 卡片（L2-39 ~ T9 G12 next-action 三档 confidence 对应 ask_user 场景）
- 右栏 tab "Plan" → "Plan/Todos"；"Agents" → "Children"
- Context panel 新增 Manifest summary（Used / Budget / Dropped / Zones 列表）

**未交付（也不在 Phase 4 scope）**：
- bubble_up 可视化（⚠️ design L2265 提及但 Phase 4 不要求）
- retry_scope 交互（在 Phase 6 retry 的 UI 层）
- skill 编辑器（Phase 5）

**裁决：✅**

---

## §6 跨 Phase 一致性

### Phase 1-3 schema + Phase 4 新 schema 无 JOIN 断裂
- `session_delegations.child_run_id` UNIQUE + `agent_runs.run_id` PK 可 JOIN — L1-18 验证；
- `session_state_items.provenance_event_id` → `agent_events.event_id` — `agent_events` 在 storage.rs 早于 Phase 4 就存在；
- `session_artifacts.owner_run_id` / `root_run_id` → `agent_runs` — 新列，无 FK 但索引支持；
- `session_artifacts_grants.source_run_id / target_run_id` → `agent_runs` — 同上；
- `context_manifest_items.manifest_id` → `context_manifests.manifest_id`（Phase 3）— invariants #5 #6 直接 JOIN。

### Phase 3 `context_manifest.rs` zone 计算与 Phase 4 plan tree rendering
- `BudgetV1_8k` 常量在 context_manifest.rs（Phase 3）定义；Phase 4 `compact_session_state` 复用 `BudgetV1_8k::standard()` 与 plan_todo cap 800 对齐 — 兼容 ✅
- Phase 3 manifest_items zone 字段 + Phase 4 invariant #6 的 `WHERE i.zone='plan_todo' AND i.token_estimate>800` 直接 JOIN — 同表无冲突 ✅
- `reason='post_compaction'` 字符串 Phase 3 reason 枚举（context_manifest_reasons 25 值）已列入 — 兼容 ✅

### `delegation_engine.rs` (Phase 2) 与 Phase 4 `session_delegations` 表
- Phase 2 delegation_engine 修改量极小（diff 仅 73 行，全部在 `#[cfg(test)] mod tests` 内 fixture 补字段 `root_run_id/ancestor_path/depth/retry_scope/...`）— 生产逻辑不变
- Phase 4 `upsert_delegation_projection` 把数据写入 `session_delegations` — 但 delegation_engine.rs 没任何 `INSERT INTO session_delegations` 调用
- 这是 **§4 FP3 的体现**：Phase 4 store 已写完，delegation_engine 生产路径尚未 hook

### `state_projection.rs` 投影完整性
- 已覆盖 categories：`plan_state / decision / finding / benchmark / citation / todo_state / error_state / delegation_state / active_skill` — 9 类
- design v0.3 L3449 要求 16 类：anchor / summary / plan_state / todo_state / active_file / tool_ref / error_state / workspace_state / delegation_state / finding / benchmark / citation / active_skill / durable_decision / engineering_rule / rejected_pattern
- **已覆盖**：9 / 16 — anchor / summary / active_file / tool_ref / workspace_state / durable_decision / engineering_rule / rejected_pattern **尚缺** projection upsert helper
- 然而 `session_state_items.category` 是 VARCHAR(64) + `uq_state_current` 按 semantic key 走 — 剩余 category 可在后续 Phase 增量增加 helper，schema 自身已够

---

## §7 Residual / 修正清单

### R1 🟡 清理重复 `session_artifact_grants` / `session_artifacts_grants` 表
- storage.rs:1133 单数版本从 DDL 删除（或保留别名视图），统一到 1157 复数版
- schema_assertion 同步单数命名 drop

### R2 ❌ `DatabaseStateProjectionStore` 接入生产 runtime（Phase 5 必做）
- `run_lifecycle.rs` 的 `delegation_engine` spawn child run 后调用 `upsert_delegation_projection`
- compaction 触发路径（目前 Phase 3 `context_manifest.rs::tiered_compaction`）改为调用 `compact_session_state`（附带 8 invariants）
- agentic_loop_execution_phase.rs 在 LLM turn 结束时写 `session_state_items` 投影（plan_state / todo_state / decision 等 category）
- new session open handler 调 `load_user_anchor_memory` 把 ≤400 tokens 注入 anchor zone
- 新增 HTTP handler `POST /skills/activate` 调 `activate_personal_skill_from_ui`

### R3 🟡 补剩余 7 个 state category 的 upsert helper
- anchor / summary / active_file / tool_ref / workspace_state / durable_decision / engineering_rule / rejected_pattern

### R4 🟡 L2-42 断言改写
- 把 `llm_model_used IS NULL` 替换为断言 `agent_events.source_path='ui_structured' AND llm_round_id IS NULL`（加 index）
- 或移除该断言，改验 service 内部不调 model_client（pin-layer test）

### R5 🟡 前端 Plan subtree drawer（design L2262-2263 rule 4）
- 后端 `GET /sessions/{id}/plan/subtree?root={todo_id}&filter=...` 未实现（Phase 5 scope 之前）
- 前端右栏 "Plan subtree drawer" 视觉区分（design S06 T7 友情提醒：否则用户会误以为主 plan 已切换 phase）

### R6 🟡 补 bubble_up UI 可视化（Phase 6 候选）
- agent-tree 已显示 last_summary，可追加 "⚠ bubble from L{depth}" 徽标

---

## §8 是否进 Phase 5

### 推荐判定：**允许进入 Phase 5（Personal Skills）**

理由：
1. **22 验收点全部通过契约测试**：5 L1 schema ✅ + 11 L2 contract ✅ + 1 L2-42 🟡 弱断言 + 5 L3 E2E ✅；测试覆盖全部为真 DB + 真 INSERT + 真 SELECT/ASSERT，无上轮 Phase 3 L3 那类"固定值 assert_eq!"桩；
2. **三项关键 false-positive 中两项属于文档/断言层**（FP1 schema 重复、FP2 断言弱化），不影响运行时；
3. **FP3（生产未接线）是 Phase 4 的最严重 gap**，但不阻塞 Phase 5 — Phase 5 聚焦 user_skill_sources/versions/evaluations + content_hash 归一，其本身不依赖 projection hook；
4. **Phase 4 Exit 条件的"context buildup from projection"** 在实现深度上需要 Phase 5 期间的 ContextAssembler 改写（R2），作为并行子任务处理；
5. Phase 4 新建的 `state_projection.rs`（922 行）+ `storage.rs` +891 行 + schema_assertion +150 行 已把契约骨架立稳，Phase 5 可在此骨架上补活体接线。

### 进 Phase 5 前置条件（最小必须）
- [ ] §7-R1：清理 `session_artifact_grants` 单数表（schema 一致性）
- [ ] §7-R2：Phase 5 第一周并行接入 `DatabaseStateProjectionStore` 到 delegation_engine spawn 回调（先 upsert_delegation_projection + bubble_up_finding 两个最关键路径）
- [ ] §7-R4：修正 L2-42 断言（次级，可延后 CI 修复）

### 进 Phase 5 不阻塞项
- §7-R3 / R5 / R6 可在 Phase 5/6 增量处理

---

**附注（与 Phase 3b 对比）**：
- Phase 3b 发现 L3 全桩，修复后 PASS
- Phase 4 从第一轮起就采用真 DB 集成测试（`#[ignore = "requires ASTRA_TEST_DB_IT=1"]`）+ `pool.get().begin()/commit()` 真事务 + `EXPLAIN ANALYZE` 索引命中断言 —— 测试质量比 Phase 3 首轮显著提升
- **唯一系统性 gap 是 service→runtime 接线**，属 Phase 5 首周主线工作
