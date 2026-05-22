# MCP 下沉方案：astra-mcp 作为唯一 MCP 实现

## 当前问题

### P1 — runtime_mcp.rs 拒绝 stdio 传输

`runtime_mcp.rs:94` 在 `resolve_config()` 中只匹配 `"sse" | "http"`，其他传输类型（包括 `"stdio"`）直接返回错误。但 `astra-mcp` crate 的 `connection.rs` 已经实现了 `connect_stdio()`，且 `ContextMcpServer` 已有 `command`、`args` 字段。

**影响**：任何使用 stdio 传输的 MCP server（如本地 `npx`、`uvx` 进程）在 runtime 路径下无法工作。

### P2 — CLI 与 astra-mcp crate 重复实现

CLI 的 `mcp_client.rs`（3607 行）与 `astra-mcp` crate（1248 行）存在大量重复：

| 模块 | CLI mcp_client.rs | astra-mcp crate |
|------|-------------------|-----------------|
| 行数 | 3607 | 1248 |
| Transport | ✅ | ✅ |
| ConnectionState | ✅ | ✅ |
| RetryConfig | ✅ | ✅ |
| McpServerConfig | ✅ | ✅ |
| McpConnection | ✅ 完整 | ✅ 基础 |
| McpClientManager | ✅ 完整 | ✅ 基础 |
| McpError | ✅ | ✅ |
| 工具函数（sanitize/schema/extract） | ✅ 重复 | ✅ |
| SamplingConfig | ✅ | ❌ |
| Skills 发现 | ✅ | ❌ |
| Prompts / Resources 查询 | ✅ | ❌ |
| Ping / Ping all | ✅ | ❌ |

CLI 的 `Cargo.toml` 没有依赖 `astra-mcp`，CLI 所有代码用 `crate::mcp_client::*` 引用自己的实现。

**影响**：维护两套 MCP 栈，行为可能分化，修 bug 要修两处。

### P3 — 无法独立测试 MCP 连接

CLI 的 `/mcp` slash commands 走的是 CLI 自己的 `mcp_client.rs`，不走 `astra-mcp` crate。没有独立的 `astra mcp test` 命令来做连通性验证。

**影响**：出问题只能靠 chat session 调试，无法快速验证 MCP server 是否可达。

---

## 目标架构

```
.astra/mcp.json  ──→  CLI（manifest 加载 + /mcp slash commands + edge dispatch）
                            │
                            ▼
                      astra-mcp crate  ← 唯一的 MCP 实现
                            │
                    ┌───────┴───────┐
                    ▼               ▼
              runtime           CLI 自己的
        (context.mcp_servers)   连接管理
```

- **astra-mcp crate**：所有 MCP 协议逻辑的唯一实现
- **runtime**：通过 `RuntimeMcpManager` 消费 astra-mcp，接收 `context.mcp_servers` 配置
- **CLI**：通过 astra-mcp 消费 MCP 能力，保留 manifest 加载、slash commands、edge dispatch 等 CLI 特有胶水
- **依赖方向**：runtime → astra-mcp ← CLI，runtime 不依赖 CLI

---

## 实施步骤

### Phase 1 ✅ 已完成 (2026-05-22)

把 CLI `mcp_client.rs` 中属于"通用 MCP 能力"的部分迁入 astra-mcp，使其成为功能完备的 MCP 实现。

#### 1.1 McpConnection 补全 ✅

迁移到 `astra-mcp/src/connection.rs`：

- `complete()` — completion 支持
- `discover_skill_resources()` — 从 MCP resources 发现 skills

新增 imports: `ArgumentInfo`, `CompleteRequestParams`, `CompleteResult`, `Reference`。

#### 1.2 McpClientManager 补全 ✅

迁移到 `astra-mcp/src/manager.rs`：

- `all_prompts()` — 聚合所有 server 的 prompts
- `all_resources()` — 聚合所有 server 的 resources
- `get_prompt()` — 跨 server 查询 prompt
- `complete()` — completion 代理
- `ping()` / `ping_all()` — 连通性检查（含延迟测量）
- `server_states()` — 所有 server 的连接状态（额外迁移）

#### 1.3 类型补全 ⏭️ 跳过

`SamplingConfig` / `roots` / `set_sampling_config()` / `has_sampling()` **不迁入 astra-mcp**。

原因：`SamplingConfig` 依赖 `astra_thin_client::ThinClient`（LLM client），`roots` 是 CLI 管理本地文件系统根路径的概念。这些属于 CLI 特有能力，runtime 走自己的 LLM pipeline，引入会污染共享 crate 的依赖链。

CLI Phase 2 切 astra-mcp 时，`SamplingConfig` 和 `roots` 保留在 CLI 侧作为扩展字段。

#### 1.4 P1 修复：runtime_mcp.rs 支持 stdio ✅

在 `resolve_config()` 中增加 `"stdio"` 分支，将 `ContextMcpServer.command` 转为 `vec![command]`，`env` 初始化为空 HashMap。

#### 验证结果

- `cargo check -p astra-mcp -p astra-runtime` — 通过
- `cargo test -p astra-mcp` — 9/9 通过
- `cargo test -p astra-runtime` — 全部通过
- `cargo build` — 全工作空间编译通过

---

### Phase 2 ✅ 已完成 (2026-05-22)

#### 2.1 加依赖 ✅

CLI `Cargo.toml` 加 `astra-mcp.workspace = true`。

#### 2.2 删除重复代码 ✅

从 CLI `mcp_client.rs` 删除 (~334 行)：

- **类型**：`Transport`、`ConnectionState`、`RetryConfig`、`McpServerConfig`、`McpError`
- **工具函数**：`sanitize_tool_name()`、`mcp_tool_to_schema()`、`extract_result_text*()`、`is_dangerous_env_var()`、`truncate_with_marker()`
- **常量**：`MAX_DESCRIPTION_LENGTH`、`MAX_RESULT_CONTENT_LENGTH`、`TRUNCATION_MARKER`、`BLOCKED_ENV_*`
- 所有改为 `pub use astra_mcp::*` 重导出

#### 2.3 保留在 CLI 的代码 ✅

因 `ChangeHandler` 包含 `SamplingConfig`（依赖 `astra_thin_client`），`McpConnection` 的类型参数与 astra_mcp 版本不同，以下保留：

- `SamplingConfig` + `DEFAULT_SAMPLING_MAX_TOKENS_CAP`
- `ChangeHandler`（有 sampling 和 roots 支持）
- `McpConnection` 构造相关（connect 函数族）
- `McpClientManager`（wrapper，含 sampling/roots/skill 编排）
- `connect_and_discover_skills()` / `disconnect_and_remove_skills()`
- manifest 加载、slash commands、edge dispatch（外部文件，未改动）

#### 2.4 更新引用 ✅

`mcp_client.rs` 内部通过 `pub use astra_mcp::*` 重导出，外部文件对 `crate::mcp_client::*` 的引用无需改动。

`astra-mcp/lib.rs` 新增导出 `is_dangerous_env_var`。

#### 2.5 命名格式修正确认 ✅

切换后 `mcp_tool_to_schema` 使用 double-underscore 格式 `mcp__{server}__{tool}`，修复了 CLI 原来 `mcp_{server}_{tool}` 与 runtime dispatch 检查 `name.starts_with("mcp__")` 不一致的问题。4 个相关测试断言已更新。

#### 验证结果

- `cargo build` — 全工作空间编译通过，0 warnings
- `cargo test -p astra-cli` — 4435 passed, 0 failed
- `cargo test -p astra-mcp` — 9 passed, 0 failed
- `cargo test -p astra-runtime` — 全部通过

---

### Phase 3 ✅ 已完成 (2026-05-22)

#### 3.1 添加 CLI 子命令 ✅

`cli_args.rs` 新增：

- `McpCmd::Test(McpTestArgs)` — `astra mcp test <name> [-s scope]`
- `McpCmd::Ping(McpPingArgs)` — `astra mcp ping <name> [-s scope]`

#### 3.2 实现处理函数 ✅

`command_router.rs` 新增：

- `find_server_entry()` — 从 manifest 查找 MCP server 配置（支持 project/local/given 三层查找）
- `json_entry_to_mcp_config()` — 将 JSON 配置转换为 `McpServerConfig`
- `async fn mcp_test()` — 连接 → list tools → list resources → list prompts → 断开
- `async fn mcp_ping()` — 连接 → ping（含延迟测量）→ 断开
- `execute_mcp_command` 改为 `async fn`，调用处加 `.await`

#### 3.3 CLI 子命令新增

```bash
astra mcp test <name>    # 连接 → list tools → 断开，输出结果
astra mcp ping <name>    # 连通性检查
```

走 astra-mcp crate 的 `McpClientManager`，加载 `.astra/mcp.json` 配置后执行。

#### 验证结果

- `cargo check -p astra-cli` — 通过
- `cargo test -p astra-cli` — 4435 passed, 0 failed
- `cargo test -p astra-mcp` — 9 passed, 0 failed
- `cargo test -p astra-runtime` — 全部通过

---

## 实际改动量

| Phase | 描述 | 改动量 | 状态 |
|-------|------|--------|------|
| Phase 1 | astra-mcp 补全 + stdio 修复 | ~130 行新增 | ✅ 完成 |
| Phase 2 | CLI 切到 astra-mcp，删重复 | ~334 行删除 (3607→3273) | ✅ 完成 |
| Phase 3 | 测试命令 | ~150 行新增 | ✅ 完成 |
