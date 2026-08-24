# Astra SaaS 能力测试计划

> **目的**：验证 Astra 作为 **托管云服务（SaaS）** 对外交付时，平台侧能力是否达标——用户通过 CLI / SDK / Web 连接云端 API，而非直连数据库。
>
> **范围**：认证与租户隔离、Cloud 运行时、资源治理、Admin 运维、多客户端协议、部署拓扑、可观测性与安全。
>
> **说明**：本文聚焦 **SaaS 发布质量验证**，不涉及 CI 流水线或 PR 门禁；**专门刻画 SaaS 形态下的必测项**。
> **相关文档**：[部署架构](../design/deployment-architecture.md) · [Edge-Cloud 分执行](../design/edge-cloud-execution.md) · [多 Agent Cloud Runtime](../design/multi-agent-cloud-runtime.md) · [信任与安全](../design/trust-and-safety.md) · [系统 E2E 矩阵](./system-e2e-matrix.md)

---

## 1. 什么是「Astra SaaS 能力」

Astra SaaS 指 MatrixOrigin 托管的 **Agent Runtime 云服务**，与本地 `--local`（CLI 直连 DB）相对：

```
┌─────────────────────────────────────────────────────────────────┐
│  客户端（Edge / Thin Client）                                     │
│  astra CLI · @astra/sdk · Web UI · IDE 插件 · Gateway（企微等）   │
└────────────────────────────┬────────────────────────────────────┘
                             │ HTTPS + JWT
┌────────────────────────────▼────────────────────────────────────┐
│  Cloud（astra-server）— SaaS 核心价值所在                         │
│  认证/RBAC · /chat/turn · 上下文组装 · LLM（Key 不出云）          │
│  记忆代理 · 模型路由 · 预算/限流 · 审计 · 持久化（MatrixOne）      │
│  资源治理 · Admin · 运行时存储维护 · 多 Agent Run / Lease       │
└────────────────────────────┬────────────────────────────────────┘
                             │
              MatrixOne · Redis · Memoria ·（可选 Skill Worker）
```

**SaaS 与单机/私有化部署的区别**（测试必须覆盖）：

| 维度 | 非 SaaS（`--local` / 直连 DB） | SaaS（默认路径） |
|------|-------------------------------|------------------|
| 入口 | CLI → MatrixOne | CLI/SDK → `astra-server` API |
| 认证 | 无 / DB 凭证 | JWT + Refresh + RBAC |
| LLM Key | 可能在客户端 | **仅 Cloud 持有**，Fernet 加密存 DB |
| 工具执行 | 本地 | **Edge 本地** + Cloud 编排 |
| 多租户 | 不适用 | MatrixOne Multi-Account + API 用户隔离 |
| 配额 | 无 | ResourceGovernor（Token/Session/并发） |
| 运维 | 开发者自管 | Admin API、`astra admin`、K8s HPA |
| 审计 | 不完整 | 100% Turn 经 API 写入 audit chain |

---

## 2. SaaS 发布质量标准

| 质量维度 | 发布标准 | 主要验证手段 |
|----------|----------|--------------|
| **接入与认证** | 注册/登录/刷新/登出；未授权 401/403 明确 | Auth E2E + 安全测试 |
| **Thin Client 协议** | CLI、SDK、Web 共用同一 HTTP 协议，无 hidden state | SDK E2E + §4 场景 |
| **Cloud 运行时** | `/chat/turn` enrichment、SSE、审计链完整 | Edge-Cloud E2E（system_matrix + bridge hooks） |
| **租户与用户隔离** | 用户 A 无法读/写用户 B 的 Session/Memory/Team | 隔离矩阵测试 |
| **资源治理** | 超配额拒绝；Admin 可 override；计量准确 | ResourceGovernor + Admin API |
| **Admin 运维** | 冷启动、模型加载、用户/Token/配置管理 | Admin smoke + 部署验收 |
| **弹性与可用性** | 多副本无状态；HPA 扩缩；依赖故障可恢复 | K8s 压测 + 混沌 |
| **可观测性** | 健康检查、指标、审计导出、SLO 可查询 | 监控 + introspection API |
| **安全合规** | 生产密钥策略、Key 加密、Edge 信任边界 | 安全专项 + 渗透 |
| **数据生命周期** | 用户会话保留、诊断数据过期、运行时存储维护 | 生命周期测试 + Admin cleanup |

---

## 3. 测试分层

```
┌──────────────────────────────────────────────────────────────┐
│  第四层：SaaS 运维与规模化（K8s、HPA、多副本、监控告警）         │
├──────────────────────────────────────────────────────────────┤
│  第三层：SaaS 端到端场景（多客户端 × 典型用户旅程）              │
├──────────────────────────────────────────────────────────────┤
│  第二层：SaaS 平台能力（认证、治理、Admin、隔离、存储维护）        │
├──────────────────────────────────────────────────────────────┤
│  第一层：Cloud 运行时协议（/chat/turn、回调、Run、Sync）        │
└──────────────────────────────────────────────────────────────┘
```

**原则：**
- **第一层、第二层** 决定 SaaS 能否对外开服。
- **第三层** 验证真实客户路径（CLI 开发者、SDK 集成方、Admin 运维）。
- **第四层** 决定能否承载生产流量（Beta/GA 全量前必做）。
- 测试环境：**类生产 SaaS 栈**（MatrixOne + Redis + Memoria + 多副本 API + 真实 LLM）。

---

## 4. 第一层：Cloud 运行时协议（SaaS 核心路径）

> 本节强调 **SaaS 视角的 Cloud 运行时必过项**。

### 4.1 `/chat/turn` — SaaS 价值载体

Cloud 每一 Turn 必须完成（非简单 LLM 代理）：

1. JWT 鉴权 + 限流
2. 持久化 Edge 上报的 `tool_results`
3. 上下文组装（Memoria、Skill 索引、few-shot）
4. 模型路由 + **预算门禁**（`ResourceGovernor.check_token_budget`）
5. LLM 调用（Key 不出 Cloud）
6. 防火墙 / 置信度
7. Audit（snapshot + decision + events）
8. SSE 返回（含 `ping` 长 Turn 保活）

| 测试项 | 通过标准 |
|--------|----------|
| 首 Turn 含 `project_rules` | Cloud 缓存并按 Session 注入 |
| 多轮 tool loop（≥3） | Edge↔Cloud 闭环；最终 `turn_complete` |
| 超 Token 日预算 | Cloud 拒绝；Edge 收到明确 `error`（非 silent 截断） |
| 并发同 Session Turn | 无 SSE/状态串话 |
| LLM 429/5xx | 重试 + fallback；SSE `error` 含 `retryable` |

**自动化：**
```bash
# Edge-Cloud 协议（bridge hooks，无真实 LLM）
cargo test -p astra-runtime --test edge_cloud_round_trip_e2e --features bridge-e2e-hooks

# 在线 Matrix E2E（含 chat/turn、callback、并发隔离）
ASTRA_TEST_DB_IT=1 cargo test -p astra-runtime --test system_matrix_http_e2e --features bridge-e2e-hooks -- --ignored
```

### 4.2 Edge 回调与 Agent 注册

| API | SaaS 意义 |
|-----|-----------|
| `POST /tools/result` | Edge 工具结果进入 Cloud 持久化与下一轮组装 |
| `POST /approval/respond` | 权限审批决策写入 journal |
| `POST /agents/edge` | Edge Agent 注册到 `edge_agent_registry` |
| Task Lease（claim/renew/release） | 多 Edge / 多副本下任务不重复执行 |

**自动化：** `e2e_matrix_edge_callback_*`、`e2e_matrix_tasks_lease_and_db_assertions`、`GET /edges/status`（SDK `getEdgesStatus()`）

### 4.3 Headless Cloud Run

SaaS 必须支持 **无终端后台任务**（Thin Client 只发 HTTP）：

| API | 验证 |
|-----|------|
| `POST /chat`（异步） | 返回 `run_id` |
| `POST .../pause` / `.../resume` | 状态机正确 |
| `POST .../delegate` | 子 Run 可列、可暂停 |
| `GET /runs` | 分页列表与 DB 一致 |

**自动化：** `e2e_matrix_chat_run_pause_resume_http`、`e2e_matrix_delegate_http_boundaries`

### 4.4 Cloud Sync

| 场景 | 通过标准 |
|------|----------|
| `sync push` / `sync pull` | Edge Session 与 Cloud 一致 |
| 断网后重连 | 增量补齐；无 duplicate events |
| 跨设备恢复 Session | Cloud 为 source of truth |

详见 [sync_protocol.md](../sync_protocol.md)。

---

## 5. 第二层：SaaS 平台能力

### 5.1 认证与用户生命周期

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| 注册 | `POST /auth/register` | 用户写入 `auth_users`；重复用户名 409 |
| 登录 | `POST /auth/login` | 返回 access + refresh token |
| Token 刷新 | `POST /auth/refresh` | 新 access token 有效；旧 refresh 轮换策略正确 |
| 登出 / 过期 | 过期 access token | 401；CLI/SDK 自动 refresh 后重试 |
| `GET /auth/me` | 当前用户信息 | 与 JWT `sub` 一致 |
| 生产配置 | `ASTRA_ALLOW_INSECURE_DEFAULTS=0` | 弱密钥启动失败 |

**客户端覆盖：**
```bash
# CLI
astra login --user alice --password ***
astra chat "hello"

# SDK（packages/sdk）
ASTRA_SDK_E2E=1 npm test --prefix packages/sdk
```

**自动化：** `product_matrix_*` 中 auth journey；`http_contract` 中 auth 形状

### 5.2 RBAC 与 Admin API

Admin 操作必须 **`astra_admin` 角色**，普通用户 403。

| Admin 路由 | 验证内容 | E2E 覆盖 |
|------------|----------|----------|
| `POST /admin/init` | 冷启动 schema | 部署验收 |
| `GET/POST /admin/tokens` | API Token 管理 | `e2e_matrix_admin_tokens_smoke` |
| `POST/PUT/DELETE /models` | 模型 CRUD + Key 加密 | `e2e_matrix_models_admin_crud` |
| `GET/PUT /admin/config/{key}` | 服务端配置 | contract + 手工 |
| `PUT /admin/resources/limits/{user_id}` |  per-user 配额 override | §5.3 |
| `POST /admin/users/grant-role` | 授予 admin | Admin smoke |
| `POST /admin/cleanup` | 触发过期数据清理 | 手工 + DB 断言 |
| `GET /admin/audit` | 审计导出 | 手工 |
| `GET /admin/feedback/stats` | 反馈统计 | 手工 |

**专项流程：**
```
1. 普通用户 GET /admin/tokens → 403
2. grant astra_admin → 200
3. POST /models（mock provider）→ infra_llm_models 有记录，api_key 已加密
4. revoke astra_admin → 再次 403
```

### 5.3 资源治理（ResourceGovernor）

SaaS 按用户计量与限流，表：`resource_limits`、`resource_usage`。

| 限额项 | 默认值（参考） | 测试场景 |
|--------|----------------|----------|
| `max_sessions_per_day` | 5000 | 第 5001 个 Session → Denied |
| `max_concurrent_sessions` | 100 | 第 101 个活跃 Session → Denied |
| `max_tokens_per_day` | 10,000,000,000 | 达到每日预算后，后续 Turn → Denied；显式 `0` 仍表示不限量 |
| `max_concurrent_bash` | 100 | 配置读写（执行侧并发限流尚未接入） |
| `max_disk_bytes` | 10 GiB | 配置读写（执行侧磁盘限流尚未接入） |

| 测试项 | 通过标准 |
|--------|----------|
| `GET /resources/usage` | 与 DB `resource_usage` 一致 |
| `GET /resources/limits` | 返回 effective limits（custom 或 default） |
| Admin `PUT /admin/resources/limits/{user_id}` | 立即生效；`0` = unlimited |
| 原子计数 | 并发 increment 无丢失（`ON DUPLICATE KEY UPDATE`） |
| Fail-open 策略 | DB 短暂故障时 log warning，不 crash 全站 |

**单元测试：** `resource_governor.rs` 内 `InMemoryResourceGovernor` / `DatabaseResourceGovernor` tests

### 5.4 多租户与用户隔离

> 应用层无 `tenant_id`；多租户由 **MatrixOne Multi-Account** 在部署层隔离（见 [trust-and-safety.md §10](../design/trust-and-safety.md)）。

**用户级隔离（每个 SaaS 实例必测）：**

| 资源 | 测试 |
|------|------|
| Session | 用户 A 的 `GET /sessions/{id}` 对用户 B → 404/403 |
| Memory | Memoria 代理以 JWT user 为准；body 伪造无效 |
| Team | `e2e_matrix_team_*` 跨用户 negative |
| Audit / Events | 无法读取他人 causal chain |
| Skills 配置 | user scope 互不可见 |
| Resource usage | 计量按 user_id 独立 |

**租户级隔离（Multi-Account 部署时额外必测）：**

| 测试项 | 通过标准 |
|--------|----------|
| Account A / Account B 独立连接串 | 数据完全不可见 |
| 同版本 API 连不同 Account | 行为一致，数据隔离 |
| Skill 配置 tenant scope（P2） | tenant 级 settings 不泄漏到其他 tenant |

### 5.5 用户会话保留与运行时存储维护

用户可见的 C1 会话历史（`agent_sessions`、对话事件及其关联状态）不会因空闲或年龄被后台任务改变或删除。显式删除会先在独立于会话根记录的 `agent_session_lifecycle_fences` 中持久化不可逆的删除意图；该栅栏在数据库删除完成后继续保留，所有可能创建或更新会话根的写路径必须在同一事务中锁定并检查它。若前台删除中断，后台在短暂 grace 后按该意图重试完成删除；已经排队的延迟写入不得复活会话。

C3 运行时诊断与 C1 历史分离：模型请求诊断（`model_request_context_events`）不包含消息或工具输出，完整 attempt 对保留 30 天；prompt 组装诊断（`prompt_request_records`、`prompt_deltas`）保留 90 天，并按子表先于父表的顺序删除。每个 owner/session 或 owner/harness scope 最多保留 2,048 条模型请求诊断；scope 压缩只删除完整的 accepted/terminal attempt 对，未完成 attempt 可暂时超过上限。30 天过期清理以 attempt 为原子单位，在该 attempt 的最新诊断事实超过保留期后删除其全部记录，因此进程崩溃留下的未完成 attempt 也会最终回收。后台维护由分布式 lease 保证同一周期只有一个服务实例执行，并以索引有序的候选批次工作；它还会清理过期 fork grace pin、孤儿 fork/manifest/reference，以及无引用 conversation segment。默认每类维护每次最多选取 500 个数据库对象或一个有界 attempt 批次，避免全表聚合和长期锁表。

**测试方式：**
- 单元：验证删除重试只按持久删除栅栏选取；运行时维护不会因 idle/ended/closed 删除 C1 会话；诊断压缩不会拆分 attempt 对；诊断候选查询有索引顺序和批次上限
- 集成：在会话删除前排入延迟事件，删除完成后再消费，断言会话不能复活；验证 90 天 prompt 父子诊断被删除而活跃会话和新诊断保留；创建孤儿运行时存储并触发维护，断言只有无引用对象被回收
- Admin：`POST /admin/cleanup` 处理其授权范围内的 TTL 数据；不得删除未显式请求删除的 C1 会话历史

### 5.6 模型与 LLM 托管

| 测试项 | 通过标准 |
|--------|----------|
| `GET /models` | 普通用户可见已激活模型列表（无 Key） |
| Admin 加载模型 | Key Fernet 加密；DB 无明文 |
| 模型路由 | Cloud 按任务复杂度/SLO 选模型 |
| Trusted domains | Admin 配置 LLM endpoint 白名单 |
| Rate limit cooldown | LLM 429 后 cooldown + fallback（见 `unhappy_llm_behaviors.rs`） |

### 5.7 Memoria 代理（Cloud 侧）

Edge **不直连** Memoria；Cloud 在 `/chat/turn` 内代理记忆检索与写入。

| 测试项 | 通过标准 |
|--------|----------|
| 记忆注入 | Turn 前 Cloud 检索并注入 system prompt |
| 多用户隔离 | user_id 来自 JWT |
| Memoria 宕机 | Chat 主链路可用；记忆功能明确降级/报错 |

---

## 6. 第三层：SaaS 端到端场景

### 6.1 开发者 CLI 接入（默认 SaaS 路径）

```
astra login
astra chat "分析 src/main.rs"
  → Cloud /chat/turn × N（含本地 read_file）
astra session list          → GET /sessions
astra replay <id>           → POST /sessions/{id}/replay
```

**验收：** 全程无 `--local`；`~/.astra/credentials.json` 有效；审计链完整。

### 6.2 SDK / Web 集成方

```
AstraClient.login → createSession → createRun → SSE/WS 收流
→ pauseRun → listRuns → getSessionEvents → getEdgesStatus
```

**验收：** `@astra/sdk` E2E 全绿；Web 与 CLI 对同一 Session 行为一致。

### 6.3 平台 Admin 冷启动

```
make stack-up-server-only
astra admin init
astra admin model load ...
curl /health → ok
注册用户 → Chat 成功
```

**验收：** 全新环境 **30 分钟内** 可对外提供 SaaS Chat。

### 6.4 多 Edge 协作

```
Edge-A 注册 → claim lease → 执行 tool
Edge-B 同 Session 并发 Turn → 隔离
Edge-A 断连 → lease 超时 → Edge-B 可 claim
```

**验收：** 无重复执行、无状态串话。

### 6.5 Gateway 渠道（若 SaaS 含消息网关）

```
企微/微信 → Gateway → Astra Cloud API → 回复
定时任务 /cron → 长任务 checkpoint → 崩溃 Outbox 重试
```

**验收：** 消息可靠；Gateway 用户与 Astra 用户映射正确。

### 6.6 配额与商业化前置

```
免费用户：创建 Session 至日限额 → 明确错误文案
Admin 提升 limits → 用户可继续
Token 消耗 → GET /resources/usage 准确
```

**验收：** 拒绝响应含可读 reason；无 silent 超支。

---

## 7. 第四层：SaaS 运维与规模化

### 7.1 部署拓扑验收

| 拓扑 | 必测项 |
|------|--------|
| Docker Compose（all-in-one） | 一键启动；Init → API 健康 |
| K8s + Helm | 2+ API 副本；外部 MatrixOne/Redis |
| HPA | CPU/RPS 触发扩容；扩容后 Session 粘性无要求（无状态） |
| 滚动升级 | 零 downtime；进行中 Run 可恢复或失败可感知 |

```bash
make stack-up-server-only

# K8s 最小 SaaS
helm install astra deployment/kubernetes/chart \
  --set matrixone.enabled=false \
  --set memoria.enabled=false
```

### 7.2 性能与容量（SaaS SLA 基线）

| 场景 | 目标 |
|------|------|
| `/health`、`GET /sessions` | P95 < 500ms @ 100 并发 |
| `/chat/turn` 首 token（TTFT） | P95 < 3s（视模型） |
| 长 Turn SSE | ping 保活；30s+ 不断连 |
| 50 租户 × 5 并发 Session | 无串话、无死锁 |
| 10 并行 Cloud Run | lease 不冲突 |
| 4h Soak（多副本） | 无连接泄漏、内存持续增长 |

### 7.3 混沌与依赖故障

| 故障 | SaaS 预期行为 |
|------|---------------|
| 单 API Pod 终止 | 其他副本接管；客户端重试成功 |
| MatrixOne 30s 不可用 | `/health` unhealthy；恢复后 OK |
| Redis 不可用 | 限流/cache 降级（按设计 fail-open/closed） |
| Memoria 不可用 | 记忆跳过；Chat 可用 |
| LLM 区域故障 | fallback_chain 切换模型 |

### 7.4 可观测性

| 项 | 验证 |
|----|------|
| `/health` | DB + Redis + 依赖状态 |
| Prometheus 指标 | RPS、延迟、错误率、Run 数 |
| `GET /introspection/*` | context trend、memory recall、retrieval quality |
| `GET /events` | Session 时间线可导出 |
| Agent SLO 表 | quality、hallucination_rate、latency（见 trust-and-safety §11） |

监控参考：`deployment/monitoring/`

### 7.5 安全专项

| 项 | 要求 |
|----|------|
| TLS | 生产强制 HTTPS |
| JWT Secret / Fernet Key | 非默认值；轮换流程文档化 |
| LLM Key | 不出 Cloud；Edge 日志无 Key |
| Edge 信任边界 | tool_result 标记 `source: edge` |
| CORS / CSRF | Web 客户端策略正确 |
| 渗透 | 常见 OWASP；跨用户 IDOR 全路径 |

---

## 8. 测试环境与数据

### 8.1 类生产 SaaS 环境

| 组件 | 要求 |
|------|------|
| astra-server | ≥2 实例（第四层） |
| MatrixOne | 独立测试 Account；可用 `ASTRA_DATABASE_PREFIX` 隔离 |
| Redis | 与生产同 major 版本 |
| Memoria | 真实实例或等价 test double |
| LLM | GA 主模型 + fallback 模型各一 |

### 8.2 测试账号矩阵

| 账号 | 角色 | 用途 |
|------|------|------|
| `user_free` | 普通 | 默认配额边界 |
| `user_premium` | 普通 + Admin 提升 limits |  unlimited / 高配额 |
| `user_admin` | `astra_admin` | Admin API |
| `user_b` | 普通 | 隔离测试 |

---

## 9. 自动化与手工分工

| 类别 | 自动化 | 手工 |
|------|--------|------|
| Auth / Session CRUD | `system_matrix_http_e2e` | — |
| /chat/turn + callback | `edge_cloud_round_trip_e2e`、matrix E2E | 长 Turn 实网 |
| Admin smoke | `e2e_matrix_admin_*`、`models_admin_crud` | init、audit 导出 |
| ResourceGovernor | `e2e_matrix_saas_resource_*` | 配额边界 UX |
| SDK | `ASTRA_SDK_E2E=1` | Web UI |
| K8s / HPA | — | 第四层专项 |
| 多租户 Multi-Account | — | 部署层专项 |
| Gateway | — | §6.5 |

**推荐发布前命令组合：**
```bash
make test-saas                                         # SaaS 平台专项（§5）
make test-offline                                      # 快速回归
make test-online                                       # Matrix E2E 全矩阵
make test-harness FORCE_MODEL=<主模型> PARALLEL=4    # Harness P0
ASTRA_SDK_E2E=1 npm test --prefix packages/sdk       # SDK SaaS 客户端
cargo test -p astra-runtime --test edge_cloud_round_trip_e2e --features bridge-e2e-hooks
cargo test -p astra-services --test services_db_integration -- --ignored  # 服务层 DB
```

**SaaS 专项自动化（`journey_saas_platform_matrix.rs`）：**

| 测试 | 覆盖 |
|------|------|
| `e2e_matrix_saas_resource_limits_read_and_admin_override` | GET /resources/* + Admin PUT limits |
| `e2e_matrix_saas_resource_daily_session_cap_denies_chat` | 日 Session 配额 → 429 + Admin 提升 |
| `e2e_matrix_saas_resource_concurrent_session_cap_denies_chat` | 并发 Session 配额 → 429 |
| `e2e_matrix_saas_admin_config_crud_rbac` | /admin/config CRUD + RBAC |
| `e2e_matrix_saas_admin_grant_revoke_rbac_flow` | grant/revoke astra_admin |
| `e2e_matrix_saas_resource_usage_per_user_isolation` | 用户级 usage 隔离 |
| `e2e_matrix_saas_auth_refresh_cycle` | Token 刷新 |
| `e2e_matrix_saas_session_cross_user_isolation` | Session IDOR（GET/PUT/cancel/DELETE） |
| `e2e_matrix_saas_events_and_audit_cross_user_isolation` | Events/Audit/Activity 隔离 |
| `runtime_maintenance_*` | 显式删除重试、C3 诊断过期与孤儿运行时存储回收 |
| `packages/sdk` `saas-remote.test.ts` | SDK 注册/Session/资源/隔离/Refresh（需 astra-server） |

---

## 10. 发布验收 Checklist（SaaS）

```
Cloud 协议（第一层）
[ ] /chat/turn SSE 协议 + 多轮 tool loop 通过
[ ] Edge 回调 + Lease + Run pause/resume 通过
[ ] Cloud Sync push/pull 一致
[ ] LLM Key 不出 Cloud；审计链 100% Turn 有 snapshot

SaaS 平台（第二层）
[ ] 认证全流程 + Token 刷新
[ ] Admin RBAC（403 → grant → 200 → revoke → 403）
[ ] ResourceGovernor 配额拒绝 + Admin override
[ ] 用户隔离矩阵全通过
[ ] 运行时存储维护 / cleanup 验证
[ ] Memoria 代理 + 降级

端到端（第三层）
[ ] CLI SaaS 路径（无 --local）8 场景
[ ] SDK E2E 全绿
[ ] 冷启动 30 分钟可 Chat
[ ] （可选）Gateway 消息闭环

规模化（第四层 — GA 全量前）
[ ] 多副本 + 滚动升级
[ ] 性能基线达标（§7.2）
[ ] 混沌测试通过（§7.3）
[ ] 监控告警 + SLO 看板就绪
[ ] 安全专项通过（§7.5）

灰度
[ ] Canary 24h 无 P0
[ ] Beta 租户反馈无未关闭 P0/P1
```

---

## 11. 排期参考

| 阶段 | 负责 | 工期 |
|------|------|------|
| Cloud 协议 + Edge-Cloud E2E | 后端 + QA | ~1.5 周 |
| Auth / Admin / 资源治理 | 后端 + QA | ~1.5 周 |
| 隔离 + 存储维护 + Memoria 代理 | 后端 + QA | ~1 周 |
| SDK / 多客户端场景 | 前端 + QA | ~1 周 |
| K8s 规模化 + 混沌 | SRE + QA | ~2 周 |
| 安全 + 渗透 | 安全 + QA | ~1 周 |
| **合计（SaaS GA）** | | **~8 周** |

Edge-Cloud 与 Engine 能力重叠部分只测一次。

---

## 12. 能力 ↔ 测试映射

| SaaS 能力 | 设计文档 | 自动化 | 手工场景 |
|-----------|----------|--------|----------|
| Edge-Cloud 分执行 | edge-cloud-execution.md | edge_cloud_round_trip_e2e | §6.1 |
| Thin Client / SDK | multi-agent-cloud-runtime §5.5 | `saas-remote.test.ts` + `test:integration:saas` | §6.2 |
| JWT + RBAC | deployment-architecture §1.1 | product_matrix auth | §5.1 |
| Admin 运维 | router_builder `/admin/*` | admin_smoke, models_admin_crud | §6.3 |
| 资源治理 | resource_governor.rs | `e2e_matrix_saas_resource_*` | §5.3, §6.6 |
| 多租户 | trust-and-safety §10 | — | §5.4 Multi-Account |
| 运行时存储维护 | runtime_maintenance.rs | 单元 + 集成 | §5.5 |
| Headless Run | multi-agent-cloud-runtime §5.3 | chat_run_pause_resume | §4.3 |
| Task Lease | sync_protocol / §9 | tasks_lease E2E | §6.4 |
| Memoria 代理 | edge-cloud-execution | memory_full_lifecycle | §5.7 |
| K8s 部署 | deployment-architecture §Topology 3 | — | §7.1 |
| Gateway | astra-gateway README | — | §6.5 |
| Agent SLO | trust-and-safety §11 | introspection API | §7.4 |

---

## 13. 已知缺口（发布前需补齐或书面豁免）

| 缺口 | 影响 | 状态 |
|------|------|------|
| ResourceGovernor HTTP E2E | 配额行为仅靠单元测试 | ✅ `e2e_matrix_saas_resource_*` |
| `/admin/config` 深度 E2E | Admin 配置回归风险 | ✅ `e2e_matrix_saas_admin_config_crud_rbac` |
| 无引用运行时存储集成 | 可达性回收 | 待补：显式删除会话后的 orphan 回收 E2E |
| `/chat/ws` 无 system E2E | Web 实时通道 | 待补：SDK WS 测试 |
| Skill tenant-scope admin（P2） | 企业多租户配置 | GA 若不含则 Out of Scope |
| Multi-Account 无自动化 | 租户隔离 | 专项手工 + 脚本化 smoke |
| Marketplace install/rollback | 技能生态 SaaS | Beta 后迭代 |
