# Astra 发布质量测试计划

> **目的**：说明 Astra Engine 在对外发布（GA）前，需要完成哪些测试、达到什么标准，才能认为产品质量达标。  
> **范围**：核心运行时、Memoria、MatrixOne、CLI/Admin、**Cloud / Edge-Cloud 分执行**、可选 Gateway。  
> **说明**：本文聚焦 **发布质量验证**，不涉及 CI 流水线、PR 门禁或日常开发流程。  
> **相关文档**：[系统 E2E 矩阵](./system-e2e-matrix.md) · [Edge-Cloud 分执行](../design/edge-cloud-execution.md) · [多 Agent Cloud Runtime](../design/multi-agent-cloud-runtime.md) · [评估与演进](../design/evaluation-and-evolution.md) · [信任与安全](../design/trust-and-safety.md)

---

## 1. 发布质量标准

Astra 对外承诺的是 **可审计、可回放、可治理的 Agent 运行时**，而非单纯 LLM 能力。发布前须满足：

| 质量维度 | 发布标准 | 怎么验 |
|----------|----------|--------|
| **核心功能** | 认证、Session、Chat、工具调用、模型管理、Skills 全部可用 | 功能测试 + 端到端场景 |
| **信任与审计** | 每次决策可回放，审计链完整，租户隔离无泄露 | 审计验证 + 隔离测试 |
| **记忆（Memoria）** | 存取检索正常，跨 Session 可用，故障可降级 | 记忆生命周期测试 |
| **Harness 能力** | Fork、预算、压缩、崩溃恢复按设计工作 | Harness 专项场景 |
| **Cloud 行为** | Edge-Cloud 分执行、/chat/turn 协议、Cloud 侧 enrichment 与审计完整 | Edge-Cloud 专项 + E2E |
| **稳定性** | 7×24 soak 无泄漏；依赖故障可恢复 | 压测 + 混沌测试 |
| **性能** | 满足延迟与并发目标 | 性能测试 |
| **安全** | 无高危漏洞；密钥与权限符合生产要求 | 安全测试 |
| **部署** | 全新环境可一键部署并 Chat | 部署验收 |
| **Agent 能力** | 在代表性 Benchmark 上无显著退化 | 外部 Benchmark 抽样（辅助） |

---

## 2. 测试总体思路

发布质量验证分三层，**由内到外、由平台到能力**：

```
┌────────────────────────────────────────────────────────┐
│  第三层：Agent 能力验证（Benchmark 抽样，辅助决策）      │
├────────────────────────────────────────────────────────┤
│  第二层：端到端业务场景（真实用户路径，发布核心）          │
├────────────────────────────────────────────────────────┤
│  第一层：平台功能与信任能力（Astra 差异化，发布必过）      │
└────────────────────────────────────────────────────────┘
```

**原则：**
- **第一层、第二层** 决定能否发布。
- **第三层** 用于能力对标和选型，**不能单独作为发布依据**（公开 Benchmark 可信度见 §7.7）。
- 所有测试应在 **类生产环境**（MatrixOne + Memoria + Redis + 真实 LLM）上执行。

---

## 3. 第一层：平台功能与信任能力（发布必过）

### 3.1 核心运行时

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| 用户认证 | 注册、登录、Token 刷新、登出 | 全流程正常；未授权请求被拒绝 |
| Session 管理 | 创建、列表、关闭、恢复、取消、删除 | 状态正确持久化；重启后一致 |
| Chat 对话 | 单轮/多轮、流式输出、工具调用与回调 | 对话连贯；工具结果正确回传 |
| 模型管理 | 加载、激活、切换、Key 加密存储 | 模型可用；DB 中 Key 已加密 |
| Skills | 注册、自动选择、执行 | 意图匹配正确 Skill |
| Plan 模式 | 结构化规划与执行（若 GA 包含） | 计划可生成并执行 |
| Checkpoint | 保存检查点、恢复 | 状态可恢复 |
| 持久化 | API 重启、进程崩溃 | Session 与审计数据不丢失 |

**建议执行方式：**
- 手工走查关键用户路径（见 §4 场景清单）
- 自动化：`make test-online`（平台 E2E 矩阵，30+ journey）
- 自动化：`make test-harness`（YAML 用例，见 §3.5 P0 清单）

### 3.2 信任与审计（Astra 核心差异化）

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| 上下文快照 | 每次 LLM 调用前写入 snapshot | DB 中 `ctx_snapshots` 有记录 |
| 因果链 | 多轮对话链路完整 | `causal_chain_id` 可追溯 |
| 决策回放 | `astra replay <session-id>` | 可复现当时输入与输出 |
| 审计 API | `/audit/stats`、`/audit/mutations` | 数据与 DB 一致 |
| 用户隔离 | Memory 代理、Session、Team | 用户 A 无法访问用户 B 数据 |

**专项验证：**
1. 完成一次含工具调用的多轮对话。
2. 用 `astra replay` 回放该 Session。
3. 核对 DB 中 snapshot、decision audit、causal chain 记录完整。
4. 用两个账号分别操作，确认无法互访。

### 3.3 记忆（Memoria）

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| 完整生命周期 | store → search → retrieve | 跨 Turn 可检索到先前写入内容 |
| 意图路由 | 记忆相关意图正确路由 | 触发 Memoria 而非错误路径 |
| 故障降级 | Memoria 短暂不可用 | Chat 主链路仍可用；记忆功能明确报错 |
| 多用户隔离 | 不同 user_id | 互不可见 |

**自动化用例：** `memory_full_lifecycle.yaml`、`memory_tracking_intent_routes_correctly.yaml`

### 3.4 Harness 特有能力

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| Fork / prefix 继承 | 子 Agent 继承父上下文 | Fork cache 命中；子任务结果正确 |
| 上下文压缩 | 长对话压缩 | 近期读取内容保留 |
| Token 预算 | 超限拦截 | 超出预算时被阻断，不 silent 超支 |
| 反幻觉 | 向量检索场景 | 不编造不存在的内容 |
| 崩溃恢复 | 进程异常退出 | Journal 可解析；Session 可续 |

**P0 自动化用例（每次发布前必跑）：**

- `hello_text_contains`
- `multi_turn_context_retention`
- `memory_full_lifecycle`
- `fork_prefix_hit_end_to_end`
- `harness_budget_enforcement`
- `crash_robustness_journal_parseable`
- `anti_hallucination_two_vectors`

```bash
make test-harness FORCE_MODEL=<GA主模型> PARALLEL=4 RUNS=2
```

**通过标准：**
- Hard criteria：100%
- Soft criteria：≥ 90%
- Judger 平均分：≥ 0.7（若启用 LLM 评判）
- 基础设施失败：0

### 3.5 多 Agent / Team（若 GA 包含）

| 测试项 | 通过标准 |
|--------|----------|
| Team 创建、更新、删除 | CRUD 正常；DB 一致 |
| Team 快照 | 可保存与恢复 |
| 跨用户隔离 | 无法访问他人 Team |
| 委派与并发 | 同 Session 并发 Turn 不串话 |

### 3.6 Cloud / Edge-Cloud 行为（P0 — 发布必过）

Astra 采用 **Edge-Cloud 分执行**：Edge（用户机器）跑本地工具与交互循环驱动；Cloud（服务端）负责 LLM、上下文组装、记忆检索、路由、预算、审计与持久化。详见 [Edge-Cloud 分执行](../design/edge-cloud-execution.md)。

#### 3.6.1 职责划分验证

| 执行位置 | 应测能力 | 通过标准 |
|----------|----------|----------|
| **Cloud** | LLM 调用（Key 不泄露到 Edge） | API Key 仅服务端持有；Edge 日志无 Key |
| **Cloud** | 上下文组装（记忆、Skill 索引、few-shot） | `/chat/turn` 前 Cloud 注入正确；DB 有 assembly 记录 |
| **Cloud** | 模型路由、预算门禁、限流 | 超预算拒绝；路由符合配置 |
| **Cloud** | 防火墙验证、置信度、成本追踪 | `warning` 事件可触发；usage 正确 |
| **Cloud** | 审计持久化（snapshot、decision、events） | 每 Turn 完整写入 MatrixOne |
| **Edge** | 本地工具（read/bash/git/grep 等） | 工具在 Edge 执行；结果经 callback 回 Cloud |
| **Edge** | 权限审批（Y/N/Always/Deny） | 审批流程在 Edge；结果经 `/approval/respond` 回传 |
| **Edge** | 项目规则加载（`.astra/`、`CLAUDE.md` 等） | 首 Turn 上传；Cloud 按 Session 缓存 |

#### 3.6.2 `/chat/turn` 协议（核心 Cloud 路径）

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| 请求格式 | `session_id`、`messages`、`tool_results`、`project_rules` | 缺必填字段返回 4xx |
| SSE 流式响应 | `text_delta`、`tool_call`、`usage`、`turn_complete` | 事件类型正确；`turn_complete` 在最后 |
| SSE 心跳 | 长 Turn 期间 `ping` 保活 | 30s+ 无数据时连接不断 |
| Turn 语义 | `has_tool_calls: true/false` | true → Edge 执行工具后再 Turn；false → 本轮结束 |
| 多轮闭环 | Edge 工具结果 → 再次 `/chat/turn` | 多轮工具调用链路完整 |
| 并发隔离 | 同 Session 并发 Turn | 无 SSE/状态串话（见 E2E journey） |
| 乱序/重复 callback | tool_result 乱序或重复提交 | 幂等；不破坏 Turn 状态 |

**自动化覆盖：** `system_matrix_http_e2e` 中 `product_matrix_*`、`e2e_matrix_*_tool_*`、`edge_cloud_round_trip_e2e`（bridge hooks）。

#### 3.6.3 Edge 回调与注册

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| `/tools/result` | Edge 回传工具执行结果 | Cloud 持久化并进入下一轮组装 |
| `/approval/respond` | Edge 回传审批决定 | 决策写入 journal；非法 session 拒绝 |
| Edge Agent 注册 | `POST /agents/edge` | `edge_agent_registry` 有记录 |
| Task Lease | claim / renew / release | 租约不泄漏；超时可回收 |
| 回调鉴权 | 无 Token / 畸形 payload | HTTP 边界错误明确，不 crash |

#### 3.6.4 Cloud 侧 Headless 与后台 Run

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| 后台 Chat Run | `POST /chat` 异步运行 | 返回 `run_id`；可查询进度 |
| 暂停 / 恢复 | `POST .../pause`、`.../resume` | 状态正确；可继续执行 |
| 委派 | `POST .../delegate` | 子任务可追踪；列表可查 |
| Thin Client | CLI/Web 经同一 HTTP 协议 | 无客户端专属 hidden state |

详见 [多 Agent Cloud Runtime](../design/multi-agent-cloud-runtime.md) §5.3 Headless Cloud Runtime。

#### 3.6.5 Cloud Sync（Edge ↔ MatrixOne）

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| Push / Pull | `/sync` 或 slash `sync push/pull` | Edge 与 Cloud 状态一致 |
| 增量同步 | Delta 相对全量快照 | 带宽显著小于全量（约 85–90% 减少） |
| 断点续传 | 网络中断后重连 | 可增量补齐；无 duplicate 事件 |
| 冲突处理 | 多端同时修改 | 有明确策略（last-write 或 merge 规则） |

详见 [sync_protocol.md](../sync_protocol.md)。

#### 3.6.6 Cloud 信任边界

| 测试项 | 验证内容 | 通过标准 |
|--------|----------|----------|
| Edge 数据标记 | tool_result 来自 Edge | 审计链标注 `source: edge`（不可信输入） |
| Cloud 数据标记 | LLM 输出、记忆、组装上下文 | 标注 `source: cloud` |
| Memory 代理 | Edge 伪造 `user_id` | Cloud 以 JWT 用户为准，覆盖 body |
| Prompt 注入 | 恶意 tool 输出 | 防火墙/校验层处理，不 silent 透传 |

详见 [信任与安全](../design/trust-and-safety.md) Edge Trust Boundary。

#### 3.6.7 Cloud 故障与降级

| 故障 | 预期 Cloud 行为 |
|------|----------------|
| LLM 429/5xx | 重试 + `fallback_chain`；SSE 返回 `error`（`retryable`） |
| Memoria 不可用 | 记忆检索跳过或报错；`/chat/turn` 主链路仍可用 |
| MatrixOne 短暂不可用 | `/health` → unhealthy；恢复后自动 OK |
| Cloud API 重启 | 进行中 Run 可恢复或明确失败；已持久化 Turn 不丢 |
| Edge 断连 mid-turn | Cloud 侧 Turn 超时/取消可感知；lease 可回收 |

---

## 4. 第二层：端到端业务场景（发布必过）

模拟真实用户使用路径，建议 **手工 + 自动化结合**，至少覆盖以下场景：

### 4.1 新用户首次使用

```
注册 → 登录 → 加载模型 → 首次 Chat → 查看 Session 历史
```

**验收：** 30 分钟内从零到完成首次有效对话。

### 4.2 日常开发助手

```
创建 Session → 多轮问答 → 调用工具（读文件/执行命令）→ 切换模型 → 关闭 Session
```

**验收：** 工具调用正确；模型切换生效；Session 可恢复。

### 4.3 记忆增强对话

```
写入记忆 → 新 Session 检索 → 验证跨 Session 上下文
```

**验收：** 第二轮 Session 能引用第一轮写入的信息。

### 4.4 规划与复杂任务

```
/plan 或 plan 命令 → 多步执行 → verify_task 验证交付
```

**验收：** 计划可分解执行；verify_task 给出结构化结论。

### 4.5 审计与合规场景

```
完成一次决策 → replay 回放 → 导出 audit 数据 → 核对 snapshot
```

**验收：** 合规人员可回答「当时 Agent 为什么这样决策」。

### 4.6 管理员运维

```
冷部署 → astra-admin init → model load → 健康检查 → 用户管理
```

**验收：** 全新环境 30 分钟内达到可 Chat 状态。

```bash
cd deployment/all-in-one
docker compose --profile app up -d --build
```

### 4.7 Gateway 场景（若随版本发布）

```
企微/微信消息 → Gateway → Astra → 回复 → 定时任务 → 长任务 checkpoint
```

**验收：** 消息可靠送达；崩溃后 Outbox 重试成功。

### 4.8 Edge-Cloud 分执行场景（发布必过）

模拟 **CLI Edge + Cloud API** 的典型闭环（Astra 默认架构）：

```
Edge: astra chat
  → Cloud: POST /chat/turn（首 Turn，含 project_rules）
  ← Cloud: SSE tool_call（read_file、bash 等）
Edge: 本地执行工具
  → Cloud: POST /tools/result 或下一 Turn 携带 tool_results
  ← Cloud: SSE text_delta + turn_complete
  → Cloud: 持久化 snapshot + audit（MatrixOne）
Edge: astra replay / audit 验证
```

**子场景必测：**

| # | 场景 | 验收 |
|---|------|------|
| 1 | 单轮纯文本（无工具） | Cloud 返回文本；审计链完整 |
| 2 | 多轮工具调用（≥3 次 tool loop） | Edge↔Cloud 往返正确；最终答案正确 |
| 3 | 长 Turn（LLM 思考 >30s） | SSE ping 保活；不 ReadTimeout |
| 4 | 权限审批中断 | Edge 弹审批；`/approval/respond` 后续执行 |
| 5 | Cloud sync push/pull | Edge 与 Cloud Session 状态一致 |
| 6 | 后台 Run + pause/resume | `run_id` 可查；暂停后恢复 |
| 7 | Edge 断网 mid-turn | 重连后可续或明确失败态 |
| 8 | 预算耗尽 | Cloud 拒绝 Turn；Edge 收到明确错误 |

**验收：** 上述 8 个子场景全部通过；Cloud 侧 100% Turn 有 audit snapshot。

---

## 5. 第三层：Golden Session 回归（发布必过）

对 **已沉淀的高质量真实 Session** 做回放，验证 prompt、skill、模型变更不会导致退化。

### 5.1 Golden 库建设

- 数量：≥ 50 条
- 来源：预发/内测中 `quality_score ≥ 4.0` 的 Session
- 覆盖：多种 Skill、多种模型、单轮/多轮、含/不含工具
- 存储：`tests/fixtures/golden_sessions/`（脱敏）

### 5.2 回放流程

```
1. 对当前候选版本创建沙箱
2. 在沙箱中回放全部 Golden Session
3. 对比：完成率、quality_score、Token 成本、审计链完整性
```

### 5.3 通过标准

| 指标 | 标准 |
|------|------|
| 回放通过率 | ≥ 95% |
| 高分 Session quality_score | 不得下降 > 0.2 |
| Token 成本 | 不得上升 > 20%（除非有书面批准） |
| 审计链 | 100% 回放 Session 有完整 snapshot |

---

## 6. 非功能测试（发布前必做）

### 6.1 性能

| 场景 | 目标 |
|------|------|
| API 读接口（`/health`、`/sessions`） | P95 < 500ms @ 100 并发 |
| Chat 并发（50 Session × 5 Turn） | 无串话、无死锁 |
| Cloud `/chat/turn` 长 Turn | SSE ping 保活；30s+ 不断连 |
| Cloud 后台 Run 并发 | 10 并行 Run；lease 不冲突 |
| Memoria 检索（1000 次 store + search） | Search P95 < 2s |
| 4 小时 Soak | 无连接泄漏、内存持续增长 |

### 6.2 可靠性与混沌

| 故障注入 | 预期 |
|----------|------|
| API 进程 kill -9 后重启 | 进行中任务失败可感知或可恢复 |
| MatrixOne 短暂不可用 | `/health` 变 unhealthy，恢复后自动 OK |
| Memoria 宕机 | 记忆降级，Chat 核心仍可用 |
| Cloud API 重启 | 进行中 Run 可恢复或失败可感知；已持久化 Turn 不丢 |
| Edge 断连 mid-turn | Cloud Turn 超时/取消；lease 可回收 |
| LLM 429/5xx | 重试与 fallback_chain 生效 |
| 磁盘满 | 明确报错，数据不静默损坏 |

### 6.3 安全

| 项 | 要求 |
|----|------|
| 生产配置 | `ASTRA_ALLOW_INSECURE_DEFAULTS=0` |
| LLM Key | DB 中 Fernet 加密，明文不可见 |
| 租户隔离 | 全部隔离测试通过 |
| SQL / 命令注入 | Edge 工具防护有效 |
| Prompt 注入 | 不可信 tool 结果被过滤或标记 |
| 依赖漏洞 | 无未修复高危 CVE |

---

## 7. Agent 能力 Benchmark（发布辅助，非阻塞）

用于 **能力对标和模型选型**，不作为唯一发布依据。以下仅列出 **与 Astra 产品能力直接相关** 的业界 Benchmark（Harness、工具编排、记忆、审计、Coding、长任务、MCP、权限策略）。

### 7.1 与 Astra 相关的能力维度

| 维度 | Astra 对应能力 | 相关 Benchmark |
|------|----------------|----------------|
| **Harness / Scaffold** | Agent 执行壳、fork、预算、压缩 | Harness-Bench |
| **Skills / 上下文** | Skill 选择与 AGENTS.md 类上下文 | eth-sri/agentbench |
| **Tool / MCP** | 工具编排、MCP Server | BFCL-V3-MT、MCP-Universe |
| **权限 / 策略** | Permission 模式、工具审批 | τ²-Bench |
| **Coding Agent** | verify_task、代码工具 | SWE-bench Verified、Terminal-Bench |
| **长 Horizon** | Plan、Team、委派 | AgencyBench |
| **记忆（Memoria）** | 跨 Session 存取检索 | ASTRA-bench †、LongMemEval |
| **信任 / 安全** | 幻觉防护、审计、拒答 | CAR-bench |
| **Astra 生态** | 官方评测（规划中） | astra-bench |

† **ASTRA-bench**（Assistant Skills in Tool-use, Reasoning & Action-planning）与 MatrixOrigin Astra **无关**，名称巧合；用于评估 Memoria 个人上下文场景。

---

### 7.2 相关 Benchmark 清单

| Benchmark | 测什么 | 与 Astra 的关系 | 链接 |
|-----------|--------|-----------------|------|
| **Harness-Bench** | 固定 workflow，只变 Harness，比成功率/成本/trace | **核心** — 直接验证 Harness 价值 | [论文](https://arxiv.org/html/2605.27922) |
| **eth-sri/agentbench** | AGENTS.md / 仓库上下文对 Coding Agent 的影响 | Skills + 项目上下文策略 | [GitHub](https://github.com/eth-sri/agentbench) |
| **BFCL-V3-MT** | 多轮 Function Calling | 工具编排、Skill 触发准确性 | [Leaderboard](https://gorilla.cs.berkeley.edu/leaderboard.html) |
| **MCP-Universe** | MCP 工具生态 | `astra mcp` 集成能力 | GitHub |
| **τ²-Bench** | 工具 + 用户模拟 + 策略遵守 | Permission / 审批策略 | [GitHub](https://github.com/sierra-research/tau2-bench) |
| **SWE-bench Verified** | 真实 GitHub Issue 修 Bug | Coding Agent + verify_task | [swebench.com](https://www.swebench.com/) |
| **Terminal-Bench** | 终端命令行任务 | CLI Agent（`astra` 命令行） | [GitHub](https://github.com/laude-institute/terminal-bench) |
| **AgencyBench** | 长 Horizon 真实任务（~90 tool calls） | Plan / Team / 委派 | [GitHub](https://github.com/GAIR-NLP/AgencyBench) |
| **ASTRA-bench** † | 时变个人上下文 + 工具规划 | Memoria 场景 | [论文](https://arxiv.org/html/2603.01357v1) |
| **LongMemEval** | 长对话记忆检索 | Memoria 检索准确率 | [GitHub](https://github.com/xiaowu0162/LongMemEval) |
| **CAR-bench** | 幻觉检测、拒答 | 信任与安全设计对齐 | GitHub |
| **astra-bench** | MatrixOrigin 官方 Agent 评测 | 未来首选套件 | [astra-suite roadmap](https://github.com/matrixorigin/astra-suite) 规划中 |

**不在本计划范围内（与 Astra 无直接产品对应，不纳入发布 Benchmark）：**  
WebArena、OSWorld、GAIA、AgentBench(THUDM)、通用桌面/浏览器 Agent、纯 API 调用榜（ToolBench/API-Bank）等。

---

### 7.3 GA 前推荐抽样

| 优先级 | Benchmark | 抽样量 | 主要指标 |
|--------|-----------|--------|----------|
| **P0** | Harness-Bench | 完整协议 | 成功率、Token 成本 |
| **P0** | BFCL-V3-MT | 官方多轮子集 | Accuracy |
| **P1** | SWE-bench Verified | 50 实例 | Resolve rate |
| **P1** | AgencyBench | 10 任务 | Rubric 通过率 |
| **P1** | ASTRA-bench † | 100 场景 | 任务成功率 |
| **P2** | Terminal-Bench | 20 任务 | Pass rate |
| **P2** | LongMemEval | 20 查询 | Recall@k |
| **P2** | MCP-Universe | 子集 | MCP 任务成功率 |
| **P2** | τ²-Bench | 零售子集 | 策略遵守率 |
| **P2** | CAR-bench | 子集 | 幻觉/拒答率 |
| **P2** | eth-sri/agentbench | 抽样 | 上下文对完成率影响 |

**跨版本对标（建议性）：**

| 对比基准 | 标准 |
|----------|------|
| 相对上一版本，P0/P1 | 退化 ≤ 5 分（P0）、≤ 10 分（P1） |
| 审计 Sidecar | 100% 任务有 `context_snapshot` + `causal_chain_id` |

---

### 7.4 Astra 能力 ↔ Benchmark 映射

| Astra 能力 | 发布必测（§3–§5） | Benchmark |
|------------|-------------------|-----------|
| Harness / fork / budget | Harness P0 用例 | Harness-Bench |
| Skills / 上下文 | skill_discovery 等 | eth-sri/agentbench |
| 工具编排 | E2E + tool_sequence | BFCL-V3-MT |
| MCP | `astra mcp` E2E | MCP-Universe |
| 权限 / 审批 | Permission 场景 | τ²-Bench |
| 记忆（Memoria） | memory_full_lifecycle | ASTRA-bench †、LongMemEval |
| 审计 / 回放 | replay + audit API | 审计 Sidecar（全部 Benchmark 附加） |
| Coding Agent | verify_task | SWE-bench Verified、Terminal-Bench |
| 长任务 / Plan / Team | 委派、Team 场景 | AgencyBench |
| 信任 / 安全 | 隔离 + 渗透 | CAR-bench |
| Gateway / IM | 手工场景 §4.7 | 无统一 Benchmark — 自建场景 |

---

### 7.5 执行方式

1. **Adapter**：Benchmark 任务 → `astra chat` / `astra plan` / `/chat/turn`。
2. **固定模型**：GA 主模型 + `.models.yaml` 快照，写入结果元数据。
3. **审计 Sidecar**：每条任务断言完整审计链（Astra 增值指标）。
4. **归档**：`benchmarks/results/<benchmark>/<version>/`（脱敏、gitignore）。

```bash
astra chat -m "<task-prompt>" \
  --model <GA主模型> \
  --permission-mode auto \
  --quiet > benchmarks/results/<benchmark>/<version>/output.txt
```

**每次运行记录：** astra 版本、模型、Benchmark 版本、抽样量、主指标、审计链完整率、日期、Token 成本。

---

### 7.6 发布节奏

| 时机 | 内容 |
|------|------|
| GA 前 | §7.3 全部 P0 + P1 |
| 换主模型 | P0 重跑 |
| 每季度 | P2 抽样 + 与上季度对比 |
| astra-bench 就绪后 | 替代部分外部套件作为首选 |

---

### 7.7 可信度说明

[BenchJack](https://arxiv.org/html/2605.12673v1)（2026）表明 SWE-bench 等榜单存在刷分风险。本计划所采纳的 Benchmark 中，**AgencyBench、Terminal-Bench** 等环境型套件相对更可靠；分数 **仅作方向参考**，发布决策仍以 §3–§5 为准。

---

## 8. 预发与灰度验证（发布最后一步）

| 阶段 | 范围 | 时长 | 观察重点 |
|------|------|------|----------|
| 内测 | 团队 + 种子用户 | 1–2 周 | 功能遗漏、体验问题 |
| Canary | 5% 流量 | 24h | 5xx、P95、审计完整性 |
| Beta | 20% 流量 | 72h | 完成率、Memoria 命中、用户反馈 |
| GA | 100% | — | SLO 看板全绿 |

**回滚条件（任一触发即回滚）：**
- API 5xx > 1% 持续 5 分钟
- Chat 完成率较内测基线下降 > 10%
- 出现跨租户数据泄露或审计链断裂

---

## 9. 发布验收 Checklist

```
平台功能（第一层）
[ ] 认证、Session、Chat、工具、模型、Skills 全流程通过
[ ] 审计链完整：snapshot + causal_chain + replay 可用
[ ] Memoria 生命周期与隔离通过
[ ] Cloud / Edge-Cloud：§3.6 + §4.8 全部通过
[ ] Harness P0 用例 100% 通过（test-harness）
[ ] 平台 E2E 矩阵零失败（test-online）

业务场景（第二层）
[ ] §4 中 8 类端到端场景手工验收通过（含 §4.8 Edge-Cloud）
[ ] 全新环境部署 30 分钟内可 Chat

Golden 回归（第三层）
[ ] ≥ 50 条 Golden Session 回放通过率 ≥ 95%

非功能
[ ] 性能达标（§6.1）
[ ] 混沌测试通过（§6.2）
[ ] 安全测试通过（§6.3）
[ ] 生产密钥与配置合规

灰度
[ ] Canary 24h 无回滚触发条件
[ ] Beta 72h 用户反馈无 P0/P1 未关闭问题

辅助（Benchmark 对标）
[ ] §7.3 P0+P1 业界统一 Benchmark 最小集已跑
[ ] 结果归档于 benchmarks/results/<version>/（含模型、版本、审计 Sidecar）
[ ] 相对上版本无 P0/P1 显著退化（或已书面说明）
[ ] 已知 Benchmark 局限性已写入对外文档
```

---

## 10. 缺陷与发布决策

| 级别 | 示例 | 能否发布 |
|------|------|----------|
| **P0** | 数据丢失、跨租户泄露、认证绕过、审计链断裂 | **否** |
| **P1** | Chat 不可用、记忆完全失效、不可恢复崩溃 | **否**（GA 阻塞） |
| **P2** | 单 Skill 异常、非核心 API 错误、性能退化 ≤ 20% | 可发布，须跟踪修复 |
| **P3** | 文档、UI、Benchmark 分数波动 | 可发布，记入已知问题 |

---

## 11. 测试环境与数据要求

| 要求 | 说明 |
|------|------|
| 环境 | 类生产：MatrixOne + Memoria + Redis + 真实 LLM |
| 模型 | 至少覆盖 GA 主模型 + 一个备用 Provider |
| 数据 | Golden Session、Benchmark 轨迹须脱敏 |
| 密钥 | 测试用 Key 不得提交 git；生产 Key 单独管理 |
| 记录 | 每次发布测试保留：版本号、模型、通过率、缺陷清单 |

---

## 12. 首版 GA 测试工作量估算

| 工作项 | 负责人 | 工期 |
|--------|--------|------|
| 平台功能 + 信任能力测试 | QA | ~2 周 |
| Cloud / Edge-Cloud 专项 | QA + 后端 | ~1.5 周 |
| 端到端业务场景验收 | QA + 产品 | ~1 周 |
| Golden Session 库建设（50 条） | QA | ~2 周 |
| Harness / E2E 自动化跑通 | QA + 后端 | ~1 周 |
| 性能 + 混沌 + 安全 | QA + SRE | ~2 周 |
| Benchmark 抽样（可选） | QA | ~1 周 |
| 内测 + 灰度 | 全员 | ~2 周 |
| **合计** | | **~9–11 周** |

---

## 附录 A：能力 ↔ 测试对照

| Astra 能力 | 发布必测 | Benchmark |
|------------|----------|-----------|
| Harness / fork / budget | Harness P0 用例 | Harness-Bench |
| Skills / 上下文 | skill_discovery 等 | eth-sri/agentbench |
| 工具编排 | E2E + tool_sequence | BFCL-V3-MT |
| MCP | `astra mcp` E2E | MCP-Universe |
| 权限 / 审批 | Permission 场景 | τ²-Bench |
| 记忆 | memory_full_lifecycle | ASTRA-bench †、LongMemEval |
| 审计 / 回放 | replay + audit API | 审计 Sidecar |
| Coding | verify_task | SWE-bench Verified、Terminal-Bench |
| 长任务 / Team | 委派、Team 场景 | AgencyBench |
| Cloud / Edge-Cloud | §3.6、§4.8 | edge_cloud_round_trip_e2e、system_matrix E2E |
| 信任 / 安全 | 隔离 + 渗透 | CAR-bench |
| Gateway / IM | 手工场景 §4.7 | 自建场景 |

## 附录 B：GA 前 Benchmark 执行清单

| # | Benchmark | 抽样 | 必跑 |
|---|-----------|------|------|
| 1 | Harness-Bench | 完整协议 | ✓ P0 |
| 2 | BFCL-V3-MT | 多轮子集 | ✓ P0 |
| 3 | SWE-bench Verified | 50 | ✓ P1 |
| 4 | AgencyBench | 10 | ✓ P1 |
| 5 | ASTRA-bench † | 100 | ✓ P1 |
| 6 | Terminal-Bench | 20 | P2 |
| 7 | LongMemEval | 20 | P2 |
| 8 | MCP-Universe | 子集 | P2 |
| 9 | τ²-Bench | 零售子集 | P2 |
| 10 | CAR-bench | 子集 | P2 |
| 11 | eth-sri/agentbench | 抽样 | P2 |

每次运行记录：`astra` 版本、模型、Benchmark 版本、抽样量、主指标、审计链完整率、日期、Token 成本。
