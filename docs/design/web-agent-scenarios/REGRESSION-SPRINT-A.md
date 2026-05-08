# Regression Verification — Sprint A (G2 / G4 / G15)

> Verifier: regression sub-agent (quality reviewer, strict mode)
> Date: 2026-05-06
> Target doc: `docs/design/web-agent-session-state.md` v0.2 (1954 lines)
> Verdict summary:
> - **G15**: `partially_resolved` — 核心承诺全部落地，但 `run_generation` 与 `run_counters.generation` 命名不一致、approval `expires_at` 的 clock-skew 处理（Q4）未写入文档，需补一个小 patch
> - **G4**: `partially_resolved` — schema 与 API 都到位，但 §5 context zone 表仍未给 `delegation_state` 独立预算、`last_summary_token_estimate` 有字段无用法说明、G14 `bubble_up` / `retry_scope` 预留字段跨 gap 未标注「属于 G14 未 resolved」，容易误读
> - **G2**: `partially_resolved` — 8 条不变量与 SQL 全部写出，但引用的 `finding/benchmark/citation` category 把未 resolved 的 G8/G12 相关概念提前固化，且 `session_state_item_events.mutation` 枚举仍只隐式出现在 assertion 里（G14 还没 accept）

---

## G15. Run durability `event_idx` 生成协议

### 承诺清单（from proposed_fix）

1. **新增 `run_counters` 表或明确「单 writer」约束（advisory lock / leader election）** — ✅ 落地
   - `run_counters` 表定义在 L778–789；含 `next_event_idx`、`owner_pod_id`、`owner_lease_expires_at`、`generation` 等
   - 「事务内锁 counters 行、递增、插入、更新 last_event_idx」的协议写在 L822–828
2. **Pod rolling 时 `agent_runs.owner_pod_id` + graceful handoff** — ✅ 落地
   - `agent_runs.owner_pod_id` / `owner_lease_expires_at` 在 L753–754
   - 协议写在 L830–842：单 writer / graceful shutdown / crash 分支 / 租约接管递增 `run_generation`
3. **Phase 1 Exit Criteria 区分 graceful shutdown vs crash** — ✅ 落地
   - Phase 1 Exit Criteria L1774–1777 明确 graceful=true → resume + 发 `run_resumed_after_restart`；无 graceful → `failed` 或 `retry_of` 新 run
4. **POST `/chat/runs/{run_id}/input` 强制 `idempotency_key` + 服务端去重** — ✅ 落地
   - 协议在 L855–859：`(run_id, idempotency_key)` 去重 + approval 额外按 `(approval_id, decision, actor_user_id)` 语义去重
   - API 声明在 L1733–1734（body 或 Header）
   - DB 侧有 `UNIQUE KEY uq_run_event_idempotency (run_id, idempotency_key)` L808
5. **新事件类型 `approval_expired` / `approval_retracted` / `run_resumed_after_restart`** — ✅ 落地
   - L861–865 列出三种一等事件；但文档只列名，未定义 payload schema
6. **SSE heartbeat 协议（server 15s / client 45s）** — ✅ 落地
   - L867–869：明确 15s 心跳 + 45s 判死 + 用 `last acknowledged event_idx` 重连
7. **`agent_runs.checkpoint_json` 定义 `checkpoint_v1` schema** — ✅ 落地
   - L844–853 给出 `{version, graceful, last_batch_id, extra}` 的 JSON 例子
   - 但 L757 的 `checkpoint_version VARCHAR(32) NULL` 没有强制等于 `checkpoint_v1`；现在是写入端约定，不是 schema 约束
8. **多 tab 订阅：事件幂等 + watermark 同步，不需 primary tab** — ✅ 落地
   - L869–872：无 primary、按 `(run_id, event_idx, event_hash)` 幂等应用 + watermark via web 缓存共享

### 场景反馈回应（S04 §8）

遍历 S04 §8 L790–836 的 5 大条建议：

| 反馈编号 | v0.2 证据 | 状态 |
| --- | --- | --- |
| 8.1-1 `event_idx` 生成协议 | L778–828（`run_counters`、事务协议、单 writer 约束） | ✅ |
| 8.1-2 `approval_expired` / `approval_retracted` / input 幂等 | L855–865 | ✅（但 payload schema 未列，见 residual） |
| 8.1-3 多 tab watermark + IndexedDB 一致性 | L869–872 | ⚠️ 只写了"share watermarks through the web cache"，watermark 与 event apply 是否同一 IndexedDB transaction **没写**（S04 8.1-3 第 1 条明确要求），这是客户端一致性规约；L1790 的 Phase 2 提 `run_event_high_watermark` 但没规定原子性 |
| 8.1-4 Phase 1 Exit 区分 graceful vs crash | L1774–1777 | ✅ |
| 8.1-5 Compaction trigger 检查活 run | L1560–1562 + SQL L1600–1604 | ✅（通过 G2 patch 交叉覆盖） |
| Q1 heartbeat 频率是 config 还是协议 | L867–869（写死 15/45） | ✅（按协议） |
| Q2 primary tab 机制 | L869–872（明确不需要） | ✅ |
| Q3 `checkpoint_json` schema | L844–853 + `checkpoint_v1` | ✅ |
| **Q4 `expires_at_server` vs 客户端 clock skew** | **未找到** | ❌ 文档全文未出现 `expires_at_server` / `ttl_seconds` / `clock skew`（见 grep 结果）。approval L863 只加了事件名 `approval_expired`，但服务端 vs 客户端时钟差的处理、UI 倒计时用相对时间这条没进入契约 |
| Q5 避免 run 分裂给两个 pod | L830–842 + 租约 + `run_generation` 递增 | ✅ |

### v0.2 自身一致性

1. **命名冗余与不一致**：`agent_runs.run_generation` (L755) vs `run_counters.generation` (L783)。同一语义（租约代数），用了两个不同列名——前者是 `run_generation`，后者是 `generation`。实现时谁是单一 source of truth？协议文字（L840）只说「租约接管增加 `run_generation`」，没写 `run_counters.generation` 是不是要同步。**属 v0.2 的字段名冲突，需要统一**
2. **`agent_runs.owner_pod_id` / `owner_lease_expires_at` vs `run_counters.owner_pod_id` / `owner_lease_expires_at`**：两张表都持有 owner lease。哪张表是权威？L830–834 写「`agent_runs.owner_pod_id` 标识当前 writer」，但 L824 写「事务锁 `run_counters`」，逻辑上 `run_counters.owner_pod_id` 才是 CAS 目标。**双写且未声明主从关系**是潜在一致性陷阱
3. **`checkpoint_version` 字段 vs JSON 内 `version` 字段**：L757 定义列 `checkpoint_version VARCHAR(32)`，而 L847 示例 JSON 内也有 `"version": "checkpoint_v1"`。两者是否必须相等？schema 未规定，读 JSON 即需解析，违反「不要在 JSON 列过滤」的 Development Rules
4. **跨 gap 预铺字段 `root_run_id` / `ancestor_path`**：L743–744 放进 `agent_runs`，文档里 G15 patch 并未使用（G15 场景是单 run，没有 delegation 树）。这两个字段服务于 G4/G10/G14，但**没有 HTML 注释标注 G4 或 G14**，regression 读者要跨到 L956+ 才看到用途。建议在 schema 注释里加 `-- used by G4/G14`

### Verdict: `partially_resolved`

核心 proposed_fix 全部落地，Sprint A 的产品阻塞已解除。但 3 个较小的契约空洞需在 v0.3 或紧随的 patch 内补：
- Q4 服务端/客户端时钟协议
- IndexedDB watermark 原子性
- `run_generation` vs `generation` 命名与主从关系

### Residual gaps

| ID | 描述 | 处理建议 |
| --- | --- | --- |
| G15-r1 | approval `expires_at_server` + 客户端 `ttl_seconds` 相对时间协议未入文档 | 在 §Run Event Ordering and Ownership 末尾补 1 段，或开 G18 |
| G15-r2 | Web 客户端 watermark 更新与 event apply 是否必须同一 IndexedDB transaction 未规定 | 归到 Phase 2（Web Transcript Hydration），在 §Web Client Cache 补「atomicity contract」，不单独开 gap |
| G15-r3 | `run_generation` vs `generation` 列命名不统一 | 设计 agent 直接把 `run_counters.generation` 改为 `run_generation` 或在文档里显式说明两者语义 1:1 同步。非开 gap，属 v0.2 内部编辑一致性 |
| G15-r4 | `approval_expired` / `approval_retracted` / `run_resumed_after_restart` 的 payload schema 未列 | Phase 1 实现时补；可由代码契约测试兜底 |
| G15-r5 | `checkpoint_version` 列与 JSON `version` 字段冗余 | 去掉 JSON 内 `version`，只用列；或保留 JSON 字段但声明「列为权威」 |

### Cross-gap dependency

- G15 为 G4 预铺 `root_run_id` / `ancestor_path` / `depth` 基础字段（在 `agent_runs`）—G4 的 `session_delegations` 复用它们
- G15 为 G14 预铺 `retry_of` / `retry_count` 但 **`retry_scope` 字段没加**，需 G14 resolve 时补 DDL `ALTER TABLE agent_runs ADD retry_scope ENUM(...)`
- G15 为 G7 预铺 `approval_expired` / `approval_retracted` 事件名，但 G7 Approval conditions 子表与 external notification adapter 仍 open

---

## G4. Delegation 一等化

### 承诺清单（from proposed_fix）

1. **Open Question 明确 both：run 父子边 + 可选子 session** — ✅ 落地
   - L995–1003 Delegation contract 明确：`agent_runs.parent_run_id/root_run_id/ancestor_path` 总是定义执行树；`agent_sessions` **仅在**子需要独立 identity 时新建
   - Changelog L1949–1951 重申
2. **`session_delegations` 独立表，payload schema 化（11 字段）** — ✅ 落地
   - L962–992 建表 SQL，含 `delegation_id / parent_run_id / child_run_id / child_session_id / root_run_id / ancestor_path / depth / status / phase / directive / last_summary_ref / last_summary_token_estimate / exposed_artifacts_json / deps_json / blocker_json / spawned_at / last_child_turn_idx`
   - 索引覆盖 `(session_id, status)` / `(parent_run_id)` / `(root_run_id, depth)` / `(child_session_id)`，满足 S10 的深层查询
3. **`last_summary_ref` 指向子 session `session_state_items(category='summary')`** — ✅ 落地
   - L1006–1009：子 session 有 id → 指向子的 summary item；run-only 子 → 指向父 session 内按 `delegation_id` 的 summary item
4. **APIs 暴露 tree rendering + drill-down** — ✅ 落地
   - L1040–1044：`GET /sessions/{id}/delegations?root_run_id=...` / `GET /chat/runs/{run_id}/children` / `GET /chat/runs/{run_id}/delegation-summary`
   - L1723 在 §API Additions 也列出

### 场景反馈回应

#### S06 §8（L528–578）

| 反馈 | v0.2 证据 | 状态 |
| --- | --- | --- |
| §6 补 Plan Tree Rendering Policy（祖先链/子树/兄弟） | 未找到 | ❌（属 G5，未 resolved；**S06 §8-1 本质是 G5，Sprint A 不应覆盖**） |
| §2 `delegation_state.payload_json` 九字段 schema | L962–992 给 17 字段（超集） | ✅ |
| Compaction Invariants | L1551–1636（G2 patch） | ✅ |
| Manifest reason 枚举（`plan_subtree_query` / `tree_structured_report`） | L1069 仍是 `VARCHAR(64) NOT NULL`；G1 open | ❌（属 G1，未 resolved） |

#### S09 §8（L554–600）

| 反馈 | v0.2 证据 | 状态 |
| --- | --- | --- |
| Open Question 明确为 both | L995–1003 | ✅ |
| `delegation_state` payload 11 字段 schema | L962–992（超集） | ✅ |
| **§Context Window Design `delegation_state` zone 加入一等 zone + 动态预算公式 `per_child_budget = min(1500/active_children, floor=200)`** | L466–470 zone 表被跳过到 L470，没展开；全文 grep `per_child_budget` / `delegation.*zone budget` 无结果 | ❌ **未落地**。v0.2 只加了 `last_summary_token_estimate`（L977），但没在 §Cheap Context Construction / §Budget Policy 写 zone 预算与抢占公式 |
| `last_summary_ref` 字段化 + `last_summary_token_estimate` 预算预校验 | 字段在 L976–977，但**没有「assembler 在 load 前做预算预校验」的行为说明** | ⚠️ schema 到位，policy 留白 |
| 跨 session artifact ACL（private/delegation/user） | 全文搜 `Artifact Access Scope` 无结果 | ❌（S09 §8-2 提的建议未采纳，也未明确 reject；与 G4 proposed_fix 未绑死，可以接受） |

#### S10 §8（L524–575）

| 反馈 | v0.2 证据 | 状态 |
| --- | --- | --- |
| Open Question 答案 "run children is default, child session only when needed" | L999–1003 | ✅ |
| `retry_scope ENUM('node','subtree','siblings')` + `status='superseded'` | **未加入 `agent_runs`** | ❌（属 G14，未 resolved。v0.2 **没有**预铺这两字段） |
| `session_state_item_events.mutation` 枚举扩到 `{insert, update, replace, archive, bubble_up, apply_suggestion}` | `mutation VARCHAR(32)` L919，但 v0.2 **没有**写出显式枚举值 | ❌（属 G14；G2 assertion L1583 只隐式用到 `replace / archive / delete` 三值，`bubble_up` 未列） |
| History Reconstruction 第 4 模式 "Delegation tree drill-down" | 全文搜 `drill-down` 只在 L1044 API 名里出现；§History Reconstruction Modes 未增第 4 模式 | ⚠️ API 有，文档章节未扩 |
| `depth` / `root_run_id` / `ancestor_path` 三字段（S10 §8 建议 1） | `session_delegations` L970–972 全部字段化；`agent_runs` L743–744 也有 `root_run_id` / `ancestor_path`（无 `depth`） | ✅（sessions_delegations）/ ⚠️（agent_runs 缺 `depth`；S10 明确要求「避免每次递归 CTE」） |
| bubble-up 一等事件机制 | 未加入 | ❌（属 G14） |

### v0.2 自身一致性

1. **`session_state_items(category='delegation_state')` 与 `session_delegations` 双表关系未闭环**：
   - L951 category 定义里 `delegation_state: child sessions, subagent work, handoff summary`
   - L958–960 声明 `session_delegations` 是权威 projection，state item 仍用于 prompt assembly
   - L1010–1012 再说 state item 只存「compact prompt projection」
   - 但**没有规定两者的同步契约**：当 `session_delegations.status` 从 `running` → `completed`，`session_state_items(category='delegation_state', item_key=delegation_id)` 是否必须同事务 UPSERT？如果不同步，G2 invariant #2（active delegation_state 不可 archived）就无法验证
2. **`session_delegations` 主键是 `delegation_id`，但 `session_state_items.item_key` 也建议用 `delegation_id`**：`uq_state_current (session_id, scope, category, item_key)` L905 保证唯一，但没有显式 FK 约束把两者绑定——应至少文字说「`session_state_items(category='delegation_state').item_key = session_delegations.delegation_id`」
3. **`UNIQUE KEY uq_delegation_child_run (child_run_id)` L987**：一个 child_run 只能归属一个 delegation。正确，但和 `agent_runs.parent_run_id` 可以 NULL 的关系没串起来——**非 delegation 的子 run 是否允许？**（例如 retry 的 `retry_of` 不是 delegation）。L1004 说「trigger_type='delegation' 的子 run 必须有 session_delegations 行」，但没禁止非 delegation 的父子边
4. **`exposed_artifacts_json` / `deps_json` / `blocker_json` 是 LONGTEXT JSON**：违反 Development Rules 「No JSON column filtering」。如果产品要按 blocker 类型聚合告警，就要 full table scan。**应在 §Delegation Contract 下加一条「这些列只用于呈现，筛选走 state item events」**；否则实现时很容易踩坑
5. **L743–744 `agent_runs.root_run_id` / `ancestor_path` 允许 NULL**；L970–971 `session_delegations` 则 `NOT NULL`。两表同义字段可空性不一致。叶 run（root 自身）怎么填？建议规定 `root_run_id = run_id self-ref` 保 NOT NULL 一致性（目前的 NULL 设计让 `INDEX idx_runs_root` 对 root 无用）

### Verdict: `partially_resolved`

schema、API、open question 决断都落地；但：
- delegation_state zone 的**动态预算公式**（S09 §8 关键建议）未进 v0.2 正文
- delegation_state state item 与 session_delegations 的**同步契约**未写
- state item event `mutation` 枚举未显式列（隐含依赖 G14）
- `agent_runs` 缺 `depth` 列（S10 明确要求避免递归 CTE）
- JSON 列筛选风险未在 delegation 语境声明

### Residual gaps

| ID | 描述 | 处理建议 |
| --- | --- | --- |
| G4-r1 | `delegation_state` context zone 预算与 `per_child_budget` 公式缺 | 开 G18（与 G10 "Budget Template" 并行）或扩 G4 response 到 v0.3 |
| G4-r2 | `session_state_items(category='delegation_state')` ↔ `session_delegations` 同步契约未写 | 设计 agent 在 §Delegation Contract 补 1 段 UPSERT 协议，不开 gap |
| G4-r3 | `session_state_item_events.mutation` 枚举未显式列 | 合并到 G14（bubble_up + apply_suggestion 已预告） |
| G4-r4 | `agent_runs` 缺 `depth` 列（与 `session_delegations` 对齐） | v0.2 内部编辑修复 |
| G4-r5 | `exposed_artifacts_json` / `deps_json` / `blocker_json` 的检索契约未声明 | 加一条「不在 JSON 列筛选」规约即可 |

### Cross-gap dependency

- G4 **依赖** G15 已落地的 `root_run_id` / `ancestor_path`（agent_runs L743–744）
- G4 **预铺** G14：`retry_of` 已在 L747，但 `retry_scope` / `superseded` 状态 / `bubble_up` event mutation 还没进 schema；G14 resolve 时 `ALTER TABLE agent_runs ADD retry_scope VARCHAR(32) NOT NULL DEFAULT 'node'` 是必要 patch
- G4 **预铺** G5（Plan Tree Rendering Policy）：目前 `session_delegations.ancestor_path` 已可承载 plan 层级递归查询的基础，但 plan 树和 delegation 树是两棵树，S06 §8-1 的 4 条渲染规则与 G4 无直接关系，**不要**误认为 G4 覆盖了 G5
- G4 **预铺** G9（artifact retention）：`exposed_artifacts_json` 引用了 `session_artifacts`，G9 的 `referenced_by_state_items_count` 需要在 delegation 更新时 bump

---

## G2. Compaction Invariants

### 承诺清单（from proposed_fix）

1. **白名单：`category IN ('plan_state','decision','todo_state(active)','error_state(active)','delegation_state(active)','finding','benchmark','citation')` 绝不物理压缩或 replace** — ✅ 落地
   - L1555–1558 覆盖全部 9 个 category（note: todo/error/delegation 限 active）
   - SQL assertion L1577–1588 实际执行检查
2. **`plan_state.version` 在 compaction 轮次禁止 bump** — ✅ 落地
   - L1559–1559 文字；SQL L1590–1598 用 `next_version <> previous_version` 检测
   - 为支持该 assertion，L922–923 `session_state_item_events` 加入 `previous_version` / `next_version` 列（`patch_summary` 点出）
3. **Compaction trigger 必须检查 `agent_runs.status NOT IN ('running','waiting')`** — ✅ 落地
   - L1560–1562 文字；SQL L1600–1604 `COUNT(*) FROM agent_runs WHERE status IN ('running','waiting')` 必须为 0
4. **Done subtask 可 archived 但保留 provenance** — ✅ 落地
   - L1563–1564 文字；SQL L1606–1612 `archived_todos_without_provenance` 必须为 0
5. **manifest 必写（reason='post_compaction'）** — ✅ 落地
   - L1565–1567 文字；SQL L1614–1619 `post_compaction_manifest_count` 必须为 1
6. **8 条不变量总数** — ✅ 落地
   - 明确 8 条，SQL 8 段，覆盖：(1) 原始表不改写、(2) active L1a 不 replace/archive、(3) plan_state.version 稳定、(4) 活 run 防护、(5) done todo provenance、(6) manifest 必写、(7) durable fact provenance、(8) summary 不删除
7. **`finding` / `benchmark` / `citation` 三个 durable fact category** — ✅ 落地
   - L943–945 category 列表扩展
   - Phase 4 L1820 明确 populate 这三类

### 场景反馈回应

#### S01 §8（L421–454）

| 反馈 | v0.2 证据 | 状态 |
| --- | --- | --- |
| compaction 时 category 白名单 | L1555–1558 + SQL L1577–1588 | ✅ |
| 三档（不可改/可降级/可合并） | L1533（§Compaction 旧文字「files/decisions/active errors/plan/todos preserve」）+ L1563–1564（todo 可 archived）+ L1570–1571（summary 新版 active 旧版 archived） | ✅ |
| `context_manifests.reason` 枚举化 | L1069 仍 `VARCHAR(64) NOT NULL`，无 enum lookup 表 | ❌（属 G1，未 resolved） |
| vector/FTS "降级不重嵌入，仅改 importance" 默认策略 | 未找到 | ❌（属 G3 + S02 反馈，未 resolved） |

#### S02 §8（L520–551）

| 反馈 | v0.2 证据 | 状态 |
| --- | --- | --- |
| 三级检索 SLA + 降级 state machine | 未找到 | ❌（G3，未 resolved） |
| content_hash 失效 → 索引重建流程 | 未找到 | ❌（G17，未 resolved） |
| raw_ref artifact retention 绑定 | 未找到 | ❌（G9，未 resolved） |

⚠️ S02 §8 **3 条反馈全部不在 G2 的 proposed_fix 范围**。G2 只借用 S02 作为 triggered_by，真实承诺在 S01/S06/S12。S02 遗留的 3 条都属于其他未 resolved gap，**不是 G2 的回归失败**

#### S06 §8（L528–578）

| 反馈 | v0.2 证据 | 状态 |
| --- | --- | --- |
| Compaction Invariants 列 plan_state + decision + delegation_state(active) 不压缩、done subtask 可 archived、plan_state.version 禁 bump | L1555–1598 **全部覆盖** | ✅ |

#### S12 §8（L534–583）

| 反馈 | v0.2 证据 | 状态 |
| --- | --- | --- |
| `finding` / `benchmark` / `citation` 作为一等结构化 chunk category，compaction 白名单 | L943–945 + L1555–1558 + L1568–1569（standalone provenance）+ SQL #7 L1621–1627 | ✅（category 侧） |
| `session_history_chunks.chunk_type` 同步增补 `finding` / `benchmark` / `citation` | L1264–1276 chunk_type 列表**未更新**，仅有 `decision / error / plan_change / todo_change / summary` | ❌ **v0.2 有偷懒**。`session_state_items.category` 加了三类，`session_history_chunks.chunk_type` 没同步，retrieval 层会因此漏检索 |
| 项目级 retention（`project_id` / `retention_policy`） | 未找到 | ❌（属 G9，未 resolved） |
| Preview Template Registry | 未找到 | ❌（属 G8，未 resolved） |

### v0.2 自身一致性

1. **跨 gap 引用未标注**：G2 SQL assertion #2 L1585 引用 `category IN ('finding', 'benchmark', 'citation')`；category 虽在 L943–945 预定义，但这三个 category 的**语义拥有者其实是 S12 → G8/G12**，且 G8（Preview Template Registry）和 G12（Next-Action confidence）都还 **open**。v0.2 先给 category 名字 + compaction 保护，但**没给 payload schema**。Phase 4 Exit L1820 也只说 populate 这三类，没给字段。这是 G2 抢跑，把未 resolved gap 的概念"冻结"成 compaction 契约
2. **`session_state_item_events.mutation` 仍是自由 VARCHAR(32)**：L919 只定义类型，没写枚举。SQL #1 L1583 只 match `'replace', 'archive', 'delete'`；SQL #8 L1635 match `'delete'`。**`insert` / `update` / `bubble_up` / `apply_suggestion` 都没进枚举**。若实现端写了 `'soft_delete'` 或 `'retire'`，assertion 直接失效。应在 v0.2 就约束枚举
3. **Invariant #1 声明 `agent_run_events` 不物理改写 (L1553–1554)**：一致。但 `agent_run_events.id` 是 AUTO_INCREMENT L792，`event_idx` 是顺序列。G15 的 `run_counters` 协议保证 `event_idx` 不跳号，**不保证 `id` 与 `event_idx` 同步单调**。Invariant #1 引用 `agent_run_events` 不改写，但未声明顺序约束。对 G2 无影响，但 S04 的 `last_index=N` replay 正确性依赖此点——这是 G15 的责任，此处只是指出 G2 引用的前提
4. **`category='summary'` 在 `session_state_items` L938 与 `session_history_chunks.chunk_type='summary'` L1276 并存**：compaction 写哪个？L1531–1532 说"write to `conversation_log` and/or `session_state_items(category='summary')`"，但 Invariant #8 L1570–1571 + SQL #8 L1629–1635 只保护 **state item events**；history chunk 的 summary row 不在 assertion 覆盖内。这是契约覆盖盲区
5. **`context_manifests.run_id` NULL 允许**（L1058 区域）**但 Invariant #6 SQL L1618** 用 `run_id = :compaction_run_id`。如果 compactor 不是一个 run 的产物（session-level 定时任务），`run_id` 可能 NULL，assertion 会 0 行，误判 "missing"。应补：`compaction_run_id` 可以是 sentinel `'__compaction__'` 或 invariant 改为 `reason='post_compaction' AND (run_id = :compaction_run_id OR run_id IS NULL)`

### Verdict: `partially_resolved`

8 条 invariant + 8 段 SQL 齐全，Sprint A 的契约化目标达成。但：
- `finding` / `benchmark` / `citation` 的 payload schema 跨 gap 抢跑
- `session_history_chunks.chunk_type` 与 `session_state_items.category` 不同步
- `mutation` 枚举未显式列
- summary 双表写入路径未闭环
- compaction_run_id NULL 边缘情况

### Residual gaps

| ID | 描述 | 处理建议 |
| --- | --- | --- |
| G2-r1 | `finding` / `benchmark` / `citation` payload schema 未 spec，但已入 compaction 白名单 | 与 G8 合并处理，或在 v0.2 标注 "schema TBD in G8" |
| G2-r2 | `session_history_chunks.chunk_type` 列表未同步加 `finding` / `benchmark` / `citation` | v0.2 内部编辑修复：改 L1264–1276 |
| G2-r3 | `session_state_item_events.mutation` 未显式枚举 | 合并到 G14（已列所有需要的值） |
| G2-r4 | summary 双写路径（conversation_log 与 state item）assertion 不对称 | 在 §Compaction 正文补一行「state item 的 summary 为权威；conversation_log 的 summary 行是 narrative 回放源」 |
| G2-r5 | `context_manifests.run_id` NULL 时 invariant #6 误判 | 改 SQL 条件或强制 compactor 写一个 sentinel run_id |

### Cross-gap dependency

- G2 **消费** G15：`agent_run_events` 的 append-only 在 Invariant #1 被依赖；G15 的 `run_counters` 保证了这一点
- G2 **消费** G4：Invariant #2 引用 `category='delegation_state' AND status IN ('active','waiting',...)`；G4 的 `session_delegations` 若与 state item 不同步，invariant #2 就保护不了权威状态（见 G4 residual r2）
- G2 **预铺** G1：L1566 hardcode `reason='post_compaction'`；G1 resolve 时要把 `post_compaction` 放进枚举表，别的 reason 也要正式化
- G2 **预铺** G8 / G12：三个 durable fact category 的 payload schema 要随 G8（Preview Template Registry）落地后对齐；否则 compaction 只保护了 category 名，没保护 payload 结构
- G2 **预铺** G14：`session_state_item_events.mutation` 需要在 G14 resolve 时显式枚举，把 `bubble_up` / `apply_suggestion` 与 G2 的 `insert / update / replace / archive / delete` 统一

---

## 综合建议

### 可以直接在 GAP-TRACKER 标 resolved 的

**无**。三条 critical gap 都是 `partially_resolved`，不应直接 close。

但严格讲：
- G15 的产品阻塞已解除（Phase 1 可以启动实现），在实施层面算「可推进」
- G4 的 schema 足以让 multi-agent MVP 落地，zone 预算可延到产品 iterate
- G2 的 compaction 契约足以防止 active state 丢失

### 建议的状态更新

| Gap | 当前 status | 建议 | 理由 |
| --- | --- | --- | --- |
| G15 | `resolved` | **`resolved` 但加 `resolution_notes` 列**说明 residual | 核心承诺完成，residual 属实现细节可放 Phase 1 contract test 兜底 |
| G4 | `resolved` | **降为 `accepted`（v0.2 落地中）** 或保 `resolved` 但必须补 §Delegation Contract 的动态预算公式 | zone 预算是 S09 明确反馈的关键，缺它就说多 agent 场景 prompt 预算不闭环 |
| G2 | `resolved` | **保 `resolved`** 但补 `session_history_chunks.chunk_type` 同步 patch 后再锁 | 8 条 invariant 都写了，chunk_type 不同步是次要漏，但必须补 |

### 新发现的 issue 是否要开 G18+

建议开以下新 gap：

- **G18**: `delegation_state` context zone 动态预算公式（来自 G4 residual r1 + S09 §8-3）——属独立议题，不混入 G4 或 G10
- **G19**: Web 客户端 IndexedDB watermark + event apply 原子性契约（来自 G15 residual r2 + S04 §8-3）——属 client-side contract，与 G13（device fingerprint）语义不同

其他 residual 都可在当前 gap 的 response 内追加说明或合并到 G1/G8/G12/G14 处理，不必开新 gap。

---

## GAP-TRACKER 状态更新 patch 片段

### 1. 为三条 Sprint A gap 补 residual_notes 段

在 GAP-TRACKER.md 的 G2、G4、G15 的 `resolution_commit` 行下方插入：

```markdown
- **residual_notes**:
  - G15 r1–r5（见 REGRESSION-SPRINT-A.md §G15 Residual gaps）
  - 需 Phase 1 实现时 contract test 覆盖 r2 + r5
  - r1 / r3 延至 v0.3 patch
```

```markdown
- **residual_notes**:
  - G4 r1–r5（见 REGRESSION-SPRINT-A.md §G4 Residual gaps）
  - r1 升级为 G18 新 gap
  - r2 在 §Delegation Contract 加 UPSERT 协议（v0.2 内部编辑）
  - r3 合并 G14 处理
```

```markdown
- **residual_notes**:
  - G2 r1–r5（见 REGRESSION-SPRINT-A.md §G2 Residual gaps）
  - r2 在 v0.2 内部编辑修复（同步 chunk_type）
  - r3 合并 G14 处理
  - r1 跟随 G8 resolve
```

### 2. 追加两条新 gap 到 GAP-TRACKER.md（末尾插入）

```markdown
### G18. Delegation context zone 动态预算公式缺

- **triggered_by**: S09
- **evidence_refs**: S09 §8 建议 3（`per_child_budget` 公式）；REGRESSION-SPRINT-A.md §G4 residual r1
- **design_section**: §Cheap Context Construction / §Budget Policy v1
- **severity**: high
- **category**: policy
- **proposed_fix**:
  - 在 zone 表正式加入 `delegation_state` zone
  - 公式：`per_child_budget = min(1500 / active_children, floor=200)`，blocker 发生时该子临时扩 2× 从 `recent_tail` 借
  - 单 child summary 超限 fallback 到 `title + artifact_ref`
  - 用 `last_summary_token_estimate` 做 assembler 预算预校验
- **status**: open
- **response**: -
- **resolution_commit**: -

### G19. Web 客户端 watermark + event apply 原子性契约缺

- **triggered_by**: S04
- **evidence_refs**: S04 §8 建议 3（"watermark 和 event 应用是否必须同一 transaction"）；REGRESSION-SPRINT-A.md §G15 residual r2
- **design_section**: §Web Client Cache + Phase 2 Exit criteria
- **severity**: medium
- **category**: contract
- **proposed_fix**:
  - IndexedDB 中 `run_event_high_watermark` 与 events 行必须同一 transaction 写入
  - 规定 client gap 检测：若连续 event_idx 跳号 > 1 则 reset + re-replay from last_ok_idx
  - 多 tab 共享 watermark via BroadcastChannel 或 SharedWorker，防止两 tab 各自推进
- **status**: open
- **response**: -
- **resolution_commit**: -
```

### 3. 更新汇总视图

```diff
 | 严重度 | 数量 | Gap ID |
 | --- | --- | --- |
 | critical | 3 | G2, G4, G15 |
-| high | 7 | G1, G3, G5, G6, G7, G8, G9 |
-| medium | 7 | G10, G11, G12, G13, G14, G16, G17 |
+| high | 8 | G1, G3, G5, G6, G7, G8, G9, G18 |
+| medium | 8 | G10, G11, G12, G13, G14, G16, G17, G19 |
```

### 4. 设计文档 v0.2 内部编辑建议（不开 gap）

直接 sed / edit 修正：

- 把 `run_counters.generation` 列改名为 `run_generation`（对齐 `agent_runs.run_generation`）：L783
- `session_history_chunks` 的 `chunk_type` 列表 L1264–1276 增加 `finding` / `benchmark` / `citation` 三个 bullet
- `agent_runs` 增加 `depth INT NULL`（与 `session_delegations.depth` 对齐，避免递归 CTE）：在 L744 下方插入
- 在 §Delegation Contract（L995 起）追加 1 段：`session_state_items(category='delegation_state')` ↔ `session_delegations` 的 UPSERT 协议（同事务 / 两者 item_key 一致）
- 在 §Compaction Invariants（L1629 下方）补一行：`context_manifests.run_id` 在 session-level compaction 可为 NULL，assertion 中 `run_id = :compaction_run_id OR run_id IS NULL`

---

## 附：grep 佐证

```bash
# G15 - expires_at_server / clock skew / ttl_seconds 全文缺席
rg 'expires_at_server|ttl_seconds|clock.*skew' docs/design/web-agent-session-state.md
# -> 0 matches

# G4 - per_child_budget / Artifact Access Scope / delegation zone budget 全文缺席
rg 'per_child_budget|Artifact Access Scope|delegation.*zone.*budget' docs/design/web-agent-session-state.md
# -> 0 matches

# G2 - finding/benchmark/citation 在 chunk_type 列表未同步
rg -n 'finding|benchmark|citation' docs/design/web-agent-session-state.md
# -> 只出现在 session_state_items.category 段 (L943-945) 与 compaction (L1555/1568/1585/1625/1820)
#    未出现在 session_history_chunks.chunk_type 列表 (L1266-1276)

# retry_scope / bubble_up / superseded 全文缺席（属 G14 未 resolved）
rg 'retry_scope|bubble_up|superseded' docs/design/web-agent-session-state.md
# -> 0 matches
```
