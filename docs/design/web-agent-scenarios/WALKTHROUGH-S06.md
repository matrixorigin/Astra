# WALKTHROUGH — S06 Nested Plan Compiler

> 对 `S06-nested-plan-compiler.md` 按 v0.2 设计做端到端运行推演。
> 关注：4 层 plan 树渲染策略、DAG 增量、3 次 compaction 骨架保全、T25 派 sub-agent codegen、T35 按树出报告。

## §1 场景标识与 v0.2 映射

- **一句话场景**：PL 博士用 3 周从零写 `lucid` 小语言编译器；5 phase × 3–7 section × 0–3 subtask 共 60+ todo，树深 4 层，40 回合，3 次 compaction，T25 派 1 个 child session 做 LLVM IR codegen。
- **被测 DAUT**：A6（compaction 保骨架）、A11（60+ todo 4 层树）、A13（delegation 一等关系）。
- **本场景最重要的 v0.2 章节/字段**：
  - §6 Plans and Todos / `session_todos` schema：L2203–2241（`parent_id` / `depth` / `path` / `backlog_pool_id` + `idx_todos_parent_status`）。
  - §Plan Tree Rendering Policy（G5）：L2247–2300（4 条渲染规则 + 祖先链 CTE + 子树 pending 查询 + 物化路径）。
  - §Delegation Contract（G4）：L1321–1426（`session_delegations` 表 + projection sync contract）。
  - §Delegation State Budget（G18）：L2442–2471（`per_child_budget = min(1200, max(200, floor(1500/active_children)))`）。
  - §Compaction Invariants（G2）：L2496–2596（8 条 + SQL assertion；invariant #3 禁止 compaction bump `plan_state.version` @L2511–2512）。
  - §Context Manifest Reason Enum（G1）：L1500–1519 已收录 `plan_subtree_query` / `tree_structured_report` / `partial_blocker_review`。

## §2 运行前提

- **Schema 初态**：Phase 2–6 schema 已落地：`agent_sessions` / `agent_runs`（含 `depth`/`ancestor_path`/`root_run_id`）/ `session_todos`（带 `depth`/`path`）/ `session_todo_deps` / `session_state_items` / `session_state_item_events` / `session_delegations` / `context_manifests`+`context_manifest_reason_types`+`context_manifest_items`。`agent_sessions` 新建 1 行 `sess-lucid`；`agent_runs` 创建 `run_T0` 作为首轮 root run；项目归到 `project_id=proj-lucid-compiler`（retention=`project_long_term`，子 session 继承）。
- **Workspace**：cloud workspace，tool 集合 = {`edit_file`, `read_file`, `write_file`, `run_cargo`, `grep`, `write_artifact`, `llvm_emit`（edge 占位，T1 未启用）}。`session_state_items(category='workspace_state')` 1 行；`reachability_probe.last_ok_at = t0`（G11，非本场景核心）。
- **Budget template**：非私有化部署，默认 `budget_v1_200k`（Claude Sonnet 4.5 类）。每 `context_manifests` 行写 `tokenizer_id='cl100k'` + `budget_template_id='budget_v1_200k'`（G10 的 8k 模板用不上，但字段仍然要填）。
- **Active skills**：`plt-pl-design` + `rust-compiler-engineering`，载入 `session_state_items(category='active_skill')` 各 1 行；per-session，依据 S06 §T1 的 900 token 预算。

## §3 端到端运行轨迹

下列 12 回合，每回合 3 列：`User 动作` / `Server behavior`（引 v0.2 行号）/ `User sees`。

---

### T1 · 顶层 plan 首次生成

| 列 | 内容 |
| --- | --- |
| **User 动作** | "把 lucid 编译器铺一个 5 阶段 plan，lexer/parser/typechecker/codegen/runtime，每阶段 3–7 section，section 下展开 subtask，先到第 3 层。" |
| **Server behavior** | 单事务：批量 INSERT 41 条 `session_todos` rows（5 phase parent_id=NULL depth=0；24 section depth=1；12 subtask depth=2），每行写 `path='lexer/tokenizer'` 等物化路径（L2212、L2297）；全部 `status='pending'` `source='plan_seed'`。INSERT 1 条 `session_state_items(category='plan_state', version=1, payload_json=<phase→section 骨架>)`；写 `session_state_item_events`（mutation='insert'，provenance_event_id=本轮 event id，previous_version=NULL next_version=1，L2550 口径）。写 `context_manifests(reason='initial_turn', budget_template_id='budget_v1_200k', tokenizer_id='cl100k', total_estimated_tokens≈5100, dropped_count=0)` + 41 行 `context_manifest_items`（其中 36 条 `included=FALSE reason='zone_budget'` 属于 subtask / 非首 phase section）。 |
| **User sees** | SSE：`state.todos.created` ×41 批次事件；UI plan panel 渲染 5 phase 展开状态，lexer 首屏展开 section + subtask；右栏"上下文清单"显示 `reason=initial_turn`、plan_todo zone 800 token。 |

---

### T5 · lexer 深入开发

| 列 | 内容 |
| --- | --- |
| **User 动作** | "token 类型分九类 Ident/Int/Float/Str/Op/Keyword/LParen/RParen/Layout，把 tokenizer 下面那几条 subtask 拆细。" |
| **Server behavior** | UPDATE 3 条既有 subtask `description`；INSERT 2 条新 subtask（`layout-sensitive-newline`/`op-classification`，parent_id=tokenizer_todo_id，position=4/5，depth=2，path=`lexer/tokenizer/…`）。UPDATE `lexer.tokenizer` 自身 `status='in_progress'`。`session_state_item_events` 7 条 mutation：3 `update`（subtask 描述）+ 2 `insert`（新子）+ 1 `update`（tokenizer 状态）+ 1 `update`（provenance event）。`plan_state.version` 不 bump（骨架未变）。渲染侧走 G5：以当前子 `layout-sensitive-newline` 为 `:current_todo_id` 跑 L2269–2283 的祖先链 CTE（3 行：lexer→tokenizer→当前）；以 `:section_todo_id=lexer.tokenizer` 跑 L2287–2294 的子树 pending 查询（5 行）。`plan_todo` zone≈1000 token。 |
| **User sees** | plan panel 高亮 `lexer.tokenizer`（in_progress），5 条 subtask 展开；上下文清单显示 `reason=normal_turn`，plan_todo 1000/recent_tail 1200。 |

---

### T10 · "给我看 typechecker 那块还没动的子任务"（跨子树查询）

| 列 | 内容 |
| --- | --- |
| **User 动作** | "跳一下——typechecker 那块还没动吧？把那整棵子树里 pending 的 subtask 给我看一下，我想先对 HM 实现顺序。" |
| **Server behavior** | 这是 **explicit API**（G5 rule 4 @L2262–2263）：前端发 `GET /sessions/sess-lucid/plan/subtree?root=typechecker&filter=pending,blocked`。后端按 L2287–2294 的"当前子树 pending"SQL 变形为**递归 CTE**（L2297 fallback 路径）：用 `idx_todos_parent_status(session_id, parent_id, status, position)` L2228 逐层下探 3 层，直到叶子。返回 17 行（6 section + 11 pending subtask）。预计 100ms 内命中索引，17 行返回。同时本轮 LLM turn 的 `context_manifests.reason='plan_subtree_query'`（L1511 已枚举），`total_estimated_tokens≈5700`，`dropped_count=24`（其他 phase 所有非 typechecker 的 active todo 全部 `included=FALSE reason='out_of_scope_phase'`，**v0.2 未枚举这个 drop_reason 字符串，但 `context_manifest_items.reason` 是 VARCHAR(128) 字段（L1486）允许实现自定义**，建议登记）。`plan_todo` zone 破例扩到 2200，同时 `recent_tail` 压到 1000 借出 200 token。不写入 `session_todos`。 |
| **User sees** | UI 右栏"plan subtree drawer"滑出：typechecker/6 section/11 pending subtask 展示。SSE `manifest.assembled reason=plan_subtree_query`。工程师第一眼能看懂——因为祖先链 + 兄弟 section 的 title 都渲染了。**但**：如果前端没把 subtree drawer 和主 plan panel 做视觉区分，用户可能以为"主 plan 已经切到 typechecker"。 |

---

### T15 · parser 中段插入 pratt 子 plan

| 列 | 内容 |
| --- | --- |
| **User 动作** | "开始写表达式 parser 之前，在 parser.expr 下面开一个 pratt-precedence section，里面放 binding-power/led/nud/right-assoc-tests 4 条 subtask。" |
| **Server behavior** | 单事务：INSERT 1 条 section（parent_id=parser.expr, depth=1, position=3, path=`parser/expr/pratt-precedence`——注意 depth=1 是因为 section 本就 depth=1，这里是 `parser.expr` 的**兄弟 section** 按 S06 §T15 描述——actually S06 写 "section `parser.pratt-precedence` (parent_id=parser.expr, position=3)"，**这与 S06 §5 T15 快照 "plan 树最大深度=3" 矛盾**——若 pratt 是 expr 的 child，则深度就是 4）。按 S06 字面 parent=`parser.expr`，depth=2 section；pratt 下 4 subtask depth=3。INSERT 4 条 subtask（parent=pratt-precedence）。UPDATE `plan_state.version 1→2`，`session_state_item_events` 写一行 mutation='update' with `previous_version=1 next_version=2`（L2550 便于 G2 invariant #3 assertion），`payload_json` 的 parser section list 里新增 `pratt-precedence`。INSERT 1 条 `session_state_items(category='decision', item_key='parser.pratt.choice', payload_json=<Pratt 选型理由>)`。渲染走 G5：当前路径 `parser→expr→pratt-precedence`（祖先链 3 行）+ 兄弟 section 6 条（title+status，L2261）。**L2228 索引 `idx_todos_parent_status` 能精确命中 `(session_id, parent_id=parser.expr, status, position)` 扫描**。 |
| **User sees** | plan panel 刷新：parser.expr 下多了一层 pratt-precedence 节点及 4 subtask。UI 侧边栏显示 decision "为什么 Pratt 而非 shunting yard"。plan_todo 1300/recent_tail 1600。 |

---

### T20 · type checker 回溯改 parser.ast-design（done → in_progress）

| 列 | 内容 |
| --- | --- |
| **User 动作** | "AST 没有 Variant / Pattern::Constructor 节点，回去给 parser.ast-design 补 3 条 subtask，typechecker 的 adt 相关 subtask 标 blocked。" |
| **Server behavior** | 单事务：UPDATE `parser.ast-design` 自身 `status: done → in_progress`（G14 enum `update`，L1156–1162 已列）；**这里不是 `replace`**——`replace` 语义是整行 payload 整体替换（G14 enum），状态回退用 `update` 即可。INSERT 3 条 subtask（`add-variant-node` / `add-constructor-pattern` / `update-ast-visitor`，parent=parser.ast-design，position=append-tail）。UPDATE 3 条 typechecker 下 subtask `status='blocked'`。INSERT 3 行 `session_todo_deps(todo_id=typechecker.adt-inference, depends_on=add-variant-node/…)`（L2232–2240）。INSERT 1 条 `decision`（"回溯补 Variant 节点"）。`plan_state.version` **不 bump**——payload_json 的 phase→section 两层骨架未变（subtask 增删不改骨架，这是 S06 隐含的 plan_state 粒度契约；**v0.2 未显式定义何时 bump**，是隐含 friction）。渲染走 G5：当前路径临时切 `parser→ast-design`，祖先链 + 3 条 active subtask 全渲染。**问题**：typechecker.adt-inference 处在 parser 子树之外，按 G5 rule 3 作为兄弟 phase 只渲染 title + status。实际业务上用户需要看到 blocked 链（"我是被哪三条 parser 新增任务 block 的"），G5 未规定 blocked 子项的冒泡渲染——**friction F2**。 |
| **User sees** | plan panel 显示 parser.ast-design 重新变黄（in_progress）+ 3 新子 + 依赖线；typechecker.adt-inference 显示红色 blocked 徽章。但 prompt 侧 LLM 只拿到 title + status，不带 blocker reference 字段。 |

---

### T22 · 第一次 compaction

| 列 | 内容 |
| --- | --- |
| **User 动作** | "parser 补的 3 条走完了，我去吃饭，压一下前面。" |
| **Server behavior** | 触发 compaction job。先跑 G2 invariant #4 前置：`SELECT COUNT(*) FROM agent_runs WHERE session_id AND status IN ('running','waiting')`，必须=0（L2554–2557）。之后：INSERT 1 行 `session_state_items(category='summary', version=1, token_estimate≈900)`，覆盖 T1–T20 narrative。**骨架保全**：G2 invariant #2（L2507–2510）保证 `plan_state`/4 条 `decision`/active `delegation_state`（本轮还没建）绝不被 mutation='replace'/'archive'/'delete'。完成的 14 条 lexer subtask UPDATE `status='archived'`，G2 invariant #5（L2516–2517）要求保留 provenance。`session_transcript_items` 把 T2–T18 的 23 条 tool_call/tool_output 标 `is_compacted=TRUE`（不物理删，只改投影）。写 `context_manifests(reason='post_compaction', compaction_version='v1.0', dropped_count=23)`（L2518–2520 强制）。跑 G2 的 8 个 assertion SQL（L2529–2591），全返回 0（`post_compaction_manifest_count` 返回 1）。 |
| **User sees** | UI 显示"会话已压缩：14 条 subtask 归档、23 条 tool output 折叠"；plan panel 的骨架没动，lexer 节点变灰色显示 "14/14 done"。上下文清单新增一行 `reason=post_compaction`。 |

---

### T25 · codegen 派 sub-agent（delegation depth=1）

| 列 | 内容 |
| --- | --- |
| **User 动作** | "codegen 派一个 sub-agent 专做 LLVM IR，只对 codegen.ir-emission 这一 section 负责，输入是 typed AST，输出是 .ll + IR 片段。" |
| **Server behavior** | 单事务：(a) 创建 child `agent_sessions(session_id='sess-lucid-codegen-llvm', project_id=proj-lucid-compiler)`；(b) 创建 child `agent_runs(run_id='run_child_T25', parent_run_id='run_T25', root_run_id='run_T0', ancestor_path='run_T0/run_T25/run_child_T25', depth=1, trigger_type='delegation')`（L902–907 主表已含这些列）；(c) INSERT 1 行 `session_delegations(delegation_id, session_id=父 sess, child_session_id='sess-lucid-codegen-llvm', child_run_id='run_child_T25', parent_run_id='run_T25', root_run_id='run_T0', ancestor_path='run_T0/run_T25/run_child_T25', depth=1, status='running', phase='ir-lowering', directive='LLVM IR emission only', last_summary_token_estimate=0)`（表 L1327–1358）；(d) INSERT 1 条 `session_state_items(category='delegation_state', item_key='delegation:<delegation_id>', payload_json=<G4 JSON shape L1384–1403>)`——按 G4 projection sync contract（L1411–1425）这两行必须同事务；(e) 父 session 的 `codegen.ir-emission` section UPDATE `status='delegated'`（**v0.2 `session_todos.status` 枚举未列 delegated，S06 表述隐含——friction F3**）。G18 预算（L2451）：active_children=1 → per_child_budget = min(1200, max(200, floor(1500/1))) = 1200；但此轮 child 尚无 summary，`last_summary_token_estimate=0`，实际 `delegation_state` zone 渲染 ≈200 token（directive + status + phase + scope）。child session 另起独立 turn，用自己的 initial_turn manifest 生成 5 条 todo（不回推父 session 的 session_todos）。 |
| **User sees** | 左侧 chat 列表新增一个 session "lucid-codegen-llvm"（因 `child_session_id` 非 NULL，UI 可见）；父 session 的 plan panel 把 `codegen.ir-emission` 整节折叠为"→ delegated (sess-lucid-codegen-llvm)"引用；SSE `delegation.started`。 |

---

### T28 · sub-agent 出中间 artifact，父集成

| 列 | 内容 |
| --- | --- |
| **User 动作** | "child 那边第一版 lucid.ll 和 fragment 出来了吧？拿回来看，顺便把 runtime.call-abi unblock。" |
| **Server behavior** | child session 独立跑 T1–T12，期间它自己触发过 1 次 L1b summary，写 `session_state_items(category='summary')`（child scope）token_estimate≈350。child 把 `artifact:lucid.ll-v0.3` + 3 `ir-fragments` 写入 `session_artifacts(access_scope='delegation', project_id=proj-lucid-compiler)`（G9 artifact scope，参见 §Artifact Access Scope）。父 session 侧：UPDATE `session_delegations` row：`last_summary_ref='state_summary_child_L1b_v1'`、`last_summary_token_estimate=350`、`exposed_artifacts_json=[artifact:lucid.ll-v0.3, ir-frag-1…3]`、`phase='progress'`、`last_child_turn_idx=12`、`updated_at=now`；**同一事务** UPSERT `session_state_items(category='delegation_state', item_key='delegation:<del_id>')`，payload_json 追加 `last_handoff_at/child_summary_text/child_artifact_refs[]/status='progress'`（L1411–1417 projection sync contract）。`session_state_item_events` mutation='update' 1 行。父 UPDATE `runtime.call-abi` subtask `status='pending'`（unblock），DELETE 对应 1 行 `session_todo_deps`。G18 预算：active_children 仍=1，per_child_budget=1200；当前 summary=350 tokens < 1200，按 L2460–2461 "Pre-check last_summary_token_estimate" 直接放入 zone，实际占 550（summary 350 + artifact_refs 列表 + title/phase/status）。 |
| **User sees** | 父 session chat 流里 agent 回答引用 `artifact:lucid.ll-v0.3` 的 preview；UI 文件面板可点开 artifact。plan panel runtime.call-abi 变黄。 |

---

### T30 · 第二次 compaction

| 列 | 内容 |
| --- | --- |
| **User 动作** | "runtime 前两段走完了，再压一下。child 那边自己跑着。" |
| **Server behavior** | 同 T22 流程，但这次有 active `delegation_state`（status='progress'）。G2 invariant #2（L2507–2510）**显式保护 active delegation_state**——不能 replace/archive。实现上：compactor 先 query `session_state_items WHERE category IN ('delegation_state','plan_state','decision') AND status='active'` 建白名单，再跑 summary 生成，最后 assertion 核验。已完成 subtask 从 17 累加到 31 条，全部 UPDATE `status='archived'`（G2 invariant #5）。写 summary v2 token≈1000，同时把 summary v1 UPDATE `status='archived'`（G2 invariant #8 禁 delete）。manifest reason='post_compaction', compaction_version='v1.1', dropped_count=31。**关键 friction**：G2 invariant #3（L2543–2551）要求 `plan_state` 在 compaction 轮次 `previous_version==next_version`。T30 不改 plan_state，满足。 |
| **User sees** | UI 再次提示"已压缩"；delegation 指示灯仍为绿色（child 跑着，父没阻塞）。 |

---

### T35 · 按 plan 树结构出总结报告

| 列 | 内容 |
| --- | --- |
| **User 动作** | "最后写文章附录了——按 plan 树结构给我一份总结报告：每个 phase→section 下的 decision + rationale，child 出的 IR artifact 在 codegen 那节列。别写成 changelog，我要树。" |
| **Server behavior** | 本轮 reason='tree_structured_report'（L1512 已枚举）。context builder 做 3 次结构化查询：(1) `session_todos` 全树（WHERE session_id=? ORDER BY path ASC），返回 68 行，按 `path` 字段（L2212 物化路径）天然层级排序，**避免递归 CTE**；(2) `session_state_items WHERE category IN ('decision','delegation_state') AND scope='session'` 8+1=9 行；(3) `session_artifacts WHERE session_id=? OR (project_id=proj-lucid-compiler AND access_scope='delegation' AND exposed_to_parent=TRUE)` 45 行。应用层把 decision/artifact 按 `item_key` 前缀（如 `parser.pratt.*` `codegen.ir.*`）挂到对应 section node 下——**这个前缀约定在 v0.2 未显式规定**（friction F4）。`retrieved_facts` zone 破例扩到 4500 token。manifest dropped_count=0；**新 reason**已枚举。agent 按树结构生成报告（不是 transcript 回放）。 |
| **User sees** | chat 流里 agent 输出一份结构化 Markdown 树（phase→section→decision→artifact），工程师能看懂。UI manifest 面板显示 `reason=tree_structured_report, retrieved_facts=4500`。 |

---

### T38 · 第三次 compaction

| 列 | 内容 |
| --- | --- |
| **User 动作** | "差不多了，最后收尾前再压一次。" |
| **Server behavior** | child session 此时已 completed，父 session 的 `session_delegations.status='closed'`，同事务 `session_state_items(category='delegation_state').status='archived'`（G4 projection sync L1420–1421）。注意：**compactor 执行时 invariant #2 只保护 `status='active/in_progress/blocked/waiting'` 的 delegation_state（L2540）**——此时 delegation 已是 closed/archived，可被压进 summary。但 S06 期望"保留 final handoff 摘要"——compactor 允许保留 archived 项的 `payload_json`，只是不再占 prompt 预算。`summary` v3 token≈1100；archived subtask 从 31 升到 59；decision 达 8 条全部 active 保留（L2508）。 |
| **User sees** | 最终 compaction 提示；plan panel 除 3 条交付 todo 外全灰。 |

---

### T40 · 交付

| 列 | 内容 |
| --- | --- |
| **User 动作** | "定稿作为附录。代码打 tag v0.1.0-lucid。" |
| **Server behavior** | 3 条交付 todo UPDATE `status='done'`。INSERT 1 条 `session_state_items(category='summary', scope='session', status='active', version=final)` + 1 条 `session_state_items(category='anchor', item_key='delivery', payload_json=<v0.1.0-lucid>)`。`reason='final_delivery_summary'`（G1 enum L1501–1519 包含此 reason，由 changelog 注记 L3021 确认）。artifact `lucid.ll-v0.3` 被 `session_state_items(category='citation')` 引用，`session_artifacts.referenced_by_state_items_count++`（G9）。 |
| **User sees** | 最终报告；UI 显示"delivered v0.1.0-lucid"徽章；chat 列表该 session 变为 completed。 |

---

## §4 Frictions

按严重度排序。

### F1 · G5 "current-section pending subtree query" SQL 只能查 1 层 parent_id，T10 需要的是跨 section 递归子孙查询
- **Ref**：T10；v0.2 L2285–2295
- **Observation**：G5 给的 pending-subtree SQL（L2287–2294）条件是 `parent_id = :section_todo_id`，即**只查当前 section 的直接 child**；而 T10 的真实需求是"typechecker 根节点下所有层级的 pending subtask"，深度 3—4，跨 6 个 section。实际必须走 L2297 的递归 CTE fallback。
- **Why it's friction**：文档把"祖先链"和"当前 section subtree"写成两条 SQL，唯独没给出"**任意深度的子孙 pending 过滤**"这条 T10 最常用的第三种形态。工程师要么 copy-paste 改 CTE，要么走物化 path 做 `WHERE path LIKE 'typechecker/%' AND status IN ('pending','blocked')`——后者更高效但文档没明确授权。
- **Severity**：`notable`

### F2 · Blocked 跨子树节点在 plan_todo zone 的渲染规则未定义
- **Ref**：T20；v0.2 L2256–2266
- **Observation**：T20 之后 `typechecker.adt-inference` 3 条 blocked subtask 处在**非当前路径**的兄弟 phase 下。G5 rule 3（L2261）说兄弟 section "title + status only"，rule 2 只管当前 section。结果 LLM prompt 里只知道 typechecker 有一条 blocked 状态，但看不到 **block 原因链**（blocked-by parser.ast-design 三条新增 subtask）。
- **Why it's friction**：实现者要么偷偷把 blocker 链塞 plan_todo（破坏 G5 预算契约），要么塞 `retrieved_facts`（污染检索 zone 语义），要么塞 recent_tail（无语义）。agent 可能因此在 T21–T23 期间忘掉自己 block 在哪。用户侧在 UI 有可视化，但 prompt 缺失。
- **Severity**：`notable`

### F3 · `session_todos.status` 枚举未列 `delegated`，T25 侧的折叠渲染靠实现约定
- **Ref**：T25；v0.2 L2216 `status VARCHAR(32) NOT NULL`
- **Observation**：v0.2 未显式枚举 `session_todos.status` 允许取值（schema 只写 VARCHAR(32)），S06 §T25 要求把 `codegen.ir-emission` section 标成"已 delegated 到 child"。实现可能 (a) 新增 status='delegated'（污染 G2 invariant #2 的 active 白名单 L2540 要不要包含它？）或 (b) 保持 in_progress + 靠 `session_state_items(category='delegation_state').payload_json.scope_section` 反向引用（当前方案）。两种选择都合理但没有统一约定。
- **Why it's friction**：G2 L2540 "active delegation_state"有显式状态名单 `('active','in_progress','blocked','waiting')`，若补 delegated 需同步修订。实现团队会在 review 时反复拉锯。
- **Severity**：`notable`

### F4 · T35 按 plan 树报告需把 `session_state_items.item_key` 作为"section 锚点"前缀匹配，v0.2 无此约定
- **Ref**：T35；v0.2 §6 + §State Projection
- **Observation**：S06 §T35 要把 6 条 decision 和 1 条 delegation_state 归并到对应 section（比如 `parser.pratt.choice` 挂到 `parser.pratt-precedence` section）。这隐含约定：`item_key` 使用 `<phase>.<section>.<detail>` 点分前缀，以便应用层 SQL `WHERE item_key LIKE 'parser.%'` 归并。v0.2 §Session State Projection 没有规定 `item_key` 编码模式，只说"primary identifier"。
- **Why it's friction**：不同团队/不同 skill 拼 item_key 的方式会不一致，T35 的 join 就会漏掉。如果项目想把 decision 与 `session_todos.todo_id` 硬绑（而非前缀），应在 `session_state_items` 增一列 `linked_todo_id`。
- **Severity**：`notable`（触发概率高，影响 T35 报告正确性）

### F5 · `plan_state.version` bump 触发条件未在非 compaction 路径说明
- **Ref**：T15（bump）/ T20（不 bump）/ T28（不 bump）；v0.2 L2510–2512
- **Observation**：G2 invariant #3 明确 compaction 不可 bump，但**正向语义**"何时 SHOULD bump"没写。场景约定：phase 或 section 集合变化 bump，subtask 增删不 bump。但 T15 加 section bump 到 2，T20 给 section 追加 subtask 不 bump，这个"section 是骨架，subtask 不是"的判定在 v0.2 找不到根据。S06 隐含此约定。
- **Why it's friction**：不同开发者读同一文档会写出不同语义。如果 subtask 增也被错 bump，G2 assertion 不会报错（只管 compaction 轮次），但 T22 compaction 之后的 plan 骨架投影会漂移。
- **Severity**：`notable`

### F6 · delegation 建立和 child session 创建的 trigger 顺序无契约
- **Ref**：T25；v0.2 §G4 L1411–1425
- **Observation**：G4 要求 `session_delegations` 和 `session_state_items(delegation_state)` 同事务写。但 child `agent_sessions` 的创建是另一个"业务事务"。实际 T25 必须：(a) 父事务先写 `agent_sessions(child)` → (b) 再写 `agent_runs(child with parent_run_id)` → (c) 写 `session_delegations + state_item`。如果 (a)(b) 成功、(c) 失败，child session 成了孤儿。v0.2 没给出补偿或"从 session_delegations 回填"的约定。
- **Why it's friction**：实现要么 (a)(b)(c) 同事务（跨表压力）要么加 reconciliation job。G4 projection sync 只覆盖 (c) 内两表。
- **Severity**：`minor`（corner case，但生产会遇到）

### F7 · `budget_v1_200k` 实际并未在 v0.2 命名
- **Ref**：T1–T40 全程；v0.2 §Budget Policy v1 L2381–2406；§Small-Window L2408–2438
- **Observation**：文档只定义 `budget_v1_8k`（小窗口，L2414）。大窗口模板只给 Zone 表（L2385），**没有命名的 template id**。但 `context_manifests.budget_template_id` 是 NOT NULL 友好字段（L1450 默认 NULL 但生产必填）。实现必须自拟字符串，不同部署产出不可聚合。
- **Severity**：`minor`

---

## §5 新 gap 候选

### G20（候选）· `session_todos` 子孙递归查询的 SQL 规范未定义
- **triggered_by_turn**: T10
- **related_v0.2_section**: §Plan Tree Rendering Policy L2285–2298
- **severity**: medium
- **rationale**: G5 给的两条 SQL 解决"祖先链"和"单级 subtree"。跨层的"任意深度子孙 + status 过滤"——也就是 `GET /sessions/{id}/plan/subtree?root=...` 后端 SQL 本质——既没给递归 CTE 模板，也没给 `path LIKE` 模板，导致实现各家不同。T10 / T35 都落在这条路径上。
- **proposed_fix**: 在 G5 后补一段第 3 条 SQL：`SELECT … FROM session_todos WHERE session_id=? AND path LIKE ?||'/%' AND status IN (?,…) ORDER BY path ASC`，并说明 `path` 物化路径维护触发器（每次 insert/move 重算子树）。

### G21（候选）· `session_state_items.item_key` 命名/前缀约定缺
- **triggered_by_turn**: T15 / T35
- **related_v0.2_section**: §Proposed Schema §2 Session State Projection
- **severity**: medium
- **rationale**: T35 按 plan 树汇总 decision 和 delegation_state 时必须靠 `item_key` 前缀匹配回 section。v0.2 未规定 item_key 的编码规则（自由文本 vs 点分 vs 关联 todo_id）。跨 session 的 user-scope item（G6）也会撞同一问题。
- **proposed_fix**: 引入可选列 `session_state_items.linked_todo_id VARCHAR(128) NULL`（指向 `session_todos.todo_id`）并规定 decision/delegation_state 必须关联；或在文档明确 `item_key` 形如 `<phase>.<section>[.<detail>]`。

（T20 的"blocked 跨子树渲染"不列新 gap——它是 G5 规则 2/3 的自然盲区，可在 G5 加一条规则 5"blocked todo 若被当前路径 todo 依赖，则即使不在当前 section 也要渲染其 title+blocker_reason"。作为 G5 追加条款，不是新 gap。）

（T25 的 `session_todos.status='delegated'` 枚举问题属 friction F3，可在 G14 mutation enum 同级补一个 `session_todos.status_enum`，作为 G14 的延伸，不单列新 gap。）

---

## §6 Gap 组合效应检验

### 组合 A：G5 + G2（compaction 后 plan 树裁剪）
- **运行时**：T22/T30/T38 三次 compaction 前后，G5 的 plan_todo zone 渲染依赖 `session_todos.status!='archived'`（L2260）；G2 invariant #5（L2516）把 done subtask 标 archived 来减压；G2 invariant #3（L2511）保 `plan_state.version` 不 bump。联动效果：compaction 后，done 子树自然从 G5 渲染路径消失，plan 骨架经 `plan_state.payload_json` 由 anchor zone 独立渲染。典型数值：T22 前 plan_todo 1500 → T22 后 1100（14 条 archived 掉出 render，summary 补 900）。
- **v0.2 是否覆盖**：yes。
- **潜在坑**：archived done subtask 被冒险"反悔"（T20 典型：`parser.ast-design` done 后又要重启，但 S06 把它保持 done→in_progress，不走 archived）——若实现激进地把 done subtask 立即 archived，T20 类反悔会触发 `status: archived → in_progress` 逆转，G2 invariant #5 的 "provenance 必须保留" 能护住，但 agent prompt 可能因 G5 过滤不再看到这个 todo。**建议**：G5 加一条"archived-to-active 逆转时必须临时重入当前路径渲染"。

### 组合 B：G5 + G14（T20 parser AST 回溯的 mutation 选择）
- **运行时**：T20 把 `parser.ast-design` done 回退为 in_progress，3 条新 subtask 挂上去。G14 mutation enum（L1156–1162）允许 `update` / `archive` / `replace`。选择：
  - 对 `parser.ast-design` 自身：mutation='update'（状态字段变），不是 `replace`（payload 整体替换，语义过重）也不是 `archive`（它要复活，不能归档）。
  - 对 3 条新 subtask：mutation='insert'。
  - 对 3 条 typechecker blocked subtask：mutation='update'（status: pending → blocked）。
- **v0.2 是否覆盖**：partial。G14 enum 列举了动作，但"done 反悔应走 update 而非 replace/archive"这个 **rollback 语义** 没在文档写明，靠工程师判断。
- **建议**：在 G14 的 mutation enum 后补一段"Rollback semantics"，列举常见 rollback 场景映射到哪个 mutation（done→in_progress = update；already compacted summary rollback = new summary version + archive old；etc）。

### 组合 C：G4 + G18（T25 sub-agent 派出的预算）
- **运行时**：T25 建 1 个 child（active_children=1），G18 公式（L2451）`per_child_budget = min(1200, max(200, floor(1500/1))) = 1200`。T25 此轮尚无 child summary，`last_summary_token_estimate=0`，实际 delegation_state zone 渲染仅 title+phase+status+directive+scope ≈200 token（与 S06 §T25 表格的 200 token 吻合）。T28 child summary=350 → zone 占 550（仍 < 1200 上限，OK）。T30 child 继续跑，L2465–2467 "若 child 有 active blocker 可借 recent_tail 用 2×per_child_budget"——本场景 child 无 blocker，不走这条。
- **v0.2 是否覆盖**：yes。数值完全符合 G18 公式。
- **潜在坑**：S06 场景始终 active_children=1，G18 公式在 1 child 情形退化成"min(1200, max(200, 1500)) = 1200"——上限其实比总 cap 1500 小 300 token（"预留"给 meta 字段）。但文档没解释这 300 token 预留给谁（title/directive 头信息）。**不建议新 gap**——friction 级。

### 组合 D：G5 + G1（tree_structured_report 查询效率）
- **运行时**：T35 `reason='tree_structured_report'`（G1 enum L1512）。后端要对 68 行 `session_todos` 做 `ORDER BY path ASC`（利用 G5 的物化 path 字段 L2212），比递归 CTE 省 5× 时间。配合 `session_state_items WHERE category IN ('decision','delegation_state')` 9 行 + `session_artifacts` 45 行。3 次查询并行。索引使用：`idx_todos_session_position`（L2225 按 `(session_id, position)`）对按 path 的排序不是最优——应有 `INDEX (session_id, path)`。**缺索引** → full table scan 68 行，数据量小暂时可接受，但场景 S14 类 300 todo 就会慢。
- **v0.2 是否覆盖**：partial。G5 L2297 提到"path 用于 bounded subtree scan"但没在 schema 增对应索引。
- **建议**：给 `session_todos` 补 `INDEX idx_todos_session_path (session_id, path)`；或把 T35 类全树查询拆成 phase 粒度并行。

---

## §7 Token walkthrough：T22（compaction）与 T23（下一轮）

### T22 · 第一次 compaction 的 manifest 账本

`reason='post_compaction'`，`compaction_version='v1.0'`，`tokenizer_id='cl100k'`，`budget_template_id='budget_v1_200k'`。

| Zone | Token | 内容来源 | 预算依据 |
| --- | ---: | --- | --- |
| system_static | 1200 | 固定 system prompt | 稳定前缀 cache（L2387） |
| tool_schemas | 1600 | `edit/read/write/run_cargo/grep/write_artifact` 的 schema（llvm-tools 为 edge placeholder，occupy ~200） | 按 active-agent/skill 修剪（L2388） |
| skills | 900 | `plt-pl-design` + `rust-compiler-engineering` 两个 skill 文本 | 默认 900（session-scope active skill） |
| session_anchor | 400 | session_id/user/project/workspace 摘要 | hard cap（L2389） |
| plan_todo | 1100 | 祖先链 `parser→ast-design`（3 行）+ 新 unblock 的 `typechecker.adt-inference` 子树（3 subtask）+ 兄弟 section title（14 条，每条 30 token） | 取自 G5 渲染策略 |
| recent_tail | 1600 | 最近 2 轮（T21 mid-edit + T22 compaction 指令） | L2391 |
| summary | 900 | 新 L1b summary v1（T1–T20 narrative） | G2 invariant #6 必写 |
| retrieved_facts | 300 | T15 AST decision 原文引用（Pratt 决策对 typechecker 未来有用） | 按相关性 |
| tool_previews | 0 | compaction 轮不塞 tool preview | 0 |
| delegation_state | 0 | T25 尚未触发 | 0 |
| workspace | 200 | cloud workspace 元数据 | L2392 |
| **合计** | **8200** | | |
| reserved_output | 8000 | 留给 LLM 响应 | L2400 |
| safety_buffer | 500 | — | — |
| **grand total** | **16700** | 预算 200k，占用率 8.3% | 远低于 cap |

`dropped_count=23`：T2–T18 期间 23 条 tool_call/tool_output rows 被标 `is_compacted=TRUE`，对应 `context_manifest_items` 23 行 `included=FALSE reason='compacted_tool_output'`（估计节省 ≈14k token）。14 条 lexer subtask 归档 `reason='archived_subtask'`（估计节省 ≈1200 token）。

### T23 · compaction 后下一轮 "继续推 typechecker.adt-inference" 的 manifest 账本

`reason='normal_turn'`（G1 enum L1505），非 compaction 轮。

| Zone | Token | 与 T22 差异 |
| --- | ---: | --- |
| system_static | 1200 | = |
| tool_schemas | 1600 | = |
| skills | 900 | = |
| session_anchor | 400 | = |
| plan_todo | 1000 | 当前路径切 `typechecker→adt-inference`，祖先链 3 行 + 当前 section 4 条 subtask + typechecker 兄弟 section 5 条 title |
| recent_tail | 2000 | T22 summary + 本轮 user prompt + 前一轮 agent 响应 |
| summary | 900 | v1 summary（active） |
| retrieved_facts | 400 | 按需引用 T8 typechecker 初始 decision |
| tool_previews | 300 | 最近 1 个 cargo run 输出 preview |
| delegation_state | 0 | 仍未触发 |
| workspace | 200 | = |
| **合计** | **8900** | vs T22 增 700（主要 recent_tail +400/tool_previews +300） |

**关键点 1**：compaction 是 token 净节省——没有 T22 的话，T23 的 recent_tail 会试图塞 T21–T22 旧对话（约 3500 token），现在只塞 900 token 的 summary+基础元。实际收益：`3500 - (900summary + 800recent_tail delta) ≈ 1800 token/轮`，连续 10 轮累积约 18k token。

**关键点 2**：如果 T23 触发跨子树查询（`reason='plan_subtree_query'`），plan_todo 临时扩到 2200，recent_tail 压到 1000（借 200 token），合计仍 ≤ 9500 tokens，离 cap 很远。

**关键点 3**：T22 post-compaction manifest 的 `included=FALSE` 行（23+14=37 行）必须全部登记到 `context_manifest_items` 供 G1/G2 可观测。实现上若偷懒把 "is_compacted=TRUE 的 row 统一以 1 行 summary 代替"会破坏 `dropped_count` 字段准确性。

---

## §8 最终判断

- **本场景下 v0.2 是否可实现**：✅（有 5 条 notable friction，2 条 medium 新 gap 候选，但无 blocking）。
- **顺畅度**：⚠️ 有明显摩擦。plan 树渲染策略 G5 在"深度递归查询 SQL"和"blocked 跨子树渲染"两处留了实现空间，工程师第一次落地会反复拉锯。compaction 三段式 + delegation 预算公式在数值上配合良好。
- **一句话总结**：v0.2 的 G5+G2+G4+G18 组合把 S06 的"60+ todo × 4 层 × 3 次 compaction × 派 sub-agent × 按树报告"完整压到 12k token 以内（T35 报告轮次），结构化投影查询替代 transcript 回放省下 88% token；但 G5 策略当前只回答了 2/3 的树查询形态，递归子孙查询与 blocked 反向链渲染需要在下一轮设计补齐。
