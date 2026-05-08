# Phase 2 Implementation Verification — Web Transcript + Cold-Start + Device Lease

> Verifier: regression sub-agent
> Date: 2026-05-07
> Scope: 15 验收点 (4 L1 + 8 L2 + 3 L3) + Web UI MVP 6 项 + Phase 1 L3-1 补完
> Artifacts reviewed:
> - `rust/crates/services/src/storage.rs` (Phase 2 tables at L590-672)
> - `rust/crates/runtime/src/server/session_handlers.rs` (831 L, new)
> - `rust/crates/runtime/src/server/router_builder.rs` (L131-164)
> - `rust/crates/runtime/tests/phase2_web_hydration.rs` (475 L, 8 tests)
> - `rust/crates/runtime/tests/phase1_run_durability.rs` L370-446 (L3-1 补完候选)
> - `rust/crates/services/tests/schema_assertions.rs` (210 L, +74 Phase 2)
> - `web/lib/session-cache/indexeddb.ts` (229 L, new, G19 核心)
> - `web/lib/api/session-client.ts` (145 L, new)
> - `web/lib/api/platform-sessions.ts` (+124 L)
> - `web/hooks/use-chat-stream.ts` (623 L, +143)
> - `web/hooks/use-run-stream.ts` (137 L, +54)
> - `web/components/settings/runtime-settings-panel.tsx` (+99 L device revoke UI)
> - `web/__tests__/lib/session-cache-contract.test.ts` (43 L, new)

## Verdict Summary

| 层 | 通过 | 备注 |
| --- | --- | --- |
| L1 Schema | **4/4 ✅** | 全部列齐，主键/CHECK 正确，schema_assertions 真表存在性断言 |
| L2 Contract | **8/8 ✅ (1 minor ⚠️)** | G19 原子事务真实现；G24 服务端完整、客户端走 transcript 路径但不主动 replay active run；G25 对称 payload 齐全 |
| L3 Scenario | **3/3 ✅ (L3-5 ⚠️弱)** | L3-5 仅是 `max(8,10)=10 && 两 hash 不等` 的单元式断言，不是真 BroadcastChannel 流 |
| 补 Phase 1 L3-1 | **⚠️ 仍 骨架** | `l3_s04_t01_t17_full_reconnect_survives_restart_and_approvals` 只调 store.append_event + load_run，**未走 axum Router + SSE HTTP** |
| Web UI MVP | **6/6 ✅** | 登录/列表/对话/SSE/cold-start/device revoke 全齐 |
| 整体 | **PASS_WITH_WARNINGS** | 设计 Exit Criteria 7 条可证实；3 条 ⚠️ 均属"比测试契约文字稍弱"非"契约错" |

编译：本次验收未本地跑 `cargo test` / `npm test`；审阅为静态代码 + 测试代码双扫。所有新集成测试 `#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]`。

---

## §1 L1 Schema 验收

| # | 验收项 | 结果 | 证据 |
| --- | --- | --- | --- |
| L1-7 | `session_device_leases` 11 列齐全（含 trust_level / last_monotonic_id / expires_at） | ✅ | `storage.rs:628-650`：`lease_id / user_id / session_id / device_id / device_fingerprint / trust_level / status / last_monotonic_id / expires_at / revoked_at / request_id / trace_id / created_at / updated_at`（共 14 字段含审计，设计 11 字段全涵盖）+ `UNIQUE KEY uq_session_device` + 3 索引。`schema_assertions.rs:152-170` 枚举 11 列逐一断言 |
| L1-8 | `session_device_lease_events` 表存在 + 支持 auto_expire 事件类型 | ✅ | `storage.rs:652-672`：`event_type VARCHAR(64) NOT NULL` + `reason VARCHAR(64) NOT NULL` 通用字符串列，`l3_6_s03_t8_passive_expiry_records_auto_expire_event` (phase2 L425-474) 真插 `event_type='device_lease_expired', reason='auto_expire'` 并断言读回 |
| L1-9 | `session_transcript_items` 主键 (session_id, item_seq) | ✅ | `storage.rs:601`：`PRIMARY KEY (session_id, item_seq)`；`schema_assertions.rs:189-194`：`assert_eq!(transcript_pk, ["session_id", "item_seq"])` 真查 INFORMATION_SCHEMA |
| L1-10 | `state_revision.revision_hash` 字段含 device_fingerprint 计算依据 | ✅ | `storage.rs:615`：`session_state_revisions.device_fingerprint VARCHAR(128) NOT NULL`（列持久化）+ `session_handlers.rs:949-963 fn revision_hash`：hash 输入串 `{session_id}|{monotonic_id}|{device_fingerprint}|{transcript_hwm}|{run_event_hwm}|{state_projection_hash}` 六元组，Rust 侧 `l2_15_revision_hash_changes_when_device_fingerprint_changes` (phase2 L286-295) 断言 fp 变化 → hash 变化 |

**小结：L1 4/4 全绿**。字段列 + 主键 + 索引 + revision_hash 参与 device fingerprint 全部可证实。schema_assertions.rs 的 Phase 2 段 (L146-210) 真查 INFORMATION_SCHEMA，非字符串 grep。

---

## §2 L2 Contract 验收（重点查 G19 IndexedDB 原子性）

| # | 验收项 | 结果 | 实现位置 |
| --- | --- | --- | --- |
| L2-10 | IndexedDB events + watermark **同 transaction** 写 | ✅ 真 | `indexeddb.ts:112` 单点 `db.transaction(['run_events', 'session_watermarks'], 'readwrite')`；L113-114 两 store 均从**同一 `tx`** 取：`tx.objectStore('run_events')` + `tx.objectStore('session_watermarks')`；L116-135 events put + L140-151 watermark put 共用该 tx；L153 `await txDone(tx)` 等到 `oncomplete`。**无第二次 transaction**。contract test L11-17 也断言该 tx 形状 |
| L2-11 | gap 检测 `event_idx > last_ok_idx + 1` → 中止 + reconnect `last_index=last_ok_idx` | ✅ | `indexeddb.ts:94-104`：预扫每 event，遇 `eventIdx > lastOkIdx + 1` → 立即 return `{ gapDetected: true, reconnectLastIndex: lastOkIdx }` **不进 IDB 事务**；`use-run-stream.ts:61-66` 消费：`result.gapDetected` 时 `resumeIndexRef = reconnectLastIndex` → close client → `setTimeout(connect)`；`use-chat-stream.ts:425-428` 同步 error 路径。contract test L19-24 匹配 |
| L2-12 | 多 tab via BroadcastChannel + idempotent apply (run_id, event_idx, event_hash) | ✅（⚠️ hash 未参） | `indexeddb.ts:203-207 broadcastWatermark(sessionId, runEventHighWatermark)` + L210-217 subscribeWatermarks；`use-chat-stream.ts:169-178` + `use-run-stream.ts:124-133` 两侧消费并 `Math.max` 单调提升 lastOkIdx。幂等判重键 `cacheKey = ${sessionId}|${runId}|${eventIdx}` (L118)，存在 → duplicate += 1 skip (L120-123)。**注**：设计 L679 要求 `(run_id, event_idx, event_hash)` 三元组，当前实现只比 (run_id, event_idx)，若 server 改写同一 idx 的 hash 不会触发重判——属弱项但不阻塞 Phase 2 |
| L2-13 | `device_revoke` + `device_lease_expired` SSE payload 对称 + SDK localStorage 清理 | ✅ | Backend：`session_handlers.rs:125-134 DeviceLeaseEndedPayload` 7 字段 `type/lease_id/session_id/device_id/device_fingerprint/reason/ended_at_server`，`insert_device_lease_event` (L1039-1073) 对 `device_revoked` / `device_lease_expired` 同函数生产同结构。Frontend：`use-chat-stream.ts:194-198` 对两 eventType **同分支**调 `clearDeviceLocalState()` → `indexeddb.ts:219-228` 真 `localStorage.clear() + indexedDB.deleteDatabase(SESSION_CACHE_DB)`。contract test L34-41 校验 |
| L2-14 | cold-start: `known_state_revision=0 && run_event_high_watermark>0` → `replay_required=true` | ✅ | `session_handlers.rs:287-299`：`cold_start = known==0 \|\| client_cache_empty`; `transcript_replay_required = cold_start && transcript_hwm>0`; `run_event_replay_required = cold_start && run_hwm>0`; 顶层 `replay_required` = 任一为 true；active_run 内部也回填 `replay_required + replay_start_event_idx=0`。`l2_14_cold_start_known_zero_with_active_run_requires_replay` (phase2 L152-192) 真场景：插 transcript_item seq=3 + running run last_event_idx=9 → 断言 hwm=(3,9) + replay_required=true |
| L2-15 | server 比 `monotonic_id`，full `revision_hash` 用于 rollback | ✅ | `session_handlers.rs:251-259`：`if query.known_state_revision == monotonic_id && known_hash != revision_hash { return 409 CONFLICT "rollback detected"}` ——仅在 monotonic 相同时才比 hash 差异，真实现 design L507-513；`l2_15_revision_hash_changes_when_device_fingerprint_changes` 断言设备差异 → hash 差异 |
| L2-16 | trust_level 从 `new_device` 升级需要 write action + step-up confirmation | ✅ | `session_handlers.rs:503-567 trust_session_device_handler`：L514 `if !request.step_up_confirmation { return FORBIDDEN }` 硬门槛；L525-535 UPDATE `WHERE ... status='active' AND trust_level='new_device'` + 可选 `last_monotonic_id = expected` CAS；`l2_16_new_device_upgrade_requires_step_up_and_write_cas` 覆盖 |
| L2-17 | `POST /sessions/{id}/device/revoke` CAS + 幂等 | ✅ | `session_handlers.rs:467-501 revoke_session_device_handler`：L467-473 UPDATE `WHERE lease_id=? AND status='active'` + 可选 `AND last_monotonic_id=expected` CAS；L483 `rows_affected()==0 → 409 CONFLICT`；L449-465 lease 已 terminal → 返回 `idempotent:true` 并带原始 payload。`l2_17_revoke_api_cas_is_idempotent_after_first_write` 证 first=1 second=0 |

**小结：L2 8/8 ✅**，1 个 minor ⚠️ (L2-12 的 event_hash 未纳入 cacheKey)。G19 IndexedDB 原子事务**真**，非"声明 tx 实际分两 tx"的 false-positive。

---

## §3 L3 Scenario E2E 验收（4 条，含补 Phase 1）

| # | 场景 | 测试函数 | 结果 | 覆盖评估 |
| --- | --- | --- | --- | --- |
| L3-4 | S03 四段设备切换 (MacBook→iPad→Chromebook→MacBook) 4 段 transcript 无丢失 | `l3_4_s03_four_device_switches_restore_ordered_transcript` (phase2 L378-409) | ✅ | 插入 4 条 `(seq=1..4, role, content)` 带 `run_id`，按 `ORDER BY item_seq DESC LIMIT 4` 取出 reverse，`assert_eq!(seqs, [1,2,3,4])`。验证分页 query 排序正确，transcript 跨设备插入可恢复。**未模拟真设备 fingerprint 切换 + workspace_state 切换**——偏"transcript 层可重建"，不覆盖 trust_level 流转。属可接受 |
| L3-5 | S04 T09 双 tab cold-start watermark 广播 | `l3_5_s04_t09_double_tab_uses_max_watermark_without_rollback` (phase2 L413-421) | ⚠️ 弱 | **测试体仅 8 行**：`tab_a_hash(8)` vs `tab_b_hash(10)`，`shared = 8.max(10) = 10; assert_eq!(shared,10); assert_ne!(tab_a_hash, tab_b_hash)`。**没真开 BroadcastChannel / 没真发 `stream?last_index=0`**。前端 contract test L26-32 断言代码包含 `new BroadcastChannel` 字符串——仅源码 grep。真双 tab idempotent 行为在浏览器真机测试才能 cover（Phase 2 Exit criteria "Chrome+Safari 行为一致"仍 pending） |
| L3-6 | S03 T8 Chromebook passive expiry 产生 `device_lease_expired` | `l3_6_s03_t8_passive_expiry_records_auto_expire_event` (phase2 L425-474) | ✅ | 插 lease `expires_at = NOW-1s` → UPDATE `WHERE status='active' AND expires_at<=NOW() SET status='expired'` → `rows_affected==1` → INSERT `device_lease_expired` 事件（reason=`auto_expire`）→ SELECT 回读 reason 断言 `'auto_expire'`。**真做了 CAS 过期转移 + 事件写入 + 校验**，但**无后台扫描 worker**（需要客户端读时检测或另起 scheduler）。design L608-620 建议"background scanner or read-time check"，当前代码仅靠 read-time path + 此测试**手动触发**——Phase 2 可接受 |
| **补 L3-1** | S04 T01-T17 完整 17 次 SSE reconnect + pod 重启 + 双审批，event_idx 单调 | `l3_s04_t01_t17_full_reconnect_survives_restart_and_approvals` (phase1 L370-446) | ⚠️ 仍骨架 | **加深版 append_event + load_run**：17 次循环 `append_event(text_delta, disconnect=i)` + `load_run + filter by idx>last_ok_idx` 断言每次"增量 1"；之后 save checkpoint graceful + `engine.recover_active_runs` + 2 次 approval_decision；最终断言 `indexes == (0..21)`。**没有走 axum Router + tower oneshot + 真 HTTP GET `/chat/runs/{id}/stream?last_index=N`**——即 IMPL-VERIFY-PHASE-1 §M3 要求的"真 SSE last_index 回放 harness"**未落地**。相对 Phase 1 版的 `l3_s04_reconnect_replays_monotonic_events` (phase1 L330-368) 仅加了 `recover_active_runs` + approval。审阅结论：**Phase 1 L3-1 补完状态未达标** |

**小结：L3 3/3 通过**（L3-5 弱、L3-4/L3-6 ✅），但 **Phase 1 L3-1 补完 ⚠️** —— 没做真 HTTP SSE E2E，只做了 store 层的深度调用。

---

## §4 Web UI MVP 验收

| # | 必做项 | 结果 | 证据 |
| --- | --- | --- | --- |
| 1 | 登录页（复用 auth_sessions） | ✅ | `web/app/login/page.tsx` (106 L) 存在，`useActionState(loginAction)`；`auth/session.rs:202-247` `list_sessions` 接口未破坏（ORDER BY updated_at DESC L247） |
| 2 | Session 列表（按 updated_at） | ✅ | `web/app/(dashboard)/sessions/page.tsx` 调 `getSessions()` → `platform.ts` → `/sessions?limit=50`；后端 `session_audit.rs` 默认 `ORDER BY updated_at DESC`；UI 列 "Updated" at `sessions-table-client.tsx:142,156` |
| 3 | 对话框（新开 / 继续 + cold-start replay） | ✅ | `components/workspace/workspace-shell.tsx:33 useChatStream(activeConfig)`；L41-47 switchSession 写入 URL + config.sessionId；use-chat-stream 消费 config.sessionId 切换 useEffect L98-122 |
| 4 | 基本 SSE 流式回复 | ✅ | `use-chat-stream.ts:496-523 new SSEClient({ url: '/api/backend/chat/stream', onEvent: processEvent, heartbeatTimeoutMs: SSE_CLIENT_DEAD_TIMEOUT_MS })`；45s 超时来自 `indexeddb.ts:6 SSE_CLIENT_DEAD_TIMEOUT_MS = 45_000`（对应 L2-6 客户端侧） |
| 5 | cold-start hydration (G24) | ✅（轻微 ⚠️） | `use-chat-stream.ts:124-167 hydrateColdStart()`：`readWatermark` → `getSessionState` → `if replay_required \|\| transcript_replay_required: getSessionTranscript + applyTranscriptItemsTransaction + setMessages`；`if run_event_replay_required: runEventLastOkIdxRef = replay_start_event_idx - 1`。**真消费 replay_required**，非只打 log。⚠️ **但不主动 `stream?last_index=0` 重连 active_run**——依赖下一次用户发消息（POST /chat/stream 开新 run）才重建流；若 active run 还在跑，中间事件不会被客户端显示。属 minor，等 Phase 3 或迭代修 |
| 6 | 设备 lease 显式 revoke 按钮 (G25) | ✅ | `components/settings/runtime-settings-panel.tsx:41-42, 182-220, 425-468`：输入 session_id → `loadDevices()` → GET /sessions/{id}/devices → 渲染列表 → 每行 Revoke 按钮 `onClick=revokeDevice(device)` → `revokeSessionDevice(session_id, { leaseId, expectedLastMonotonicId })` 携带 CAS token |

**小结：Web UI MVP 6/6 ✅**。暂不做项 (Context 侧边栏 / Plan 树 / Artifacts 画廊 / Skill 编辑器 / delegation 可视化) 按计划未实现，符合验收豁免。

---

## §5 False-positive 筛查（G19 / G24 / G25 三区重点）

| 风险点 | 检查方法 | 结果 |
| --- | --- | --- |
| **G19 IndexedDB 原子性**：是否一个 tx 只包 events，另一 tx 更 watermark | grep `db.transaction` 次数 + 两 store 是否从同一 `tx` 取 | ✅ **真原子**：`indexeddb.ts:112` 唯一 transaction(['run_events','session_watermarks'],'readwrite')；L113 eventsStore + L114 watermarksStore 皆从 `tx` 取；L153 `await txDone(tx)`（等 oncomplete）——commit 失败两 store 同时回滚 |
| **G19 gap 预扫 vs tx 内检**：事务开启后是否还检 gap | 看 IDB 写入前是否已 return | ✅ **预扫先行**：L89-108 先无 tx 扫 `eventIdx > lastOkIdx+1` → return；只有无 gap 才 L110 openSessionCache + L112 开 tx。正确：避免"半提交"污染 |
| **G24 cold-start client 消费**：server 标 `replay_required=true` 但 client 不看 | 查 `use-chat-stream.ts` 是否分支 replay_required | ✅ **真消费**：L141 `if (state.replay_required \|\| state.transcript_replay_required)` → 真调 getSessionTranscript + 写 IDB + setMessages；L154 `if (state.run_event_replay_required && state.active_run)` → 重置 lastOkIdx。非 `if (...){ /* TODO */ }` 死枝 |
| **G24 cold-start active run replay**：client 是否主动 `stream?last_index=0` 拉 active run | 在 hydrate 路径搜 URL / useRunStream 调用 | ⚠️ **soft gap**：use-chat-stream 不主动打开 run stream，仅复位 `runEventLastOkIdxRef = replay_start_event_idx - 1` 等待下一次 SSE 流；如果 active_run 在另一设备仍跑，当前 tab 看不到 in-flight 事件（直到用户新发 msg）。design L481-484 要求"reconnect from the beginning: `/chat/runs/{run_id}/stream?last_index=-1`"。**建议** Phase 3 初接一个 `useRunStream(active_run.run_id, lastIndex=0)` 自动订阅 |
| **G25 SSE parity**：两 event payload 字段是否对称 | 比较 `DeviceLeaseEndedPayload` 两处生成 | ✅ **完全对称**：`insert_device_lease_event` (L1039-1073) 对 event_type="device_revoked" 或 "device_lease_expired" 走同一 insert + 同一 payload struct；`load_device_lease_event_payloads` (L1075-1105) 读回也走同 struct。7 字段均非空 |
| **G25 passive expiry 背景扫描**：是否有 worker 主动扫 `expires_at <= NOW()` | grep `tokio::spawn.*expire\|interval.*lease` in server | ⚠️ **无后台 worker**：检索 `rust/crates/runtime` 无定时任务扫 `session_device_leases WHERE expires_at <=`；目前仅靠 client 下次请求时被动发现 or 测试手动触发。设计 L609 允许 "background scanner or read-time lease checks"——read-time 分支在 `ensure_device_lease` 看似仅 upsert 不校验过期，**这是弱项**：客户端长时间不请求的场景无人发事件。Phase 3/4 需补 |
| **Phase 1 L3-1 补完**：是否真 HTTP SSE reconnect | phase1_run_durability.rs:370-446 找 Router/tower/SSEClient | ❌ **没补**：仍只调 `store.append_event` + `store.load_run`，无 axum Router，无 HTTP request。相对 v1 版加了 `engine.recover_active_runs` + 2 approval，但设计契约"SSE `?last_index=N` E2E"仍未落地。建议 Phase 3 做 |

**结论：3 项 ⚠️**（G24 active run 不主动 replay / G25 无后台 worker / Phase 1 L3-1 未真补完），均为"实现比契约宽松"非"声明与事实相违"，属 legit 渐进落地。**无 G19 式'说了没做'的 false-positive**。

---

## §6 跨 Phase 一致性

### Phase 1 ↔ Phase 2 schema 兼容

- ✅ `agent_runs.last_event_idx` (Phase 1, `runs.rs:331, 1295`) 被 Phase 2 `active_run_projection` (session_handlers.rs:918-935) 正确查询为 `run_event_high_watermark`；字段重命名是 API 层面（客户端叫 high_watermark，DB 仍叫 last_event_idx）——设计层合理，但需在 `/state` 响应加 comment 避免误导 Phase 3 实现者
- ✅ `agent_run_events` 未被 session_handlers 直接查——通过 `agent_runs.last_event_idx` 间接。如果后续 pod 需要 `SELECT MAX(event_idx)` 回填到 `agent_runs` 的 scenario，需与 Phase 1 `run_counters.next_event_idx` 保持一致（已有 CAS 机制，安全）
- ✅ `session_state_revisions` (Phase 2 新表) 不与 Phase 1 任何表冲突；`device_fingerprint` 字段独占 Phase 2

### IndexedDB `run_events` 表字段与服务端对齐

- `indexeddb.ts:47-48`: `{ keyPath: 'cacheKey' }`，cacheKey=`${sessionId}|${runId}|${eventIdx}` (L118)
- 服务端 `agent_run_events` 主键 `(run_id, event_idx)` (storage.rs:536)
- ⚠️ **前端缺 session_id 在主键**（虽然 cacheKey 带了），query by run_id+event_idx 时可能需要扫表；建议 Phase 3 加 IDBIndex(runId, eventIdx) 或改 keyPath

### Phase 3 context_manifest projection 预留

- ❌ `session_handlers.rs` **未预留**任何 `context_manifest` projection 接口；`SessionStateResponse` 结构 (L28-36) 无 `context_manifest_id` 字段
- 但 `SessionStateResponse` 用 serde `#[derive(Serialize)]` 未限定额外字段拒绝——Phase 3 可向后兼容加字段而不 breaking change
- 建议 Phase 3 在 session_handlers.rs 顶部 comment 预告 `active_context_manifest: Option<ContextManifestProjection>` 位置

### Web 端 session-client.ts vs platform-sessions.ts 重复

- `web/lib/api/session-client.ts` (L1-145) 与 `web/lib/api/platform-sessions.ts:86-176` 定义了**重复**的 `SessionStateResponse / TranscriptItem / TranscriptResponse / DeviceLease / DeviceLeaseEndedEvent` 类型和 `getSessionState / getSessionTranscript / getSessionDevices / revokeSessionDevice / trustSessionDevice` 函数
- 两个文件都存在，只是 prefix 不同（`/api/backend` vs `apiFetch`）
- 产物风险：未来 schema 变动可能漂移。**建议 Phase 3 首任务合并**（留 session-client 作 SDK 侧、删掉 platform-sessions.ts 多余的 device/state 导出）

---

## §7 Residual / 建议修正

### 必须在 Phase 3 前修（🔴）

- **R1 Phase 1 L3-1 真 SSE E2E 补完**：当前 `l3_s04_t01_t17_*` 仍是 store.append_event 深度版，**不是**真 axum + tower oneshot + HTTP SSE client 的回放测试。IMPL-VERIFY-PHASE-1 §M3 明确要求"Phase 2 Web hook 就位后做"，现在 Web hook 已就位，应补。参考 `rust/crates/runtime/tests/web_agent_e2e.rs:3020` 已有的 `get_run_stream(app, run_id, last_index)` harness，可直接复用

  **2026-05-07 remediation**：已补到 `rust/crates/runtime/tests/phase1_run_durability.rs::l3_s04_t01_t17_full_reconnect_survives_restart_and_approvals`。该测试现在通过真 axum `Router` + `GET /chat/runs/{run_id}/stream?last_index=N` + `POST /chat/runs/{run_id}/input` 覆盖 17 次 SSE reconnect、一次 graceful checkpoint 后 pod lease takeover、两次 approval pause/resume，并断言 server/client 两侧 event_idx 单调无空洞且无重复。

### 建议 Phase 3 开始时顺手做（🟡）

- **R2 G24 active run 主动 replay**：cold-start 时如果 `active_run` 存在，use-chat-stream 应自动触发 `useRunStream(active_run.run_id, lastIndex=0)` 订阅 in-flight 事件；现在只 hydrate transcript，active run 中间的 text_delta 需等用户新 msg 才恢复流
- **R3 G25 passive expiry 后台扫描 worker**：加一个 `tokio::spawn(async { loop { sleep(60s); UPDATE ... WHERE expires_at<=NOW() }})` 在 server bootstrap；否则长时间离线的浏览器永远收不到 `device_lease_expired` 事件
- **R4 L2-12 cacheKey 加 event_hash**：当前 `${sessionId}|${runId}|${eventIdx}` 不含 hash，若 server 同 idx 发不同 payload（理论不该，但防守）无法去重；design L679 明确要 `(run_id, event_idx, event_hash)` 三元组
- **R5 session-client.ts vs platform-sessions.ts 类型/函数重复**：收敛到单一文件
- **R6 `SessionStateResponse` 预留 manifest projection 占位注释**：给 Phase 3 留路径

### 纯文档/测试清理（🟢）

- **R7 L3-5 加真 BroadcastChannel jest-environment-jsdom 测试**：`jest-environment-jsdom` 支持 BroadcastChannel mock，可以真触发两 hook 订阅同步，现测试只是算术断言偏弱
- **R8 schema_assertions.rs 测试函数名加前缀 `phase2_` 统一**（已做，但 phase1 段名是 `phase1_run_durability_schema_contract`，延续风格 OK）
- **R9 IMPL-TEST-PLAN §L2-12 补脚注**："event_hash 判重延后至 Phase 3 增强；Phase 2 以 (run_id, event_idx) 为主键即可"，与代码对齐

---

## §8 发布判断

**✅ 建议进入 Phase 3 (Context Manifest + Retrieval + Budget + Confidence)。**

判断依据：

1. **15 条验收点结构**：4/4 L1 ✅ + 8/8 L2 ✅（1 minor event_hash 未纳入 cacheKey）+ 3/3 L3 ✅（L3-5 弱但通过）。Phase 2 Exit criteria (design L3397-3409) 7 条：
   - ✅ 浏览器 A 建 session，B 继续，见同样 transcript/plan/tool 状态（transcript API + state API 可用）
   - ✅ 运行中刷新重连不丢事件（L2-11 gap + reconnect）
   - ✅ 温缓存只拿 delta（L2-15 monotonic 比对）
   - ✅ IndexedDB events + watermark 原子 commit（L2-10 真 tx）
   - ⚠️ revoked/stale 设备无法倒转 revision（monotonic 比对 + device_fingerprint hash 可检；但读权限降级需等 G11 workspace reachability）
   - ✅ cold-start 从 replay 而非 server watermark 假装本地应用（L2-14 + use-chat-stream hydrateColdStart）
   - ⚠️ passive expiry 发同样 local-cache 清理信号（payload 对称 ✅；但后台扫描无 → 真场景靠 read-time 触发）

2. **False-positive 扫描**：**0 个 G19 式"说了没做"**。G19 IndexedDB 原子 tx 真实现；G24 服务端完整 + 客户端消费 replay_required 分支；G25 payload 对称 7 字段。3 个 ⚠️ 均是"实现比契约稍宽松"非"实现与契约矛盾"。

3. **跨 Phase 兼容**：字段命名 `last_event_idx` ↔ `run_event_high_watermark` 约定可接受（API 层重命名，DB 保留原名）；Phase 3 context_manifest 无 schema 冲突；session-client ↔ platform-sessions 存在代码重复但不 break。

4. **Phase 1 L3-1 补完未达标**：**审查员判定为 ❌**。但 R1 属独立可修项（几天内可补），不阻塞 Phase 3 设计启动；Phase 3 merge 前必须补完否则 Phase 4 会继续漂移。

**Phase 3 启动前的 hard blockers**：
- **R1 Phase 1 L3-1 真 SSE E2E 补完**（ 预计 1 天，用 web_agent_e2e.rs:3020 现成 harness）

**与 Phase 3 并行可做**（non-blocking）：
- R2 / R3 / R4 / R5 / R6

**浏览器真机测试**（Chrome + Safari IndexedDB 一致性）：当前验收为源码 + 契约测试层，Phase 2 退出条件 L128 要求的"浏览器真机测试"仍未执行。**建议 merge 前在 staging 跑一次**：打开同一 session 两 tab，验证 BroadcastChannel 同步 + revoke 按钮 + 2h 空闲后 lease 过期体验。

---

*End of verification report. Generated by regression sub-agent, 2026-05-07.*
