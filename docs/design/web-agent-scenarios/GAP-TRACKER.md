# Web Agent Session State — Gap Tracker

> Status: v0.3 (Sprint D: G20–G27 resolved; G28/G29 intentionally left open for implementation contract-test follow-up)
> Linked scenarios: [INDEX.md](INDEX.md)
> Linked design doc: [../web-agent-session-state.md](../web-agent-session-state.md)
> Purpose: 承载从 14 个压测场景 (S01–S14) 及回归验证反推出的**设计缺陷条目**。每条 gap 必须由设计负责人/设计 agent 给出明确响应（accept / reject / defer / need_clarify）并推进到 resolved。

## 使用约定

### 字段

| 字段 | 取值 |
| --- | --- |
| `gap_id` | G1–G19（稳定，不重用） |
| `title` | 一句话描述 |
| `triggered_by` | 触发该 gap 的场景编号列表（S01–S14） |
| `evidence_refs` | 场景文档里最能佐证该 gap 的章节/行号提示 |
| `design_section` | 当前设计文档里相关章节（可能为空 = 设计完全没覆盖） |
| `severity` | `critical` / `high` / `medium` / `low` |
| `category` | `schema` / `contract` / `policy` / `api` / `doc` |
| `proposed_fix` | 一句话提案（TOP5 建议里展开的具体动作） |
| `status` | `open` / `in_review` / `accepted` / `rejected` / `deferred` / `partially_resolved` / `resolved` |
| `response` | 设计负责人对该 gap 的书面响应（必填，accept/reject/defer 都需写理由） |
| `resolution_commit` | 若 resolved，对应的 commit 或 section revision |

### 工作流

```text
open → in_review → { accepted → resolved ; rejected ; deferred }
                              ↑ 回归场景验证（rerun scenarios 第 8 章对照）
```

### 响应要求

对每个 gap，设计 agent 必须：
1. **读**触发场景对应章节（至少 §2 DAUT + §8 反馈）
2. **判断**：当前设计是否已覆盖（给出设计文档具体行号证据）？
3. **决策**：accept / reject / defer / need_clarify
4. **补**：若 accept，列出要改的 schema / 契约 / 章节，关联到 v0.2 patch

---

## Gap 列表

### G1. `context_manifests.reason` 未枚举化

- **triggered_by**: S01, S06
- **evidence_refs**: S01 §8 建议 2；S06 §8 建议 4
- **design_section**: §Proposed Schema §3 Context Manifests
- **severity**: high
- **category**: schema + contract
- **proposed_fix**: 把 `context_manifests.reason` 升级为 Rust enum + schema enum，含 `initial_turn` / `post_compaction` / `history_recall_{structured|fts|vector}` / `large_tool_output_gated` / `plan_subtree_query` / `tree_structured_report` / `workspace_switch` / `approval_resume` / `cross_session_recall`。提供 lookup table 供 SQL 聚合。
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Manifest reason is an observability contract. Free-form strings make compaction, retrieval, plan subtree, approval, and delegation turns impossible to aggregate reliably.",
  "doc_sections": [
    "web-agent-session-state.md#Context Manifests",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added context_manifest_reason_types lookup table.",
    "Specified Rust-side validation when MatrixOne cannot enforce the logical FK.",
    "Seeded reasons for lifecycle, compaction, retrieval, tool gating, plan tree, workspace, approval, cross-session recall, delegation, and final delivery."
  ],
  "assigned_phase": "Phase 3",
  "cross_gap_dependency": {
    "consumes": ["G2 reason=post_compaction", "G5 plan_subtree_query/tree_structured_report", "G7 approval_resume", "G18 delegation reasons"],
    "does_not_resolve": ["G10 tokenizer_id/budget template"]
  },
  "test_plan": ["DB seed/validation test for allowed reasons", "Context manifest write rejects unknown reason"]
}
```
- **resolution_commit**: §Context Manifest Reason Enum @v0.2

### G2. Compaction Invariants 未契约化

- **triggered_by**: S01, S04, S06, S12
- **evidence_refs**: S01 §8 建议 1；S04 §8 Q5 + 8.1.5；S06 §8 建议 3；S12 §8 建议 2
- **design_section**: §Context Assembly / §Compaction
- **severity**: critical
- **category**: contract + policy
- **proposed_fix**: 新增一节 "Compaction Invariants"，8 条白名单规则 + 每条配 SQL assertion：
  - `category IN ('plan_state','decision','todo_state(active)','error_state(active)','delegation_state(active)','finding','benchmark','citation')` 绝不物理压缩或 replace
  - `plan_state.version` 在 compaction 轮次禁止 bump
  - compaction trigger 必须检查 `agent_runs.status NOT IN ('running','waiting')`
  - Done subtask 可 archived 但保留 provenance
  - manifest 必写（reason=`post_compaction`）
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Compaction cannot be only a summary-writing optimization. It must be a contract that protects active structured state and keeps the raw audit chain reconstructable.",
  "doc_sections": [
    "web-agent-session-state.md#Compaction Invariants",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added 8 compaction invariants.",
    "Added SQL assertions for active-state protection, plan version stability, active-run guard, todo provenance, manifest existence, durable fact provenance, and summary non-deletion.",
    "Added durable fact categories finding, benchmark, and citation.",
    "Added previous_version and next_version to session_state_item_events so plan version assertions are queryable without JSON filtering."
  ],
  "assigned_phase": "Phase 4"
}
```
- **resolution_commit**: §Compaction Invariants @v0.2 (lines 1543–1645, plus chunk_type sync L1276–1279 and NULL-run assertion fix L1614–1619); residuals resolved by Sprint B G8 + v0.2 internal edits
- **residual_notes** (from regression REGRESSION-SPRINT-A.md §G2, all closed per REGRESSION-SPRINT-B.md):
  - ~~r1 (`finding/benchmark/citation` payload schema)~~ → resolved via G8 (§Preview Template Registry @v0.2)
  - ~~r2 (`session_history_chunks.chunk_type` 未同步)~~ → resolved in v0.2 internal edit (chunk_type 列表现含三类)
  - ~~r3 (`session_state_item_events.mutation` 枚举未显式列出)~~ → resolved via G14 (§Delegation Retry and Bubble-Up Contract @v0.2)
  - ~~r4 (manifest `run_id` NULL 场景)~~ → resolved in v0.2 assertion fix
  - ~~r5 (compaction summary authority ambiguity)~~ → resolved in v0.2 by defining `session_state_items(category='summary')` as context-assembly authority and transcript summaries as replay/display aids

### G3. 三级检索 SLA + 降级 state machine 缺

- **triggered_by**: S01, S02
- **evidence_refs**: S02 §8 建议 1；S01 §8 建议 3
- **design_section**: §Cheap Context Construction / §History Chunks and Retrieval Index
- **severity**: high
- **category**: contract + policy
- **proposed_fix**: 新增 §"Retrieval State Machine"。规定每级 SLA：structured < 50ms / FTS < 200ms / vector < 500ms，全链 < 1s。每级超时或空结果必须降级写 `agent_events` 事件 `retrieval.<stage>_<reason>` (例 `retrieval.vector_stale`, `retrieval.fts_timeout`)。定义每轮 retrieval 预算（最多 1 次 vector 查询）。
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Old-detail lookup must be bounded and explainable. Structured, FTS, vector, and raw-load stages need explicit SLA, caps, and degradation events to avoid silent token/cost blowups.",
  "doc_sections": [
    "web-agent-session-state.md#Retrieval State Machine",
    "web-agent-session-state.md#Implementation Plan",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added staged retrieval SLA table.",
    "Defined hard caps and retrieval.<stage>_<reason> events.",
    "Specified one vector query per turn and stale vector hash skip/reindex behavior.",
    "Defined retrieval event payload contract."
  ],
  "assigned_phase": "Phase 3",
  "cross_gap_dependency": {
    "consumes": ["G1 history_recall_* reasons", "G6 cross-session user index"],
    "feeds": ["G9 raw_ref retention behavior"],
    "does_not_resolve": ["G17 content hash normalization contract"]
  },
  "test_plan": ["Retrieval state machine unit tests for timeout/empty/stale paths", "DB integration test asserts degradation agent_events are written"]
}
```
- **resolution_commit**: §Retrieval State Machine @v0.2

### G4. `delegation_state.payload_json` 未 spec

- **triggered_by**: S06, S09, S10
- **evidence_refs**: S06 §8 建议 2；S09 §8 建议 1 + 建议 1；S10 §8 建议 1
- **design_section**: §Proposed Schema §2 Session State Projection + §Open Questions "Multi-agent web UX"
- **severity**: critical（阻塞 multi-agent 产品化）
- **category**: schema + contract
- **proposed_fix**:
  1. Open Question 明确答案：**both** — `agent_runs.parent_run_id` 始终持有父子 run 关系；`agent_sessions` 仅在子 agent 需要独立 identity 时新建
  2. `delegation_state` payload 升级为 schema JSON（建议独立表 `session_delegations`），至少含 `{child_session_id, child_run_id, delegation_id, depth, root_run_id, ancestor_path, status, phase, last_summary_ref, last_summary_token_estimate, exposed_artifacts[], blocker, deps[], directive, spawned_at, last_child_turn_idx}`
  3. `last_summary_ref` 指向子 session 的 `session_state_items(category='summary')`
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "The design should not leave child-agent identity open. The execution tree and the product session identity are separate concerns.",
  "doc_sections": [
    "web-agent-session-state.md#Session State Projection",
    "web-agent-session-state.md#Open Questions",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Resolved the open question as both: parent/child run edges are mandatory, child sessions are optional identities.",
    "Added session_delegations as the queryable projection table.",
    "Specified delegation_state payload fields and last_summary_ref semantics.",
    "Added delegation APIs for tree rendering and drill-down."
  ],
  "assigned_phase": "Phase 4"
}
```
- **resolution_commit**: §Delegation Contract @v0.2 (lines 956–1062, plus agent_runs.depth L744 and Projection Sync Contract L1046–1062); residuals resolved by Sprint B G18/G9
- **residual_notes** (from regression REGRESSION-SPRINT-A.md §G4, all resolved per REGRESSION-SPRINT-B.md):
  - ~~r1 (delegation context zone 动态预算公式)~~ → resolved via G18 (§Delegation State Budget @v0.2)
  - ~~r2 (state item ↔ `session_delegations` UPSERT 同步协议)~~ → resolved in v0.2 Sprint A (§Projection Sync Contract)
  - ~~r3 (agent_runs 缺 `depth` 列)~~ → resolved in v0.2 Sprint A
  - ~~r4 (Artifact Access Scope / cross-session artifact ACL)~~ → resolved via G9 (§Artifact Access Scope @v0.2)
  - ~~r5 (`superseded` 状态 / `retry_scope`)~~ → resolved via G14 (§Delegation Retry and Bubble-Up Contract @v0.2)

### G5. Plan Tree Rendering Policy 缺（4 层以上）

- **triggered_by**: S06
- **evidence_refs**: S06 §8 建议 1
- **design_section**: §Proposed Schema §6 Plans and Todos
- **severity**: high
- **category**: policy
- **proposed_fix**: 新增 §"Plan Tree Rendering Policy"。规定：
  - 祖先链必渲染（full title + status）
  - 当前 section 下非 archived subtask 渲染（title + 1 行 summary）
  - 兄弟 section 仅渲染 title
  - 跨子树查询作为 explicit API (`GET /sessions/{id}/plan/subtree?root=...`) 而非 prompt 副作用
  - 配 2 段 SQL（祖先链查询 / 子树 pending 查询）
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Deep plan trees need deterministic structural rendering. A flat active-todo list cannot support ancestor path, sibling compression, subtree inspection, or tree reports.",
  "doc_sections": [
    "web-agent-session-state.md#Plans and Todos",
    "web-agent-session-state.md#Plan Tree Rendering Policy",
    "web-agent-session-state.md#API Additions",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added depth/path/summary/provenance fields and parent/status index to session_todos.",
    "Added rendering rules for ancestor chain, current-section subtasks, sibling sections, and explicit subtree API.",
    "Added ancestor-chain and pending-subtree SQL examples."
  ],
  "assigned_phase": "Phase 4",
  "cross_gap_dependency": {
    "consumes": ["G1 plan_subtree_query/tree_structured_report reasons"],
    "related": ["G6 backlog_pool_id for post-MVP todos"],
    "does_not_resolve": ["G14 delegation tree drill-down bubble_up semantics"]
  },
  "test_plan": ["Plan tree renderer unit tests for 4+ depth", "DB query tests for ancestor/subtree index usage"]
}
```
- **resolution_commit**: §Plan Tree Rendering Policy @v0.2

### G6. 跨 session scope（`session_state_items.scope='user'`）缺

- **triggered_by**: S05, S11
- **evidence_refs**: S05 §8 建议 1；S11 §8 建议 1
- **design_section**: §Proposed Schema §2 Session State Projection + §State Layers L3
- **severity**: high
- **category**: schema
- **proposed_fix**:
  - `session_state_items` 的 `scope` 字段扩枚举为 `'session' / 'user' / 'project' / 'workspace'`
  - `scope='user'` 的 category 白名单：`durable_decision` / `engineering_rule` / `rejected_pattern`
  - 载荷含 `origin_session_id` + `origin_chunk_id` provenance
  - ContextAssembler 初始化任何新 session 时按 `(user_id, scope='user', status='active')` 加载进 anchor zone，预算 ≤ 400 token
  - 同时为 S05 的 backlog 新增 `session_todos.backlog_pool_id` 与 `status='backlog'` 第三态
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Cross-session memory is a first-class scope, not an accidental history search. It needs category whitelist, provenance columns, and user-leading indexes.",
  "doc_sections": [
    "web-agent-session-state.md#Session State Projection",
    "web-agent-session-state.md#History Chunks and Retrieval Index",
    "web-agent-session-state.md#Cross-Session Scope and User Memory",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added user/project/workspace scope contract.",
    "Added user-scope categories durable_decision, engineering_rule, rejected_pattern.",
    "Added origin_session_id/origin_chunk_id/origin_state_item_id columns and user-scope index.",
    "Added idx_history_user_type_created for cross-session structured retrieval.",
    "Added backlog_pool_id and status='backlog' semantics for reusable todos."
  ],
  "assigned_phase": "Phase 4",
  "cross_gap_dependency": {
    "consumes": ["G3 retrieval state machine"],
    "related": ["G16 per-session active_skill semantics"],
    "does_not_resolve": ["G13 device fingerprint/revision reconciliation"]
  },
  "test_plan": ["Cross-session retrieval authorization test requiring user_id", "New-session context assembly budget test for <=400 token user memory"]
}
```
- **resolution_commit**: §Cross-Session Scope and User Memory @v0.2

### G7. Approval conditions 子表 + external notification adapter 缺

- **triggered_by**: S07
- **evidence_refs**: S07 §8 全部
- **design_section**: §Interaction Semantics + §Proposed Schema §2
- **severity**: high（合规场景必须）
- **category**: schema + api
- **proposed_fix**:
  1. `approval_state.conditions[]` 从 payload_json 提升为独立表 `session_approval_conditions(approval_item_id, condition_type, condition_spec_json, check_trigger, added_by, added_at)`
  2. 新增 §"External Notification Adapter"：Lark/Slack 推送协议、idempotency key、delivery receipt；对应 `agent_run_events` 事件 `notification_dispatched` / `notification_acknowledged`
  3. `waiting_for_edge` 超时策略：默认 300s 超时转 `failed` 或 `waiting_for_user`（per-run configurable）
  4. 审批状态机: `pending_approvers → approved → pending_requester_confirm → running → completed`
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Approval is compliance state. Conditions, external notifications, requester confirmation, expiry, and edge timeout must be queryable and replayable without relying on transcript interpretation.",
  "doc_sections": [
    "web-agent-session-state.md#Session State Projection",
    "web-agent-session-state.md#Interaction Semantics",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added session_approval_conditions table.",
    "Added session_external_notifications table.",
    "Specified approval_state payload contract and state machine.",
    "Added notification_dispatched/notification_acknowledged, requester_confirm, and edge_timeout events.",
    "Specified server-time expires_at_server and ttl_seconds display semantics."
  ],
  "assigned_phase": "Phase 4",
  "cross_gap_dependency": {
    "consumes": ["G15 durable run events and idempotency"],
    "related": ["G11 edge reachability and detach semantics"],
    "does_not_resolve": ["G12 next-action confidence thresholds"]
  },
  "test_plan": ["48h approval resume integration test", "Approval condition indexed query test", "Notification idempotency callback test"]
}
```
- **resolution_commit**: §Approval State and External Notification Adapter @v0.2

### G8. Preview Template Registry（按 tool_name 分级）缺

- **triggered_by**: S08, S12
- **evidence_refs**: S08 §8 建议 1；S12 §8 建议 1
- **design_section**: §Proposed Schema §4 Tool Result and Artifact References
- **severity**: high
- **category**: contract + api
- **proposed_fix**: 新增 §"Preview Template Registry"。每个 tool 声明 `preview_template.yaml`：`version` / `first_class_columns[]` / `fts_field_weights{}` / `max_preview_bytes`。Runtime 在渲染 preview 时按模板校验；无模板的工具走 400 字符兜底。设计文档附录给 pg_dump / slow_query_analyzer / fetch_url / parse_pdf 等 baseline 模板。
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Large and high-volume tool output cannot use one preview policy. Preview shape, first-class columns, FTS weights, and max size must be tool-versioned so context assembly can be cheap and accurate.",
  "doc_sections": [
    "web-agent-session-state.md#Tool Result and Artifact References",
    "web-agent-session-state.md#Preview Template Registry",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added preview_template_registry.",
    "Extended session_tool_outputs with preview template/version, normalize_version, preview status, preview token/byte caps, and common first-class columns.",
    "Added baseline templates for pg_dump, slow_query_analyzer, fetch_url, parse_pdf, llm_extract_findings, and benchmark_slice.",
    "Specified finding/benchmark/citation payload contracts."
  ],
  "assigned_phase": "Phase 6",
  "cross_gap_dependency": {
    "consumes": ["G2 compaction durable fact categories"],
    "feeds": ["G9 citation reference counters and retention"],
    "related_to_G14": "Preview extraction may create finding/benchmark/citation through normal insert/update state mutations in Sprint B. Cross-delegation promotion and user-visible apply_suggestion/bubble_up remain G14 and are not resolved here.",
    "does_not_resolve": ["G17 content hash normalization details beyond normalize_version hook"]
  },
  "test_plan": ["Preview template validation tests", "Tool output write rejects over-budget preview unless truncated", "FTS/structured query test uses first-class columns instead of JSON filters"]
}
```
- **resolution_commit**: §Preview Template Registry @v0.2

### G9. Artifact retention 与 manifest 引用双向感知缺

- **triggered_by**: S08, S12
- **evidence_refs**: S08 §8 建议 2；S12 §8 建议 3
- **design_section**: §Artifact and Tool Output Policy + §Lazy Indexing
- **severity**: high
- **category**: schema + policy
- **proposed_fix**:
  - `session_artifacts` 增列：`retention_policy ENUM('default','project_long_term','permanent')` / `retention_until TIMESTAMP` / `status ENUM('active','expiring','expired')` / `referenced_by_manifest_count INT` / `referenced_by_state_items_count INT` / `referenced_by_citation_count INT`
  - `agent_sessions` 增 `project_id` + `project_retention_policy`，同 `project_id` 下资源统一按项目策略
  - GC 前 T-7 天做"活跃 manifest 扫描"，仍被引用则延期或迁冷存
  - Context builder 对 `expired` artifact 降级渲染为 "historical, raw no longer available, summary preserved"
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Artifact GC cannot be independent from manifests, state items, citations, derived artifacts, and project retention policy. Otherwise old sessions render broken artifact refs even though summaries still point to them.",
  "doc_sections": [
    "web-agent-session-state.md#Artifact Retention and Access Scope",
    "web-agent-session-state.md#API Additions",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added additive session_artifacts retention/status/access-scope/reference-counter columns.",
    "Added agent_sessions project_id and project_retention_policy extensions.",
    "Specified T-7 GC preflight scan, cold-storage/tombstone behavior, and expired artifact context degradation.",
    "Specified private/delegation/user artifact access scopes and presigned URL download policy."
  ],
  "assigned_phase": "Phase 6",
  "cross_gap_dependency": {
    "consumes": ["G8 citation payload schema and preview references", "G4/G18 delegation tree access paths"],
    "related_to_G14": "G14 bubble_up may later increase referenced_by_state_items_count when a child exposes an artifact upward. G9 defines counters and retention but does not define bubble_up mutation semantics.",
    "does_not_resolve": ["G17 content hash normalization", "G13 device trust/revocation"]
  },
  "test_plan": ["Artifact GC preflight DB assertion test", "Expired artifact context degradation test", "Delegation access-scope authorization test"]
}
```
- **resolution_commit**: §Artifact Retention and Access Scope @v0.2

### G10. 小窗口 Budget Template（≤16k）缺

- **triggered_by**: S14
- **evidence_refs**: S14 §8.1.1 + §附录 A
- **design_section**: §Context Assembly / §Budget Policy v1
- **severity**: medium（私有化部署必需）
- **category**: policy + doc
- **proposed_fix**: 在 §Budget Policy v1 加一节。`budget_v1_8k` 模板：anchor 200 / plan_todo 400 / recent_tail 2000（可临时扩 2800）/ summary 500 / retrieved 1000 / tool_previews 500 / system+tool_schemas 3400 / 预留 500 output + 200 safety。明确：小窗口下 tool_schemas 与 retrieved 必须重度压缩，tail 必须保底。manifest 必须记录 `tokenizer_id`（不同模型 token 估算差 15%）。
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Small-window deployments cannot use a linearly scaled large-window budget. The policy needs a fixed profile with tokenizer tracking so token estimates remain explainable.",
  "doc_sections": [
    "web-agent-session-state.md#Budget Policy v1",
    "web-agent-session-state.md#Small-Window Budget Template",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added budget_v1_8k with explicit zone caps.",
    "Added tokenizer_id and budget_template_id to context_manifests.",
    "Specified recent_tail floor, aggressive tool/retrieved pruning, and default vector disablement for small windows."
  ],
  "assigned_phase": "Phase 3",
  "cross_gap_dependency": {
    "consumes": ["G3 retrieval state machine", "G18 delegation budget"],
    "does_not_resolve": ["G12 next-action thresholds"]
  },
  "test_plan": ["Budget allocator test for <=16k models", "Manifest persistence test for tokenizer_id and budget_template_id"]
}
```
- **resolution_commit**: §Small-Window Budget Template @v0.2

### G11. Workspace `reachability_probe` 字段缺

- **triggered_by**: S03, S14
- **evidence_refs**: S03 §8 建议 3；S14 §8.1.2
- **design_section**: §Cloud Workspace vs Edge Workspace
- **severity**: medium
- **category**: schema + policy
- **proposed_fix**:
  - `session_state_items(category='workspace_state').payload_json` 加 `reachability_probe { last_ok_at, last_fail_at, probe_method, rtt_ms }`
  - Cloud relay 在转发 edge tool call 前先 200ms HEAD 探活
  - 新增 §"Degradation Semantics" 子节：明确 `online / offline / reconnecting / detached` 四态与 authority 投影规则
  - 新增 `POST /edge/bridges/{id}/detach` API 与 re-attach identity 对账
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Workspace authority and edge reachability are different facts. The agent must not see edge-only tools when the edge bridge is unreachable or detached.",
  "doc_sections": [
    "web-agent-session-state.md#Cloud Workspace vs Edge Workspace",
    "web-agent-session-state.md#Workspace Reachability and Degradation Semantics",
    "web-agent-session-state.md#API Additions",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added reachability_probe payload shape.",
    "Defined online/offline/reconnecting/detached semantics.",
    "Specified 200ms cloud relay probe before edge tool forwarding.",
    "Added detach/reattach API contract."
  ],
  "assigned_phase": "Phase 4",
  "cross_gap_dependency": {
    "consumes": ["G13 device lease for reattach identity"],
    "feeds": ["G7 waiting_for_edge timeout behavior"]
  },
  "test_plan": ["Edge probe timeout removes edge tools from manifest", "Detach/reattach identity reconciliation test"]
}
```
- **resolution_commit**: §Workspace Reachability and Degradation Semantics @v0.2

### G12. Next-Action confidence 阈值未定义

- **triggered_by**: S05, S14
- **evidence_refs**: S05 §8 建议 3；S14 §8.1.3
- **design_section**: §Next-Action Extraction Cost
- **severity**: medium
- **category**: policy
- **proposed_fix**: 规定三档阈值：
  - `≥0.8` 自动执行，回答里标注依据
  - `0.5–0.8` 列候选 ask_user
  - `<0.5` 直接「没看懂」
  - 同 session 1h 内连续 3 次 ask_user 降一档阈值
  - 歧义 state machine：一轮 turn 最多产出 N 条 suggestion，不同 source 可并存但不可互相覆盖
  - `suggested_next_action.expires_at` 默认策略（per-category：approval 24h / todo 7d / hint 1h）
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Continue semantics must be deterministic and bounded. Confidence thresholds prevent low-confidence small-model guesses from turning into unintended tool execution.",
  "doc_sections": [
    "web-agent-session-state.md#Next-Action Extraction Cost",
    "web-agent-session-state.md#Next-Action Confidence State Machine",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added confidence thresholds >=0.8, 0.5-0.8, and <0.5.",
    "Specified multi-suggestion coexistence and max 5 suggestions per turn.",
    "Added expires_at defaults for approval/todo/hint.",
    "Added ask-user fatigue policy."
  ],
  "assigned_phase": "Phase 3",
  "cross_gap_dependency": {
    "consumes": ["G14 apply_suggestion mutation"],
    "related": ["G7 requester confirmation and pending approval resumes"]
  },
  "test_plan": ["NextActionExtractor threshold tests", "Ambiguous continue turn creates ask_user and preserves accepted structured suggestion"]
}
```
- **resolution_commit**: §Next-Action Confidence State Machine @v0.2

### G13. `state_revision` 不纳 device fingerprint

- **triggered_by**: S03, S14
- **evidence_refs**: S03 §8 建议 1 + 建议 2；S14 §8.1.4
- **design_section**: §Web Client Cache + §API Additions
- **severity**: medium（安全）
- **category**: schema + contract
- **proposed_fix**:
  - `state_revision` 等效 hash 纳入 `device_fingerprint`
  - 服务端 if-none-match 比 `monotonic_id` 而非 hash 整体，保证单调性的同时让回退可检测
  - 新增 §"Revision Reconciliation" 子节，定义三类路径：delta / gap 全量重置 / CAS 冲突 409
  - 引入 `session_lease(device_id, expires_at)` + `POST /sessions/{id}/device/revoke` + `trust_level` 三态
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Device-aware revision reconciliation is required for cross-device restore and stale-cache safety. The server should compare monotonic ids while using hashes to detect rollback/corruption.",
  "doc_sections": [
    "web-agent-session-state.md#Web Client Cache",
    "web-agent-session-state.md#Revision Reconciliation and Device Lease",
    "web-agent-session-state.md#API Additions",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Changed state_revision to monotonic_id + revision_hash + device_fingerprint.",
    "Added session_device_leases table.",
    "Defined delta, gap full reset, and CAS 409 paths.",
    "Added trusted/new_device/unknown_device trust levels and revoke/list APIs."
  ],
  "assigned_phase": "Phase 2",
  "cross_gap_dependency": {
    "feeds": ["G19 watermark atomicity", "G11 edge reattach identity"],
    "closes": ["G15-r5 auth_sessions/device lease residual"]
  },
  "test_plan": ["Cross-device stale cache reset test", "CAS conflict returns 409 without mutation", "Device revoke blocks future session reads/writes"]
}
```
- **resolution_commit**: §Revision Reconciliation and Device Lease @v0.2

### G14. `bubble_up` 一等事件 + `retry_scope` 字段缺

- **triggered_by**: S10
- **evidence_refs**: S10 §8 建议 2 + 建议 3
- **design_section**: §Proposed Schema §1 Durable Runs + §2 Session State Projection
- **severity**: medium
- **category**: schema + contract
- **proposed_fix**:
  - `agent_runs` 增 `retry_scope ENUM('node','subtree','siblings')`；原 run 状态转 `'superseded'`（非物理删除）
  - `session_state_item_events.mutation` 枚举扩至 `{insert, update, replace, archive, bubble_up, apply_suggestion}`
  - `bubble_up` payload 强制含 `{bubble_seq, severity, source_run_id, original_item_id, bubble_target_scope}`
  - `apply_suggestion` payload 关联 `suggested_next_action.id`
  - 新增 §"Delegation Tree Drill-Down"（第 4 种 History Reconstruction Mode）
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Deep delegation needs retry scope, superseded audit branches, and bubble-up/apply-suggestion mutations as first-class contracts. Otherwise parent sessions cannot safely inspect or retry child subtrees.",
  "doc_sections": [
    "web-agent-session-state.md#Durable Runs",
    "web-agent-session-state.md#Session State Projection",
    "web-agent-session-state.md#History Reconstruction Modes",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added agent_runs.retry_scope.",
    "Defined status='superseded' audit behavior.",
    "Enumerated session_state_item_events.mutation values.",
    "Specified bubble_up and apply_suggestion payload contracts.",
    "Added delegation tree drill-down reconstruction mode."
  ],
  "assigned_phase": "Phase 4",
  "cross_gap_dependency": {
    "closes": ["G2-r3 mutation enum residual", "G4-r5 retry_scope/superseded residual"],
    "consumes": ["G12 suggested_next_action confidence state machine", "G8/G9 fact/artifact counters"]
  },
  "test_plan": ["Delegation retry_scope contract tests", "bubble_up state event propagation test", "superseded branch remains audit-readable"]
}
```
- **resolution_commit**: §Delegation Retry and Bubble-Up Contract @v0.2

### G15. `agent_run_events.event_idx` 生成协议缺（单 writer / run ownership）

- **triggered_by**: S04
- **evidence_refs**: S04 §8.1.1 + §8.2 Q1/Q5
- **design_section**: §Proposed Schema §1 Durable Runs + §Implementation Plan Phase 1
- **severity**: critical（durable run 正确性基石）
- **category**: contract + schema
- **proposed_fix**:
  - 新增 `run_counters` 表或明确「单 writer」约束（advisory lock / leader election）
  - Pod rolling 时 `agent_runs.owner_pod_id` + graceful handoff 协议，避免 run 分裂给两个 pod
  - Phase 1 Exit Criteria 区分 graceful shutdown（`checkpoint_json.graceful=true` → 恢复）vs crash（无 checkpoint → 标 failed，提示 retry）
  - POST `/chat/runs/{run_id}/input` 强制 `idempotency_key` + 服务端去重
  - 新事件类型 `approval_expired` / `approval_retracted` / `run_resumed_after_restart`
  - SSE heartbeat 协议（server 15s 心跳 / client 45s 判死）
  - `agent_runs.checkpoint_json` 定义 `checkpoint_v1` schema（`{graceful, last_batch_id, extra}`）
  - 多 tab 订阅：事件幂等 + watermark 同步，不需 primary tab
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Durable run replay is unsafe without a deterministic event index allocator and single-writer ownership. The API also needs idempotent input and explicit restart semantics before web runs can survive rolling deploys and multi-tab reconnects.",
  "doc_sections": [
    "web-agent-session-state.md#Durable Runs",
    "web-agent-session-state.md#Run Event Ordering and Ownership",
    "web-agent-session-state.md#Implementation Plan",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added run_counters and owner lease columns.",
    "Specified transaction-based event_idx allocation.",
    "Specified checkpoint_v1, graceful handoff, crash behavior, and run_resumed_after_restart.",
    "Required idempotency_key for POST /chat/runs/{run_id}/input.",
    "Specified approval_expired, approval_retracted, SSE heartbeat, and multi-tab watermark replay."
  ],
  "assigned_phase": "Phase 1",
  "cross_gap_dependency": {
    "closed_by": [
      "G7 approval expires_at_server/ttl_seconds clock-skew contract",
      "G13 auth_sessions/device lease reconciliation",
      "G19 IndexedDB watermark/event apply atomicity"
    ],
    "feeds": ["G14 superseded retry audit branches"]
  },
  "test_plan": [
    "Run event_idx allocator single-writer contract test",
    "Graceful restart vs crash restart integration test",
    "Idempotent run input test",
    "IndexedDB watermark atomic apply UI cache test from G19"
  ]
}
```
- **resolution_commit**: §Run Event Ordering and Ownership @v0.2 + §Approval State and External Notification Adapter @v0.2 + §Revision Reconciliation and Device Lease @v0.2 + §Web Event Watermark Atomicity @v0.2
- **residual_notes** (from regression REGRESSION-SPRINT-A.md §G15, all closed in Sprint C):
  - ~~r1 (approval clock-skew / `expires_at_server` / `ttl_seconds` 协议)~~ → resolved via G7 (§Approval State and External Notification Adapter @v0.2)
  - ~~r2 (IndexedDB watermark + event apply 原子性)~~ → resolved via G19 (§Web Event Watermark Atomicity @v0.2)
  - ~~r3 (`run_counters.generation` 字段名与 `agent_runs.run_generation` 不一致)~~ → resolved in v0.2 internal edit（列名统一为 `run_generation`）
  - ~~r4 (Phase 1 Exit criteria 未在文档显式区分 graceful vs crash 的集成测试)~~ → resolved in Phase 1 exit criteria and G15 test_plan
  - ~~r5 (`auth_sessions` 多设备 lease 与 G13 device_fingerprint 关联未建)~~ → resolved via G13 (§Revision Reconciliation and Device Lease @v0.2)

### G16. Personal skill per-session activation 语义缺

- **triggered_by**: S11, S13
- **evidence_refs**: S11 §8 建议 3；S13 §8 建议 1 + 建议 2
- **design_section**: §Personal Skills (§9)
- **severity**: medium
- **category**: contract + schema
- **proposed_fix**:
  - `skill_installations` 表示「user 在 workspace 可用」，**不等于**自动加载进 prompt
  - per-session 激活走 `session_state_items(scope='session', category='active_skill', item_key=<skill_name>, payload_json.version_id=<frozen>)`
  - `version_id` 固定快照，不跟 registry `active_version` 自动切换（防 race）
  - `skill_installations` 加 `scope ENUM('user','session','workspace')` 字段
  - 新表 `user_skill_evaluations(source_id, version_id, run_id, hits, suspects, false_positives, payload_json, created_at)` 独立承载试用反馈（不塞 session_state_items）
  - user-level preference `auto_activate_skills_on_topic_match=true/false`
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Installed skills and active prompt skills are separate concepts. Session activation must freeze a concrete version and skill evaluations need queryable columns for quality gates.",
  "doc_sections": [
    "web-agent-session-state.md#Personal Skills",
    "web-agent-session-state.md#Personal Skill Activation and Evaluation",
    "web-agent-session-state.md#API Additions",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added skill_installations.scope/session_id/workspace_id/auto_activate_on_topic_match extensions.",
    "Added user_skill_evaluations table.",
    "Specified active_skill session-state payload with frozen version_id.",
    "Defined skill zone rendering rules and quarantined version behavior."
  ],
  "assigned_phase": "Phase 5",
  "cross_gap_dependency": {
    "consumes": ["G6 active_skill category and session scope"],
    "feeds": ["G17 skill content hash normalization"]
  },
  "test_plan": ["Per-session skill activation freezes version_id", "Installed skill not loaded without activation", "Skill evaluation quality gate query test"]
}
```
- **resolution_commit**: §Personal Skill Activation and Evaluation @v0.2

### G17. `content_hash` 规范化规则缺

- **triggered_by**: S08, S13
- **evidence_refs**: S08 §8 建议 3；S13 §8 建议 3
- **design_section**: §Proposed Schema §4 + §9 Personal Skills
- **severity**: medium
- **category**: contract
- **proposed_fix**: 新增 §"Content Hash Normalization Contract"：
  - 每个 tool / skill source 声明 `normalize_version VARCHAR(16)`
  - 规范化函数剔除时间戳、版本字符串、绝对路径、随机标识、yaml key 顺序、空白
  - 同 schema 变更规范化规则时 `normalize_version` 必须 bump
  - pg_dump / slow_query_analyzer / SKILL.md yaml 各给基线规则（设计文档附录）
  - 配合 G9 的 artifact dedup 才真正能复用
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Hash stability is a reuse and audit contract. Tools and skills need explicit normalization versions, otherwise deduplication, stale-vector checks, and skill version history drift.",
  "doc_sections": [
    "web-agent-session-state.md#Tool Result and Artifact References",
    "web-agent-session-state.md#Personal Skills",
    "web-agent-session-state.md#Content Hash Normalization Contract",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added normalize_version to session_artifacts and user_skill_versions.",
    "Specified content hash input as sha256(normalize_version(raw_bytes)).",
    "Added SKILL.md canonical input rule.",
    "Added baseline rules for pg_dump, slow_query_analyzer, fetch_url, parse_pdf, and SKILL.md."
  ],
  "assigned_phase": "Phase 6",
  "cross_gap_dependency": {
    "consumes": ["G8 normalize_version hook", "G9 artifact dedup/retention"],
    "feeds": ["G3 vector stale detection", "G16 skill version audit"]
  },
  "test_plan": ["Normalization golden tests for baseline tool outputs", "SKILL.md key-order/whitespace hash stability test", "normalize_version bump invalidates dedup"]
}
```
- **resolution_commit**: §Content Hash Normalization Contract @v0.2

### G18. Delegation context zone 动态预算公式缺

- **triggered_by**: S09
- **evidence_refs**: S09 §8 建议 3；REGRESSION-SPRINT-A.md §G4 residual r1
- **design_section**: §Cheap Context Construction / §Budget Policy v1
- **severity**: high
- **category**: policy
- **proposed_fix**:
  - 在 zone 表正式加入 `delegation_state` zone
  - 公式：`per_child_budget = min(1500 / active_children, floor=200)`，blocker 发生时该子临时扩 2× 从 `recent_tail` 借
  - 单 child summary 超限 fallback 到 `title + artifact_ref`
  - 用 `last_summary_token_estimate` 做 assembler 预算预校验
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Delegation summaries need their own budget formula. Otherwise multi-agent parent prompts either omit blockers or expand child context until they evict recent tail and plan state.",
  "doc_sections": [
    "web-agent-session-state.md#Budget Policy v1",
    "web-agent-session-state.md#Delegation State Budget",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Added delegation_state as a first-class budget zone.",
    "Specified total cap 1500 and corrected floor formula per_child_budget = min(1200, max(200, floor(1500 / active_children))).",
    "Specified blocker 2x expansion by borrowing from recent_tail.",
    "Specified last_summary_token_estimate pre-check and title+artifact_ref fallback."
  ],
  "assigned_phase": "Phase 3",
  "cross_gap_dependency": {
    "consumes": ["G4 session_delegations.last_summary_token_estimate", "G1 delegation manifest reasons"],
    "feeds": ["G9 delegation artifact access scope"],
    "does_not_resolve": ["G14 delegation retry/bubble_up semantics"]
  },
  "test_plan": ["Budget allocator tests for 1/3/5 child agents", "Blocked-child budget borrowing manifest test"]
}
```
- **resolution_commit**: §Delegation State Budget @v0.2

### G19. Web 客户端 watermark + event apply 原子性契约缺

- **triggered_by**: S04
- **evidence_refs**: S04 §8 建议 3；REGRESSION-SPRINT-A.md §G15 residual r2
- **design_section**: §Web Client Cache + Phase 2 Exit criteria
- **severity**: medium
- **category**: contract
- **proposed_fix**:
  - IndexedDB 中 `run_event_high_watermark` 与 events 行必须同一 transaction 写入
  - 规定 client gap 检测：若连续 event_idx 跳号 > 1 则 reset + re-replay from last_ok_idx
  - 多 tab 共享 watermark via BroadcastChannel 或 SharedWorker，防止两 tab 各自推进
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Server-side run ordering is insufficient if the browser advances its watermark before local event rows are durable. G19 closes the client half of G15 replay correctness.",
  "doc_sections": [
    "web-agent-session-state.md#Web Client Cache",
    "web-agent-session-state.md#Web Event Watermark Atomicity",
    "web-agent-session-state.md#Changelog v0.2"
  ],
  "patch_summary": [
    "Specified IndexedDB transaction that writes run_events and run_event_high_watermark atomically.",
    "Added gap detection and replay from last_ok_idx.",
    "Specified BroadcastChannel/SharedWorker multi-tab coordination with no primary-tab lock."
  ],
  "assigned_phase": "Phase 2",
  "cross_gap_dependency": {
    "closes": ["G15-r2 IndexedDB watermark/event apply residual"],
    "consumes": ["G13 revision/device reconciliation", "G15 event_idx ordering"]
  },
  "test_plan": ["IndexedDB atomic event apply test", "Gap replay test from last_ok_idx", "Multi-tab BroadcastChannel stale watermark test"]
}
```
- **resolution_commit**: §Web Event Watermark Atomicity @v0.2

---

## 汇总视图（G1–G19，按严重度）

| 严重度 | 数量 | Gap ID |
| --- | --- | --- |
| critical | 3 | G2 ✅ resolved, G4 ✅ resolved, G15 ✅ resolved |
| high | 8 | G1 ✅, G3 ✅, G5 ✅, G6 ✅, G7 ✅, G8 ✅, G9 ✅, G18 ✅ (all resolved) |
| medium | 8 | G10 ✅, G11 ✅, G12 ✅, G13 ✅, G14 ✅, G16 ✅, G17 ✅, G19 ✅ (all resolved) |

**G1–G19 进度**: **19 resolved / 0 partially / 0 open**（共 19 条 gap）

## 汇总视图（G1–G19，按类别）

| 类别 | 数量 | Gap ID |
| --- | --- | --- |
| schema | 10 | G1, G4, G6, G7, G9, G11, G13, G14, G15, G16 |
| contract | 12 | G1, G2, G3, G4, G8, G12, G13, G14, G15, G16, G17, G19 |
| policy | 7 | G2, G3, G5, G9, G10, G12, G18 |
| api | 2 | G7, G8 |
| doc | 1 | G10 |

（一个 gap 可能跨多类别，故合计 > 19）

## 回归验证协议

当设计 agent 完成一轮 patch 后，按以下协议验证 gap 是否真 resolved：

1. 对每个标 `accepted` 的 gap，找到其 `triggered_by` 场景
2. 重读该场景的 §2 DAUT 与 §8 反馈
3. 对照新设计文档的对应章节
4. 检查 proposed_fix 的每一条是否落地（schema 改了？契约写了？枚举全了？）
5. 若全部落地 → `status='resolved'`，回填 `resolution_commit`
6. 否则 → 退回 `in_review` 并在 `response` 字段补追问
7. 对 `resolved` / `partially_resolved` gap 建立实现期测试 TODO：至少映射到 contract test、DB assertion test、HTTP/SSE integration test、UI cache test、或 restart/chaos test 之一。实现完成前必须把测试文件或测试 issue 回填到该 gap 的 `test_plan` / `residual_notes`，防止设计文档和代码实现漂移。

这个过程可以人工做，也可以起一个 reviewer sub-agent 专门跑（每个 gap ~5 分钟）。

---

## 端到端推演阶段追加 Gap (G20–G29)

> 来源: 14 个 WALKTHROUGH-S<NN>.md 推演报告 + WALKTHROUGH-AGGREGATE.md 聚合
> 说明: Sprint A/B/C 结束后，从 gap review 转入端到端运行推演，发现 v0.2 "契约完备" 之下的"运行层摩擦"。
> 共 10 条新 gap：4 critical + 4 high + 2 medium

### G20. 跨兄弟子 session 的 artifact/state 访问权限未定义

- **triggered_by**: S09 (blocking), S10 (blocking)
- **evidence_refs**: WALKTHROUGH-S09.md §4 F5 + §5；WALKTHROUGH-S10.md §6 组合 4；WALKTHROUGH-AGGREGATE.md §Critical G20
- **design_section**: §Artifact Access Scope L1716–1720, §Delegation Contract L1321–1426
- **severity**: critical
- **category**: schema + contract
- **proposed_fix**: access_scope 从 parent/descendants 扩展为 `same_root_tree`；为兄弟场景引入 `sibling_exposed_artifacts[]` 白名单；或按 `ancestor_path LIKE '/root/%'` 前缀匹配；文档化"谁在 delegation 树内可读 what"的规则表
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "S09/S10 prove the v0.2 parent+descendant wording is too narrow: sibling child agents under the same root need explicit, auditable artifact access without falling back to ad hoc bypasses.",
  "doc_sections": [
    "web-agent-session-state.md#Artifact Retention and Access Scope",
    "web-agent-session-state.md#Delegation Tree Artifact ACL",
    "web-agent-session-state.md#API Additions",
    "web-agent-session-state.md#Changelog v0.3"
  ],
  "patch_summary": [
    "Deprecated ambiguous access_scope='delegation' and introduced delegation_direct plus same_root_tree semantics.",
    "Added owner_run_id/root_run_id indexes and session_artifact_grants.",
    "Added sibling_exposed_artifacts_json and an access matrix for owner, parent, descendants, siblings, root orchestrator, and different roots.",
    "Specified that raw download and context rendering use the same ACL check."
  ],
  "assigned_phase": "Phase 4",
  "cross_gap_dependency": {
    "consumes": ["G4 delegation tree projection", "G9 artifact access scope", "G14 bubble_up propagation"],
    "feeds": ["G27 raw_ref resolver access_check"],
    "does_not_resolve": ["G28 cancel mutation", "G29 checkpoint extra"]
  },
  "test_plan": ["Sibling artifact access integration test for S09/S10", "Different-root denial test", "Grant revocation audit test"]
}
```
- **resolution_commit**: §Delegation Tree Artifact ACL @v0.3 (L2155-L2229)

### G21. G18 delegation zone budget 公式在大 fan-out 时自相矛盾

- **triggered_by**: S10 (blocking)
- **evidence_refs**: WALKTHROUGH-S10.md §4 F-blocking-1；WALKTHROUGH-AGGREGATE.md §Critical G21
- **design_section**: §Delegation State Budget L2442–2471
- **severity**: critical
- **category**: policy
- **proposed_fix**: 公式改为 `max(200, floor(budget_total/n))`；当 n > 7 时按 priority/blocker 状态过滤到 top-K 活跃子；无法渲染的子降级为 "2 more blocked, click to expand"
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "The old formula is mathematically inconsistent for fan-out >= 8. The design must define top-K rendering and overflow behavior so the 1500-token delegation zone cap remains true.",
  "doc_sections": [
    "web-agent-session-state.md#Delegation State Budget",
    "web-agent-session-state.md#Changelog v0.3"
  ],
  "patch_summary": [
    "Deprecated the v0.2 formula.",
    "Defined candidate_children, rendered_children, overflow_children, and per_child_budget.",
    "Verified active child counts 1/3/5/7/8/10/15.",
    "Kept normal parent prompts under the 1500-token hard cap and moved overflow to explicit drill-down."
  ],
  "assigned_phase": "Phase 3",
  "cross_gap_dependency": {
    "consumes": ["G18 delegation_state zone", "G5/G14 delegation tree rendering and drill-down"],
    "feeds": ["G20 sibling artifact overflow refs", "G26 progressive_loading budget reporting"],
    "does_not_resolve": ["G29 batch sub-step checkpoint"]
  },
  "test_plan": ["Delegation budget property test for n=1,3,5,7,8,10,15", "Overflow children context_manifest_items test"]
}
```
- **resolution_commit**: §Delegation State Budget @v0.3 (L2968-L3011)

### G22. retry_scope 判定规则 + apply_suggestion payload 缺字段

- **triggered_by**: S10 (blocking)
- **evidence_refs**: WALKTHROUGH-S10.md §4 F-blocking-2；WALKTHROUGH-AGGREGATE.md §Critical G22
- **design_section**: §Delegation Mutation Events L1154–1215 (G14)
- **severity**: critical
- **category**: schema + contract
- **proposed_fix**: (1) G14 加 "retry_scope 选择规则"章节：user 显式 > agent 推断 > 默认 node；(2) `apply_suggestion` payload 补 `retry_scope VARCHAR(16) NULL`；(3) 所有 retry 事件必须携带 scope
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Retry scope is not a display hint; it determines which old branches are superseded and which child work is rerun. It must be selected and persisted before retry run creation.",
  "doc_sections": [
    "web-agent-session-state.md#Delegation Mutation Events",
    "web-agent-session-state.md#Retry Scope Selection and Propagation",
    "web-agent-session-state.md#Changelog v0.3"
  ],
  "patch_summary": [
    "Added retry_scope to apply_suggestion payload.",
    "Defined selection order: user explicit, suggestion payload, active child state, stateless node, group/siblings, default node.",
    "Required retry_scope on suggestion application, retry run creation, retry events, and audit links.",
    "Made unknown retry_scope a hard validation failure."
  ],
  "assigned_phase": "Phase 4",
  "cross_gap_dependency": {
    "consumes": ["G12 suggested_next_action confidence", "G14 retry/superseded audit branches"],
    "feeds": ["G21 fan-out overflow tests when retry creates many children"],
    "does_not_resolve": ["G28 cancel mutation"]
  },
  "test_plan": ["Retry scope inference tests for node/subtree/siblings", "apply_suggestion retry_scope persistence test", "Superseded branch audit test"]
}
```
- **resolution_commit**: §Retry Scope Selection and Propagation @v0.3 (L1346-L1397)

### G23. session_tool_outputs 批量写入契约缺（N+1 问题）

- **triggered_by**: S08 (blocking)
- **evidence_refs**: WALKTHROUGH-S08.md §4 F-blocking；WALKTHROUGH-AGGREGATE.md §Critical G23
- **design_section**: §Tool Result and Artifact References L1560–1610
- **severity**: critical
- **category**: policy + contract
- **proposed_fix**: 补一节 "Batch Insert Contract"：规定 batch boundary + txn size 上限 + tool runner 推荐 batch 协议（每 100-500 行一个 batch）；引入 `session_tool_output_batches` 表追踪 batch 完成状态；Phase 2 Exit 加 "1000-row insert < 500ms" 硬约束
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "S08's 1000-file scan is a primary workload. Without a batch contract, per-row writes cause multi-second latency and partial-write inconsistencies between aggregate previews and detail rows.",
  "doc_sections": [
    "web-agent-session-state.md#Tool Result and Artifact References",
    "web-agent-session-state.md#Tool Output Batch Insert Contract",
    "web-agent-session-state.md#Implementation Plan",
    "web-agent-session-state.md#Changelog v0.3"
  ],
  "patch_summary": [
    "Added batch_id/batch_seq/batch_row_idx columns and batch indexes to session_tool_outputs.",
    "Added session_tool_output_batches.",
    "Defined 100-500 row batch boundaries and 500 row/16MiB transaction caps.",
    "Specified rollback/failed batch behavior and the 1000-row <500ms contract target."
  ],
  "assigned_phase": "Phase 6",
  "cross_gap_dependency": {
    "consumes": ["G8 preview template validation", "G17 content hash normalization"],
    "feeds": ["G27 tool runner registration and baseline templates"],
    "does_not_resolve": ["G29 checkpoint_v1.extra"]
  },
  "test_plan": ["1000 session_tool_outputs bulk insert <500ms", "Partial batch rollback hides detail rows", "Context assembly ignores incomplete batches"]
}
```
- **resolution_commit**: §Tool Output Batch Insert Contract @v0.3 (L1858-L1932)

### G24. Cold-start hydration：state API 与 stream API watermark 契约未闭环

- **triggered_by**: S03, S04
- **evidence_refs**: WALKTHROUGH-S04.md §5 gap "Cold-Start"；WALKTHROUGH-S03.md §3 T5 iPad 冷启；WALKTHROUGH-AGGREGATE.md §High G24
- **design_section**: §Web Client Cache L373–436, §Durable Runs L1052–1057
- **severity**: high
- **category**: contract + api
- **proposed_fix**: 补一节 "Cold-Start Hydration"：client 首次打开（local watermark=0）必须先按 transcript API 拉历史或 stream?last_index=0 全量 replay；server 在 /sessions/state 响应加 `replay_required=true` 当检测到 `known_state_revision=0` 但 `active_run.run_event_high_watermark>0`
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "A server watermark is not evidence that a cold browser has local rows. Cold clients need explicit replay flags so they hydrate transcript and run events instead of skipping directly to the server high watermark.",
  "doc_sections": [
    "web-agent-session-state.md#Web Client Cache",
    "web-agent-session-state.md#Cold-Start Hydration",
    "web-agent-session-state.md#Implementation Plan",
    "web-agent-session-state.md#Changelog v0.3"
  ],
  "patch_summary": [
    "Added replay_required, transcript_replay_required, and run_event_replay_required fields to state responses.",
    "Defined cold-start request semantics for known_state_revision=0/client_cache_empty=true.",
    "Specified transcript paging and run stream replay from the beginning before local watermarks advance.",
    "Explicitly forbids cold clients from using server hwm as local last_index."
  ],
  "assigned_phase": "Phase 2",
  "cross_gap_dependency": {
    "consumes": ["G13 revision reconciliation", "G19 IndexedDB watermark atomicity", "G15 run event replay"],
    "feeds": ["G25 local cache clearing after lease end"],
    "does_not_resolve": ["G29 checkpoint extra"]
  },
  "test_plan": ["Cold device with empty IndexedDB replays transcript and run events", "Warm cache still uses delta path", "State API replay flags test"]
}
```
- **resolution_commit**: §Cold-Start Hydration @v0.3 (L441-L498)

### G25. Auto-expiry vs explicit revoke — SSE event parity 缺 (安全)

- **triggered_by**: S03
- **evidence_refs**: WALKTHROUGH-S03.md §5 候选 B；WALKTHROUGH-AGGREGATE.md §High G25
- **design_section**: §Revision Reconciliation and Device Lease L438–498 (G13)
- **severity**: high
- **category**: schema + contract
- **proposed_fix**: 要求 expires_at 被动过期必须 emit `device_lease_expired` SSE 事件（payload 与 `device_revoked` 对称）；client SDK 在任一事件触发本地 storage 清理；零信任设备（Chromebook 类）的场景集成测试覆盖
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Passive lease expiry has the same security impact as explicit revoke. Without a push event, zero-trust devices can keep sensitive local session state until the next failed API call.",
  "doc_sections": [
    "web-agent-session-state.md#Revision Reconciliation and Device Lease",
    "web-agent-session-state.md#Device Lease End Event Parity",
    "web-agent-session-state.md#Implementation Plan",
    "web-agent-session-state.md#Changelog v0.3"
  ],
  "patch_summary": [
    "Added session_device_lease_events.",
    "Defined active->revoked and active->expired terminal transitions.",
    "Required device_revoked and device_lease_expired SSE/WS events with symmetric payloads.",
    "Specified client SDK local storage clearing behavior for either event."
  ],
  "assigned_phase": "Phase 2",
  "cross_gap_dependency": {
    "consumes": ["G13 session_device_leases", "G24 cold-start cache semantics"],
    "feeds": ["G11 edge reattach identity checks"],
    "does_not_resolve": ["G28 cancel mutation"]
  },
  "test_plan": ["Passive lease expiry emits device_lease_expired", "Explicit revoke and expiry both clear local storage", "Duplicate expiry scanners emit one event"]
}
```
- **resolution_commit**: §Device Lease End Event Parity @v0.3 (L560-L630)

### G26. context_manifests.reason 枚举补强（多场景汇总）

- **triggered_by**: S05, S11, S12, S13, S14
- **evidence_refs**: WALKTHROUGH-S05.md §5 G22；WALKTHROUGH-S11.md §5 #1；WALKTHROUGH-S12.md §5 intent-aware；WALKTHROUGH-S13.md §5 G22；WALKTHROUGH-S14.md §5 #2 progressive_loading；WALKTHROUGH-AGGREGATE.md §High G26
- **design_section**: §Manifest Reason Enumeration L1504–1515 (G1)
- **severity**: high
- **category**: contract
- **proposed_fix**: G1 枚举表后补 5-7 个新值：`ambiguity_clarification`, `user_memory_promote/archive/revise`, `cross_skill_alignment`, `progressive_loading`, `intent_driven_preview_expand`；补 fallback 规则 "unknown reason → `other` + 告警"；引入 `turn_intent VARCHAR(64)` 字段支持 intent-aware budget profile（来自 S12）
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "Five walkthroughs found legitimate manifest turns that would otherwise become free-form reason strings. Reason remains the aggregation key; turn_intent handles budget mode without exploding reason values.",
  "doc_sections": [
    "web-agent-session-state.md#Context Manifests",
    "web-agent-session-state.md#Manifest Reason Enumeration",
    "web-agent-session-state.md#Budget Policy v1",
    "web-agent-session-state.md#Changelog v0.3"
  ],
  "patch_summary": [
    "Added turn_intent to context_manifests.",
    "Added ambiguity_clarification, execute_after_clarification, user_memory_* reasons, progressive_loading, intent_driven_preview_expand, and other.",
    "Kept cross_skill_alignment as the canonical skill comparison reason.",
    "Defined unknown reason fallback to other with manifest.reason_unknown alert.",
    "Added seeded turn_intent values and budget_override audit fields."
  ],
  "assigned_phase": "Phase 3",
  "cross_gap_dependency": {
    "consumes": ["G1 reason enum", "G10 small-window budget", "G12 next-action ambiguity", "G16 cross-skill state"],
    "feeds": ["G21 progressive loading overflow reporting", "G27 preview expansion intents"],
    "does_not_resolve": ["G28 cancel mutation"]
  },
  "test_plan": ["Manifest reason enum seed includes Sprint D values", "Unknown reason writes other plus alert", "turn_intent benchmark_comparison raises tool_previews with audit override"]
}
```
- **resolution_commit**: §Manifest Reason Enumeration @v0.3 (L1718-L1764)

### G27. 工具级 normalize_version + preview_template baseline 覆盖不足

- **triggered_by**: S01, S02, S08
- **evidence_refs**: WALKTHROUGH-S01.md §4 F4 + §5 (cargo/rustc/clippy)；WALKTHROUGH-S02.md §5 GapCand-S02-02 (raw_ref scheme)；WALKTHROUGH-S08.md §5 (parent_output_id + normalize_version NULL)；WALKTHROUGH-AGGREGATE.md §High G27
- **design_section**: §Preview Template Registry L1615–1669 (G8), §Content Hash Normalization L2163–2194 (G17)
- **severity**: high
- **category**: contract + doc
- **proposed_fix**: (1) G8 baseline 附录扩充至 15+ 工具模板（含 cargo/rustc/clippy/sql_compat_scan 等）；(2) 引入 `raw_ref` canonical scheme 注册表（artifact://, s3://, conversation_log:// 等）；(3) G17 规定 `normalize_version=NULL` 等价于 `raw_v1`（恒等）或改 NOT NULL；(4) tool runner registration 强制声明 template version
- **status**: resolved
- **response**:
```json
{
  "decision": "accepted",
  "rationale": "The v0.2 baselines were too sparse for real coding/research/tool-output workloads. Tool runners need registered preview/normalize contracts and raw_ref parsing must be canonical before cross-session/delegation retrieval is reliable.",
  "doc_sections": [
    "web-agent-session-state.md#Preview Template Registry",
    "web-agent-session-state.md#Tool Baseline, Raw Ref, and Runner Registration",
    "web-agent-session-state.md#Content Hash Normalization Contract",
    "web-agent-session-state.md#Implementation Plan",
    "web-agent-session-state.md#Changelog v0.3"
  ],
  "patch_summary": [
    "Added tool_runner_registry and raw_ref_scheme_registry.",
    "Defined canonical raw_ref format and baseline schemes including artifact, s3, conversation_log, object_store, cold_storage, and blob.",
    "Expanded baseline templates to Rust tools, SQL compatibility tools, PDF/URL tools, file tools, eslint, and skill_diff.",
    "Deprecated normalize_version=NULL and mapped legacy NULL to raw_v1.",
    "Added parent_output_id and session_artifact_provenance for derived/multi-source outputs."
  ],
  "assigned_phase": "Phase 6",
  "cross_gap_dependency": {
    "consumes": ["G8 preview template registry", "G9 artifact ACL/retention", "G17 normalization contract", "G23 batch output writes"],
    "feeds": ["G3 vector stale normalization checks", "G20 raw_ref resolver ACL checks"],
    "does_not_resolve": ["G28 cancel mutation", "G29 checkpoint extra"]
  },
  "test_plan": ["Tool runner cannot write without registered template/normalize_version", "raw_ref parser golden tests", "Rust/sql_compat baseline template validation", "legacy NULL normalize_version reads as raw_v1"]
}
```
- **resolution_commit**: §Tool Baseline, Raw Ref, and Runner Registration @v0.3 (L1990-L2097)

### G28. session_state_item_events.mutation 枚举需补 cancel

- **triggered_by**: S05
- **evidence_refs**: WALKTHROUGH-S05.md §5 G20；WALKTHROUGH-AGGREGATE.md §Medium G28
- **design_section**: §Session State Item Events mutation enum L1156–1166 (G14)
- **severity**: medium
- **category**: schema + contract
- **proposed_fix**: 补 `cancel` 值（区别于 archive：cancel 表示"决策性撤销"，archive 表示"完成归档"）；文档化 mutation 选择决策树
- **status**: open
- **response**: -
- **resolution_commit**: -

### G29. checkpoint_v1.extra 推荐结构未定义（batch 内部断点）

- **triggered_by**: S04
- **evidence_refs**: WALKTHROUGH-S04.md §5 "checkpoint_v1.extra"；WALKTHROUGH-AGGREGATE.md §Medium G29
- **design_section**: §Run Event Ordering and Ownership L1007–1016 (G15)
- **severity**: medium
- **category**: contract
- **proposed_fix**: §Durable Runs 补 checkpoint_v1.extra 的 "recommended sub-step fields"（`partial_progress: {step_index, total_steps, resumable_marker}`），不强制但给 schema hint；Phase 1 Exit 加 "batch + tool-call 双层 checkpoint resume" 测试
- **status**: open
- **response**: -
- **resolution_commit**: -

## G20–G29 汇总视图

| 严重度 | 数量 | Gap ID |
| --- | --- | --- |
| critical | 4 | G20 ✅ resolved, G21 ✅ resolved, G22 ✅ resolved, G23 ✅ resolved |
| high | 4 | G24 ✅ resolved, G25 ✅ resolved, G26 ✅ resolved, G27 ✅ resolved |
| medium | 2 | G28 ⭕ open, G29 ⭕ open |

## 整体 19 → 29 gap 全景（含 Sprint A/B/C + 端到端推演）

| 状态 | 数量 | Gap |
| --- | --- | --- |
| ✅ resolved | 27 | G1–G27 (Sprint A/B/C/D critical+high 闭环) |
| ⚠️ partially_resolved | 0 | — |
| ⭕ open (implementation contract-test follow-up) | 2 | G28, G29 |

**Sprint D 已消化 G20–G27 (8 条 critical+high)**。G28/G29 按本轮范围保留到实现期 contract test 或后续 Sprint。
