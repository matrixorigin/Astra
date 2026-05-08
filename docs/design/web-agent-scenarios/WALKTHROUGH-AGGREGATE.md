# Walkthrough Aggregate — Cross-Scenario Gap Merging

> Source: 14 × WALKTHROUGH-S<NN>.md (6298 行)
> Aggregated: 2026-05-07
> Method: 手工归并（两次 sub-agent 因 Bedrock 上游 thinking.enabled.budget_tokens 500/503 故障失败）
> 原始 gap 候选数（各场景独立 §5 + §4 blocking friction 合计）: **~39 条**
> 聚合后唯一 gap 数: **15**
> 建议开真 gap (G20+): **10**（4 critical + 4 high + 2 medium，其余合并或延后）

## 背景

Sprint A/B/C 把 17 条原始 gap（G1-G17）+ 2 条回归新 gap（G18/G19）全部标为 resolved，v0.2 文档扩到 3040 行。但 **14 个场景的端到端推演** 发现：v0.2 在"契约层面是完备的"之下，还有"运行层面会踩"的坑 —— 这正是 contract test 兜底之前必须消化的工作。

关键区分：
- **friction** (notable/minor): 实现时会发现但可以约定解决；不需要新 gap
- **friction (blocking)**: 必须升级为 gap
- **gap candidate**: 场景 §5 明确提出，设计空白

## Critical Gaps (场景判 blocking)

### G20. 跨兄弟子 session 的 artifact/state 访问权限未定义

- **聚合来源**: [S09 F5 blocking + S10 §6 组合 4 observation + S09 §5 edge gap]
- **现象**: v0.2 §Artifact Access Scope L1716–1720 把 `delegation` scope 定义为 "parent + descendants"。但多 agent 并行时 (S09 FE/BE/DBA 三兄弟)、delegation 树里非直系关系 (S10 L3 executor-2 ↔ L3 reviewer-3 ↔ L1 reporter) 都是常态。严格按文字语义，be-agent 读不到 dba-agent 的迁移 SQL artifact（都是 orchestrator 的 child），reporter 看不到同层 executor 的 findings。
- **为什么 critical**: S09 T6/T7/T12 三处主线触发，场景**跑不通**，不是绕行问题。
- **建议修复**: access_scope 从 parent/descendants 扩展为 `same_root_tree`；为"兄弟" 场景引入 `sibling_exposed_artifacts[]` 明确白名单；或采用 delegation_id 的 path-based 前缀匹配规则（`ancestor_path LIKE '/root/%'`）。
- **涉及 v0.2 章节**: §Artifact Access Scope L1716–1720, §Delegation Contract L1321–1426

### G21. G18 delegation zone budget 公式在大 fan-out 时自相矛盾

- **聚合来源**: [S10 F-blocking-1]
- **现象**: G18 L2450–2465 公式 `per_child_budget = min(1200, max(200, floor(1500/n)))`。当 `active_children ≥ 8` 时，floor=200 × 8 = 1600 > 1500 hard cap。S10 T9（执行期含 retry pair）可达 n=10+，公式两个约束不能同时成立。
- **为什么 critical**: 纯数学 bug，S10 暴露的**真实计算反例**。
- **建议修复**: 公式改为 `max(200, floor(budget_total/n))`；当 `n > 7` 时自动按 priority/blocker 状态过滤活跃子 agent 到 top-K；或降级为分页式 child summary（未入 prompt 的子放 "2 more blocked, click to expand"）。
- **涉及 v0.2 章节**: §Delegation State Budget L2442–2471

### G22. retry_scope 判定规则 + apply_suggestion payload 缺 retry_scope 字段

- **聚合来源**: [S10 F-blocking-2]
- **现象**: G14 L1194 声明 `retry_scope ENUM('node','subtree','siblings')`，但没说"什么情况选哪个"。更糟的是 G14 L1181–1189 apply_suggestion payload 只含 `{suggested_next_action_id, chosen_candidate_id, source, confidence_at_accept}`，**无 retry_scope 字段**。S10 T8 用户说"重做 executor-2 的数据库扫描"时无法把 scope 传进 retry 请求。
- **为什么 critical**: retry 是 bubble_up 的对称操作，retry_scope 不能传达导致 agent 只能猜 scope = node（默认），触发副作用最小，但用户可能想 subtree。
- **建议修复**: (1) 在 G14 加一段 "retry_scope 选择规则"：user 显式指定 > agent 推断 > 默认 node；(2) apply_suggestion payload 加 `retry_scope VARCHAR(16) NULL` 字段。
- **涉及 v0.2 章节**: §Delegation Mutation Events L1154–1215

### G23. session_tool_outputs 批量写入契约缺（N+1 问题）

- **聚合来源**: [S08 §4 blocking + S08 §5]
- **现象**: v0.2 §Tool Result L1560–1610 定义 `session_tool_outputs` 表，但只在 L1886 对 `session_history_chunks` vector 说 "should be batch inserted"。S08 T5 的"扫 1000 个 SQL 文件、每个写一条 session_tool_outputs"如果按 per-row insert 实现会把回合延迟拖到 3–5s 并撞爆 DB 连接池。
- **为什么 critical**: 不是可选优化，是会直接让 S08 跑不动的实现陷阱。
- **建议修复**: §Tool Result 补一节 "Batch Insert Contract"：规定 batch boundary + txn size 上限 + 各 tool runner 推荐 batch 协议（每 100-500 行一个 batch）；引入 `session_tool_output_batches` 表追踪 batch 完成状态。
- **涉及 v0.2 章节**: §Tool Result and Artifact References L1560–1610

## High Gaps (≥2 场景独立发现 或明显 UX/实现阻塞)

### G24. Cold-start hydration：state API 与 stream API watermark 契约未闭环

- **聚合来源**: [S04 §5 gap "Cold-Start" + S03 F1 "iPad 冷启"暗含]
- **现象**: v0.2 §Web Client Cache L403–415 open-session flow 只覆盖"有 cached state"的情况。**冷启动 tab/设备**（local watermark=0）拿到 `/sessions/{id}/state` 返回的 `active_run.run_event_high_watermark=131` 后，如果直接塞给 `stream?last_index=131` 会跳过历史 replay，IndexedDB 为空 → 历史永远补不齐。
- **为什么 high**: S04 T09 双 tab 场景直接暴露；S03 iPad 段也是冷启路径。跨场景出现。
- **建议修复**: 补一节 "Cold-Start Hydration"：client 先按 transcript API 拉历史；server 在 /sessions/state 响应里加 `replay_required=true` 当检测到 `known_state_revision=0` 但 `active_run.run_event_high_watermark>0`；client 必须 `stream?last_index=0` 全量 replay 或分页 transcript 拉取。
- **涉及 v0.2 章节**: §Web Client Cache L373–436, §Durable Runs L1052–1057

### G25. Auto-expiry vs explicit revoke — SSE event parity 缺 (安全)

- **聚合来源**: [S03 §5 B (high, 安全)]
- **现象**: G13 定义了 `POST /sessions/{id}/device/revoke` 显式路径会发 SSE `device_revoked` 事件。但 `session_device_leases.expires_at` **被动过期**时走的是定时扫描/读时检查路径，v0.2 未规定是否发对等 SSE 事件。结果："未显式 logout 即离开" 的 Chromebook（零信任设备）localStorage 清不掉，敏感 session state 残留。
- **为什么 high**: 安全零信任场景（公用设备）直接踩；S03 T8 场景真实压测。
- **建议修复**: 要求 expires_at 被动过期也必须 emit `device_lease_expired` SSE 事件（payload 与 `device_revoked` 对称），并让 client SDK 在任一事件触发本地 storage 清理。
- **涉及 v0.2 章节**: §Revision Reconciliation and Device Lease L438–498

### G26. context_manifests.reason 枚举补强（汇总多场景）

- **聚合来源**: [S05 §5-G22 + S11 §5-#1 + S12 §5 (intent-aware) + S13 候选 G22 + S14 §5-#2 "progressive_loading"]
- **现象**: G1 L1504–1515 定义了 16 个 reason 枚举值，但场景推演反复发现**新枚举缺失**：
  - `ambiguity_clarification` (S05 T10 "继续" 歧义)
  - `user_memory_promote` / `user_memory_archive` / `user_memory_revise` (S11 T10)
  - `cross_skill_alignment` (S13 T11 跨 skill reference)
  - `progressive_loading` (S14 小窗口下分轮展开)
  - `intent_driven_preview_expand` (S12 T9/T11 benchmark_comparison)
- **为什么 high**: 5 个场景独立撞到；枚举不全则 G1 观测性聚合 SQL 失效，各实现塞自由字符串。
- **建议修复**: 在 G1 枚举表后补 5–7 个新值（列举上面五类）；补一条"如果触发未知 reason，必须 fallback 到 `other` 并告警"。同时引入 `turn_intent VARCHAR(64)` 字段支持 intent-aware budget profile (S12 建议)。
- **涉及 v0.2 章节**: §Manifest Reason Enumeration L1504–1515

### G27. 工具级 normalize_version + preview_template baseline 覆盖不足

- **聚合来源**: [S01 §5 (cargo/rustc/clippy 缺) + S02 §5 (raw_ref scheme 缺) + S08 §5 (normalize_version NULL 矛盾)]
- **现象**: G8 L1615–1669 与 G17 L2163–2194 给了 pg_dump/slow_query_analyzer/fetch_url/parse_pdf/SKILL.md 五个 baseline 模板。但场景里的常见工具**全部没覆盖**：
  - S01: Rust 工具链 cargo/rustc/clippy 用于编译/测试/lint 输出
  - S02: raw_ref 字符串 URL scheme（如 `artifact://`、`s3://`、`conversation_log://`）没有 canonical 格式
  - S08: `sql_compat_scan`、`pg_schema_structurize`、`slow_query_analyzer` 复用 preview template 但细节未覆盖
  - S08: `normalize_version=NULL` 的合法性与 hash 契约冲突（slowlog 有意不规范化）
- **为什么 high**: 3 个场景独立发现，且是"跑得起来但各实现写法漂"的典型表现；contract test 覆盖成本高。
- **建议修复**: (1) G8 baseline 附录扩充 10+ 工具模板；(2) 引入 `raw_ref` canonical scheme 注册表；(3) G17 规定 `normalize_version=NULL` 等价于 `raw_v1`（恒等规范化）或改 NOT NULL；(4) 所有 tool runner 必须在 registration 时声明模板 version。
- **涉及 v0.2 章节**: §Preview Template Registry L1615–1669, §Content Hash Normalization L2163–2194

## Medium Gaps (单场景但影响观察性/契约)

### G28. session_state_item_events.mutation 枚举需补 cancel + retry 细节

- **聚合来源**: [S05 §5-G20 + S10 F-blocking-2 衍生]
- **现象**: G14 L1156–1166 枚举 8 个 mutation 值，但不含 `cancel`。S05 T3 "折扣砍掉" 需要把 todo 从 active 直接设为 cancelled，实现者要在 `update`/`archive`/`replace` 之间选，语义各异。
- **建议修复**: 补 `cancel` 值（不同于 archive，表示"决策性撤销"不是"完成归档"）；补"mutation 选择决策树"文档。
- **涉及 v0.2 章节**: §Session State Item Events mutation enum L1156–1166

### G29. checkpoint_v1.extra 推荐结构未定义（batch 内部断点）

- **聚合来源**: [S04 §5 "checkpoint_v1.extra"]
- **现象**: G15 L1007–1016 `checkpoint_v1` 强制 `last_batch_id` (VARCHAR)，batch 内部的 sub-step (如"INSERT 到第 n 行") 留 extra 自由 JSON。S04 T11 的 pod 重启若只能从 batch 起点重跑，最坏情况一次重启重做 500k 行 INSERT。
- **建议修复**: §Durable Runs 补 checkpoint_v1.extra 的 "recommended sub-step fields"（`partial_progress: {step_index, total_steps, resumable_marker}`），不强制但给 schema hint；Phase 1 Exit 加 "batch + tool-call 双层 checkpoint test"。
- **涉及 v0.2 章节**: §Run Event Ordering and Ownership L1007–1016

## Low / 工业化周边（不单独开 gap，合并或 contract test 兜底）

这些散点由 14 份 §4 Frictions 和 §5 边缘建议合并，**每条都有对应实现约定或在已 resolved gap 的 implementation clarifications 下补即可**：

- **render_mode 枚举化** (S02) — 合并到 G1 的 reason 枚举统一扩展
- **raw_ref URL scheme canonical** (S02) — 合并到 G27
- **retrieval.vector_stale payload 漏 normalize_version** (S02) — 合并到 G3 的 event payload spec
- **session_state_items.category canonical enum** (S02) — 作为 G6 implementation clarification
- **waiting_for grammar 字符串格式** (S04, S14) — G15 implementation clarification
- **session_approval_conditions.check_trigger 复合语义** (S07) — G7 implementation clarification
- **linked_approval_ref 索引列** (S07) — G7 implementation clarification
- **audit export post_complete 事件类型** (S07) — G7 implementation clarification
- **session_todos 子孙递归 SQL 规范** (S06) — G5 implementation clarification
- **session_state_items.item_key 命名前缀约定** (S06) — G4 Projection Sync Contract 下补
- **drill-down API 按业务 key (仓库名/文件名) 查** (S10) — G4 delegation-summary API 扩展
- **父 session 视角子 run 聚合 UI** (S09) — G14 implementation clarification
- **Personal skill auto-quarantine trigger** (S13) — G16 implementation clarification
- **GET /skills/user query 参数 spec** (S13) — G16 implementation clarification
- **false_positive_rate 分母口径** (S13) — G16 implementation clarification
- **Small-model JSON schema validation fallback** (S14) — G12 implementation clarification
- **Edge reachability probe 触发时机** (S14) — G11 implementation clarification

## 各场景 → 聚合 gap 映射

| Scenario | §5 原 gap 数 | 聚合后 gap | 其余合并到 Low |
| --- | --- | --- | --- |
| S01 | 3 | G27 | 3 low |
| S02 | 4 | G27 | 3 low |
| S03 | 4 | G25 | 3 low |
| S04 | 3 | G24, G29 | 1 low |
| S05 | 3 | G26, G28 | 1 low |
| S06 | 2 | (无) | 2 low |
| S07 | 3 | (无) | 3 low |
| S08 | 2 | G23, G27 | 1 low |
| S09 | 1 (+F5 blocking) | G20 | 1 low |
| S10 | 3 (2 blocking) | G20, G21, G22 | 1 low |
| S11 | 2 | G26 | 1 low |
| S12 | 1 | G26 | 0 |
| S13 | 4 | G26 | 3 low |
| S14 | 4 | G26 | 3 low |

映射说明：
- **14 个场景共产生 ~39 条原始候选**（§5 + §4 blocking）
- **聚合去重后: 15 个独特主题**
- **建议真正开 G20-G29: 10 个**（4 critical + 4 high + 2 medium）
- 其余 5 主题合并到上述或作为 low/implementation clarification

## 重复模式分析

| 模式 | 触发场景数 | 建议 gap |
| --- | --- | --- |
| **A. Reason/Mutation/Category 枚举不全** | 6 (S01/S02/S05/S11/S13/S14) | G26, G28 |
| **B. 跨层/兄弟 access 权限** | 2 (S09/S10) 但都 blocking | G20 |
| **C. 工具 baseline 覆盖不足** | 3 (S01/S02/S08) | G27 |
| **D. Cold-start/lease/event parity** | 2 (S03/S04) | G24, G25 |
| **E. Budget 公式/精细粒度** | 2 (S10/S04) | G21, G29 |
| **F. 批量/契约对齐/留白** | 多场景零散 | 合并到 implementation clarifications |

## Verdict

### v0.2 端到端可实现性

- **12/14 场景可实现**（✅ / ⚠️ 标签）
- **2 个场景在主线触发 blocking**：S09（兄弟 ACL）、S10（公式矛盾 + retry_scope 字段）
- **3 个 high gap 跨 2 场景独立暴露**，不是单场景孤立问题

### 意外 blocker 数

**4 个 critical-级**（G20/G21/G22/G23）—— 在 gap review 阶段没被识别，在端到端推演阶段才暴露。这 4 个必须在 Phase 1–3 实现前关闭。

### 最严重的 3 条共振 gap

1. **G20 兄弟子 session ACL** — S09/S10 都阻塞；语义必须在 schema 文本里修正
2. **G26 manifest reason 枚举不全** — 5 场景连续撞到，观测性基础设施
3. **G27 工具 baseline 覆盖** — 3 场景撞到，工程化落地门槛

### 建议 Sprint D 范围

**只做 critical + high**：G20/G21/G22/G23 + G24/G25/G26/G27 共 8 条（4+4），按同样的"patch → 回归"循环。G28/G29 可并入 Sprint D 末尾或延至实现期 contract test。

### 实现期 Contract Test 必须兜底的

1. **batch insert 写入性能**（G23）—— 1000 rows < 500ms 硬约束
2. **delegation 树查询索引命中**（G5/G21）—— EXPLAIN ANALYZE 必须走索引
3. **cross-tree artifact access**（G20）—— 所有兄弟场景集成测试
4. **cold-start hydration**（G24）—— 多 tab + 多设备 E2E
5. **manifest reason 枚举完整性**（G26）—— 生产 reason 必在枚举中，unknown 降级 + 告警
6. **auto-expiry SSE parity**（G25）—— 安全测试用例（Chromebook localStorage 清理）
7. **budget 公式边界**（G21）—— fan-out = 1, 3, 5, 7, 8, 10, 15 的属性测试
8. **retry_scope 字段传递**（G22）—— user say "redo everything" vs "redo just this" 两路径
9. **checkpoint sub-step resume**（G29）—— batch 执行到 50% pod kill 的恢复测试
10. **normalize_version NULL 处理**（G17+G27）—— 统一走 raw_v1 或 NOT NULL 迁移

## Final Decision

v0.2 **不适合直接发布**。建议走 **Sprint D** 消化 G20–G27 八条（4 critical + 4 high），然后再回归。G28/G29 可并入 Sprint D 末尾。剩余 low/implementation clarification 类目进入 Phase 1 实现期的 contract test 清单，不再作为 gap 追踪。
