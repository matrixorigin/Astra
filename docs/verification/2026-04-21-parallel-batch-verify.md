# 2026-04-21 Parallel + Batch + SelfModel 验证手册

本文档配套 PR `feat/self-model-parallel-batch-2026-04-21`，说明本 PR 涉及的三类改动如何本地验证。

## 涉及改动

| 模块 | 提交 | 说明 |
|------|------|------|
| SelfModel 4-field 注入 | `c523af42` | `skills` / `tool_health` / `scenario` / `recent_signals` 真实接入 |
| 批量工具调用引导 | `14e3d9d2` | system prompt 指导模型将只读工具调用合并到同一回合 |
| 并发上限信号量 | `62beb013` | `MAX_CONCURRENT_TOOL_EXECUTIONS = 10` |
| StreamingToolExecutor 基础设施 | `bbdd97aa` | `on_tool_block` / `harvest_completed` / `discard` |
| `Arc<ToolExecutor>` 重构 | `657b6d38` | SSE hook 可持有 `'static` 执行器 |
| 生产 speculation + HTTP e2e | `dee66bfb` | `ASTRA_STREAMING_TOOL_EXEC=1` 真正生效 |

## 离线验证

```bash
make check
make test-offline
```

关键测试：

- `cargo test -p astra-runtime --test self_model_injection_e2e` — 4 条用例覆盖 happy / unhappy / complex / signal tail-bound。
- `cargo test -p astra-turn-core --test parallel_tool_exec_cap_test` — 20 个只读工具并发时峰值 ≤ 10。
- `cargo test -p astra-turn-core --test streaming_tool_exec_sse_integration` — sse_stream_host 层端到端，500ms stream gap + 2×400ms 工具，开启 speculation 后 wall-clock 从 1304ms → 902ms（约 31% 降幅）。
- `cargo test -p astra-cli --test streaming_tool_exec_http_e2e` — 真实 HTTP/SSE 边界的 3 条用例：
  - `happy_speculation_on_starts_tools_mid_stream`：`ASTRA_STREAMING_TOOL_EXEC=1` 时工具在 stream 中途即启动，总耗时 < 1100ms；
  - `unhappy_denied_tool_is_not_speculated`：被 permission manager 拒绝的工具永不投机执行；
  - `complex_speculation_off_no_mid_stream_starts`：env 未设时工具 100% 在 stream 结束后才启动。

## Speculation 开关

```bash
# 开启（读-only 工具在 SSE stream 未结束时即后台执行）
export ASTRA_STREAMING_TOOL_EXEC=1

# 关闭（回退到 stream 结束后串行/并发）
unset ASTRA_STREAMING_TOOL_EXEC
```

Speculation 仅对「工具名在 `is_read_only_tool` 列表内 且 权限 pre-check 为 Allow」的工具生效；Step 3 skill/delegation 截流时自动 `discard(call_id)` 避免重复执行；harvested 结果保证 **每工具恰好一次** `ToolCompleted` 事件与 journal 条目。

## 手动烟测（真实 LLM）

前置条件：`~/.astra/credentials.json` 中配置可用 profile。

1. **并发批处理** — 观察同一轮内多条 `tool_call` 事件：
   ```bash
   astra chat --profile <p>
   > 同时列一下当前目录和读一下 README.md 前 20 行
   ```
   预期：一条 assistant 消息内 2 个 tool_calls；CLI 渲染显示并发分组 spinner。

2. **SelfModel 自省** — 两轮会话：
   ```
   > 帮我列目录并读 README
   ...
   > 你刚才调用了哪些工具、用了哪些技能？
   ```
   预期：模型基于注入的 SelfModel 事实回答，不虚构未调用的工具。

3. **Speculation 开关对比** — 带 `ASTRA_STREAMING_TOOL_EXEC=1` 跑一遍、取消后再跑一遍，观察工具日志时间戳：开启时工具开始时间早于 `run_finished` 事件。

## YAML e2e 用例

本 PR 同时新增 2 条 YAML 用例（由外部 e2e harness 驱动）：

- `scripts/e2e/cases/parallel_tool_batching.yaml`
- `scripts/e2e/cases/self_awareness_scenario.yaml`

断言原语沿用 harness 既有支持：`tool_called` / `response_contains` / `response_contains_any` / `db` / `llm_judge`。

## 回归检查清单

- [ ] `make check` 干净
- [ ] `make test-offline` 全绿
- [ ] `streaming_tool_exec_http_e2e` 3/3
- [ ] `ASTRA_STREAMING_TOOL_EXEC` 开/关均功能正常
- [ ] `ToolExecutor::active_session_id()` 签名变化（现返回 owned `Option<String>`）未影响下游调用方

## 已知折衷

`streaming_tool_exec_http_e2e` 驱动到 `consume_sse_stream` + `SseStreamHost` 管线，未复制整个 `astra-cli` chat_stream 栈（auth / 工具注册表 / 渲染状态开销过大）。CLI 层生产 wiring 的正确性由 offline 单测与集成测试覆盖，HTTP 边界由本测试覆盖。
