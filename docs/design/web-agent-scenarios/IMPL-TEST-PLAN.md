# Implementation Test Plan — astra Web Agent Session State

> Status: Draft v1.0
> Date: 2026-05-07
> Owner: regression reviewer
> 目的: 建立"实现 ↔ 设计"之间的**强契约锚点**，防止 Phase 1-6 实现过程中偏离 14 场景推演所依赖的假设
> 参考: design doc v0.3 (3653 行) / 14 WALKTHROUGH-S<NN>.md / REGRESSION-SPRINT-A/B/C/D.md

## 为什么需要这份文件

Sprint A-D 历经 4 轮 "patch → 回归"，19 gap review + 10 端到端推演 gap 全部处理。但到目前为止我们只验证了**设计文档上的一致性**，没验证**代码实现是否遵守设计**。

实现期最常见的偏离模式：
1. **实现者图方便自己改 schema**（少一列、改一个命名、合并两个表）
2. **实现者"意会"契约**（`check_trigger` 字符串随意构造，`reason` 枚举塞自由值）
3. **实现者绕过不变量**（compaction 里 update 了 active `plan_state`，单 writer 被省略）
4. **Web 端自作主张**（browser 拿 state_revision 直接改、cold-start 不走 transcript replay）

**这份测试方案的作用**：每个 Phase 结束时，我按下列清单验收，任意一项不过就要求设计 agent 修正。

## 契约验收原则

**三层防线**：

| 防线 | 工具 | 作用 |
| --- | --- | --- |
| **L1 Schema assertion** | SQL CHECK constraint / DB migration test | 防止 DDL 偏离设计（字段/类型/索引/枚举）|
| **L2 Contract test** | Rust integration test | 防止行为偏离设计（状态机/协议/不变量）|
| **L3 Scenario E2E test** | end-to-end test 跑真实场景 | 防止组件组合后偏离（14 场景里的关键片段）|

每条测试点标注防线级别和场景锚点。

## Phase 1: Run Durability

**目标**: `agent_runs` + `run_counters` + `agent_run_events` 三张表 + `DatabaseRunStateStore` 实现。

**涉及 Gap**: G15 (run durability) / G19 (watermark atomicity) / G22 (retry_scope) / G23 (batch insert) / G29 (checkpoint.extra)

### L1 Schema 验收（6 条）

| # | 验收项 | 证据来源 |
| --- | --- | --- |
| L1-1 | `run_counters` 表含 `next_event_idx` / `owner_pod_id` / `owner_lease_expires_at` / `run_generation` 字段 | v0.3 L778-789 |
| L1-2 | `agent_run_events` 含 `UNIQUE KEY uq_run_event_idx` + `UNIQUE KEY uq_run_event_idempotency` | v0.3 L807-808 |
| L1-3 | `agent_runs` 含 `root_run_id` / `ancestor_path` / `depth` / `retry_of` / `retry_scope` | v0.3 L743-747 |
| L1-4 | `retry_scope` CHECK constraint 值 ∈ {'node', 'subtree', 'siblings'} | G22 resolution |
| L1-5 | `checkpoint_v1` 的 JSON schema 文档化（version / graceful / last_batch_id / extra.partial_progress） | G29 resolution |
| L1-6 | `session_tool_output_batches` 表存在 | G23 resolution |

### L2 Contract 验收（9 条）

| # | 验收项 | 场景锚点 | 测试类型 |
| --- | --- | --- | --- |
| L2-1 | 并发两个 pod 抢同一 run 的 lease，只有一个拿到（advisory lock / CAS on `owner_pod_id`）| S04 T11 | concurrent test |
| L2-2 | event_idx 分配走 `run_counters` FOR UPDATE，从不扫 MAX(event_idx) | S04 T07 | query trace |
| L2-3 | pod graceful shutdown 写 `checkpoint.graceful=true`，新 pod 恢复发 `run_resumed_after_restart` 事件 | S04 T11, S07 T5/T7 | integration |
| L2-4 | pod crash (无 checkpoint) 下一 pod 标 `status='failed'`，**不**继续同 run | S04 F7 | fault injection |
| L2-5 | 重复 `POST /chat/runs/{id}/input` with same `idempotency_key` 只产生一次事件 | S04 T14 | integration |
| L2-6 | SSE heartbeat server 15s / client 45s 超时判死 | S04 F5 | timing test |
| L2-7 | retry_of 存在时 retry_scope 必须填；**原 run 切 `'superseded'`（⚠️ 延后至 Phase 4）** | S10 T8 | Phase 4 |
| L2-8 | batch_insert ≤500 行 / ≤16 MiB，超过拆 batch | S08 T5 | property test |
| L2-9 | 1000 行 session_tool_outputs INSERT < 500ms（MatrixOne 本地实例基准）| S08 blocker | performance |

**注（IMPL-VERIFY-PHASE-1 反馈）**：
- L2-7 拆为两部分：字段持久化（retry_scope 列 + payload）属 Phase 1；`superseded` 状态转换属 Phase 4（与 G14 mutation 契约同步）。Phase 1 验收只验前者。
- L2-1 到 L2-9 对应测试均用 `#[ignore = "requires ASTRA_TEST_DB_IT=1"]` 标记，**CI pipeline 必须显式启用** `ASTRA_TEST_DB_IT=1` 才能跑到行为分支，否则 PR merge 前人工确认在本地 MatrixOne 实例跑过。

### L3 Scenario E2E（3 条）

| # | 场景片段 | 期望 |
| --- | --- | --- |
| L3-1 | **S04 T01-T17 断线续传**：模拟 17 次断连 + 1 次 pod 重启 + 2 次审批，最终 SSE 事件无丢失、无重复，event_idx 单调递增 | 全链路不丢事件 |
| L3-2 | **S08 T5 批量工具输出**：扫 1000 文件，每个写 session_tool_outputs，整体回合完成 < 2s | batch 生效 |
| L3-3 | **S10 T8 retry_scope**：user 说"重做 executor-2"，agent 正确携带 retry_scope，原 executor run 变 superseded | scope 传递 |

**注（IMPL-VERIFY-PHASE-1 反馈）**：
- L3-1 Phase 1 仅做"追加+重读 event_idx 单调"骨架测试；真 SSE `?last_index=N` 多次 reconnect 的完整 E2E 要等 Phase 2 Web hook 就位后补充一次跨 Phase 回归。
- L3-3 Phase 1 仅验 `retry_scope` 字段持久化（payload 含该字段的断言）；`superseded` 状态转换归 Phase 4 完成，届时补充对称测试。

### Phase 1 退出条件

- [ ] 6/6 L1 schema assertions 全过
- [ ] 9/9 L2 contract tests 全绿
- [ ] 3/3 L3 scenario E2E 全跑通
- [ ] Changelog v0.3 与实现 commit 双向挂钩

---

## Phase 2: Web Transcript Hydration + Cold-Start + Device Lease

**目标**: `/sessions/{id}/state` / `/sessions/{id}/transcript` / `session_device_leases` / IndexedDB 原子写入 / cold-start hydration。

**涉及 Gap**: G13 (device fingerprint) / G19 (watermark atomicity) / G24 (cold-start) / G25 (SSE parity)

### L1 Schema（4 条）

| # | 验收项 | 证据 |
| --- | --- | --- |
| L1-7 | `session_device_leases` 11 列齐全（含 trust_level / last_monotonic_id / expires_at） | v0.3 L454-473 |
| L1-8 | `session_device_lease_events` 表存在 + 支持 auto_expire 事件类型 | G25 resolution |
| L1-9 | `session_transcript_items` 主键 (session_id, item_seq) | v0.3 原 schema |
| L1-10 | `state_revision.revision_hash` 字段含 device_fingerprint 计算依据 | G13 resolution |

### L2 Contract（8 条）

| # | 验收项 | 场景锚点 |
| --- | --- | --- |
| L2-10 | 浏览器端 IndexedDB events + watermark 同 transaction 写 | S04 T08 |
| L2-11 | gap 检测：`event_idx > last_ok_idx + 1` → 中止 batch，reconnect `last_index=last_ok_idx` | S04 F1 |
| L2-12 | 多 tab via BroadcastChannel 共享 watermark，idempotent apply 判重 | S04 T09 |
| L2-13 | `device_revoke` 与 `device_lease_expired` 的 SSE payload 对称，client SDK 在任一事件触发 localStorage 清理 | S03 §5 B |
| L2-14 | cold-start: `known_state_revision=0` 且 `active_run.run_event_high_watermark>0` 时，响应 `replay_required=true` | S04 §5, G24 |
| L2-15 | state_revision 比较：server 比 `monotonic_id`，full `revision_hash` 用于 rollback 检测 | S03 T11 |
| L2-16 | trust_level 从 `new_device` 升级需要 write action + step-up confirmation | S03 T8 |
| L2-17 | revoke API (`POST /sessions/{id}/device/revoke`) 带 CAS，幂等 | G13 resolution |

### L3 Scenario（3 条）

| # | 场景片段 | 期望 |
| --- | --- | --- |
| L3-4 | **S03 跨设备**：MacBook → iPad 冷启 → Chromebook 零信任 → MacBook 回归。4 段 transcript 无丢失，workspace_state 切换正确，Chromebook localStorage 清除 | 设备切换全链路 |
| L3-5 | **S04 T09 双 tab**：同 session 开 tab#1 + tab#2，tab#2 收到 tab#1 的 watermark 广播但发 `stream?last_index=0` 全量 replay 而非跳过 | cold-start 路径 |
| L3-6 | **S03 T8 Chromebook 未 logout 关网页**：2 小时后 lease 自动过期，server 发 `device_lease_expired` SSE，虚拟测试客户端收到并清理 | passive expiry parity |

### Phase 2 退出条件

- [ ] 4/4 L1 + 8/8 L2 + 3/3 L3
- [ ] 浏览器真机测试（Chrome 稳定版 + Safari）IndexedDB 行为一致

---

## Phase 3: Context Manifest + Retrieval + Budget + Confidence

**目标**: `context_manifests` + manifest reason 枚举 + 三级检索 SLA + budget_v1_8k / budget_v1_200k + delegation budget 公式 + confidence 状态机。

**涉及 Gap**: G1/G3/G10/G12/G18/G21/G26 (manifest + retrieval + budget)

### L1 Schema（5 条）

| # | 验收项 | 证据 |
| --- | --- | --- |
| L1-11 | `context_manifests.reason` 枚举含 16 seed + 9 新增（G26）| v0.3 L1504-1515 + G26 resolution |
| L1-12 | `context_manifests` 含 `turn_intent` / `tokenizer_id` / `budget_template_id` | G10, G26, S12 |
| L1-13 | `context_manifest_items.render_mode` 枚举（含 `code_block_preserved`）| S02 §5 |
| L1-14 | unknown reason fallback 到 `'other'` + 告警事件 schema | G26 resolution |
| L1-15 | `raw_ref` 字段注册表（artifact://, s3://, conversation_log://）| G27 resolution |

### L2 Contract（14 条）

| # | 验收项 | 场景锚点 |
| --- | --- | --- |
| L2-18 | 每个 LLM call 必写一条 `context_manifests` | S01 T60 |
| L2-19 | manifest items 含 included=FALSE 的 dropped 条目（可审计 dropped_count）| S06 T22 |
| L2-20 | 三级检索 SLA：structured <50ms, FTS <200ms, vector <500ms；超时降级写 `retrieval.<stage>_<reason>` 事件 | S02 T2 |
| L2-21 | vector 检索前按 `content_hash + normalize_version` 双字段比对判 stale | S02 F3, G17+G27 |
| L2-22 | cross-session retrieval 必须带 `user_id` filter，缺失报 `CrossSessionAuthMissing` | S11 T3 |
| L2-23 | G21 delegation budget 公式：per_child = max(200, floor(1500/n))；n>7 按 priority 过滤到 top-K | S10 F-blocking-1 |
| L2-24 | delegation zone 在 blocker 状态下扩 2×，从 recent_tail 借（recent_tail floor 1600）| S09 T10 |
| L2-25 | budget_v1_8k 模板：anchor 200 / plan_todo 400 / recent_tail 2000 / summary 500 / retrieved 1000 / tool_previews 500 / system+tool_schemas 3400 | S14 T5 |
| L2-26 | budget property test: fan-out n=1/3/5/7/8/10/15 各值下预算 ≤ cap | G21 resolution |
| L2-27 | confidence ≥0.8 auto-accept / 0.5-0.8 ask_user / <0.5 拒绝 | S14 T5 |
| L2-28 | 同 session 1h 内 3 次 ask_user 降阈值，小模型独立候选不生效 | S14 F4 |
| L2-29 | suggested_next_action 有 expires_at: approval 24h / todo 7d / hint 1h | G12 resolution |
| L2-30 | tool preview 按 preview_template_registry 分级长度：per-file 80 / 聚合 1200 | S08 T5 |
| L2-31 | unknown tool 无 template → fallback 400B + 告警 | G8/G27 |

### L3 Scenario（4 条）

| # | 场景片段 | 期望 |
| --- | --- | --- |
| L3-7 | **S02 T1-T10 10GB 检索**：3 级检索命中 + raw SQL 不截断 + 9.7k token manifest | 三级检索 SLA |
| L3-8 | **S14 T5 8k 窗口歧义**：小窗口下所有 zone 总和 ≤ 7300，ask_user 候选 ≤3，confidence 分级正确 | 小窗口 + confidence |
| L3-9 | **S10 T4 + T9 多子 delegation 预算**：3 child 平均 500 token，n=8 时 top-K 过滤 | fan-out 公式 |
| L3-10 | **S01 T38 第二次 compaction**：token 净节省 ≥1000，manifest reason='post_compaction'，dropped_count 准确 | compaction 正确性 |

### Phase 3 退出条件

- [ ] 5/5 L1 + 14/14 L2 + 4/4 L3
- [ ] budget property test 手工验算 7 个 n 值全绿

---

## Phase 4: State Projection + Compaction + Delegation + Cross-Session Memory

**目标**: `session_state_items` + scope='user' memory + compaction invariants 8 条 SQL + delegation tree + bubble_up + retry_scope + personal skill activation。

**涉及 Gap**: G2/G4/G5/G6/G14/G16/G20 (projection + compaction + delegation + memory)

### L1 Schema（5 条）

| # | 验收项 | 证据 |
| --- | --- | --- |
| L1-16 | `session_state_items.scope` 枚举 {'session', 'user', 'project', 'workspace'} | G6 resolution |
| L1-17 | `session_state_item_events.mutation` 枚举 8 值 + CHECK constraint | G14 resolution |
| L1-18 | `session_delegations` 表 15 列齐全（含 root_run_id / ancestor_path / depth / last_summary_ref） | G4 resolution |
| L1-19 | `session_artifacts.access_scope` 枚举 + 明确 deprecated 的 'delegation' 值 | G20 resolution |
| L1-20 | `session_artifacts_grants` 表（G20 ACL 矩阵） | G20 resolution |

### L2 Contract（12 条）

| # | 验收项 | 场景锚点 |
| --- | --- | --- |
| L2-32 | Compaction invariants 8 条 SQL assertion 每次 compaction 跑过全返 0（除 manifest count=1） | S01 T38 / S06 T22 |
| L2-33 | active `plan_state` / `decision` / `finding` / `benchmark` / `citation` / active `todo_state` / active `error_state` / active `delegation_state` 绝不被 compaction replace/archive/delete | G2 |
| L2-34 | `plan_state.version` 在 compaction 轮次不 bump | G2 inv#3 |
| L2-35 | session-level compaction trigger 前检查 `agent_runs.status NOT IN ('running', 'waiting')` | G2 inv#4 |
| L2-36 | `session_state_items` ↔ `session_delegations` 同事务 upsert | G4 Projection Sync |
| L2-37 | bubble_up 事件 5 层传递：L4 → L3 → L2 → L1 → L0，每层写 state_item_events.mutation='bubble_up' | S10 T7 |
| L2-38 | cross-tree artifact access：same_root_tree scope 允许同 root 兄弟访问；L2 reporter ↔ L3 executor-2 artifact 可达 | S09 T6 / S10 T10 |
| L2-39 | scope='user' memory 在新 session open 时按 `(user_id, scope='user', status='active')` 加载到 anchor zone，预算 ≤400 token | S11 T3 |
| L2-40 | cross-session retrieval SQL 命中 `(user_id, chunk_type, created_at)` 复合索引（EXPLAIN ANALYZE 确认）| S11 T3 |
| L2-41 | personal skill per-session activation：`active_skill` state_item + frozen `version_id` | S13 T8 |
| L2-42 | skill activate 不走 LLM，走 UI structured intent | S13 T8 |
| L2-43 | backlog_pool_id 跨 session 可恢复：新 session 按 pool_id 挂回 todos | S05 §5 |

### L3 Scenario（5 条）

| # | 场景片段 | 期望 |
| --- | --- | --- |
| L3-11 | **S05 T1-T10 plan thrashing**：8 次需求改动后 active todos ≤3，cancelled/backlog 总 ~15 | plan_state 版本化 |
| L3-12 | **S06 T22 compaction + 60 todos**：plan_todo zone ≤800 token，tree 骨架保留 | G5 + G2 联动 |
| L3-13 | **S09 T6 跨兄弟 artifact**：be-agent 读 dba-agent 的 migration SQL artifact | G20 访问矩阵 |
| L3-14 | **S10 T7 bubble_up**：L3 critical finding 5 层传到 L0，< 100ms 延迟 | delegation tree |
| L3-15 | **S11 T3-T8 跨 session**：3 月前 decision chunk 命中 FTS + provenance 含 source_session_id | cross-session memory |

### Phase 4 退出条件

- [ ] 5/5 L1 + 12/12 L2 + 5/5 L3
- [ ] compaction invariants 8 条 SQL 日常 CI 跑

---

## Phase 5: Personal Skills

**目标**: `user_skill_sources` + `user_skill_versions` + `user_skill_evaluations` + activation state + content_hash normalization。

**涉及 Gap**: G16 / G17

### L1 Schema（4 条）

| # | 验收项 |
| --- | --- |
| L1-21 | `user_skill_sources` + `user_skill_versions` 主外键 |
| L1-22 | `user_skill_evaluations` 表独立（非 session_state_items）|
| L1-23 | `version.status` 枚举 {'draft', 'published', 'superseded', 'quarantined'} |
| L1-24 | SKILL.md `normalize_version` 必填，bump 规则文档化 |

### L2 Contract（6 条）

| # | 验收项 | 场景锚点 |
| --- | --- | --- |
| L2-44 | 相同 SKILL.md 内容两次 submit 产生相同 content_hash（yaml 顺序/空白归一） | S13 determinism |
| L2-45 | active version 切换不影响 draft；skill_installations ≠ auto-activated | S13 T8 |
| L2-46 | `user_skill_evaluations` 独立表：false_positive_rate 分母口径统一 | S13 §5 G23 |
| L2-47 | 跨 session skill search SQL 走 `(user_id, skill_name)` 索引 | S13 T11 |
| L2-48 | auto_activate_skills_on_topic_match 开关生效 | S11 T7 |
| L2-49 | normalize_version=NULL 等价 raw_v1 恒等或 NOT NULL | G27 |

### L3 Scenario（1 条）

| # | 场景片段 | 期望 |
| --- | --- | --- |
| L3-16 | **S13 T1-T12 skill 迭代 7 版**：append-only 7 版本，切回 v2 走 structured，quarantined 状态 ready | skill lifecycle |

---

## Phase 6: Artifact + Retention + Preview Templates

**目标**: `session_tool_outputs` + `session_artifacts` retention/ACL + preview_template_registry + tool_runner_registry。

**涉及 Gap**: G8/G9/G17/G27 (artifact + preview + retention + normalization)

### L1 Schema（3 条）

| # | 验收项 |
| --- | --- |
| L1-25 | `session_artifacts` 含 retention_policy / retention_until / referenced_by_*_count |
| L1-26 | `preview_template_registry` + 18+ baseline 模板 |
| L1-27 | `tool_runner_registry` + `raw_ref_scheme_registry` |

### L2 Contract（8 条）

| # | 验收项 | 场景锚点 |
| --- | --- | --- |
| L2-50 | T-7 天 GC 守护扫描活跃 manifest + state_items 引用，非零延期或迁冷存 | S08 T12 / S12 T10 |
| L2-51 | preview_template 按 tool_name 注册；未注册 tool 走 400B fallback + 告警 | S08 §5 |
| L2-52 | content_hash 规范化：pg_dump/fetch_url/parse_pdf/SKILL.md 各基线规则生效 | G17 resolution |
| L2-53 | 大 preview (3GB pg_dump) 不进 prompt；artifact_ref + preview_text ≤1000 字符 | S08 T3 |
| L2-54 | project_long_term retention 延期 artifact 到项目生命周期结束 | S12 T10 |
| L2-55 | presigned URL 下载不经 API server 内存 | S08 T11 |
| L2-56 | expired artifact 在 context_manifest 渲染 "historical, raw no longer available" 占位 | G9 |
| L2-57 | turn_intent='benchmark_comparison' 时 tool_previews zone 扩 2500，从 recent_tail 借 | S12 T9 |

### L3 Scenario（2 条）

| # | 场景片段 | 期望 |
| --- | --- | --- |
| L3-17 | **S08 T3-T12 DBA 审计**：3GB pg_dump + 1000 文件扫描 + 800MB slowlog，全链 ≤50ms 查询 | artifact 不进 prompt |
| L3-18 | **S12 T3-T12 14 天综述**：200 URL + 50 PDF + 结构化 preview，intent-aware budget flex 生效 | preview + retention |

---

## 跨 Phase 联合验收（Phase 6 完成后）

### 端到端场景重放（必须全过）

| # | 场景 | 涉及 Phase |
| --- | --- | --- |
| E2E-1 | S01 Rust 60 轮重构全链 | 1+3+4+6 |
| E2E-2 | S04 断线续传 17 次 | 1+2 |
| E2E-3 | S07 48h 审批跨 2 次发版 | 1+3+4 |
| E2E-4 | S10 5 层 delegation + bubble_up | 1+3+4 |
| E2E-5 | S14 8k 小窗口 + 4 设备切换 | 2+3+6 |

这 5 个是最"硬"的 E2E 测试，任何一个跑不通都不能发 v1。

### 性能基准

| # | 基准 | 目标 |
| --- | --- | --- |
| PERF-1 | 打开 session 的 hot path query | <50ms (p99) |
| PERF-2 | 三级检索链 | structured <50ms, FTS <200ms, vector <500ms |
| PERF-3 | 1000 行 tool_outputs batch insert | <500ms |
| PERF-4 | compaction 8 条 SQL assertion | <100ms 全跑 |
| PERF-5 | manifest 构建 | <100ms |

---

## Web UI MVP 范围（本次实现）

按用户指示，web 只做最简：

### 必须有
- [ ] 登录 + 权限（auth_sessions 已有）
- [ ] Session 列表（按更新时间排序）
- [ ] 对话框（新开 session / 继续 session）
- [ ] 基本 SSE 流式回复
- [ ] cold-start hydration（G24）
- [ ] 设备 lease 显式 revoke 按钮（G25 被动过期由 SDK 处理）

### 暂不做
- ❌ Context 侧边栏（Phase 3 完成后再加）
- ❌ Plan/Todo 树可视化
- ❌ Artifacts 画廊
- ❌ 个人 skill 编辑器
- ❌ Multi-agent delegation 可视化

## 阶段性验收流程（我的职责）

1. **设计 agent 完成 Phase N 实现** → 提交说"Phase N 做完"
2. **我启动一个 regression sub-agent** 按此文件 Phase N 章节验收
3. **sub-agent 跑 L1 schema check (SQL query on DB) + L2 code scan (grep for assertions) + L3 scenario test run**
4. **产出 IMPL-VERIFY-PHASE-N.md**，列出每条测试点状态
5. **任何 L1/L2 不过 → 退回设计 agent 修正**
6. **L3 不过 → 标记 warning，允许进下一 Phase 但必须在总联合验收补**

## Gap 到测试映射（反向查表）

| Gap | Phase | 测试点 |
| --- | --- | --- |
| G1 reason enum | 3 | L1-11, L1-14 |
| G2 compaction invariants | 4 | L2-32 to L2-35 |
| G3 retrieval SLA | 3 | L2-20 to L2-22 |
| G4 delegation | 4 | L1-18, L2-36, L2-37 |
| G5 plan tree | 4 | L3-12 |
| G6 scope='user' | 4 | L1-16, L2-39 |
| G7 approval conditions | 3 | (phase 1 外，单独 L2) |
| G8 preview template | 6 | L1-26, L2-51 |
| G9 artifact retention | 6 | L2-50, L2-54 |
| G10 budget_v1_8k | 3 | L2-25 |
| G11 workspace reachability | (暂未独立 phase，合 phase 2) | — |
| G12 confidence | 3 | L2-27 to L2-29 |
| G13 device fingerprint | 2 | L1-7, L2-15, L2-17 |
| G14 mutation enum | 4 | L1-17 |
| G15 run durability | 1 | L1-1, L2-2, L2-3 |
| G16 skill activation | 5 | L2-45, L2-46 |
| G17 content_hash | 6 | L2-44, L2-52 |
| G18 delegation budget | 3 | L2-23, L2-24, L2-26 |
| G19 watermark atomicity | 2 | L2-10 to L2-12 |
| G20 cross-tree ACL | 4 | L1-19, L1-20, L2-38, L3-13 |
| G21 fan-out formula | 3 | L2-23, L2-26 |
| G22 retry_scope | 1/4 | L1-3, L1-4, L2-7 |
| G23 batch insert | 1 | L1-6, L2-8, L2-9 |
| G24 cold-start | 2 | L2-14, L3-5 |
| G25 SSE parity | 2 | L1-8, L2-13, L3-6 |
| G26 reason extension | 3 | L1-11, L1-14 |
| G27 tool baseline | 6 | L1-27, L2-51, L2-52 |
| G28 mutation cancel | 4 (延期) | — |
| G29 checkpoint.extra | 1 (延期) | — |

## 终止条件

**只有当所有 Phase 1-6 的 L1 + L2 + E2E-1..5 全绿**，才能发布 v1。
**任何一个红灯都要追溯到哪个 gap 的实现偏离了设计**，然后修正。

这就是"花了这么多功夫做推演，不让它浪费"的契约。
