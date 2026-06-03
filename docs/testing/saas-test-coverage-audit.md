# SaaS 测试覆盖率自查

> **关联文档**：[SaaS 能力测试计划](./saas-test-plan.md)  
> **入口命令**：`make test-saas`（50 个 `e2e_matrix_saas_*` + 1 个 `session_reaper_db_integration`）  
> **最后更新**：2026-06-02  
> **分支**：`add_saas_testcases` @ `/Users/heni/workspace_hn/astra`

---

## 1. 图例

| 符号 | 含义 |
|------|------|
| ✅ | 已在 `make test-saas` 中自动化 |
| 🟡 | 仓库内有测试，但未纳入 `make test-saas`；或仅单元/部分断言 |
| ❌ | 未覆盖（需新增自动化或书面豁免） |
| 🔧 | 部署/手工验收项 |

---

## 2. 自动化清单（`make test-saas`）

### 2.1 Rust Matrix E2E（46 项）

| # | 测试名 | 计划章节 | 模块 |
|---|--------|----------|------|
| 1 | `e2e_matrix_saas_resource_limits_read_and_admin_override` | §5.3 | platform |
| 2 | `e2e_matrix_saas_resource_daily_session_cap_denies_chat` | §5.3, §6.6 | platform |
| 3 | `e2e_matrix_saas_resource_concurrent_session_cap_denies_chat` | §5.3 | platform |
| 4 | `e2e_matrix_saas_admin_config_crud_rbac` | §5.2 | platform |
| 5 | `e2e_matrix_saas_admin_grant_revoke_rbac_flow` | §5.2 | platform |
| 6 | `e2e_matrix_saas_resource_usage_per_user_isolation` | §5.3, §5.4 | platform |
| 7 | `e2e_matrix_saas_auth_refresh_cycle` | §5.1 | platform |
| 8 | `e2e_matrix_saas_session_cross_user_isolation` | §5.4 | platform |
| 9 | `e2e_matrix_saas_events_and_audit_cross_user_isolation` | §5.4 | platform |
| 10 | `e2e_matrix_saas_auth_negative_paths` | §5.1 | negative |
| 11 | `e2e_matrix_saas_resource_governance_negative_paths` | §4.1, §5.3 | negative |
| 12 | `e2e_matrix_saas_resource_concurrent_cap_recovery` | §5.3, §6.6 | negative |
| 13 | `e2e_matrix_saas_task_lease_negative_paths` | §4.2 | negative |
| 14 | `e2e_matrix_saas_auth_logout_and_expired_token` | §5.1 | negative |
| 15 | `e2e_matrix_saas_resource_limits_extended_fields` | §5.3 | negative |
| 16 | `e2e_matrix_saas_task_lease_contested_and_expired_reclaim` | §4.2, §6.4 | negative |
| 17 | `e2e_matrix_saas_edge_tool_result_success_path` | §4.2 | negative |
| 18 | `e2e_matrix_saas_memoria_proxy_degradation` | §5.7 | negative |
| 19 | `e2e_matrix_saas_run_cross_user_isolation` | §4.3, §5.4 | negative |
| 20 | `e2e_matrix_saas_run_double_pause_conflict` | §4.3 | negative |
| 21 | `e2e_matrix_saas_edges_status_smoke` | §4.2 | negative |
| 22 | `e2e_matrix_saas_approval_respond_success_path` | §4.2 | negative |
| 23 | `e2e_matrix_saas_platform_health_and_auth_me` | §5.1, §7.4 | coverage |
| 24 | `e2e_matrix_saas_auth_refresh_token_rotation` | §5.1 | coverage |
| 25 | `e2e_matrix_saas_memory_proxy_user_isolation` | §5.4, §5.7 | coverage |
| 26 | `e2e_matrix_saas_models_list_and_key_encryption` | §5.6 | coverage |
| 27 | `e2e_matrix_saas_session_lifecycle_positive` | §5.1, §6.1 | coverage |
| 28 | `e2e_matrix_saas_resource_usage_increments_after_chat` | §5.3, §6.6 | coverage |
| 29 | `e2e_matrix_saas_run_cancel_cross_user_and_owner` | §4.3, §5.4 | coverage |
| 30 | `e2e_matrix_saas_approval_respond_deny_path` | §4.2 | coverage |
| 31 | `e2e_matrix_saas_chat_run_pause_resume_positive` | §4.3 | coverage |
| 32 | `e2e_matrix_saas_admin_tokens_rbac_smoke` | §5.2 | coverage |
| 33 | `e2e_matrix_saas_auth_register_login_positive` | §5.1 | coverage |
| 34 | `e2e_matrix_saas_auth_duplicate_email_register` | §5.1 | coverage |
| 35 | `e2e_matrix_saas_runs_list_pagination_positive` | §4.3 | coverage |
| 36 | `e2e_matrix_saas_edge_agent_registration_smoke` | §4.2 | coverage |
| 37 | `e2e_matrix_saas_admin_cleanup_rbac_smoke` | §5.2, §5.5 | coverage |
| 38 | `e2e_matrix_saas_admin_audit_rbac_smoke` | §5.2 | coverage |
| 39 | `e2e_matrix_saas_skills_cross_user_isolation` | §5.4 | coverage |
| 40 | `e2e_matrix_saas_team_cross_user_isolation` | §5.4 | coverage |
| 41 | `e2e_matrix_saas_session_replay_compare_smoke` | §6.1 | coverage |
| 42 | `e2e_matrix_saas_delegate_http_boundaries` | §4.3 | coverage |
| 43 | `e2e_matrix_saas_session_replay_post_positive` | §6.1 | coverage |
| 44 | `e2e_matrix_saas_admin_feedback_stats_rbac` | §5.2 | coverage |
| 45 | `e2e_matrix_saas_run_projection_smoke` | §4.3 | coverage |
| 46 | `e2e_matrix_saas_session_audit_after_chat_smoke` | §5.4, audit | coverage |

### 2.2 Services DB 集成（1 项）

| 测试名 | 计划章节 |
|--------|----------|
| `reaper_marks_stale_active_session_idle_then_ended` | §5.5 |

### 2.3 SDK（可选，需 npm + astra-server 健康）

| 脚本 | 计划章节 |
|------|----------|
| `packages/sdk` → `test:integration:saas` | §6.2 |

---

## 3. 分层覆盖率自查

### 3.1 第一层：Cloud 运行时协议（§4）

| 测试项 | 状态 | 对应自动化 |
|--------|------|-----------|
| 首 Turn 含 `project_rules` | 🟡 | `edge_cloud_round_trip_e2e`（未进 test-saas） |
| 多轮 tool loop（≥3） | 🟡 | 同上 / matrix `chat/turn` 系列 |
| 超 Token 日预算 | ✅ | `e2e_matrix_saas_resource_governance_negative_paths` |
| 并发同 Session Turn 无串话 | 🟡 | `e2e_matrix_same_session_concurrent_turns_isolated` |
| LLM 429/5xx + retryable | 🟡 | `unhappy_llm_behaviors.rs` 单元级 |
| `POST /tools/result` | ✅ | `e2e_matrix_saas_edge_tool_result_success_path` |
| `POST /approval/respond` | ✅ | success + deny 两条 |
| `POST /agents/edge` | ✅ | `edge_agent_registration_smoke` + task lease 内嵌 |
| Task Lease claim/renew/release | ✅ | negative + contested/reclaim |
| `GET /edges/status` | ✅ | `e2e_matrix_saas_edges_status_smoke` |
| `POST /chat` 异步 run | ✅ | 多处 chat 用例 |
| pause / resume | ✅ | `e2e_matrix_saas_chat_run_pause_resume_positive` |
| run cancel | ✅ | `e2e_matrix_saas_run_cancel_cross_user_and_owner` |
| `POST .../delegate` | ✅ | `e2e_matrix_saas_delegate_http_boundaries` |
| `GET /runs` 分页 | ✅ | `e2e_matrix_saas_runs_list_pagination_positive` + run 隔离 |
| Cloud Sync push/pull | ❌ | 需专项 sync E2E |
| 断网重连 / 跨设备 | ❌ | 手工 / 长场景 |

**§4 小结**：`test-saas` 内约 **75%**；含 matrix 全库约 **85%**。

---

### 3.2 第二层：SaaS 平台能力（§5）

#### §5.1 认证

| 测试项 | 状态 | 测试 |
|--------|------|------|
| 注册 | ✅ | bootstrap + `auth_register_login_positive` |
| 重复用户名 | ✅ | `auth_negative_paths` |
| 重复 email | ✅ | `auth_duplicate_email_register` |
| 登录 | ✅ | platform + `auth_register_login_positive` |
| Token 刷新 | ✅ | `auth_refresh_cycle` + `auth_refresh_token_rotation` |
| 登出 / 过期 token | ✅ | `auth_logout_and_expired_token` |
| `GET /auth/me` | ✅ | `platform_health_and_auth_me` |
| 弱密钥启动失败 | ❌ | 需独立进程测试 `ASTRA_ALLOW_INSECURE_DEFAULTS=0` |

#### §5.2 Admin / RBAC

| 路由/项 | 状态 | 测试 |
|---------|------|------|
| 普通用户 403 | ✅ | 各 admin smoke |
| grant/revoke admin | ✅ | `admin_grant_revoke_rbac_flow` |
| `/admin/tokens` | ✅ | `admin_tokens_rbac_smoke` |
| `/admin/config` CRUD | ✅ | `admin_config_crud_rbac` |
| `/models` CRUD + Key 加密 | ✅ | `models_list_and_key_encryption` |
| `PUT .../resources/limits` | ✅ | resource_* 系列 |
| `/admin/cleanup` | ✅ | `admin_cleanup_rbac_smoke` |
| `/admin/audit` | ✅ | `admin_audit_rbac_smoke`（smoke，非深度导出） |
| `/admin/init` 冷启动 | 🔧 | 部署验收 |
| `/admin/feedback/stats` | ❌ | 待补 |

#### §5.3 资源治理

| 限额项 | 状态 | 测试 |
|--------|------|------|
| 日 Session 上限 | ✅ | `resource_daily_session_cap_denies_chat` |
| 并发 Session | ✅ | `resource_concurrent_session_cap` + `concurrent_cap_recovery` |
| 日 Token | ✅ | `resource_governance_negative_paths` |
| `max_concurrent_bash` | 🟡 | `resource_limits_extended_fields` 读写字段，无 HTTP 拒绝 |
| `max_disk_bytes` | 🟡 | 同上 |
| `GET /resources/usage` | ✅ | `resource_usage_increments` + `usage_per_user_isolation` |
| `GET /resources/limits` | ✅ | `resource_limits_read_and_admin_override` |
| Admin override / unlimited(0) | ✅ | override + governance negative |
| 原子计数 | 🟡 | 单元测试 `resource_governor.rs` |
| Fail-open | ❌ | 需 DB 故障注入 |

#### §5.4 隔离

| 资源 | 状态 | 测试 |
|------|------|------|
| Session IDOR | ✅ | `session_cross_user_isolation` |
| Memory 代理 | ✅ | `memory_proxy_user_isolation` |
| Team | ✅ | `team_cross_user_isolation` |
| Events/Audit/Activity | ✅ | `events_and_audit_cross_user_isolation` |
| Skills | ✅ | `skills_cross_user_isolation` |
| Run 控制 | ✅ | `run_cross_user_isolation` + `run_cancel_cross_user_and_owner` |
| Resource usage | ✅ | `resource_usage_per_user_isolation` |
| Multi-Account 租户 | ❌ | 部署层手工 |

#### §5.5 Session Reaper

| 项 | 状态 | 测试 |
|----|------|------|
| active→idle→ended | ✅ | `reaper_marks_stale_active_session_idle_then_ended` |
| ended→deleted / 文件清理 | 🟡 | reaper 单元 + cleanup smoke |
| batch_limit | 🟡 | 单元级 |
| Admin cleanup 与 reaper 一致 | 🟡 | cleanup smoke，未断言 reaper 等价 |

#### §5.6 模型托管

| 项 | 状态 | 测试 |
|----|------|------|
| `GET /models` 无 Key | ✅ | `models_list_and_key_encryption` |
| Admin 加载 + Fernet | ✅ | 同上 + DB 断言 |
| 模型路由 / Trusted domains | ❌ | 需 routing E2E |
| Rate limit cooldown | 🟡 | `unhappy_llm_behaviors.rs` 单元 |

#### §5.7 Memoria 代理

| 项 | 状态 | 测试 |
|----|------|------|
| 记忆注入 | 🟡 | stub 返回空 memories |
| 多用户隔离 | ✅ | `memory_proxy_user_isolation` |
| Memoria 宕机降级 | ✅ | `memoria_proxy_degradation` |

**§5 小结**：`test-saas` 内约 **88%**。

---

### 3.3 第三层：端到端场景（§6）

| 场景 | 状态 | 说明 |
|------|------|------|
| CLI SaaS 路径 | ❌ | 无 CLI 自动化 |
| SDK E2E | 🟡 | `test:integration:saas`（需 npm + server） |
| Web UI 一致 | ❌ | 手工 |
| Admin 冷启动 30min | 🔧 | 部署验收 |
| 多 Edge 协作 | 🟡 | lease 测试部分覆盖 |
| Gateway 企微 | ❌ | Out of Scope |
| 配额 UX 文案 | ✅ | 429 + detail 含 token/session |

**§6 小结**：约 **35%**。

---

### 3.4 第四层：运维规模化（§7）

| 类别 | 状态 |
|------|------|
| K8s / HPA / 多副本 | ❌ 手工 |
| 性能基线 §7.2 | ❌ |
| 混沌 §7.3 | ❌ |
| `/health` | ✅ `platform_health_and_auth_me` |
| Prometheus / introspection | 🟡 其他 matrix，未进 test-saas |
| 安全专项 §7.5 | 🟡 部分（IDOR、Key 加密）；TLS/渗透 ❌ |

**§7 小结**：约 **15%**。

---

## 4. 总体覆盖率估计

| 层级 | test-saas 内 | 含 matrix 全库 |
|------|-------------|----------------|
| §4 Cloud 协议 | ~75% | ~85% |
| §5 平台能力 | ~88% | ~90% |
| §6 客户端 E2E | ~35% | ~40% |
| §7 运维规模化 | ~15% | ~20% |
| **加权（GA 必测 §4+§5 为主）** | **~82%** | **~85%** |

> **测试计划功能点**见上文 §4 估计（~82%）。**代码行覆盖率**见 §7（llvm 实测）。

---

## 5. 待补充项（优先级）

### P0 — 建议下一批纳入 `make test-saas`

1. `edge_cloud_round_trip_e2e` 纳入 `make test-saas` 或独立 wrapper target
2. ~~`POST /sessions/{id}/replay`~~ → ✅ `e2e_matrix_saas_session_replay_post_positive`
3. `max_concurrent_bash` / `max_disk_bytes` HTTP 拒绝路径
4. ~~`/admin/feedback/stats` smoke + RBAC~~ → ✅ `e2e_matrix_saas_admin_feedback_stats_rbac`
5. SDK `test:integration:saas` 在 CI/本地固定跑（需 Node）

### P1

6. 同 Session 并发 Turn 隔离 → 纳入 test-saas（`e2e_matrix_same_session_concurrent_turns_isolated`）
7. Cloud Sync push/pull E2E
8. `ASTRA_ALLOW_INSECURE_DEFAULTS=0` 启动失败测试
9. ResourceGovernor fail-open（DB 短暂不可用）
10. Reaper ended→deleted + 文件系统断言

### P2 / 手工豁免

11. Multi-Account 租户隔离
12. K8s / HPA / 混沌 / 性能
13. CLI 8 场景、Web UI、Gateway
14. `/chat/ws` WebSocket E2E

---

## 6. 运行说明

```bash
# 标准入口（需 MatrixOne + .env）
make test-saas

# LLVM 代码行覆盖率（需: cargo install cargo-llvm-cov）
make test-saas-coverage
# → coverage/saas-llvm/summary.txt
# → coverage/saas-llvm/html/index.html

# 若 dev-api 已占用 DB 连接池：
make dev-stop
```

**Makefile 默认 env**：`ASTRA_DB_POOL_MAX_CONNECTIONS=5`、`ASTRA_DB_GLOBAL_MAX_CONNECTIONS=10000`（避免串行 E2E 触发全局连接 cap）。

---

## 7. LLVM 代码行覆盖率（实测）

> **命令**：`make test-saas-coverage`（2026-06-03，37/42 matrix + reaper；5 用例失败仍出报告）  
> **工具**：`cargo-llvm-cov 0.8.7` · **范围**：`-p astra-runtime -p astra-services`

### 7.1 汇总

| 范围 | Lines | Hit | **Line Cover** |
|------|-------|-----|----------------|
| **两 crate 合计** | 89,752 | 19,072 | **21.25%** |
| **SaaS 核心 30 文件** | 15,597 | 7,603 | **48.7%** |

SaaS 核心 = auth、resource_governor、admin、chat、edge callback、session、team、skills、task lease、replay、runs、models 等 handler/service 文件（详见 `summary.txt`）。

### 7.2 核心模块行覆盖率（节选）

| Cover | 模块 |
|-------|------|
| 100% | `resource_handlers.rs` |
| 92% | `services/storage.rs` |
| 85% | `services/auth/jwt.rs` |
| 65% | `services/resource_governor.rs` |
| 56% | `admin_handlers.rs` |
| 55% | `run/lifecycle.rs` |
| 50% | `services/auth/mod.rs` |
| 48% | `chat_handlers.rs` |
| 42% | `edge_callback_handlers.rs`、`session_reaper.rs` |
| 33% | `auth_handlers.rs` |
| 16% | `task_handlers.rs` |
| 10% | `task_lease.rs` |
| 8% | `session_handlers.rs` |

### 7.3 解读

- **21% crate 合计**：E2E 未触达 WS、LLM client、tool executor 等大模块。
- **49% SaaS 核心**：更能回答「case 对对应功能的代码覆盖」。
- 测试计划 **~82%**（测项） vs 代码行 **~49%**（核心）：E2E 覆盖「路径」不覆盖「全部分支」。

### 7.4 LLVM 查漏新增（2026-06-03）

| 用例 | 目标模块（跑前 cover） | 覆盖路径 |
|------|------------------------|----------|
| `session_replay_post_positive` | `replay.rs` ~38% | POST `/sessions/{id}/replay` + 跨用户 403 |
| `admin_feedback_stats_rbac` | `admin_handlers.rs` ~56% | GET `/admin/feedback/stats` RBAC + filter |
| `run_projection_smoke` | `run/handlers.rs` ~21% | GET `/chat/runs/{id}/projection` |
| `session_audit_after_chat_smoke` | `audit_handlers.rs` ~8% | audit summary/turns/tools/errors + `/audit/stats` |

### 7.5 历史：跑 coverage 时曾失败的 5 个用例（已修复）

| 用例 | 原因 |
|------|------|
| `edge_tool_result_success_path` | API 现需 `result_hash` |
| `resource_*_cap_*`（3 个） | POST /chat 未返回 429 |
| `resource_usage_per_user_isolation` | `sessions_created` 未 increment |

---

## 8. 变更记录

| 日期 | 变更 |
|------|------|
| 2026-06-03 | 新增 `make test-saas-coverage`；实测 line cover 21.25%（crate）/ 48.7%（SaaS 核心 30 文件） |
| 2026-06-02 | 初版：42 matrix + 1 reaper；新增 10 项 coverage 测试；总体 §4+§5 约 82% |
