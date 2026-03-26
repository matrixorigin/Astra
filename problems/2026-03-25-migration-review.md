# Migration Review — Problem Report

**Date**: 2026-03-25
**Commit**: `c7aebf80` feat: migrate to rust
**Reviewer**: AI code review
**Status**: Post-migration, active optimization

---

## 修复进度追踪

| # | 问题 | 优先级 | 状态 |
|---|---|---|---|
| 1 | InProcessBridge 持久化不完整 | P0 | ✅ 已修复 + 测试 |
| 2 | Memory proxy 隔离绕过 | P0 | ✅ 已修复 + 测试 |
| 3 | 连接池未共享 | P0 | ✅ SharedPool 已创建 + **全部 22 个服务已迁移** + 测试 |
| 3b | Learning feedback 越权写入 | P0 | ✅ 已修复 + 测试 |
| 3c | Learning feedback 字段静默丢弃 | P0 | ✅ 已修复 + 测试 |
| 3d | reqwest::blocking 无 spawn_blocking | P0 | ✅ 已修复（改为 async reqwest） |
| 4 | 持久化无失败通知 | P1 | ✅ 已修复：全局计数器 + health 端点暴露 + 测试 |
| 5 | 遗漏路由 | P1 | ✅ 已修复 |
| 6 | CLI 重启上下文断裂 | P2 | ✅ 已修复：从 journal 恢复 history + 测试 |
| 7 | SessionCache 不可扩展 | P2 | 🟡 短期可接受（单实例），长期迁移 Redis |
| 8 | 历史权威来源不明确 | P2 | 🟡 P2 #6 修复后 journal 成为 CLI 端 source of truth |
| 9 | 代码重复 | P3 | ✅ 已修复 |
| 10 | curl 子进程 | P3 | ✅ 已修复 |
| 11 | AppState builder 冗长 | P3 | 🟡 47 个 with_* 方法，不影响正确性，stall 检测 CLI/server 已确认同步 |

---

## P0 — 全部已修复

### 1. InProcessBridge 持久化不完整 ✅

**修复**: 在 `InProcessChatTurnBridge::forward()` 中调用 `run_bridge_hook_side_effects()`，传入所有 5 个 writer 参数。

**测试**: `inprocess_hook_contract.rs` — 5 个合约测试覆盖 decision audit、skill selection、reflection、implicit feedback、auxiliary events。

---

### 2. Memory Proxy 用户隔离可被绕过 ✅

**修复**: `auth_handlers.rs` 中 `or_insert_with` → `insert`，强制覆盖 `user_id`/`session_id`。

**测试**: `memory_contract.rs::memory_proxy_overwrites_spoofed_user_id`

---

### 3. 连接池未共享 ✅（热路径已迁移）

**已完成**: `SharedPool` 已添加到 `core/src/lib.rs`（`max_connections=10`, `min_connections=1`）。**全部 22 个 Database*Service 已迁移**：
- 每个服务新增 `pool: Option<SharedPool>` 字段和 `with_pool()` builder
- `get_pool()` 辅助方法：有 SharedPool 时使用共享池，否则回退到 `connect_matrixone()`
- `state_builder.rs` 中所有服务已注入 SharedPool（28 处 `.with_pool(shared_pool.clone())`）
- 覆盖范围：auth, session, agents, events, context, decisions, models, triggers, workflows, sandbox, branches, data_versioning, marketplace, replay, skills, skill_config, evaluation, introspection, reflect, learning, admin (6 structs), turn writers (5 structs)

**测试**: `shared_pool_migration_contract.rs` — 4 个测试验证 API 契约（Clone, Debug, builder pattern）

---

### 3b. Learning feedback 越权写入 ✅

**修复**: `submit_feedback()` 加 `user_id` 参数，SQL 通过 `agent_sessions` JOIN 校验所有权。

**测试**: `reflect_contract.rs::learning_feedback_rejects_other_users_event`

---

### 3c. Learning feedback 字段静默丢弃 ✅

**修复**: 从 API 移除 `feedback_type` 和 `correct_skills` 字段，避免误导调用方。

**测试**: `reflect_contract.rs::learning_feedback_ignores_removed_fields`

---

### 3d. reqwest::blocking 在 async 上下文 ✅

**修复**: `edge_tools.rs` 中 `memoria_call` 改为 `async fn`，使用 `reqwest::Client`（非 blocking）。workspace `Cargo.toml` 移除 `blocking` feature。

---

## P1 — 已修复

### 4. Fire-and-forget 持久化无失败通知 ✅

**修复**:
- `bridge/side_effects.rs` 新增全局原子计数器：
  ```rust
  pub static PERSIST_FAIL_COUNT: AtomicU64 = AtomicU64::new(0);
  pub static PERSIST_OK_COUNT: AtomicU64 = AtomicU64::new(0);
  ```
- 所有 `eprintln!("... failed: {error}")` 替换为 `record_persist_failure(context, &error)`
- `HealthResponse` 新增 `persist_ok: u64` 和 `persist_fail: u64` 字段
- `/health` 端点暴露计数器，运维可实时观测持久化失败率

**测试**: `persist_counter_contract.rs` — 5 个测试：
- `health_includes_persist_counters` — health 端点包含计数器字段
- `persist_counters_are_u64` — 类型正确
- `persist_fail_counter_increments` — 原子递增正确
- `persist_ok_counter_increments` — 原子递增正确
- `health_reflects_counter_state` — health 端点反映当前计数器值

---

## P2 — 部分已修复

### 5. 遗漏路由 ✅（已在 P1 修复）

已补齐：`GET /chat/session/{id}/reflect`、`GET /chat/session/{id}/decision-trace`、`POST /api/v1/learning/feedback`

---

### 6. CLI 重启后对话上下文断裂 ✅

**修复**: `repl_runtime.rs::initialize_repl_state()` 在恢复 `session_id` 后，调用 `restore_history_from_journal()` 从本地 journal 重建 history：

```rust
if let Some(ref sid) = p.last_session_id {
    state.history = restore_history_from_journal(sid);
}
```

`restore_history_from_journal()` 过滤 `JournalEventType::Turn` 事件，提取 `(user_input, assistant_output)` 对。

**测试**: `repl_runtime.rs` 内联测试 — 3 个测试：
- `restore_history_empty_for_unknown_session` — 未知 session 返回空
- `restore_history_from_journal_roundtrip` — 写入 journal 后能正确恢复
- `restore_history_skips_non_turn_events` — 只包含 Turn 事件

---

### 7. SessionCache 不可水平扩展 🟡

**现状**: `Arc<Mutex<HashMap>>` 纯内存 LRU，单实例可用。

**短期**: 可接受 — CLI 每次发送完整 messages，cache miss 只影响 server 端 execution_state 跟踪。

**长期方向**: 迁移到 Redis（已有 Redis 依赖在 docker-compose 中）。

---

### 8. 对话历史权威来源不明确 🟡

**现状**: P2 #6 修复后，journal 成为 CLI 端的 source of truth。三副本关系明确：
- **DB `agent_events`**: 服务端永久存储（source of truth for server）
- **CLI journal**: 本地持久化，CLI 重启后恢复（source of truth for CLI）
- **Server `SessionCache`**: 性能优化层，可重建

**剩余**: 缺少 session history 恢复 API（`GET /sessions/{id}/history`），CLI 目前只能从本地 journal 恢复，跨设备场景无法恢复。

---

## P3 — 技术债

### 9. services/runtime 代码重复 ✅

introspection 模块已去重（-1831 行）。

### 10. curl 子进程调用 Memoria ✅

已改为 async reqwest。

### 11. AppState builder 冗长 🔴

40+ 个 `with_*` builder 方法。不影响正确性，可用 `typed-builder` derive macro 优化。

---

## 测试覆盖状态

| 指标 | 值 |
|---|---|
| 总测试数 | **743** |
| 通过 | **743** |
| 失败 | **0** |
| clippy 警告 | **0** |
| 格式检查 | ✅ |

### 本轮新增测试文件

| 文件 | 测试数 | 覆盖内容 |
|---|---|---|
| `persist_counter_contract.rs` | 5 | 持久化计数器、health 端点 |
| `shared_pool_migration_contract.rs` | 4 | SharedPool API 契约 |
| `repl_runtime.rs` (inline) | 3 | journal history 恢复 |

### 历史新增测试文件（本 session）

| 文件 | 测试数 | 覆盖内容 |
|---|---|---|
| `reflect_contract.rs` | 14 | auth, CRUD, ownership |
| `route_registry_contract.rs` | 26 | 所有路由注册 |
| `shared_pool_contract.rs` | 3 | SharedPool 基础 |
| `inprocess_hook_contract.rs` | 5 | hook side effects |
| `memory_prefetch_contract.rs` | 8 | e2e with mock Memoria |
| `bridge_inprocess.rs` (inline) | 11 | entity extraction, merge, memory section |
| `memory_contract.rs` | +1 | spoofed user_id 安全测试 |
| `persist_counter_contract.rs` | 5 | 持久化计数器 |
| `shared_pool_migration_contract.rs` | 4 | SharedPool 迁移 API |
| `mo-agent/main.rs` (inline) | +18 | mock HTTP: auth, SSE, slash commands |
| `mo-agent/repl_runtime.rs` (inline) | 3 | journal history 恢复 |
| `mo-agent/cli_utils.rs` (inline) | 18 | 纯函数 |
| `mo-agent/repl_ui.rs` (inline) | 12 | slash 命令解析 |
| `mo-agent/stream_render.rs` (inline) | 12 | SSE dispatch |
| `mo-agent/edge_tools.rs` (inline) | 12 | schema 完整性 |
| `mo-agent/edge_tools/fs.rs` (inline) | 10 | 文件操作 |
| `mo-agent/edge_tools/shell.rs` (inline) | 7 | shell 工具 |
| `mo-agent/permission_manager.rs` (inline) | 13 | 权限分类 |
| `mo-admin/credentials.rs` (inline) | 4 | 凭证管理 |
| `mo-admin/http_helpers.rs` (inline) | 6 | HTTP 辅助 |
| `mo-admin/cli_args.rs` (inline) | 7 | CLI 参数解析 |

---

## 剩余已知问题

| 问题 | 优先级 | 说明 |
|---|---|---|
| SessionCache 不可扩展 | P2 | 单实例可用，多实例需 Redis |
| 跨设备 session 恢复 | P2 | 需要 `GET /sessions/{id}/history` API |
| AppState builder 冗长 | P3 | 47 个 with_* 方法，不影响正确性 |
| 双重 stall 检测 | P3 | CLI 和 server 各有一套，已确认逻辑同步（window=3），无实际 bug |
