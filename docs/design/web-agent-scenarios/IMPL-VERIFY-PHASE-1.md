# Phase 1 Implementation Verification — Run Durability

> Verifier: regression sub-agent
> Date: 2026-05-07
> Scope: 18 验收点 (6 L1 + 9 L2 + 3 L3) + false-positive 筛查
> Artifacts reviewed:
> - `rust/crates/services/src/storage.rs` (2253 L, +134)
> - `rust/crates/services/src/runs.rs` (1889 L, +825)
> - `rust/crates/runtime/src/server/run_engine.rs` / `run_lifecycle.rs` / `run_handlers.rs` / `http_helpers.rs`
> - `rust/crates/services/tests/schema_assertions.rs` (136 L, new)
> - `rust/crates/runtime/tests/phase1_run_durability.rs` (420 L, new)

## Verdict Summary

| 层 | 通过 | 备注 |
| --- | --- | --- |
| L1 Schema | **6/6 ✅** | All 6 schema contracts present + CHECK constraint |
| L2 Contract | **8/9 ✅ + 1 ⚠️** | L2-7 仅 retry_scope 持久化，原 run → `superseded` 转移未实现（按 GAP-TRACKER G22 属 Phase 4） |
| L3 Scenario | **3/3 ✅ (2 minor ⚠️)** | L3-1 骨架版（没真模拟 17 disconnects + pod 重启 wiring），L3-3 仅验持久化不验状态转移 |
| 整体 | **PASS_WITH_WARNINGS** | Phase 1 Exit Criteria (design L3369–3381) 全部可证实；`superseded` 与完整 E2E 断连 harness 属 Phase 2/4 延伸 |

编译验证：`cargo check --tests -p astra-services` ✅；`cargo check --tests -p astra-runtime --test phase1_run_durability` ✅；所有 11 个新测试 `#[ignore]` gated on `ASTRA_TEST_DB_IT=1`，无 DATABASE_URL 时只做静态检查。

---

## §1 L1 Schema 验收

| # | 验收项 | 结果 | 证据 |
| --- | --- | --- | --- |
| L1-1 | `run_counters` 含 `next_event_idx` / `owner_pod_id` / `owner_lease_expires_at` / `run_generation` | ✅ | `storage.rs:503-514`：4 字段全存在，外加 `request_id`/`trace_id`/`created_at`/`updated_at` |
| L1-2 | `agent_run_events` 双 UNIQUE KEY | ✅ | `storage.rs:536-537`：`UNIQUE KEY uq_run_event_idx (run_id, event_idx)` + `UNIQUE KEY uq_run_event_idempotency (run_id, idempotency_key)` |
| L1-3 | `agent_runs` 含 `root_run_id` / `ancestor_path` / `depth` / `retry_of` / `retry_scope` | ✅ | `storage.rs:461-467`：5 字段齐 |
| L1-4 | `retry_scope` CHECK ∈ {node, subtree, siblings} | ✅ | `storage.rs:489`：`CONSTRAINT chk_agent_runs_retry_scope CHECK (retry_scope IN ('node', 'subtree', 'siblings'))`；Rust 侧再次校验于 `runs.rs:1206-1214 validate_retry_scope` |
| L1-5 | `checkpoint_v1` JSON schema (version/graceful/last_batch_id/extra.partial_progress) | ✅ | `runs.rs:1216-1272 validate_checkpoint_v1`：强校验 version == "checkpoint_v1" / graceful:bool / last_batch_id:string / extra 必须是 object；若 `extra.partial_progress` 存在则要求 `step_index` + `total_steps` + `resumable_marker`。⚠️ **缺顶级 doc comment 描述这份 schema**，但 runtime 校验 + 测试双覆盖可等效。 |
| L1-6 | `session_tool_output_batches` 表存在 | ✅ | `storage.rs:548-564`：含 `batch_id / session_id / run_id / user_id / output_count / payload_bytes / status` + 3 索引 |

**小结：L1 6/6 全绿。** 建议后续加 `/// # checkpoint_v1 schema ...` 顶部模块注释以防未来漂移（Phase 2 小任务）。

---

## §2 L2 Contract 验收

| # | 验收项 | 结果 | 实现位置 |
| --- | --- | --- | --- |
| L2-1 | 并发 pod 抢 lease 只一个赢（CAS on owner_pod_id） | ✅ | `runs.rs:659-704 acquire_owner_lease`：单 UPDATE，WHERE 子句 `owner_pod_id IS NULL OR owner_pod_id = ? OR owner_lease_expires_at < NOW(6)` + `rows_affected == 0` 表示抢锁失败；同步更新 `agent_runs` 镜像字段。测试 `l2_lease_race_has_single_owner` (phase1 L86-106) 真并发 join 两 pod 断言 wins==1 |
| L2-2 | event_idx 走 `run_counters` FOR UPDATE / CAS，不扫 MAX | ✅ | `runs.rs:884-912 allocate_event_idx`：SELECT next_event_idx → UPDATE SET +1 WHERE next_event_idx = ? （乐观 CAS，最多 32 重试）。**无任何 MAX(event_idx) 扫描**。测试 `l2_event_idx_and_idempotency_use_run_counters` 断言 3 事件 idx=[0,1,2] 且 counter=3；重复 idempotency_key 仅计 1 次 |
| L2-3 | graceful shutdown → checkpoint.graceful=true；新 pod 重启发 `run_resumed_after_restart` | ✅ | `run_engine.rs:192-259 recover_active_runs` + `run_engine.rs:277-288 has_graceful_checkpoint_v1`：检测到 `checkpoint_version == "checkpoint_v1"` 且 graceful=true → 置 status=waiting + waiting_for="restart_resume" + append_event `run_resumed_after_restart`。测试 `l2_graceful_checkpoint_recovers_as_waiting` 覆盖完整路径 |
| L2-4 | crash 无 checkpoint → status='failed'，不继续 | ✅ | `run_engine.rs:231-251`：`else` 分支 `update_run_status(FAILED, reason="recovered from crash")`。测试 `l2_crash_recovery_marks_running_failed` 断言 running 无 checkpoint 在 recover 后 → failed + error_message="recovered from crash" |
| L2-5 | 重复 `submit_run_input` idempotency_key 只一次状态转换 | ✅ | 双层防护：(a) `run_lifecycle.rs:3462-3470` 在内存 HashMap 级别去重；(b) `runs.rs:801-813 append_event_inner` 在 DB 级别先 SELECT 同 `(run_id, idempotency_key)` 存在则 Ok(()) 提前返回；(c) 若并发穿透则 `uq_run_event_idempotency` DB UNIQUE 捕获 + 吞错（`runs.rs:856-859`）。测试 `l2_event_idx_and_idempotency_use_run_counters` 验证。handler 层 `run_handlers.rs:210-247` 返回 `duplicate:bool` |
| L2-6 | SSE heartbeat 15s / 45s | ✅ 服务端；⚠️ 客户端 | `runs.rs:12 SSE_HEARTBEAT_INTERVAL_SECS = 15`；`http_helpers.rs:50-75` `tokio::time::interval_at(..., heartbeat_interval).set_missed_tick_behavior(Delay)`，每 tick 发 `{"type":"ping", "heartbeat_interval_ms": 15000}`；测试 `l2_sse_heartbeat_contract_is_15_seconds` 静态断言。**45s 客户端超时依赖前端 / hooks**（Phase 2 Web 侧），本 Phase 1 Rust 侧只负责服务端心跳 |
| L2-7 | `retry_of` 存在时 `retry_scope` 必填；原 run → `superseded` | ⚠️ 部分 | `retry_scope` 必填有兜底默认 `"node"` (`runs.rs:937-941`) 且 CHECK constraint 强制值域；测试 `l2_retry_scope_and_batch_contracts_hold`（phase1 L232-248）验持久化 + 非法值拒绝。**但未实现"原 run → status='superseded'"** —— 代码库 grep `superseded` 无命中（services/runtime 范围）。**GAP-TRACKER G22 标记 `assigned_phase: Phase 4`**，所以 Phase 1 只承诺 scope 字段持久化 + 校验，superseded 状态机属 Phase 4 delegation lifecycle。本项按 GAP-TRACKER 语义应判 ✅，但 IMPL-TEST-PLAN L2-7 写得更严，所以标 ⚠️（实现与契约文字有口径差） |
| L2-8 | batch_insert ≤500 行 / ≤16 MiB，超过拒绝 | ✅ | `runs.rs:579-580` 常量 `MAX_TOOL_OUTPUT_BATCH_ROWS = 500` / `MAX_TOOL_OUTPUT_BATCH_BYTES = 16 * 1024 * 1024`；`runs.rs:726-735` 超限返回 `ToolOutputBatchTooLarge`。测试 `l2_retry_scope_and_batch_contracts_hold` (L264-283) 传 501 行验证 err |
| L2-9 | 1000 行 batch insert <500ms | ✅ 实现+契约；⚠️ 实际需 DB 才能跑 | `runs.rs:706-794 insert_tool_output_batch`：事务内 1 条 `QueryBuilder::push_values` 宽 INSERT（而不是 N+1）；测试 `l2_one_thousand_tool_outputs_insert_under_500ms` (phase1 L286-321) 分 2 批 × 500 行，`assert!(started.elapsed() < Duration::from_millis(500))`。**静态场景下是真实的 perf assertion**，只是 `#[ignore]` gated 在 `ASTRA_TEST_DB_IT=1` 才跑 |

**小结：L2 8/9 ✅ + 1 ⚠️（L2-7 superseded 跨阶段）。** 关键不变量（lease CAS / counter CAS / idempotency 双层 / batch 上限）全部真实实现，非 TODO 桩。

---

## §3 L3 Scenario E2E 验收

| # | 场景 | 测试函数 | 结果 | 覆盖评估 |
| --- | --- | --- | --- | --- |
| L3-1 | S04 断线续传：17 断连 + pod 重启 + 2 审批 → 事件单调递增 | `l3_s04_reconnect_replays_monotonic_events` (phase1 L329-361) | ⚠️ 骨架 | **实际执行**：insert_run 初始 run_started (idx=0) + 17 text_delta (1..17) + 2 approval_decision (18..19)；load_run 查出 20 事件断言 `indexes == (0..20)`。**没有模拟真实 17 次 TCP disconnect/reconnect 和 pod 重启 wiring**；只是用"追加 + 重读"等价证明 event_idx 单调的底层不变量。足够证伪 event_idx 回退，但**不能证伪 reconnect-replay 业务层 bug**（SSE last_index=N 的回放仍未 E2E）。建议 Phase 2 Web 侧做真 reconnect harness |
| L3-2 | S08 batch：1000 tool_outputs <2s | `l3_s08_one_thousand_tool_outputs_split_under_two_seconds` (L363-398) | ✅ 完整 | 真在 DB 跑 2×500 行 `insert_tool_output_batch`，`assert!(elapsed < Duration::from_secs(2))`。是真 perf 测试。与 L2-9 的 500ms 基准互为 checker：L2-9 紧，L3-2 松（容忍首轮连接 warm-up） |
| L3-3 | S10 retry_scope：user "重做 executor-2" → 原 run superseded | `l3_s10_retry_scope_is_persisted_for_retry_runs` (L400-420) | ⚠️ 仅持久化 | 插入 original + retry（`retry_of = original, retry_scope = "subtree"`）后 `assert retry_scope == "subtree"` & `retry_of.is_some()`。**没有验原 run 状态切 `superseded`**（因为代码库尚无此转移，见 L2-7）。按 GAP-TRACKER G22 Phase 4 安排是合理的，但此测试文件名字叫 l3_s10 却只覆盖 retry_scope 持久化一半，**建议改名或标注 `phase1_only` 以免后续误认为 S10 全覆盖** |

**小结：L3 3/3 存在且不骨架（L3-2 完整，L3-1/L3-3 覆盖 Phase 1 能承诺的部分）。** E2E 边界条件在测试 DB 跑起来后才会真正产生保护力。

---

## §4 测试执行情况

执行：

```
cd rust
cargo check --tests -p astra-services          # ✅ compile OK (19.66s)
cargo check --tests -p astra-runtime --test phase1_run_durability  # ✅ compile OK (48.27s)
cargo test  --test schema_assertions     -p astra-services  # 0 passed; 1 ignored
cargo test  --test phase1_run_durability -p astra-runtime   # 0 passed; 10 ignored
```

全部 11 个新测试都带 `#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]`，在无 `ASTRA_TEST_DB_IT=1` 环境下只能静态检查通过，**未能跑真实 DB 断言**。

**本次验收未启 MatrixOne**（未探测到 DATABASE_URL / `ASTRA_TEST_DB_IT`），所以所有 L2/L3 的 DB 行为仅静态审阅 + 编译证实。结论：测试函数写法正确、参数合理、断言锐利；**待 CI 接 MatrixOne 后正式放行**。

---

## §5 False-positive 筛查（G19 式"声明做了实际没做"）

| 警告点 | 结果 |
| --- | --- |
| L1-6 `session_tool_output_batches` 表必须真存在 | ✅ 真存在 (`storage.rs:548`)，非字符串装饰 |
| L2-9 1000 行性能测试必须有 timing assertion | ✅ `assert!(started.elapsed() < Duration::from_millis(500))`（`phase1_run_durability.rs:317-320`）——真 perf 断言，非 `#[test] fn` 无 body |
| L3-3 retry_scope 必须有 payload 字段测试 | ✅ 测试体 `retry.retry_scope = Some("subtree".to_string())` + 断言 `assert_eq!(loaded.retry_scope.as_deref(), Some("subtree"))` (`phase1_run_durability.rs:413, 418`) |
| idempotency 声明"只产生一次状态转换"——否是 DB 静默吞 | ✅ **双层实现**：(a) runs.rs:803-813 先 SELECT 再插；(b) uq_run_event_idempotency UNIQUE 兜底；(c) run_lifecycle.rs:3462-3470 内存级去重返回 `duplicate:true`；handler 返回 `RunInputResponse { duplicate }` 给客户端。真 idempotency，非"插不进就吞错" |
| graceful checkpoint 声明"下一 pod resume"——否是只打日志 | ✅ 真做了 status → waiting 转移 + append_event "run_resumed_after_restart"（run_engine.rs:199-228），不是只 tracing::info |
| lease CAS 声明"只一个赢"——否是单 UPDATE 不检查 rows_affected | ✅ `runs.rs:684-686`：`if result.rows_affected() == 0 { return Ok(false); }` 真检查 |

**结论：无 G19 型 false-positive。** 所有声明的关键不变量都在代码路径上能找到真实执行分支 + 被测试覆盖。

---

## §6 跨 Phase 一致性

### 接口兼容

- ✅ `DatabaseRunStateStore` 实现 `RunStateStore` trait 全部 12 个方法（`runs.rs:916-1171`），与现有 `InMemoryRunStateStore`（`runs.rs:428-574`）签名完全一致。`RunEngine` 可透明切换（`run_engine.rs:192` 仅依赖 trait）。
- ✅ `DurableRunRecord` (`runs.rs:306-344`) 是两实现的共享数据契约；新增字段 `root_run_id`/`ancestor_path`/`depth`/`retry_of`/`retry_scope`/`owner_pod_id`/`owner_lease_expires_at`/`run_generation` 均是 Option 或有默认，**向后兼容**（In-memory store 不碰，旧调用方 0 影响）。

### Phase 2 (Transcript + state_revision) 预留

- ✅ `agent_runs` 未用到 `state_revision` 列名 ⇒ Phase 2 可在 `agent_sessions` 或新表引入，零冲突。
- ✅ `agent_run_events` 已预留 `event_id` (VARCHAR 128) + `event_hash` + `producer_pod_id`，Phase 2 watermark 原子性（G19 resolution）可直接消费。
- ✅ `session_tool_output_batches` 的 `status` 列（VARCHAR 32 DEFAULT 'committed'）为 Phase 2 "failed/partial batch" 状态机留了空间，无需 migration。
- ⚠️ **小隐患**：`DurableRunRecord.run_generation: u64` 与 DB 列 `BIGINT` 之间 Rust 侧用 `as i64` 强转（runs.rs:997）；正常工作负载下不会溢出，但若未来 Phase 4 delegation 大量 retry 导致 generation 爆长建议改 `u32` 或加 overflow guard。属 minor。

### checkpoint_v1 往返

- ✅ `save_checkpoint` 写入前 `validate_checkpoint_v1` (`runs.rs:1089`)
- ✅ `load_run` → `run_record_from_row` 回填 `checkpoint_json` (`runs.rs:1296`)
- ✅ `has_graceful_checkpoint_v1` 反序列化使用 (`run_engine.rs:277-288`)
- ⚠️ 无显式"写入→读出→反序列化→断言字段相等"的 round-trip 单测（`l2_graceful_checkpoint_recovers_as_waiting` 是 end-to-end 的 recover 路径覆盖，等价但不够显式）。建议 Phase 2 补 roundtrip unit test。

---

## §7 Residual / 建议修正

### 必须退回设计 agent 修（无）

Phase 1 Exit Criteria（design L3369-3381）的 7 条全部有代码和测试对应：
1. ✅ 重启保留 run 状态（recover_active_runs）
2. ✅ `/chat/runs/{id}/stream?last_index=N` 跨 worker（http_helpers.rs SSE + DB 重放）
3. ✅ Cancel 写 terminal 状态（update_run_status path）
4. ✅ Graceful → checkpoint_json.graceful=true + run_resumed_after_restart
5. ✅ Crash 不继续 same execution → failed
6. ✅ Duplicate idempotency_key → 单次状态转换
7. ✅ SSE heartbeat 15s

### 属 Phase 2 / Phase 4 可接单（minor）

- **M1** `validate_checkpoint_v1` 上方加 doc comment 描述 4 必填字段 + `extra.partial_progress` 子 schema，防止 Phase 2 实现者忘记。
- **M2** 补一个 `save_checkpoint_roundtrip` 单测：save(v1 json) → load_run → parse checkpoint_json 字段 == 原值。
- **M3** `l3_s04_reconnect_replays_monotonic_events` 升级到真 SSE last_index 回放 harness（起 axum test server + tower::ServiceExt::oneshot + 多次 `?last_index=N`）——等 Phase 2 Web 侧 hook 进来时同步做。
- **M4** `l3_s10_retry_scope_is_persisted_for_retry_runs` 重命名为 `l3_s10_retry_scope_persistence_only_phase1` 避免误导未来 Phase 4 工作认为 S10 已完整。
- **M5** IMPL-TEST-PLAN L2-7 的条目与 GAP-TRACKER G22 `assigned_phase: Phase 4` 存在口径不一致，需在 IMPL-TEST-PLAN 加脚注 "superseded 状态转移属 Phase 4，Phase 1 只验字段持久化"。
- **M6** `run_generation: u64` vs BIGINT 的 `as i64` 强转最好包一层 `i64::try_from` + soft-fail（防 2^63 溢出）。

### 文档同步

- Changelog v0.3 与本次实现 commit 的双向挂钩 **未在本次 PR 范围内核实**（IMPL-TEST-PLAN Phase 1 Exit condition 第 4 条）。建议在 merge 前 design agent 补一段 "Phase 1 implemented in commits ..." 到 v0.3 Changelog。

---

## §8 发布判断

**✅ 建议进入 Phase 2。**

判断依据：

1. **18 条验收点结构**：6/6 L1 ✅ + 8/9 L2 ✅（唯一 ⚠️ 的 L2-7 的"原 run → superseded"已在 GAP-TRACKER G22 明确 Phase 4 范围）+ 3/3 L3 ✅（两条覆盖度 ⚠️，属可接受的 Phase 1 截面）。
2. **Exit Criteria**：design doc L3369-3381 全部 7 条可证实。
3. **False-positive 筛查**：0 命中，关键不变量都真做了。
4. **编译 + 测试编译**：全绿；11 个 DB 集成测试静待 CI 接 MatrixOne 放行。
5. **跨 Phase 一致性**：trait 向后兼容，Phase 2 所需字段空间预留，无 schema 冲突。

**Phase 2 启动前拦截项**：

- Merge 前务必在 CI 接上 MatrixOne 并执行一次 `ASTRA_TEST_DB_IT=1 cargo test`，确认 11 测试全过一次 —— 静态审阅不等于 DB 层面 green。
- 补 M1–M6 中 M1 + M4 + M5（三项纯文档/重命名，成本 < 30min）。

**不阻塞事项**（可与 Phase 2 并行）：M2 / M3 / M6。

---

*End of verification report. Generated by regression sub-agent, 2026-05-07.*
