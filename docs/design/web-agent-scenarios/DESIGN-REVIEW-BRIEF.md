# Design Review Brief — Web Agent Session State v0.3

> Audience: 负责 `web-agent-session-state.md` 设计的 agent / 工程师
> Goal: 基于 14 个场景压测和端到端 walkthrough 结果，推进设计文档从 v0.1 Draft 到 v0.3
> Expected output: v0.2/v0.3 patch（具体章节 diff）+ GAP-TRACKER 状态更新
> Estimated turns: 3-5 次 review 往返

## 背景

`docs/design/web-agent-session-state.md` (v0.1 Draft) 是 astra web agent 的 session 状态与上下文设计。该设计已用 14 个 stress-test 场景（S01–S14，覆盖超长开发、跨设备、断线续传、多 agent delegation、审批等待、超大工具输出、研究综述、个人 skill 等）反向压测，并经过端到端 walkthrough 聚合，提炼出 **29 条设计 gap**。

你的任务不是重新设计，而是**针对每条 gap 给出明确响应并推进到 resolved**。

## 必读材料

按阅读顺序：

1. **[docs/design/web-agent-scenarios/GAP-TRACKER.md](./GAP-TRACKER.md)**（核心，包含 29 条 gap 的全部字段）
2. **[docs/design/web-agent-scenarios/INDEX.md](./INDEX.md)**（场景总览 + 设计能力评估矩阵 + TOP 5 改进建议）
3. **[docs/design/web-agent-session-state.md](../web-agent-session-state.md)**（当前 v0.1 设计文档，需要你修改的目标）
4. **场景文档**（按需查阅，gap 条目已标出 `triggered_by` 和 `evidence_refs`）

## 工作流（每条 gap）

### Step 1. 理解 gap

读 GAP-TRACKER 对应条目：
- `title` / `severity` / `category`
- `triggered_by` → 打开对应场景的 §2（DAUT 压测方式）和 §8（对设计的反馈）
- `evidence_refs` 指出最关键的证据段落

### Step 2. 交叉对照当前设计

读 `design_section` 指向的当前设计文档章节：
- 当前设计**是否已隐式覆盖**？（经常设计文字上"提了"但没落到 schema）
- 证据 = 引用具体行号或表列

### Step 3. 决策

在 GAP-TRACKER 对应条目的 `response` 字段填入 JSON 块：

```json
{
  "decision": "accepted | rejected | deferred | need_clarify",
  "rationale": "一段话说明为什么",
  "evidence_in_current_doc": "若 rejected，指出当前设计哪里已覆盖（引行号）",
  "patch_plan": {
    "schema_changes": ["具体 DDL 片段"],
    "doc_sections": ["要新增/修改的章节标题"],
    "contract_rules": ["要添加的不变量/枚举/状态机"],
    "api_changes": ["新增/修改的 endpoint"]
  },
  "risk_notes": "若 accepted，实现时要注意的坑",
  "assigned_phase": "Phase 1|2|3|4|5|6 (设计文档 Implementation Plan 的哪一 phase)"
}
```

### Step 4. 产出 patch

对 `accepted` 的 gap，**直接修改** `docs/design/web-agent-session-state.md`：
- 加章节 / 加字段 / 加枚举 / 加 SQL assertion
- 每条 gap 用 `<!-- GAP-FIX: G<id> -->` HTML 注释标记改动起止，便于后续 review
- **保持向后兼容**：不要删除现有章节，若要替换，保留旧段并标 `DEPRECATED in v0.2/v0.3`

### Step 5. 状态回写

在 GAP-TRACKER 对应条目更新：
- `status`: `accepted` → 打 patch → `resolved`
- `resolution_commit`: patch 提交的 commit short sha 或章节的 v0.2 版本号（例 `§Compaction Invariants @v0.2`）

## 优先级建议

按严重度 + 依赖顺序：

### Sprint A（critical, 必须先做）
- **G15** Run durability `event_idx` 协议 — 产品上线硬阻塞
- **G2** Compaction Invariants 契约化 — 是 G6/G14 的前置
- **G4** Delegation 一等化 — 解锁 multi-agent，是 G5/G14 的前置

### Sprint B（high, 紧随其后）
- **G1** Manifest reason 枚举 — 快赢，几小时就能改完，立即提升调试可聚合性
- **G3** Retrieval SLA + 降级 state machine
- **G5** Plan Tree Rendering Policy
- **G6** 跨 session scope
- **G7** Approval 产品化
- **G8** Preview Template Registry
- **G9** Artifact retention 双向感知
- **G18** Delegation context zone 动态预算公式（Sprint A regression 新增）

### Sprint C（medium, 可并行）
- G10/G11/G12/G13/G14/G16/G17/G19

### Sprint D（end-to-end walkthrough critical/high）
- G20/G21/G22/G23/G24/G25/G26/G27
- G28/G29 可留到实现期 contract test 或后续 Sprint

## 判断 accept / reject 的检验标准

当你考虑 **reject** 某条 gap 时，必须同时证明：

1. **当前设计文档已显式覆盖**：引用具体行号，不能只说"隐式可以做到"
2. **触发该 gap 的场景实际不会压到此假设**：说明为什么场景是"误报"
3. **或者该 gap 属于实现层面而非设计层面**（可以 `defer` 到实现）

当你考虑 **defer** 时，必须说明：
- 延后到哪个 phase（设计文档 Implementation Plan Phase 1–6）
- 延后的副作用（哪些场景会在此期间失败）
- 替代方案（MVP 阶段如何规避）

## 禁止项

- ❌ 不要重新设计整个 session state 架构。场景已验证高层方向正确，gap 都是细节与契约
- ❌ 不要在一次回答里处理所有 29 条 gap。每次 3–5 条，保持注意力
- ❌ 不要只改文档不改 schema。多数 gap 需要同时动 schema + 文档 + 契约
- ❌ 不要为了减工作量把 critical gap 标 reject

## 验收标准

v0.2/v0.3 完成的标志：
- GAP-TRACKER 中所有 critical gap 状态 = `resolved`
- 所有 high gap 状态 = `resolved` 或 `deferred`（含理由）
- 每条 accepted gap 在 `web-agent-session-state.md` 有对应 `<!-- GAP-FIX: G<id> -->` 标记
- 所有改动保持向后兼容
- 设计文档底部新增对应 `## Changelog v0.2` / `## Changelog v0.3` 节，列出 resolved gap 与对应章节

## 回归验证（你做完 v0.2/v0.3 后）

我（或另一 sub-agent）会执行回归验证：
1. 重读 14 个场景的 §2 DAUT 与 §8 反馈
2. 对照 v0.2/v0.3 新设计
3. 填写 GAP-TRACKER.md 末尾的「设计能力评估矩阵 v0.2」
4. 若仍有场景未覆盖，生成新的 gap（G30...）开启下一轮

## 持久 TODO：设计到自动化测试

resolved / partially_resolved 的 gap 不能只停留在设计文档。进入实现阶段时，必须逐步固化成自动化测试，避免设计和实现漂移。

规则：

1. 每个 `resolved` 或 `partially_resolved` gap 在实现关闭前必须关联至少一类可执行测试：contract test、DB assertion test、HTTP/SSE integration test、UI cache test、或 restart/chaos test。
2. GAP-TRACKER 的 `residual_notes` 或后续 `test_plan` 字段必须记录测试落点；没有测试落点的 gap 不能视为实现完成。
3. 优先把 Sprint A 转成测试：
   - G15：run owner lease、`event_idx` 分配、idempotent input、graceful/crash restart、SSE replay/heartbeat。
   - G2：compaction invariants 对应 SQL assertions。
   - G4：delegation tree、run child vs child session、`last_summary_ref`、bounded parent context rendering。
4. Sprint B 的 resolved gap 进入实现前也要转成测试：manifest reason enum、retrieval state machine、plan tree rendering、cross-session memory、approval conditions/notifications、preview templates、artifact retention、delegation budget。
5. Sprint C 的 resolved gap 进入实现前也要转成测试：small-window budget、workspace reachability、next-action confidence、device lease/revision reconciliation、delegation retry/bubble-up、personal skill activation/evaluation、content hash normalization、IndexedDB watermark atomicity。
6. Sprint D 的 resolved gap 进入实现前也要转成测试：same-root-tree artifact ACL、delegation fan-out budget、retry_scope propagation、tool output batch insert、cold-start hydration、device lease expiry SSE parity、manifest reason/turn_intent、tool runner/raw_ref/normalize baseline。
7. G28/G29 暂留实现期 contract test 兜底；进入实现前必须明确测试文件或测试 issue。
8. 后续每轮 design patch 完成后，同步更新对应测试 TODO；不要把“后面再测”只留在对话里。

## 开始

按当前 Sprint 的 gap 列表执行。若是 Sprint D，先处理 critical G20-G23，再处理 high G24-G27；G28/G29 默认只补测试 TODO，除非本轮明确要求设计 patch。

读完材料后可以直接开始改文档 + 回填 GAP-TRACKER；若要 reject/defer，必须按本 brief 的检验标准写清楚证据。
