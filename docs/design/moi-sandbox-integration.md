# MOI Sandbox Integration

> **Code Review 文档**  
> 本文档描述本 PR 为支持 MOI sandbox 场景引入的主要改动点与设计取舍，供 code review 使用。  
> 文档内容以 PR 当前 diff 为准；若与实现不一致，请以代码为准。

---

## 背景

MOI sandbox 是在 Kubernetes pod 内运行的隔离执行环境。每个 sandbox pod 内置一个 `astra-edge` 进程，通过 WebSocket 连接到 astra-server，将 AI agent 的 tool call（bash、read_file 等）代理到 pod 内本地执行。

```
用户浏览器
    │
    ▼
moi-backend / moi-core (matrixflow)
    │  HTTP POST /agents/turns
    │  Authorization: Bearer moi-user-token-v1.xxx        ← runtime token
    │  X-Astra-External-Provider: moi
    │  X-Astra-External-Action: authorize_request
    ▼
astra-server
    │  WebSocket /edge/ws  ←────────────────────────────────────────────────┐
    │                                                                         │
    │  工具分发 (DB edge dispatch / in-memory pool)                           │
    │                                                                         │
    └─► astra-edge (运行在 sandbox pod 内)                                   │
            │  Authorization: Bearer moi-user-token-v1.xxx   ← edge-registration token
            └───────────────────────────────────────────────────────────────►┘
```

astra-server 目前（github/main）已有 **server-to-server** 认证路径（matrixflow 用 HTTP header 调用 astra），本次改动在此基础上新增 **edge WebSocket 认证路径**，并修复与多 pod 部署相关的若干 bug。

---

## Token 类型说明

本次改动涉及两种格式相同、但用途不同的 `moi-user-token-v1.*` token：

| 类型 | 发行方 | TTL | 用途 | `purpose` 字段 | `edge_agent_id` |
|------|--------|-----|------|----------------|-----------------|
| **Runtime token** | moi-catalog | 10 min | matrixflow → astra HTTP API 调用（已有） | `"runtime"` | 无 |
| **Edge-registration token** | moi-backend | 30 天 | astra-edge → astra-server WebSocket 注册（本次新增） | `"edge_registration"` | 绑定 edge id |

两类 token 格式相同，通过 `purpose` 字段和 `edge_agent_id` 字段区分。Edge-registration token 还携带 `jti` 用于撤销。

---

## 改动总览

| 编号 | 类型 | 标题 | 核心文件 |
|------|------|------|---------|
| F1 | Feature | MOI edge-registration token WebSocket 认证 | `auth/mod.rs`, `edge_ws_handler.rs` |
| F2 | Feature | Upstream HTTP proxy 支持 | `astra-edge/main.rs`, `astra-thin-client/client.rs` |
| F3 | Feature | ExternalAuthorizedRequest runtime context 按 token 类型分流 | `external_runtime_context.rs`, `auth/mod.rs` |
| F4 | Feature | Edge agent allow_tools schema 动态注入 | `server_loop_host.rs`, `lifecycle/mod.rs` |
| F5 | Feature | 跨用户 edge dispatch（sandbox 服务账号场景） | `tool_edge_transport.rs`, `edge_connection_pool.rs` |
| F6 | Feature | list_catalog / issue_runtime_context by scope | `auth/external.rs`, `auth/mod.rs` |
| F7 | Feature | `edge_agent` CapabilityDescriptor 字段 | `services/src/runs.rs` |
| F8 | Feature | EdgeWs transport 路由强制走 EdgeBound | `tool_route_selection.rs` |
| B1 | Bug Fix | 连接池 generation 防竞态清理 | `edge_connection_pool.rs` |
| B2 | Bug Fix | DB 注册失败拒绝 WebSocket 连接 | `edge_ws_handler.rs` |
| B3 | Bug Fix | heartbeat 按 edge_id 隔离，防旧连接刷新 | `edge_registry.rs` |
| B4 | Bug Fix | UnconfiguredEdgeRegistryService 改为 no-op 成功 | `edge_registry.rs` |
| B5 | Bug Fix | edge_dispatch poll 快路径（MatrixOne FOR UPDATE 慢查询） | `edge_dispatch.rs` |

---

## 详细说明

### F1 — MOI edge-registration token WebSocket 认证

**文件：** `rust/crates/services/src/auth/mod.rs`, `rust/crates/runtime/src/server/edge/edge_ws_handler.rs`

#### 问题

astra-edge 在 sandbox pod 内启动后，需要通过 WebSocket `/edge/ws` 向 astra-server 注册。注册时需要出示凭证。astra-server 的 `current_principal()` 只能解码 astra 自己签发的 JWT，无法验证 moi-backend 签发的 `moi-user-token-v1` 格式的 edge-registration token。

#### 改动

**`auth/mod.rs`：**

```rust
// current_principal() 新增前置分支
if token.starts_with("moi-user-token-v1") {
    return self.principal_from_edge_token(token).await;
}
// 之后原有的 JWT decode 路径不变
```

`principal_from_edge_token()` 通过已配置的 external provider（id = `"moi"` 或 first）调用 `authorize_request`，将 token 送到 matrixflow 验证，成功后构造 `ExternalAuthorizedRequest` principal。

新增 `edge_registration_binding()` trait 方法：

```rust
async fn edge_registration_binding(
    &self,
    token: &str,
) -> Result<Option<(String, String)>, ...>
// Ok(Some((edge_agent_id, workspace_id))) — 有绑定，必须匹配
// Ok(None) — 无绑定（内部 JWT），跳过检查
// Err(_) — token 被拒绝（已撤销/无效）
```

`DatabaseAuthService` 实现再次调用 `authorize_request`，读取响应里的 `edge_agent_id` 和 `provider_scope_id`（workspace_id）。默认实现返回 `Ok(None)`，使不支持 external auth 的部署不受影响。

**`edge_ws_handler.rs`（Phase 1.5）：**

astra-edge 在 Auth 消息里自报 `edge_agent_id`。Phase 1.5 在 Auth 成功之后、Pool 注册之前，验证自报值与 token 绑定值是否一致：

```
Phase 1  : 解析 Auth 消息，current_principal() 验证 token
Phase 1.5: edge_registration_binding() 取 token 绑定的 edge_agent_id
           比对自报值，不一致 → 发 AuthError 并关闭
Phase 2  : 注册进 EdgeConnectionPool（携带 workspace_id）
Phase 2a : 写入 DB edge registry
Phase 2b : 启动 cross-pod dispatch relay
```

**防护目的：** 同一 token 的持有者不能冒充不同的 edge id。

#### 影响范围

现有 astra JWT 不以 `"moi-user-token-v1"` 开头，`current_principal()` 的原有路径完全不变。`edge_registration_binding()` 默认返回 `Ok(None)`，未实现 external auth 的部署无需修改。

---

### F2 — Upstream HTTP proxy 支持

**文件：** `rust/crates/astra-edge/src/main.rs`, `rust/crates/astra-thin-client/src/client.rs`

#### 问题

IDC 部署的 sandbox pod 运行在受限网络 namespace 中，唯一出口是 HTTP 代理（`HTTP_PROXY` / `http_proxy` 环境变量）。原有代码在两处显式调用 `.no_proxy()`，绕过了系统代理：

1. `astra-edge` 连接 astra-server WebSocket 时使用 `connect_async()`，不走代理
2. `astra-thin-client` 的 `streaming_http_client()` 和 `ThinClient::new()` 均调用 `.no_proxy()`

#### 改动

**`astra-edge/src/main.rs`：**

新增 `connect_via_proxy()` 函数，实现 HTTP CONNECT 隧道：
1. 读取 `http_proxy` / `HTTP_PROXY` 环境变量
2. TCP 连接到代理地址
3. 发送 `CONNECT target:port HTTP/1.1` 握手
4. 读取 `200 Connection Established` 响应
5. 在隧道上执行 WebSocket upgrade

连接入口统一为：
```rust
let ws_stream = if let Some(proxy_url) = proxy {
    connect_via_proxy(&url, &proxy_url).await?
} else {
    let (ws, _) = connect_async(&url).await?;
    ws
};
```

**`astra-thin-client/src/client.rs`：**

- `streaming_http_client()`：移除 `.no_proxy()`，保留注释说明禁止恢复的原因（sandbox 网络隔离）
- `ThinClient::new()`：`Client::builder()` 移除 `.no_proxy()`

同时添加了 `no_proxy` guard 测试，从源码层面阻止任何人误加回 `.no_proxy()`：
```rust
fn streaming_http_client_body_no_comments() -> String { ... }

#[test]
fn streaming_http_client_does_not_call_no_proxy() {
    let body = streaming_http_client_body_no_comments();
    assert!(!body.contains(".no_proxy()"), ...);
}
```

#### 影响范围

无代理环境（`HTTP_PROXY` 为空）：行为与原来完全等价。有代理环境：请求现在经过代理路由，这是修正而非破坏。

---

### F3 — ExternalAuthorizedRequest runtime context 按 token 类型分流

**文件：** `rust/crates/runtime/src/server/external_runtime_context.rs`, `rust/crates/services/src/auth/mod.rs`

#### 背景

`inject_effective_runtime_context_body()` 在 streaming chat 端点被调用，负责向请求 body 注入 runtime context（model gateway、MCP、skills 地址等）。

github/main 对 `ExternalAuthorizedRequest` principal 的处理是直接透传 body，因为 matrixflow 调用 astra 时已在 JSON body 里携带了 `capability_descriptors`，astra 无需再做注入。

但 astra-edge 以 edge-registration token 调用 astra HTTP API 时，同样产生 `ExternalAuthorizedRequest` principal，却没有 session，body 里也没有 capability descriptors。它需要通过 `_by_scope` 路径向 matrixflow 获取 runtime context。

#### `AuthExternalAuthorizedRequestContext` 结构变更

```rust
pub struct AuthExternalAuthorizedRequestContext {
    pub provider_id: String,
    pub external_subject: String,
    pub provider_scope_id: String,
    pub request_authorization_id: String,
    // 新增：edge-registration token 携带此字段，runtime token 为 None
    pub edge_agent_id: Option<String>,
}
```

`AuthPrincipal` 新增方法：
```rust
pub fn is_edge_registration(&self) -> bool {
    matches!(
        &self.origin,
        AuthPrincipalOrigin::ExternalAuthorizedRequest(ctx) if ctx.edge_agent_id.is_some()
    )
}
```

两个构造 `AuthExternalAuthorizedRequestContext` 的位置：
- `current_principal_for_request()`（server-to-server 路径）：`edge_agent_id: None`
- `principal_from_edge_token()`（edge WebSocket 路径）：`edge_agent_id: authorized.edge_agent_id`

#### 分流逻辑

```rust
// external_runtime_context.rs
if principal.is_external_authorized_request() {
    if principal.is_edge_registration() {
        // edge-registration token：无 session，通过 by_scope 获取 runtime context
        return inject_authorized_request_runtime_context_body(state, principal, body).await;
    }
    // runtime token（matrixflow server-to-server）：body 已有 descriptors，直接透传
    return Ok(body);
}
```

#### 影响范围

分流逻辑确保：
- matrixflow server-to-server 调用（runtime token）：行为与 github/main 完全一致（透传）
- astra-edge HTTP 调用（edge-registration token）：新路径，获取并注入 runtime context

---

### F4 — Edge agent allow_tools schema 动态注入

**文件：** `rust/crates/runtime/src/server/server_loop_host.rs`, `rust/crates/runtime/src/server/run/lifecycle/mod.rs`, `rust/crates/runtime/src/server/run/binding_resolution.rs`

#### 问题

当 chat 请求指定 `executor_binding.kind = EdgeAgent` 时（agent-binding 模式），`ServerAgenticLoopHost` 默认只安装 MCP schema，不安装 edge builtin tools（bash、read_file 等）。MOI 调用方在请求里带 `allow_tools: ["bash", "read_file"]` 时，模型看不到这些工具，tool call 失败。

#### 改动

**`server_loop_host.rs`：**

新增 `merge_allowlisted_edge_tool_schemas()` 方法：
1. 只在 `executor_binding.kind == EdgeAgent` 时有效
2. 取 `allow_tools` 中枚举的工具名与 capability-filtered 工具集的**交集**
3. 将交集中的工具 schema 注入到 host 的 `edge_tools` 列表

严格白名单：`allow_tools` 里必须显式列出工具名，不支持通配符，不允许调用方声明超过 capability set 的工具。

**`binding_resolution.rs`：**

新增 `request_needs_edge_bound_server_executor()` 函数：当请求携带 `allow_tools` 且 executor binding 类型为 `EdgeWs` 时，返回 true，触发 server executor 初始化（agent-binding 模式下也需要初始化 ServerToolExecutor 作为 scratch workspace）。

**`lifecycle/mod.rs`：**

在 run lifecycle 启动时调用 `merge_request_scoped_edge_tool_schemas()`，将 allow_tools 的交集 schema 合并到 host。

#### 影响范围

仅当 `executor_binding.kind == EdgeAgent` 且请求携带非空 `allow_tools` 时触发，现有路径不受影响。

---

### F5 — 跨用户 edge dispatch（sandbox 服务账号场景）

**文件：** `rust/crates/runtime/src/server/tool_edge_transport.rs`, `rust/crates/astra-server-types/src/edge_connection_pool.rs`

#### 问题

sandbox 中的 astra-edge 使用 moi-backend 发行的 edge-registration token 连入，该 token 的 `sub` 字段是 moi-backend 的服务账号（形如 `external_authorized:moi:svc-xxx`）。但发起 chat turn 的是工作区用户（形如 `external_authorized:moi:user-yyy`）。

原有 `try_edge_websocket()` 只在**当前用户**的 pool entries 里查找 edge，找不到 → `Unavailable`，导致 sandbox tool call 失败。

#### 改动

**`edge_connection_pool.rs`：**

`EdgeConnection` 新增字段：
```rust
pub workspace_id: Option<String>,   // 来自 edge-registration token 的 provider_scope_id
pub generation: u64,                // 单调递增，用于防竞态清理（见 B1）
```

`register_with_capabilities()` 新增参数 `workspace_id: Option<String>`，返回 `u64`（generation）。

新增 `find_edge_by_agent_id()` 方法：
```rust
pub fn find_edge_by_agent_id(
    &self,
    edge_agent_id: &str,
    workspace_id: Option<&str>,
) -> Option<(String, EdgeConnectionInfo)>
```

workspace 授权规则：若请求方和 edge 都有 `workspace_id`，二者必须相等；任一方为 None 时跳过检查（向后兼容内部 token）。

**`tool_edge_transport.rs`：**

`try_edge_websocket()` 新增 fallback 逻辑：
1. 先在当前用户的 pool entries 查找（原逻辑）
2. 若为空且 `plan.selected_executor_id()` 有值，调用 `find_edge_by_agent_id()` 跨用户查找
3. 找到则以找到的 `owner_user_id` 发起 dispatch

#### 影响范围

fallback 仅在当前用户没有 edge 连接时触发，且需要明确指定 `executor_id`；现有走 user-scoped 查找的路径不变。

---

### F6 — list_catalog / issue_runtime_context by scope

**文件：** `rust/crates/services/src/auth/external.rs`, `rust/crates/services/src/auth/mod.rs`

#### 问题

`external_catalog()` 和 `external_runtime_context()` 都需要 session 句柄。Edge-registration token 产生的 `ExternalAuthorizedRequest` principal 没有 session，无法调用这两个接口。

#### 改动

**`auth/external.rs`：**

`ExternalProviderClient` trait 新增两个方法：
```rust
async fn list_catalog_by_scope(
    &self,
    provider: &ExternalAuthProviderConfig,
    provider_scope_id: String,
    external_subject: String,
) -> Result<ExternalCatalogResponse, ...>

async fn issue_runtime_context_by_scope(
    &self,
    provider: &ExternalAuthProviderConfig,
    provider_scope_id: String,
    external_subject: String,
    request: ExternalRuntimeContextRequestData,
) -> Result<ExternalRuntimeContextResponse, ...>
```

`HttpExternalProviderClient` 实现分别 POST 到 provider 的 `list_catalog_by_scope` / `issue_runtime_context_by_scope` action endpoint（需要 matrixflow 侧同步实现这两个 action）。

`ExternalAuthorizeRequestResponse` 新增字段：
```rust
pub edge_agent_id: Option<String>,
```
Provider 在验证 edge-registration token 时，将 token payload 中绑定的 `edge_agent_id` 写入此字段，astra edge WebSocket handler 从中提取后做 Phase 1.5 校验。

**`auth/mod.rs`：**

`AuthService` trait 新增两个方法（默认返回 NOT_IMPLEMENTED）：
```rust
async fn external_catalog_by_scope(&self, principal: &AuthPrincipal) -> ...
async fn external_runtime_context_by_scope(&self, principal: &AuthPrincipal, request: ...) -> ...
```

`DatabaseAuthService` 实现从 `ExternalAuthorizedRequest` context 中提取 `provider_scope_id` + `external_subject`，转发给 `external_client`。

#### 影响范围

新增 trait 方法有默认实现（返回 NOT_IMPLEMENTED），不实现 external auth 的 stub 零成本兼容。`_by_scope` 接口只在 F3 的 edge-registration token 分支下被调用。

---

### F7 — `edge_agent` CapabilityDescriptor 字段

**文件：** `rust/crates/services/src/runs.rs`

`RuntimeCapabilityDescriptorsRequest` 新增可选字段：
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub edge_agent: Option<RuntimeCapabilityDescriptorRequest>,
```

该字段由 moi-core catalog 在选定 sandbox executor 时填入，携带 edge agent id 和 transport 类型。astra-server 读取后在 `binding_resolution.rs` 中建立 `ExecutorBinding(EdgeAgent)`，将 tool dispatch 路由到对应 edge agent。

---

### F8 — EdgeWs transport 路由强制走 EdgeBound

**文件：** `rust/crates/runtime/src/server/tool_route_selection.rs`

#### 问题

`routing_decision()` 在 `WorkspaceBindingKind::ServerSandbox` 时将 RuntimeExecutor 工具路由到 `ServerLocal`。但当 executor transport 为 `EdgeWs` 时，意味着已经有 edge agent 连接，工具必须派发到该 agent；`ServerLocal` 路由在 agent-binding 模式下找不到本地适配器，导致 bash/read_file 等工具调用失败。

#### 改动

在 `routing_decision()` 的 workspace 判断之前，先检查 transport：

```rust
if matches!(request.executor.transport, ToolTransportKind::EdgeWs) {
    return ToolExecutionRouteKind::EdgeBound;
}
```

新增测试覆盖 `ServerSandbox` 和 `EdgeWorkspace` 两种 workspace 在 `EdgeWs` transport 下均路由到 `EdgeBound`。

---

### B1 — 连接池 generation 防竞态清理

**文件：** `rust/crates/astra-server-types/src/edge_connection_pool.rs`, `rust/crates/runtime/src/server/edge/edge_ws_handler.rs`

#### 问题

edge WebSocket 断开时，handler 调用 `pool.unregister()` 清理 pool entry。若同一 edge agent 在断开期间立即重连（例如因网络抖动重试），新连接先写入 pool，随后旧连接的 cleanup 代码将新 entry 也一并删除，导致新连接对其他 pod 不可见。

#### 改动

`EdgeConnectionPool` 新增：
```rust
next_generation: Arc<AtomicU64>,  // 单调递增计数器

pub fn register_with_capabilities(...) -> u64  // 返回分配的 generation

pub fn unregister_if_generation(
    &self,
    user_id: &str,
    edge_agent_id: &str,
    expected_gen: u64,
) -> bool  // 仅在 generation 匹配时删除
```

`EdgeConnection` 新增 `generation: u64` 字段。

`edge_ws_handler.rs`：
- `register_with_capabilities()` 的返回值存入 `pool_generation`
- cleanup 时改用 `unregister_if_generation(pool_generation)`
- DB unregister 只在 `was_our_entry == true` 时执行

---

### B2 — DB 注册失败拒绝 WebSocket 连接

**文件：** `rust/crates/runtime/src/server/edge/edge_ws_handler.rs`

#### 问题

原代码：
```rust
let _ = edge_registry.register_or_update(...).await;
```
`let _ =` 忽略了错误。若 DB 注册失败，edge 在本 pod 的 in-memory pool 里可见，但其他 pod 无法通过 DB registry 路由到它，状态为"在线"但 cross-pod dispatch 实际不工作。

#### 改动

```rust
if let Err(e) = edge_registry.register_or_update(...).await {
    // 回滚 pool entry（若 generation 匹配）
    state.edge_connection_pool.unregister_if_generation(..., pool_generation);
    // 向 edge 发 AuthError 并关闭 WebSocket
    send_edge_msg(&ws_sink, EdgeServerMessage::AuthError { ... }).await;
    return;
}
```

**注意：B2 必须与 B4 同时存在。** B4 将 `UnconfiguredEdgeRegistryService` 从返回错误改为 no-op 成功，确保没有配置 DB registry 的单节点部署不会因 B2 拒绝所有 edge 连接。

---

### B3 — heartbeat 按 edge_id 隔离，防旧连接刷新

**文件：** `rust/crates/services/src/multi_agent/edge_registry.rs`

#### 问题

heartbeat 的 UPDATE 语句：
```sql
UPDATE edge_agent_registry
SET edge_id = ?, last_heartbeat_at = NOW(6)
WHERE user_id = ? AND edge_agent_id = ?
```

新连接建立后 `edge_id` 变化，旧连接的 heartbeat 仍然匹配（`edge_id` 条件不存在），持续更新 `last_heartbeat_at`，导致旧连接的 DB 行存活，其他 pod 可能路由到已断开的旧连接。

#### 改动

```sql
UPDATE edge_agent_registry
SET last_heartbeat_at = NOW(6)
WHERE user_id = ? AND edge_agent_id = ? AND edge_id = ?
```

旧连接的 heartbeat 匹配 0 行 → 返回错误 → `edge_ws_handler.rs` 对 heartbeat 错误的处理（warn log）会触发连接关闭流程。`edge_id` 字段不再在 heartbeat 时更新，只由 `register_or_update` 写入。

---

### B4 — UnconfiguredEdgeRegistryService 改为 no-op 成功

**文件：** `rust/crates/services/src/multi_agent/edge_registry.rs`

#### 问题

未配置 DB edge registry 的部署（例如单节点测试环境），`UnconfiguredEdgeRegistryService` 的 `register_or_update`、`heartbeat`、`unregister` 全部返回 `Err`。结合 B2（注册失败拒绝连接），会导致**所有 edge WebSocket 连接都被拒绝**。

#### 改动

```rust
// 修改前
async fn register_or_update(...) -> Result<EdgeAgentRecord, String> {
    Err("edge registry service not configured".to_string())
}

// 修改后：构造一个内存态的 EdgeAgentRecord 返回，模拟成功
async fn register_or_update(...) -> Result<EdgeAgentRecord, String> {
    Ok(EdgeAgentRecord { ... })
}
```

`heartbeat` 和 `unregister` 同样改为 `Ok(())`。`list_by_user` 保留返回 `Err`（跨 pod 查询在无 DB 时是预期不可用的）。

---

### B5 — edge_dispatch poll 快路径（MatrixOne FOR UPDATE 慢查询）

**文件：** `rust/crates/services/src/multi_agent/edge_dispatch.rs`

#### 问题

`poll_pending()` 每 2 秒调用一次，每次都使用 `BEGIN` + `SELECT FOR UPDATE` 事务查询 pending dispatch。MatrixOne 对空结果集的 `FOR UPDATE` 也会获取表/页级锁，耗时 8-20 秒，导致即使无 tool call 在途，每次 poll 也极慢，工具响应延迟严重。

#### 改动

在事务查询前加非锁定 COUNT 快路径：

```rust
let fast_count: i64 = sqlx::query_scalar(
    "SELECT COUNT(*) FROM edge_pending_dispatch \
     WHERE user_id = ? AND edge_agent_id = ? AND status = 'pending'",
)
.bind(user_id)
.bind(edge_agent_id)
.fetch_one(&self.pool)
.await?;

if fast_count == 0 {
    return Ok(vec![]);  // 跳过 FOR UPDATE 事务
}
// 后续原有的 SELECT FOR UPDATE 逻辑不变
```

**竞态分析：** COUNT 与 FOR UPDATE 之间如果有新 dispatch 到达，最多延迟 1 个 2s poll 周期，不影响正确性。

---

## 测试覆盖

| 测试位置 | 覆盖内容 |
|---------|---------|
| `runtime/tests/edge_ws_e2e.rs` | Phase 1.5 binding 校验：token 绑定的 edge_agent_id 与自报值不一致时连接被拒绝 |
| `services/src/multi_agent/edge_registry.rs` | `UnconfiguredEdgeRegistryService` 改为 no-op 后的行为验证 |
| `runtime/src/server/tool_route_selection.rs` | EdgeWs transport 在 ServerSandbox / EdgeWorkspace 下均路由到 EdgeBound |
| `astra-thin-client/src/client.rs` | `streaming_http_client` 中 `.no_proxy()` 不存在（源码级 guard 测试）|

---

## 未解决问题 / 已知缺口

| 编号 | 描述 | 风险 |
|------|------|------|
| GAP-1 | Edge-registration token TTL 为 30 天，astra 无法主动撤销（jti 撤销需要 matrixflow 侧 jti blocklist 接口，未实现）| 🟡 中 |
| GAP-2 | `_by_scope` 接口（F6）需要 matrixflow 侧同步实现 `list_catalog_by_scope` / `issue_runtime_context_by_scope` 两个 action，否则 edge HTTP 调用 runtime context 时返回错误 | 🔴 阻塞 |
| GAP-3 | `edge_agent` CapabilityDescriptor（F7）的填充逻辑在 matrixflow 侧（moi-core catalog），本仓库只消费，不产生，需配套改动同步上线 | 🟡 中 |

---

## 合入前置条件

1. **B2 + B4 必须同时合入**，单独合入 B2 会导致没有 DB 的部署拒绝所有 edge 连接
2. **F6 的 `_by_scope` action** 需要 matrixflow 侧同步实现，否则 astra-edge HTTP 调用 runtime context 返回 501 NOT_IMPLEMENTED
3. **F7 的 `edge_agent` descriptor** 需要 matrixflow 侧（moi-core catalog）在签发 runtime context 时填充该字段
