# Astra Web Agent Session State — 场景化验证索引

> Status: Draft v0.1
> Date: 2026-05-06
> Owner: astra runtime / web agent
> 关联设计文档: [web-agent-session-state.md](../web-agent-session-state.md)

## 目的

本目录收录一组「模拟用户 × Web Agent」的长会话交互场景，用于系统性验证
`web-agent-session-state.md` 设计方案是否能在真实的、超长的、复杂的用户工作流下：

1. 保证用户体验（快速恢复、不丢上下文、UI 响应快）
2. 节省 Token（hot/warm/cold 分层、context manifest、compaction 有效）
3. 保持审计完整（raw 事件不丢失、manifest 可回放、provenance 可追）
4. 支持扩展（plan/todo/delegation/skill 作为一等投影而非临时 JSON）

每个场景都是一个压力测试，试图触碰架构某一条核心假设的边界。

## 被测设计假设清单（Design Assumptions under Test, DAUT）

所有场景文档在「核心测试目标」章节应显式引用如下编号。

| 编号 | 假设 | 失败后果 |
| --- | --- | --- |
| **A1** | MatrixOne 是 web session 唯一权威，浏览器只发 `session_id` + 增量 watermark，服务端重建 context | 恢复失败 / 浏览器污染服务端状态 |
| **A2** | 审计状态（agent_events / conversation_log）与当前投影（session_state_items）与运行时 manifest（context_manifests）三张不同表，绝不互相替代 | 投影崩坏时无法回滚；context 污染审计 |
| **A3** | Hot/Warm/Cold 分层：hot 路径每次读都是有界索引扫描，不扫 JSON、不回放 10GB | 打开长 session 延迟不可接受 |
| **A4** | Context manifest 每轮必写、可查、含 dropped_items + reason + token_estimate | 失败/昂贵回合无法解释 |
| **A5** | 每轮 token 预算按 zone 分配（anchor/plan_todo/recent_tail/summary/retrieved/tool_previews 等），总和 ≤ context_window | 单 zone 挤爆预算 |
| **A6** | Compaction 是投影更新，不是隐藏摘要：L1a 结构化事实（files/decisions/todos/errors）必须保留 | 压缩后 agent 忘记任务状态 |
| **A7** | Run 是 durable（agent_runs + agent_run_events），server 重启 / SSE 断线可按 event_idx 续接 | 长运行一旦断开即丢失 |
| **A8** | `waiting_for_user` / `waiting_for_edge` / `waiting_for_external` 是 durable run 状态，审批/问询是 durable event | 审批后状态不一致 |
| **A9** | 大型工具输出走 session_tool_outputs + artifact_ref，prompt 里只进 preview_text | Token 随工具输出线性膨胀 |
| **A10** | 历史检索分三级：structured filters → full-text → vector（且 vector/FTS 表不频繁 UPDATE/DELETE） | 检索昂贵或失败 |
| **A11** | Plan / Todo 是一等结构化投影（session_todos + session_todo_deps），不是埋在自然语言中 | 继续任务时 agent 跑偏 |
| **A12** | 个人 skill 版本化、append-only、激活切指针；skill_registry 保持 content_hash 稳定 | 历史会话复现不出来 |
| **A13** | 父子 delegation（parent_run_id / delegation_id / session_state_items.category='delegation_state'）是一等关系 | 多 agent 协作状态混乱 |
| **A14** | Workspace 权威（cloud/edge/hybrid）持久化为 workspace_state，context 据此过滤可用工具 | 工具在错误环境执行 |
| **A15** | 下一步意图抽取优先级：structured event → rule → small model；低置信度进 suggested_next_action 而非自动执行 | 「继续」语义错误 |

## 场景清单

| 编号 | 标题 | 主要压测假设 | 文件 |
| --- | --- | --- | --- |
| **S01** | 超长开发马拉松：Rust 微服务 60 轮重构 | A2,A3,A4,A6,A10 | [S01-long-dev-marathon.md](S01-long-dev-marathon.md) |
| **S02** | 10GB 历史 session：找上个月那个 bug | A3,A6,A10 | [S02-huge-session-old-bug.md](S02-huge-session-old-bug.md) |
| **S03** | 跨设备恢复：laptop → iPad → 同事屏幕 | A1,A3,A4,A14 | [S03-cross-device-resume.md](S03-cross-device-resume.md) |
| **S04** | 断线续传：5 小时 run 中网络抖动 17 次 | A1,A7,A8 | [S04-flaky-network-reconnect.md](S04-flaky-network-reconnect.md) |
| **S05** | 反复调整 plan/todo：产品经理改 8 次需求 | A6,A11,A15 | [S05-plan-thrashing.md](S05-plan-thrashing.md) |
| **S06** | 嵌套 plan + 子 plan：实现一个编译器 | A6,A11,A13 | [S06-nested-plan-compiler.md](S06-nested-plan-compiler.md) |
| **S07** | 长等待审批：高危 SQL 等待人工 48 小时 | A7,A8,A14 | [S07-long-approval-wait.md](S07-long-approval-wait.md) |
| **S08** | 超大工具输出：pg_dump 3GB + 1000 文件扫描 | A3,A9,A10 | [S08-giant-tool-output.md](S08-giant-tool-output.md) |
| **S09** | 多 agent 并行：前端+后端+DBA 三 agent 交付一个 feature | A4,A5,A13 | [S09-multi-agent-feature.md](S09-multi-agent-feature.md) |
| **S10** | Delegation 深层嵌套：orchestrator → planner → 4 executors → reviewer | A4,A13,A15 | [S10-deep-delegation.md](S10-deep-delegation.md) |
| **S11** | 跨 session 记忆：三个月前那个数据库 schema | A3,A10,A12 | [S11-cross-session-memory.md](S11-cross-session-memory.md) |
| **S12** | 研究型长会话：200 个 URL、50 篇论文综述 | A3,A9,A10 | [S12-research-synthesis.md](S12-research-synthesis.md) |
| **S13** | 个人 skill 迭代：用户现场写 7 版 SKILL.md | A2,A12,A15 | [S13-personal-skill-iteration.md](S13-personal-skill-iteration.md) |
| **S14** | 极端边界：低端模型 8k 窗口 + workspace 切换 + 「继续」语义歧义 | A1,A5,A14,A15 | [S14-edge-cases.md](S14-edge-cases.md) |

## 场景文档统一结构

每个 `Sxx-*.md` 必须包含以下章节（顺序固定，便于横向 diff 与后续脚本抽取）：

1. **场景概览**
   - 用户画像、业务背景、一句话目标
   - 关键数值指标（预计总轮数、session 持续时间、工具调用数、artifact 数）
2. **被测设计假设**
   - 显式列出本场景压测的 DAUT 编号与解释
3. **会话时间线**（核心内容）
   - 表格或分段叙事，覆盖 T1..Tn
   - 每个关键回合含：
     - 用户输入（自然语言，模拟真实用户语气）
     - 期望的 state 变化（哪些 session_state_items 被增/改、哪些 run event 被写、哪些 artifact 被创建）
     - 期望的 context manifest：zones & tokens & 被 drop 的候选
     - LLM 能看到什么 / 明确不能看到什么
4. **上下文压力点**
   - 本场景对 token 预算 / 检索 / compaction / run 生命周期的关键挑战
5. **期望的 state projection 演化**
   - 列出 session_state_items、session_todos、session_artifacts、context_manifests 在关键节点的快照
6. **Token 节省估算**
   - 对比「朴素方案：把整条 transcript 塞给 LLM」与「本设计方案」的 token 估算
   - 给出节省百分比与计算依据
7. **潜在失败模式**
   - 如果设计实现不到位，本场景会在哪里暴露问题？列出 3–6 个可观测的失败信号
8. **对设计的反馈**
   - 本场景揭示了设计文档中哪些章节需要补强或明确？
   - 如果有 open question 无法用现有设计回答，写在这里

## 设计能力评估矩阵

基于 14 个场景第 8 章「对设计的反馈」反向推断。空白格表示该场景不显式压测该假设；填值仅针对显式涉及的 DAUT。

取值：
- ✅ 完全支持
- ◐ 基本支持，需补实现细节
- ⚠️ 设计有覆盖但假设偏弱（存在明显 gap，需补强）
- ❌ 当前设计缺失

| 场景 \ 假设 | A1 | A2 | A3 | A4 | A5 | A6 | A7 | A8 | A9 | A10 | A11 | A12 | A13 | A14 | A15 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S01 |   | ✅ | ✅ | ◐ |   | ⚠️ |   |   |   | ⚠️ |   |   |   |   |   |
| S02 |   |   | ✅ |   |   | ◐ |   |   |   | ⚠️ |   |   |   |   |   |
| S03 | ◐ |   | ✅ | ◐ |   |   |   |   |   |   |   |   |   | ⚠️ |   |
| S04 | ◐ |   |   |   |   |   | ⚠️ | ◐ |   |   |   |   |   |   |   |
| S05 |   |   |   |   |   | ◐ |   |   |   |   | ⚠️ |   |   |   | ⚠️ |
| S06 |   |   |   |   |   | ⚠️ |   |   |   |   | ⚠️ |   | ⚠️ |   |   |
| S07 |   |   |   |   |   |   | ⚠️ | ◐ |   |   |   |   |   | ⚠️ |   |
| S08 |   |   | ✅ |   |   |   |   |   | ◐ | ✅ |   |   |   |   |   |
| S09 |   |   |   | ◐ | ⚠️ |   |   |   |   |   |   |   | ⚠️ |   |   |
| S10 |   |   |   | ◐ |   |   |   |   |   |   |   |   | ⚠️ |   | ◐ |
| S11 |   |   | ◐ |   |   |   |   |   |   | ⚠️ |   | ⚠️ |   |   |   |
| S12 |   |   | ✅ |   |   |   |   |   | ⚠️ | ⚠️ |   |   |   |   |   |
| S13 |   | ✅ |   |   |   |   |   |   |   |   |   | ⚠️ |   |   | ◐ |
| S14 | ◐ |   |   |   | ⚠️ |   |   |   |   |   |   |   |   | ⚠️ | ◐ |

### 按 DAUT 汇总（被测场景数 + 最差评级）

| 假设 | 被测场景数 | 综合评级 | 主要 gap 来源 |
| --- | --- | --- | --- |
| A1 跨设备权威 | 3 (S03/S04/S14) | ◐ | 多 tab 行为、device fingerprint、state_revision gap 语义 |
| A2 审计/投影/manifest 三分 | 2 (S01/S13) | ✅ | 三表分工清晰，长场景验证通过 |
| A3 hot/warm/cold 分层 | 6 (S01/S02/S03/S08/S11/S12) | ✅/◐ | 跨 session 索引路径缺失（S11） |
| A4 每轮 manifest 必写 | 4 (S01/S03/S09/S10) | ◐ | manifest.reason 未枚举化，观测不足 |
| A5 zone 预算 | 2 (S09/S14) | ⚠️ | 小窗口模板缺、delegation zone 未在 zone 表 |
| A6 compaction 保 L1a | 4 (S01/S02/S05/S06) | ⚠️ | invariants 未契约化、plan tree 压缩规则缺失 |
| A7 durable run | 2 (S04/S07) | ⚠️ | InMemoryRunStateStore 仍是默认、graceful vs crash 缺 |
| A8 审批 durable event | 2 (S04/S07) | ◐ | conditions 子表、state machine、expired/retracted 缺 |
| A9 大型 tool 输出 | 2 (S08/S12) | ◐/⚠️ | preview template registry 缺、retention 双向感知缺 |
| A10 三级检索 | 5 (S01/S02/S08/S11/S12) | ⚠️ | SLA + 降级 state machine 缺、跨 session 索引缺、content_hash 失效重建流程缺 |
| A11 plan/todo 一等 | 2 (S05/S06) | ⚠️ | 跨 session backlog、plan_state 版本化、plan tree rendering policy 均缺 |
| A12 personal skill | 2 (S11/S13) | ⚠️ | per-session activation 语义、eval 独立表、content_hash 规范化缺 |
| A13 delegation 一等 | 3 (S06/S09/S10) | ⚠️ | child = session / run / both 未定、payload shape 未 spec、retry_scope + bubble_up 缺 |
| A14 workspace 权威 | 3 (S03/S07/S14) | ⚠️ | reachability_probe 字段缺、edge 降级语义缺、waiting_for_edge 超时策略缺 |
| A15 next-action 分级 | 4 (S05/S10/S13/S14) | ◐/⚠️ | confidence 阈值未定、ambiguity 状态机未闭环 |

## 跨场景总结

### Token 节省总体结论

| 场景 | 朴素方案 | 本设计方案 | 节省比例 | 关键机制 |
| --- | --- | --- | --- | --- |
| S01 第 60 轮 | ~200k (超窗) | ~45k | 78% | compaction + manifest zone |
| S02 10GB 检索 | ~2.5B (不可能) | ~9.7k | 99.9%+ | 三级检索 + raw_ref |
| S03 跨设备开 session | 每次完整 transcript | 几十 KB delta | 99%+ | state_revision + watermark |
| S04 17 次断连 | 重放成本 17× LLM | 0 重放 | N/A (正确性非 token) | durable run |
| S05 第 8 次改需求 | ~20k | ~3.4k | 83% | active todos only |
| S06 编译器第 40 轮 | 超 200k 窗口 | ~4-5k | 97%+ | plan tree filter |
| S07 48h 审批 | 48h 后 reconstr 不可靠 | <500 token | N/A (合规) | durable approval_state |
| S08 DBA 审计 | ~1B (3GB raw) | 每轮 ≤5.2k | 99.9%+ | artifact_ref + preview |
| S09 多 agent | ~80k | ~5k | 94% | delegation_state zone |
| S10 5 层 delegation | ~200k (指数膨胀) | ~5k | 97%+ | 树形 summary |
| S11 跨 session 记忆 | ~4M (8 sessions 合并) | ~6k | 99.85% | cross-session chunk retrieval |
| S12 研究综述 | ~50M | ~7.5k | 99.98% | structured preview + finding chunks |
| S13 skill 迭代 | ~30k | ~7.8k | 74% | append-only + active pointer |
| S14 小窗口 8k | 根本不可行 | 7.5k 压线 fit | N/A (可行性) | 激进 drop + 递进加载 |

**结论**：
- 6 个场景达成 90%+ token 节省（S02/S03/S08/S10/S11/S12）
- 3 个场景的节省**不是 token 问题而是可行性**（S04/S07/S14），即朴素方案根本不能跑
- 3 个场景节省 70–85%（S01/S05/S13），是长会话日常
- 核心机制 ROI 排名：**artifact_ref/preview > 三级检索 > compaction > delegation zone > state_revision watermark**

### 被反复触发的设计 gap（按场景共振度）

| # | Gap | 触发场景 | 共振度 |
| --- | --- | --- | --- |
| G1 | `context_manifests.reason` 未枚举化 | S01, S06 | 高（观测 + 压力定位） |
| G2 | Compaction Invariants 未契约化（L1a 白名单） | S01, S04, S06, S12 | 极高 |
| G3 | 三级检索 SLA + 降级 state machine 缺 | S01, S02 | 高 |
| G4 | `delegation_state.payload_json` 未 spec | S06, S09, S10 | 极高（multi-agent 阻塞） |
| G5 | Plan Tree Rendering Policy 缺（4 层以上） | S06 | 高 |
| G6 | 跨 session scope（`session_state_items.scope='user'`）缺 | S05, S11 | 高 |
| G7 | Approval conditions 子表 + external notification adapter 缺 | S07 | 中（合规） |
| G8 | Preview Template Registry（按 tool_name 分级） | S08, S12 | 高 |
| G9 | Artifact retention 与 manifest 引用双向感知缺 | S08, S12 | 高 |
| G10 | 小窗口 Budget Template（≤16k）缺 | S14 | 中（私有化部署必需） |
| G11 | Workspace `reachability_probe` 字段缺 | S03, S14 | 中 |
| G12 | Next-Action confidence 阈值未定义 | S05, S14 | 中 |
| G13 | `state_revision` 不纳 device fingerprint | S03, S14 | 中（安全） |
| G14 | `bubble_up` 一等事件 + `retry_scope` 字段缺 | S10 | 中 |
| G15 | `agent_run_events.event_idx` 生成协议（单 writer / run ownership） | S04 | 高（durable run 正确性） |
| G16 | Personal skill per-session activation 语义缺 | S11, S13 | 中 |
| G17 | `content_hash` 规范化规则缺 | S08, S13 | 中 |

### TOP 5 设计文档改进建议（进入 v0.2）

#### TOP 1 — Compaction Invariants 契约化 + Manifest reason 枚举

**覆盖 gap**: G1, G2
**影响**: 所有长 session（S01/S02/S04/S05/S06/S12）
**修改点**:
- 新增 §"Compaction Invariants" 节，8 条白名单规则 + 每条配 SQL assertion
- `category IN ('plan_state','decision','todo_state(active)','error_state(active)','delegation_state(active)')` 绝不压缩
- `context_manifests.reason` 升级为 Rust enum，含 `initial_turn` / `post_compaction` / `history_recall_{structured|fts|vector}` / `large_tool_output_gated` / `plan_subtree_query` / `tree_structured_report` / `workspace_switch` / `approval_resume` / `cross_session_recall` 等
- compaction trigger 必须检查 `status NOT IN ('running','waiting')`（S04 F4）

**实现成本**: 低（schema + 文档 + 几条 migration）
**回报**: 立即提升调试可聚合性、防止 compaction 破坏任务状态

#### TOP 2 — Delegation 体系一等化（multi-agent 基础设施）

**覆盖 gap**: G4, G5, G14
**影响**: S06/S09/S10 — multi-agent 全线
**修改点**:
- Open Question "child agents first-class sessions / run children / both" 明确为 **both**：
  - `agent_runs.parent_run_id` 始终持有父子 run 关系
  - `agent_sessions` 仅在子 agent 需要独立 identity（用户可见、跨 session 复用）时创建
- `session_state_items(category='delegation_state')` 的 `payload_json` 升级为有 schema JSON（9 字段：child_session_id / delegation_id / depth / root_run_id / ancestor_path / status / last_summary_ref / exposed_artifacts / directive），或抽成独立表 `session_delegations`
- 新增 §"Plan Tree Rendering Policy"：祖先链必渲染 / 当前 section 全渲染 / 兄弟 section 仅 title / 跨子树作为 explicit API
- `agent_runs` 加 `retry_scope ENUM('node','subtree','siblings')` + 原 run 状态转 `superseded`
- `session_state_item_events.mutation` 加 `bubble_up` / `apply_suggestion` 枚举

**实现成本**: 中（schema + 渲染器 + 文档）
**回报**: 解锁真正的多 agent 协作产品能力

#### TOP 3 — Retrieval SLA + 跨 session 索引 + Preview Template Registry

**覆盖 gap**: G3, G6, G8, G17
**影响**: S01/S02/S08/S11/S12 — 所有涉及检索/大输出
**修改点**:
- 新增 §"Retrieval State Machine"：structured < 50ms / FTS < 200ms / vector < 500ms；每级失败写 `agent_events` 事件 `retrieval.<stage>_<reason>`
- `session_history_chunks` 增 `(user_id, chunk_type, created_at)` 复合索引
- `session_state_items` 增 `scope ENUM('session','user','project','workspace')`；cross-session memory 落在 `scope='user'`
- 新增 §"Preview Template Registry"：每个 tool 声明 `preview_template.yaml`（version / first_class_columns / fts_field_weights / max_preview_bytes），runtime 校验
- `content_hash` 规范化 contract：声明 `normalize_version`，schema 变更时 bump
- `chunk_type` 枚举扩充 `finding` / `benchmark` / `citation`（研究类一等结构化单元）

**实现成本**: 中高（索引 + 新表/字段 + runtime 改造）
**回报**: 长 session 记忆与工具输出的可行性从 "能跑但不稳" → "可生产"

#### TOP 4 — Run Durability + Approval 体系产品化

**覆盖 gap**: G7, G15
**影响**: S04/S07 — 所有长运行、所有合规场景
**修改点**:
- §"Durable Runs" 补 `run_counters` 或单 writer 选举协议，规定 `event_idx` 生成路径
- Phase 1 Exit Criteria 拆分 graceful shutdown（checkpoint.graceful=true → resume）vs crash（标 failed）
- `agent_run_events` 新增事件类型：`approval_expired` / `approval_retracted` / `run_resumed_after_restart` / `notification_dispatched`
- POST `/chat/runs/{run_id}/input` 强制 `idempotency_key`（服务端去重）
- Approval `conditions[]` 从 `payload_json` 升级为独立子表 `session_approval_conditions`（支持按 condition_type + check_trigger 索引查询）
- 新增 §"External Notification Adapter"：Lark/Slack 推送协议 + delivery receipt 事件
- `waiting_for_edge` 加 per-run timeout；默认 300s 超时转 `failed` 或 `waiting_for_user`
- 移除 `InMemoryRunStateStore` 作为默认，生产必须 `DatabaseRunStateStore`

**实现成本**: 中高（已在 Phase 1 计划里，但需补 approval 子表 + 外部通知）
**回报**: 产品上线的硬阻塞项清除

#### TOP 5 — Personal Skill 激活语义 + 跨 session memory + 小窗口 Budget

**覆盖 gap**: G10, G12, G13, G16
**影响**: S05/S11/S13/S14 — 长期用户与私有化部署
**修改点**:
- §"Personal Skills" 补 per-session activation 语义：`skill_installations`（user-level 可用）≠ auto-activated；per-session 激活走 `session_state_items(scope='session', category='active_skill', payload_json.version_id=<frozen>)`；`version_id` 固定快照，不跟 registry `active_version` 自动切换
- 新表 `user_skill_evaluations`（独立于 session_state_items）承载 skill 试用反馈
- §"Budget Policy v1" 加 `budget_v1_8k` 模板（anchor 200 / plan_todo 400 / recent_tail 2000 / summary 500 / retrieved 1000 / tool_previews 500 / system+tool_schemas 3400）
- §"Next-Action Extraction Cost" 补 confidence 阈值：`≥0.8` 自动 / `0.5–0.8` ask_user / `<0.5` 拒绝；1h 内连续 3 次 ask_user 降阈值
- `session_state_items(category='workspace_state').payload_json` 加 `reachability_probe { last_ok_at, last_fail_at, probe_method, rtt_ms }`
- `state_revision` 包含 device_fingerprint hash；API 服务端比 `monotonic_id` 而非整体 hash

**实现成本**: 低–中（主要是文档 + 新字段 + 新小表）
**回报**: 私有化部署场景可用、跨 session 学习效应开始累积

### 整体结论

**设计文档在高层架构（三张表分工、hot/warm/cold、manifest 契约、durable run、三级检索）上方向正确**，14 个场景没有任何一个要求推翻顶层架构。但：

1. **Schema 层面 7 处关键字段/表/索引需补**（G3/G4/G6/G7/G8/G14/G15）
2. **契约层面 5 处枚举/状态机未闭环**（G1/G2/G9/G12/G17）
3. **Delegation / Multi-agent 需要一次集中加固**（G4/G5/G14）才能支撑产品化 multi-agent
4. **小窗口 + 跨设备 + 跨 session 是"私有化落地"三要素**，目前任何一条都有明显 gap（G6/G10/G11/G13）

建议以 **TOP1–TOP5** 作为 v0.2 修订的骨架。TOP1/TOP2 属于基础不可推迟；TOP3/TOP4 是产品上线硬阻塞；TOP5 是跨期产品竞争力的奠基。
