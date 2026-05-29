# Request-scoped MCP runtime

## 目标

Astra runtime 支持外部系统在一次 `/chat/stream` 请求中传入 MCP server 和 opaque credential。Astra 使用该 credential 做本轮 `tools/list` 和 `tools/call`，把返回的 tool schema 注入本轮 LLM tool surface。

这个能力是通用 runtime 能力，不包含 MOI、金盘、workspace、知识库、表权限等专用逻辑。

## 非目标

1. 不要求调用方提前注册 MCP server。
2. 不在 Astra DB 中持久化外部系统的 MCP credential。
3. 不把本轮 `tools/list` 返回的 tool schema 写入 Astra DB。
4. 不在 Astra 中解析外部 credential claims。
5. 不在 MCP 准备失败时降级成无工具 agent loop。
6. 不让 runtime 依赖 `astra-cli`。

## Chat 协议

```json
{
  "message": "用户问题",
  "session_id": "optional-session-id",
  "agent_id": "optional-agent-id",
  "model": "optional-model",
  "runtime_mcp_bindings": [
    {
      "id": "moi_jinpan_nl2sql",
      "transport": "streamable_http",
      "url": "https://moi.example.com/api/v1/workspaces/ws_xxx/mcp/http",
      "headers": {
        "Authorization": "Bearer <opaque-runtime-token>"
      }
    }
  ],
  "context": {
    "edge_profile": {
      "system_prompt_override": "..."
    }
  }
}
```

`runtime_mcp_bindings[].id` 是本轮 MCP namespace。Astra 必须 sanitize，并保证单次 request 内 public tool name 不冲突。

## Runtime flow

```text
ChatRequest.runtime_mcp_bindings
  -> validate id / url / transport / headers
  -> redact secrets for logs and errors
  -> connect MCP server
  -> tools/list with provided credential
  -> convert live tools to OpenAI-compatible schemas
  -> install schemas into current AgenticLoopHost
  -> set MCP manager on ServerToolExecutor
  -> LLM tool_call
  -> tools/call with same credential
```

如果任意 requested runtime MCP binding 准备失败，Astra 返回 structured error，不启动 agent loop。

## Tool naming

Public tool name：

```text
mcp__<runtime_mcp_binding_id>__<tool_name>
```

示例：

```text
mcp__moi_jinpan_nl2sql__query_sql
mcp__moi_jinpan_rag__search_documents
```

要求：

1. `McpServerConfig.name` 使用 sanitized runtime binding id。
2. schema conversion 和 tool dispatch 使用同一个 name。
3. 同一请求中不同 binding 的 public tool name 不能冲突。
4. LLM 只看到 public tool name、description、parameters。
5. LLM 看不到 URL、headers、token、external permission claims。

## Credential handling

credential 只在 request 内存中使用。

必须禁止出现在：

```text
HTTP response
SSE event
Debug output
trace payload
logs
session state
database
```

tool call 错误必须经过 secret redaction。如果上游错误无法安全 redaction，返回通用 `mcp_tool_execution_failed`。

## Context and cache

`runtime_mcp_bindings` 是 runtime scope input，不能进入 LLM prompt 文本。

可以记录：

```text
runtime_mcp_binding_id
public tool names
schema hash
tools/list latency
tools/call latency
```

不能记录：

```text
url with secret query
headers
auth_token
Authorization
opaque runtime credential
```

同一个 Astra session 的下一轮如果还需要 MCP tools，调用方必须再次提供 `runtime_mcp_bindings`。Astra 不能假设上一轮 credential 仍有效。

## Relation to astra-mcp crate

`astra-mcp` 仍是 runtime 和 CLI 共享的 MCP 协议实现：

```text
runtime -> astra-mcp <- CLI
```

runtime 使用它做 request-scoped connection、`tools/list`、schema conversion 和 `tools/call`。CLI 可以继续使用 `.astra/mcp.json`、slash commands、local MCP 连接管理；这些是 CLI 胶水，不进入 server-side runtime authorization model。

## Error codes

| code | 含义 |
| --- | --- |
| `mcp_runtime_binding_invalid` | binding id、url、transport、header 格式非法 |
| `mcp_runtime_discovery_failed` | 本轮 `tools/list` 失败 |
| `mcp_public_name_conflict` | public tool name 冲突 |
| `mcp_tool_execution_failed` | `tools/call` 失败 |

## MOI / 金盘调用方边界

MOI 可以把 RuntimeGrant 放在 `runtime_mcp_bindings[].headers.Authorization` 中，但 Astra 不解析该 token。

Astra 不应该出现：

```text
MOI workspace 权限判断
MOI knowledge base 权限判断
MOI table/file/chunk 过滤
金盘业务表或字段特例
```

这些必须由 MOI backend 和 MOI core MCP 执行。
