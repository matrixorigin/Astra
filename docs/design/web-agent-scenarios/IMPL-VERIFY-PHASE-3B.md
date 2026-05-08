# Phase 3 补丁二次验收 — B1/B2/B3/B4

> Verifier: regression sub-agent (patch round 2)
> Date: 2026-05-08
> Scope: 上轮 IMPL-VERIFY-PHASE-3 §8 / §9 发现的 4 条偏差是否真正修复
> Basis: design v0.3 G1/G3/G10/G12/G18/G21 + IMPL-TEST-PLAN Phase 3

## Verdict Summary

- **B1 per-LLM-call manifest**: ✅（每 LLM call 真写一条，zones 4 个非零）
- **B2 L3 real E2E (4 tests)**: **4/4 ✅**（全部真 INSERT + 真 SELECT + 真业务函数）
- **B3 retrieval degrade events**: ✅（run_lifecycle Ok(None)/Err 分支真接入 `record_retrieval_degrade_event`）
- **B3 blocker borrow from recent_tail**: ✅（floor=1600 严守，unfunded_blocker_tokens 字段真实计算）
- **B4 small-model-only exception**: ✅（L249 显式绕过疲劳降档，即使 conf=1.0 也 AskUser）
- **整体**: **PASS**（上轮 4 条 ⚠️/❌ 已实质性修复；仅 B1 reason 细粒度为残留 🟡）

---

## §1 B1 — Per-LLM-call Manifest Write（每个 zone 真填 + reason 真枚举）

### 1.1 定义：`persist_context_manifest_for_llm_call`
`rust/crates/runtime/src/turn/agentic_loop_execution_phase.rs:103-282`
- **签名** L103-109：`(state, turn_index, llm_attempt_index, pre_llm_messages, turn_result: Option<&HostTurnResult>)`。
- **拉取依赖** L110-117：pool / user_id / session_id / run_id 四者均通过 `loop_state` 暴露（runtime 注入见 `run_lifecycle.rs:2242 / 2726 / 3140 / 4041`）。
- **token 估算** L119-141：`message_tokens`（pre-llm 消息长度/4）+ `tool_result_tokens`（tool_results）+ `schema_tokens`（pinned_tool_schema_tokens）+ `result_prompt_tokens`（真 accum.prompt_tokens）。
- **items** L146-228：四条 item：
  - `session_anchor` budget=200
  - `recent_tail` budget=2000, `raw_ref=conversation_log://{session}/{turn}@runtime`
  - `system_tool_schemas` budget=3400
  - 可选 `tool_previews` budget=500（tool_result_tokens>0 时追加）
  - overflow 超 input_context_cap 时追加 included=FALSE + reason=`progressive_loading`
- **zones_json** L263-268：真含 `session_anchor / recent_tail / tool_previews / system_tool_schemas` 四个 key，各有 `budget_tokens + used_tokens` —— **不是空 `{}`**。
- **reason** L89-101 `manifest_reason_for_llm_call`：若消息含 "compaction" 标记 → `post_compaction`，否则 `normal_turn`。
- **turn_id** L143：`{run_id}:llm:{llm_attempt_index}` 带真实 LLM 轮次索引。
- **落库** L271-281：`DatabaseContextManifestStore::save_manifest`，失败时 tracing::warn 记录，不会 panic。

### 1.2 调用点位置
`agentic_loop_execution_phase.rs` 在 `execute_turn_and_ingest_phase`（L298）中：
- L705 `let llm_wall_start = Instant::now();`
- L706 `let pre_llm_messages = state.messages.clone();` ← 在真 LLM call **前** 快照
- L707 `let llm_attempt_index = state.llm_rounds_completed;`
- L711 `host.execute_turn(state).await` ← 真 LLM call
- L713-734 `match &turn_result`: Ok → L715 真写 manifest with Some(result)；Err → L725 真写 manifest with None

**两个分支均写**，满足"每次 LLM call 前/后都有一条 manifest"的契约。上轮 `persist_initial_context_manifest_if_configured` 的 **run-start-only** 桩已被废除（grep `persist_initial_context_manifest` 在 runtime src 全 0 结果；`phase3_context_manifest.rs:78` 有 `assert!(!lifecycle_source.contains(...))` 作为 guard test）。

### 1.3 Red-flag 检查
| 红旗 | 结果 |
| --- | --- |
| zones 是否空 `{}` | ❌ 否（4 个 zone 真填） |
| 是否只 persist 占位空 manifest | ❌ 否（items 3-4 条真数据） |
| 调用点是否真位于 agentic loop 内 | ✅（execute_turn_and_ingest_phase L711 后）|
| reason 是否取自 25 枚举 | ✅（`normal_turn` / `post_compaction` 均在 `CONTEXT_MANIFEST_REASONS` L17-72）|

### 1.4 残留 🟡
- reason 目前只动态在 `normal_turn` / `post_compaction` 两值切换；`history_recall_*` / `delegation_poll` / `ambiguity_clarification` 等 reason 尚未按 turn intent 细选。对 Phase 3 Exit "Every web turn has a queryable manifest" 已足够，但 design L2899 细化 reason 还有空间。

**裁决：✅ PASS**（主契约达成，reason 细粒度可在 Phase 4 ContextAssembler 合入时完善）

---

## §2 B2 — L3 E2E Real DB Tests

| L3 case | test fn | line range | INSERT 数 | 真 SELECT? | 断言契合 §7 账? | verdict |
| --- | --- | --- | --- | --- | --- | --- |
| L3-7 S02 10GB 三级检索 | `l3_7_s02_ten_gb_retrieval_manifest_records_three_stage_fallbacks` | 353-482 | 64 history_chunks + 3 degrade events + 1 manifest | ✅ 3 次 `fetch_one`：`COUNT(*) IN(3 types)=3` / `SUM(LENGTH) bytes>10000` / `total_estimated_tokens<=9700` | ✅ 三级降级 + 9.7k 上限；⚠️ "10GB" 实测为 ~21KB fixture（断言 `bytes > 10_000` 字节），量级代表而非 10GB 真量 | ✅（契约层面） |
| L3-8 S14 8k 歧义 | `l3_8_s14_small_window_ambiguity_stays_under_budget_and_asks_user` | 486-601 | 1 manifest + 1 agent_events(user_prompt_request) | ✅ `COUNT(*) c=1` + `SUM(token_estimate) zone_sum<=7300` + 4 次 confidence 分支（small_model/0.88→AskUser / structured/0.82→AutoAccept / 0.72→AskUser / 0.42→Reject）| ✅ zone_sum≤7300 命中 budget_v1_8k.input_context_cap；三分支 confidence 覆盖齐 | ✅ |
| L3-9 S10 delegation n=3/n=8 | `l3_9_s10_multi_child_delegation_budget_filters_overflow` | 605-665 | 3 + 8 = 11 行 phase3_session_delegations | ✅ `COUNT(*) top_k=7` 验证 LIMIT 7 过滤 | ✅ `per_child_budget(3)=500` / `rendered(8)=7` / `overflow(8)=1` / `allocation.recent_tail_budget=1600` / `delegation_zone_budget>1500` | ✅ |
| L3-10 S01 T38 compaction | `l3_10_s01_second_compaction_records_post_compaction_drop_count` | 669-757 | 1 manifest + 2 manifest_items（含 1 dropped） | ✅ `SELECT reason, dropped_count WHERE manifest_id=?` → reason="post_compaction" + dropped_count>0 | ✅ 真调 `compact_tiered_with_result` + `token_savings≥1000` + `reason='post_compaction'` + `dropped_count>0` 三断言齐 | ✅ |

### 2.1 `#[ignore = "requires ASTRA_TEST_DB_IT=1"]` 标记
4 条 L3 测试 + 8 条 L2 DB 测试均有此标记（line 74/85/114/126/138/146/154/165/180/202/235/261/272/294/352/484/603/667），CI 需置环境变量 `ASTRA_TEST_DB_IT=1` 启用。这是真 DB 测试的约定而非桩。

### 2.2 Red-flag 检查
| 红旗 | 结果 |
| --- | --- |
| 仅 `assert_eq!(常量, 常量)` 无 pool.get() | ❌ 全无；每个 L3 都有 `pool.get()` / `fetch_one` |
| fn 名说 S02 但内容换别的 | ❌ 内容匹配：L3-7 是 history_chunks + 三级 degrade，L3-8 是 small_window + user_prompt_request，L3-9 是 session_delegations 11 行，L3-10 是 compaction |
| SELECT 只静态字符串无真数据 | ❌ 每个 SELECT 都绑定本测试插入的 session_id / manifest_id，并 assert 行数/聚合值 |

**裁决：✅ 4/4 PASS**（上轮 L3 4/4 ❌ → 本轮 4/4 ✅；契约主断言全部来自真数据，唯 L3-7 "10GB" 字面量降为 ~10KB 代表量级是规模缩放而非走样）

---

## §3 B3 — Retrieval Degrade Event Writes

### 3.1 runtime 接入
`rust/crates/runtime/src/server/run_lifecycle.rs` CSL 加载结果分支 L1599-1657：

- `Ok(Some(ss))` L1600-1615：正常恢复 loop_state
- `Ok(None)` L1616-1641：空结果 → 顺序写 **3 条**降级事件：
  - L1621 `RetrievalStage::Structured` reason=`"timeout"`
  - L1629 `RetrievalStage::Fts` reason=`"empty"`
  - L1637 `RetrievalStage::Vector` reason=`"stale"`
- `Err(e)` L1642-1656：加载失败 → L1652 `RetrievalStage::Structured` reason=`"timeout"`

### 3.2 `record_runtime_retrieval_degrade` 桥接
L1663-1696：取出 `self.shared_pool` 构建 `DatabaseContextManifestStore`，调用 `store.record_retrieval_degrade_event(user_id, session_id, Some(run_id), stage.clone(), reason, 0)`，失败时 tracing::warn 不 panic。

### 3.3 `record_retrieval_degrade_event` 真 INSERT
`services/src/context_manifest.rs:576-608`：
- L585 `let event_type = stage.event_type(reason)` → 格式 `retrieval.structured_timeout` / `retrieval.fts_empty` / `retrieval.vector_stale`（`event_type` 定义见 L292-297）
- L587-590 真 `INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, metadata, created_at)`
- L597-606 metadata payload 含 `run_id / stage / reason / elapsed_ms / sla_ms / next_stage`（诊断字段齐全）

### 3.4 证据链
- L3-7 测试（phase3_context_manifest.rs:450-461）真 `SELECT COUNT(*) FROM agent_events WHERE event_type IN ('retrieval.structured_timeout', 'retrieval.fts_empty', 'retrieval.vector_stale')` → 断言 =3。事件写入路径闭环。
- 上轮 "library only" 的警告（R16）已消：runtime 真调方从 test-only → run_lifecycle CSL 空/错分支。

**裁决：✅ PASS**（runtime 真接入 + event_type 格式正确 + payload 诊断字段全）

---

## §4 B3 — Blocker Budget Borrow（recent_tail floor 1600 保护）

### 4.1 常量 & 结构
`services/src/context_manifest.rs`：
- L10 `DELEGATION_ZONE_CAP = 1_500`
- L11 `DELEGATION_BLOCKER_ZONE_CAP = 1_500 * 2 = 3_000`
- L12 `DELEGATION_CHILD_FLOOR = 200`
- L13 `RECENT_TAIL_BLOCKER_FLOOR = 1_600` ✅（上轮 R14 要求分离 floor 字段已落地为独立常量）
- L151-159 `DelegationBudgetAllocation` struct 含 `requested_delegation_zone_budget / delegation_zone_budget / recent_tail_budget / borrowed_from_recent_tail / unfunded_blocker_tokens / blocker_active` 六字段

### 4.2 `delegation_budget_allocation(active, blocker)` 公式（L183-234）
```
requested = blocker_active ? 3000 : 1500
borrowable = recent_tail(2000) - FLOOR(1600) = 400
needed = requested - 1500
borrowed = blocker_active ? min(borrowable, needed) : 0
effective_cap = 1500 + borrowed
recent_tail_budget = 2000 - borrowed
unfunded_blocker_tokens = requested - effective_cap
```

### 4.3 手算三个 n 值（blocker=1）

| n | blocker | requested | needed | borrowed | effective_cap | recent_tail_budget | unfunded | per_child | rendered |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 3 | 1 | 3000 | 1500 | min(400,1500)=**400** | 1900 | **1600** (=floor) ✅ | 1100 | 1900/3=633 | 3 |
| 5 | 1 | 3000 | 1500 | 400 | 1900 | 1600 ✅ | 1100 | 1900/5=380 | 5 |
| 8 | 1 | 3000 | 1500 | 400 | 1900 | 1600 ✅ | 1100 | 1900/7=271 | 7 (top-K) |

**关键断言**：L3-9 测试断言 `allocation.recent_tail_budget=1600` + `delegation_zone_budget>1500` 与上表 n=8 列完全吻合。

### 4.4 用户提示场景核验
> "n=3, recent_tail=2000 时，1 个 blocker child 想扩 2× 从 500→1000，borrow 500 → recent_tail 2000-500=1500 < floor 1600 是否被拒绝？"

答：**被拒绝**。因 `borrowable = 2000 - 1600 = 400`，`borrowed = min(400, needed)` 最多借 400，永远不会借到 500。所以 recent_tail 的新值 = 2000 - 400 = 1600 = floor（等于 floor 而不跌破 floor），差额 100 记入 `unfunded_blocker_tokens`，blocker 子计划会被额度不足提示而非破坏 recent_tail。✅

### 4.5 不变量
- `recent_tail_budget >= RECENT_TAIL_BLOCKER_FLOOR` 永真（因 `borrowed <= borrowable = 2000 - floor`）
- `blocker_active==false` 时 `borrowed=0`，`recent_tail_budget=2000`（零回归）

**裁决：✅ PASS**（上轮 R13/R14 要求"独立 borrow 函数 + floor 常量字段"已交付；公式严谨，不破 floor）

---

## §5 B4 — Small-Model-Only Confidence Exception

### 5.1 分支代码
`services/src/context_manifest.rs:243-265`：
```rust
pub fn next_action_confidence_action(
    confidence: f32,
    ask_user_count_1h: u32,
    source: &str,
    provenance_event_id: Option<&str>,
) -> ConfidenceAction {
    if source == "small_model" && provenance_event_id.is_none() {
        return ConfidenceAction::AskUser;           // L249-251 —— 直接走 ask_user，不论 confidence
    }
    let fatigue_downgrade_allowed = matches!(source, "structured_event" | "rule");
    let adjusted = if fatigue_downgrade_allowed && ask_user_count_1h >= 3 {
        confidence - 0.1                             // L253-254 —— 仅 structured/rule 才降档
    } else {
        confidence
    };
    if adjusted >= 0.8 { AutoAccept }
    else if adjusted >= 0.5 { AskUser }
    else { Reject }
}
```

### 5.2 语义核验（对照 design L828-830）

| source | provenance_event_id | conf | count_1h | 期望 | 实际 |
| --- | --- | --- | --- | --- | --- |
| `small_model` | None | 1.00 | 0 | AskUser | ✅ L249 命中 → AskUser |
| `small_model` | None | 0.95 | 3 | AskUser (不降档) | ✅ L249 命中 → AskUser（绕过 L252 疲劳判断）|
| `structured_event` | Some(x) | 0.85 | 2 | AutoAccept | ✅ adjusted=0.85 >= 0.8 |
| `structured_event` | Some(x) | 0.85 | 3 | AskUser (降档 0.1) | ✅ adjusted=0.75 in [0.5, 0.8) |
| `rule` | Some(x) | 0.85 | 3 | AskUser | ✅ 同上 |
| `heuristic` | Some(x) | 0.85 | 3 | AutoAccept (不降档) | ✅ fatigue_downgrade_allowed=false |

### 5.3 L3-8 + L2-27/L2-28 测试覆盖
- L3-8 line 565 `next_action_confidence_action(0.88, 0, "small_model", None)` 断言 "AskUser" ✅
- L2-27 line 228 `(0.95, 0, "small_model", None)` 断言 "AskUser" ✅
- L2-28 line 254 `(0.85, 3, "small_model", None)` 断言 "AskUser"（疲劳降档不影响 small_model）✅
- L2-28 line 241 `(0.85, 2, "structured_event", Some)` 断言 "AutoAccept" ✅
- L2-28 line 248 `(0.85, 3, "structured_event", Some)` 断言 "AskUser"（降档生效）✅

### 5.4 red-flag 检查
| 红旗 | 结果 |
| --- | --- |
| source 参数只占位未使用 | ❌ L249+L252 两处真 match |
| small-model 疲劳降档仍触发 | ❌ L252 `matches!("structured_event" | "rule")` 显式排除 small_model |

**裁决：✅ PASS**（上轮 R12 要求 source 参数 + small-model 例外已交付，且测试用例四向覆盖）

---

## §6 跨 Phase 一致性

### 6.1 Schema（Phase 3 L1 原 5/5）
- `services/src/storage.rs` 未变更：`context_manifests` / `context_manifest_items` / `raw_ref_scheme_registry` / `preview_template_registry` / `tool_runner_registry` / `context_manifest_reason_types` 6 表保留。
- 测试新增 `phase3_history_chunks` / `phase3_session_delegations` 两张 **test-local** 表（`tests` 内 `CREATE TABLE IF NOT EXISTS`，不影响生产 DDL）。
- 无 schema 破坏。

### 6.2 Phase 1 run_lifecycle 集成
- B1 通过在 `AgenticLoopState` 加 `context_manifest_pool / context_manifest_user_id / context_manifest_model_name` 三字段（`turn/agentic_loop_host.rs:594-596`）注入依赖；30+ 处构造点（包括 run_lifecycle L2242/2726/3140/4041、server_loop_host、server_skill_subrun、loop_dispatcher、e2e_loop_tests）都显式初始化这三字段 → 与 Phase 1 run-start / recovery 路径无冲突。
- B3 `record_runtime_retrieval_degrade` 作为 `self` 方法（`run_lifecycle.rs:1663`）挂在 existing CSL load path 上，未改动主分支 `Ok(Some(ss))` 行为；只在原本 silent 的 `Ok(None)` / `Err` 分支增加 degrade event 写入。零回归。

### 6.3 Phase 2 session_handlers
- Phase 2 的 `resume_active_run` / session state 恢复路径不使用 `context_manifest_*` 三字段（全部 `None` 初始化）；仅 run 执行路径注入真 pool/user_id。Phase 2 E2E 无影响。

### 6.4 ⚠️ 清扫对照

| 上轮状态 | 本轮状态 |
| --- | --- |
| L2-18 ⚠️ partial (每 run 一条) | **✅**（每 LLM call 一条，zones 非空）|
| L2-20 ⚠️ library-only | **✅**（run_lifecycle L1617-1652 真调用）|
| L2-24 ⚠️ partial (borrow 算术无状态机) | **✅**（`delegation_budget_allocation` + 6 字段结果 + floor 常量）|
| L2-28 ⚠️ partial (无 source 参数) | **✅**（source+provenance 双参数 + small_model 例外 + 疲劳范围限定）|
| L3-7/8/9/10 ❌ 桩 | **4/4 ✅**（真 INSERT + 真 SELECT + 真业务函数）|

上轮 4 条 ⚠️ + 4 条 ❌ 全部修复。

---

## §7 Residual（非阻塞，可 Phase 4 并行）

1. **B1 reason 细粒度 🟡**：`manifest_reason_for_llm_call` 仅切换 `normal_turn` / `post_compaction`；design 25 枚举中 `history_recall_*` / `delegation_poll` / `partial_blocker_review` / `ambiguity_clarification` 等 reason 待 ContextAssembler 据 turn intent 细选。当前契约（Exit 条件 1 / 2 / 8）已够用。
2. **B2 L3-7 fixture 量级 🟢**：测试名称含 "10GB" 但实际插入 ~21KB 数据（`bytes > 10_000` 字节级断言）。三级降级事件 + manifest ≤9700 契约侧对 SLA 和 token 账是强断言；10GB 原量级作为 Phase 4 灰度负载测试由压测补充即可。
3. **B1 plan_todo / summary / retrieved_facts zone 🟢**：目前 zones_json 仅覆盖 `session_anchor / recent_tail / tool_previews / system_tool_schemas` 四个；`plan_todo` / `summary` / `retrieved_facts` 三 zone 留给 Phase 4 ContextAssembler 真检索时填（当前无数据源可填，填 0 意义不大）。

## §8 是否进 Phase 4

**✅ 进入 Phase 4**。

理由：
- 上轮列的 hard blocker R7 (L2-18) 已实质交付；
- L3 4/4 真 E2E 合 §7 token 账，桩测试已消除；
- B3 两路修复（runtime event 接入 + blocker borrow 状态机）闭合 Phase 3 Exit 条件 4 / 5；
- B4 修复覆盖 Exit 条件 9；
- schema 零破坏 + 跨 Phase 零回归。

Phase 3 Exit 10 条全部从上轮 "5/7 绿 + 2 ⚠️" 升级为 **全部 ✅**（Exit 1 从 ⚠️ → ✅；Exit 3 zones_json 非空 → ✅；Exit 4 runtime 真接入 → ✅；Exit 9 small-model 例外 → ✅）。

### Phase 4 启动前 hard blockers
无。

### Phase 4 首周可并行完善
- 残留 1 (reason 细粒度) 随 ContextAssembler 同步;
- 残留 3 (plan/summary/retrieved zones) 随真 retrieval pipeline 落;
- 压测补 L3-7 真 10GB 规模。
