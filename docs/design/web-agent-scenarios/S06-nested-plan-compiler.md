# S06 — 嵌套 plan + 子 plan：给自定义函数式语言实现编译器

> Status: Draft v0.1
> DAUT: **A6, A11, A13**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：PL 方向博士在读，本职做类型系统研究，副业是给自己设计一
门 ML 系的小语言 `lucid`（let-polymorphism、ADT、type classes 预留、
strict evaluation）。目标不是上线，是写一篇 "bootstrapping a tiny HM
compiler" 的长文，顺便把 LLVM IR 生成那一段跑通做附录。他对 agent 的
使用习惯偏学究——喜欢先让 agent 铺一个层次化 plan，再一层一层深入，
过程中会插话："让我看一眼 type checker 那一块还没动的子任务。"

**业务背景**：编译器项目不能靠线性 todo 拍平——lexer/parser/typechecker
/codegen/runtime 各有内部结构，parser 里还会再嵌 Pratt 优先级子树，
codegen 会把 LLVM IR 独立出去派 sub-agent 做。这类工作流一旦把 plan
塞进 transcript 自然语言，agent 在第二周就会忘记哪些叶子还没闭。必须
走 `session_todos` 的 `parent_id` 自引用树 + `session_state_items
(category='plan_state')` 骨架投影。

**一句话目标**：从零实现 `lucid` 的 lexer → parser → type checker（HM
+ ADT）→ codegen（LLVM IR）→ runtime wiring，**plan 树贯穿全程、
compaction 之后骨架不丢、codegen 阶段派出的 sub-agent 产物能回推父
session**。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 会话总轮数 | 约 40 回合（分布在 3 周、10+ 个工作段） |
| `session_todos` 峰值总数 | ~68（含已 done/archived）；active 峰值 ~22 |
| plan 树最大深度 | 4 层（phase → section → task → subtask） |
| 工具调用总数 | ~230 次（edit / run_cargo / grep / read / write_artifact） |
| 产生 artifact 数 | ~45（AST 设计、parser 状态表、IR 片段、EBNF 草稿） |
| 触发 compaction 次数 | 3 次（T22 / T30 / T38） |
| Delegation child session 数 | 1 个（`lucid-codegen-llvm`，跨 T25–T28 存在） |
| 跨 session 提问"按 plan 树结构汇总" | 1 次（T35 产出最终交付树） |

## 2. 被测 DAUT

本场景瞄准"plan 树 × 动态增删 × 子 session delegation × compaction
保骨架"这四条主轴同时施压。A11 承受 60+ todo 的容量与层级压力；A6
要保证 3 次 compaction 之后 plan 骨架完整；A13 要把 delegation 作为
一等关系落地并可回溯。

| 编号 | 压测方式 |
| --- | --- |
| **A11** | `session_todos` 在 T15/T20 发生两次大规模结构变更——T15 插入 `parser.pratt-precedence` 子树（4 个新 node，parent_id 链到 `parser.expr`）；T20 因 type checker 发现 AST 不支持 ADT，回溯给 `parser.ast-design` 追加 3 条 subtask（parent_id 不变，position 重排）。T10 用户提问"type checker 那块还没动的子任务"，必须走 `parent_id` 递归 + `status='pending'` 过滤，而不是把 60+ todo 全塞进 prompt。 |
| **A6** | 3 次 compaction（T22 / T30 / T38）之后，`category='plan_state'` 的 payload 必须保留完整骨架（phase → section，2 层），并把每个被触碰过的 section 的 decision 以 `category='decision'` 独立行落地。任何把骨架压成叙述段落、或把 done 的叶子直接物理删除的实现都属违反 A6。 |
| **A13** | T25 agent 在 codegen 阶段决定把 LLVM IR 生成派给子 agent。此时必须：新建一个 child `agent_sessions` 行（独立 session_id），父 session 写 `session_state_items(category='delegation_state')` 指向 child session_id + delegation_id；父 `agent_runs.parent_run_id` 与子 run 建立边；T28 子 session 的产物以 artifact_ref + summary_text 形式回推父 session 的 `delegation_state.payload_json`，**父 session prompt 里不回放子 session transcript**。 |

## 3. 会话时间线

下面挑 12 个关键回合，覆盖 plan 首次生成、lexer 深入、plan 树裁剪查询、
parser 中段插入子树、type checker 回溯修 parser、第一次 compaction、
codegen delegation、子 session 产物集成、第二次 compaction、plan 树
报告、第三次 compaction、交付。每回合字段：**用户原话**（中文，保留
学究气）、**session_todos 树变化**（parent_id / status / 新插入）、
**manifest zone**（token 分配）、**LLM 看不到**（哪些数据不进 prompt）。

### T1 · 顶层 plan 首次生成

**用户原话**：「我准备把 `lucid` 这门小语言的编译器从零写一遍，目标
是一篇长文的附录代码。给我铺一个 5 阶段的 plan，lexer / parser /
type checker / codegen / runtime，每阶段先列出 3–7 个 section，section
下面再展开关键 subtask，不用一次性写完，先到第 3 层。」

**session_todos 树变化**：首次写入 5 个 phase 节点（parent_id=NULL）：
`lexer` / `parser` / `typechecker` / `codegen` / `runtime`；每 phase
下挂 4–6 个 section（parent_id 指向 phase），总计 24 个 section 节点；
再在 `lexer.tokenizer` / `parser.expr` / `typechecker.inference` /
`codegen.ir-skeleton` 4 个 section 下铺出 12 个 subtask。总 node 数
41，全部 `status='pending'`，`source='plan_seed'`，`priority` 按 phase
顺序分层。`session_state_items` 写入 1 条 `plan_state`（payload_json
含骨架 tree：phase→section，不含 subtask；version=1）。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1600（剪到
Rust + 文件工具 + llvm-tools 占位） / `skills` 900（`plt-pl-design`
+ `rust-compiler-engineering` 两个候选） / `session_anchor` 400 /
`plan_todo` 800（**只渲染 phase + 首阶段 section，subtask 不进**）/
`recent_tail` 0 / `summary` 0 / `workspace` 200。总 ≈ 5100，
`reason='initial_turn'`，`dropped_count=0`。

**LLM 看不到**：除 `lexer` 外其他 phase 的 section 细节，以及所有
subtask 的 description（它们在 DB、UI 可见，但 prompt 里只占 title
引用）。

### T5 · lexer 深入开发（第 5 轮里程碑）

**用户原话**：「token 类型我想按 `Ident / Int / Float / Str / Op /
Keyword / LParen / RParen / Layout` 分九类，layout token 用来后面做
off-side rule。你把 `lexer.tokenizer` 下面那几条 subtask 拆细一点，
我们一条条过。」

**session_todos 树变化**：`lexer.tokenizer` 下原有的 3 条 subtask
的 description 被 UPDATE 加入具体 token 清单；新插入 2 条 subtask
（`layout-sensitive-newline` + `op-classification`），parent_id 指向
`lexer.tokenizer`，position 分别为 4/5。`lexer.tokenizer` 自身
`status` 从 `pending` → `in_progress`。累积 `session_state_item_events`
7 条 mutation。`plan_state` 不 bump（骨架未变）。

**manifest zone**：`plan_todo` 1000（渲染策略：当前路径 `lexer →
tokenizer` 的祖先链 + 当前 section 下所有 active subtask + 下一个
section title） / `session_anchor` 400 / `recent_tail` 1200（T2–T4
的 tokenizer 设计讨论） / `tool_previews` 300。总 ≈ 5600。

**LLM 看不到**：其他 4 个 phase 下的任何 subtask 细节；`lexer` 自身
其他 section 的 description（`errors` / `diagnostics` / `source-map`
等）。

### T10 · "给我看 type checker 那块还没动的子任务"（plan 树过滤渲染）

**用户原话**：「等一下，跳一下——type checker 那块我还没动吧？把那一
整棵子树里还是 pending 的 subtask 给我看一下，我想先对一下 HM 的实现
顺序，再回来继续 lexer。」

**session_todos 树变化**：无写入。本轮的重点在**查询形态**：
context builder 必须对 `session_todos` 发起一次**祖先受限的递归查询**
——以 `typechecker` 为根（parent_id IS NULL AND item_key='typechecker'），
沿 parent_id 向下展开，过滤 `status IN ('pending','blocked')`，按
`position` 排序。实现上相当于 MatrixOne 递归 CTE 或应用层两跳 join
（phase → section → subtask）。返回 typechecker 子树 6 个 section
+ 其中 4 个 section 下的 11 个 pending subtask，共 17 条。

**manifest zone**：`plan_todo` 暴涨到 2200（一次性渲染 typechecker
整棵 pending 子树，title + 一行 description 精简），同时当前路径
（lexer）折叠为骨架引用 + 第一条 active subtask。其余 zone 收窄：
`recent_tail` 1000 / `session_anchor` 400 / `retrieved_facts` 200。总
≈ 5700，`reason='plan_subtree_query'`（**新 reason，设计文档未枚举——
见 §8**），`dropped_count=24`（其他 phase 的所有 todo 被显式 drop，
记录 drop_reason='out_of_scope_phase'）。

**LLM 看不到**：lexer 当前正在写的 tokenizer subtask 的中间代码；
parser/codegen/runtime 三个 phase 的 subtask 全部被 drop（title 都
不进 prompt）。

### T15 · parser 中段插入新子 plan（Pratt 优先级子树）

**用户原话**：「开始写表达式 parser 之前，我想专门为中缀运算符优先
级开一个子 plan——我打算走 Pratt 风格（token binding power 表 +
led/nud），不用 shunting yard。你在 `parser.expr` 下面开一个
`pratt-precedence` section，里面至少有 binding-power 表、led
dispatch、nud dispatch、right-assoc 单测 4 条 subtask。」

**session_todos 树变化**：新增 1 个 section node `parser.pratt-
precedence`（parent_id=`parser.expr`，position=3，status=pending），
再在它下面新增 4 条 subtask（binding-power-table / led-dispatch /
nud-dispatch / right-assoc-tests）。parser phase 总 node 数从 6 升到
11。`plan_state` version 从 1 → 2，payload_json 把 skeleton 从
`parser: [ast-design, expr, decl, patterns, errors, recovery]` 扩为
`parser: [ast-design, expr, pratt-precedence, decl, patterns, errors,
recovery]`，新增一条 `decision`（"为什么 Pratt 而非 shunting yard：
更好嵌入 let-expression、error recovery 点更明确"）。
`session_state_item_events` 写入 6 条 mutation。

**manifest zone**：`plan_todo` 1300（当前路径回到 `parser → expr →
pratt-precedence`，祖先链 + 兄弟 section 的 title）/ `session_anchor`
400 / `recent_tail` 1600 / `retrieved_facts` 400（拉出 T3 当时写的
token 分类 decision，供 Pratt 表引用）/ `tool_previews` 300。总
≈ 6700。

**LLM 看不到**：T1 以来 lexer 阶段所有已完成的 8 条 subtask 的
description；typechecker 子树（T10 曾一次性 dump 过）在本轮回归折叠。

### T20 · type checker 回溯改 parser AST（plan 树动态删改）

**用户原话**：「麻烦——我在写 `typechecker.adt-inference` 的时候发现
我们的 AST 压根没有 `Variant` / `Pattern::Constructor` 节点，
parser.ast-design 当时忘了，你得回去给 `parser.ast-design` 补几条
subtask，然后把 typechecker 这一块的 adt 相关 subtask 标成 blocked 等
parser 补完。」

**session_todos 树变化**：**回溯编辑**——`parser.ast-design` 下新
增 3 条 subtask（`add-variant-node` / `add-constructor-pattern` /
`update-ast-visitor`），parent_id 指向 `parser.ast-design`（不是当前
正在做的 typechecker），position 追加到末尾。`typechecker.adt-
inference` 及其 2 条子 subtask 全部 UPDATE `status='blocked'`，
`session_todo_deps` 插入 3 条依赖（typechecker.adt-inference 依赖
`add-variant-node` / `add-constructor-pattern` / `update-ast-visitor`）。
`plan_state` version 3，payload_json 不变（骨架未变，只是 subtask
增删），新增 1 条 `decision`（"AST 回溯补 Variant 节点：为何不开新
section——因为 ADT 的 AST 载体本就是 ast-design 的遗漏"）。

**manifest zone**：`plan_todo` 1500（当前路径临时切回 `parser →
ast-design`，祖先链 + blocked 的 typechecker 节点以折叠形式出现，
带 blocking_reason 字段）/ `session_anchor` 400 / `recent_tail` 1800
/ `retrieved_facts` 500（拉出 T15 的 AST 决策原文供对照）/ `summary`
0（尚未触发 compaction）。总 ≈ 7100。

**LLM 看不到**：其他 section（pratt-precedence 除了 title）的内部
subtask；lexer 已完成的全部 subtask。

### T22 · 第一次 compaction

**用户原话**：「parser 补的 3 条 subtask 走完了，我去吃饭，你把这一段
先压一下，别在前面拖着。」

**session_todos 树变化**：Compaction 触发。写入 1 条
`session_state_items(category='summary')`，token_estimate≈900，覆盖
T1–T20 的叙述压缩。**骨架保全验证**：`plan_state` version 仍为 3，
payload_json 里的 phase→section 两层骨架完整保留；所有
`category='decision'` 的 4 条 row（Pratt 选择 / AST 回溯 / token 分类
/ layout rule）status 均为 active 不可动；完成的 lexer subtask 14
条标 `status='archived'`（不物理删，token_estimate 清零）；
`session_transcript_items` 把 T2–T18 的 23 条工具输出标
`is_compacted=TRUE`。

**manifest zone**：`session_anchor` 400 / `plan_todo` 1100（骨架
+ 当前 `parser.ast-design` 的 3 条新 subtask 已 done，切回
`typechecker.adt-inference` unblocked）/ `recent_tail` 1600（压缩后）
/ `summary` 900（新 L1b）/ `retrieved_facts` 300。总 ≈ 5300，
`compaction_version='v1.0'`，`dropped_count=23`。

**LLM 看不到**：23 条被标 is_compacted 的 tool_call / tool_output
原文（≈14k tokens），全部落在 `conversation_log` 与
`session_tool_outputs`，通过 `raw_ref` 可显式重放。

### T25 · codegen 阶段派 sub-agent 专责 LLVM IR（A13 delegation）

**用户原话**：「codegen 部分我不想让主 agent 自己憋 LLVM IR，派一个
sub-agent 专门做——它只对 `codegen.ir-emission` 这一 section 负责，
输入是前面 typed AST，输出是 `.ll` 文本 + 若干 IR 片段 artifact。主
session 这边继续推 runtime 绑定。」

**session_todos 树变化**：新建 child `agent_sessions` 行
（session_id=`sess-lucid-codegen-llvm`，parent 引用通过
`delegation_state` 建立），**不写入父 session 的 session_todos**，
child session 独立维护它自己的 todo 树（初始 5 条：
`lower-to-ir / phi-nodes / adt-tagged-union / fn-call-abi / ll-file-
writer`）。父 session 写 1 条
`session_state_items(category='delegation_state', item_key='lucid-
codegen-llvm')`，payload_json 含 `{child_session_id, delegation_id,
scope_section='codegen.ir-emission', handoff_input_ref:
'artifact:typed-ast-snapshot-T24', expected_output:
'artifact:lucid.ll + ir-fragments'}`。同时主 run 的 `agent_runs`
写入新 child run 并在 `parent_run_id / delegation_id` 建边。

**manifest zone**：父 session `delegation_state` 200（child_session_id
+ scope + 约定 I/O）/ `plan_todo` 900（父 session 侧把
`codegen.ir-emission` 整个 section 折叠为"delegated → child session"
引用，其余 runtime 相关 section 正常展开）/ `recent_tail` 1400 /
`summary` 900 / `session_anchor` 400。总 ≈ 5400。

**LLM 看不到**：child session 的 todo 树、它自己的 system prompt、
skill 集合；父 session 这侧只知道有一条 delegation 引用。

### T28 · sub-agent 产出中间 artifact，父 session 集成

**用户原话**：「child session 那边跑出第一版 `lucid.ll` 和一堆
fragment 了吧？拿回来我看一眼，顺便把 runtime 那边的 `call-abi`
subtask unblock。」

**session_todos 树变化**：child session 在自己的 T1–T12 里产出
artifact：`artifact:lucid.ll-v0.3` + 3 条 `ir-fragments`。父 session
更新 `delegation_state` row：`payload_json` 追加
`{last_handoff_at, child_summary_text, child_artifact_refs[], status:
'progress'}`；child_summary_text 是 child session 自己 L1b summary
的摘录（≈350 tokens，由 child session 写入，父读取），不带 child
transcript。父 session 侧 `runtime.call-abi` subtask UPDATE
`status='pending'` unblock，`session_todo_deps` 清除一条依赖。
父 `plan_state` 不 bump（骨架未变）。

**manifest zone**：`delegation_state` 550（child_summary_text
+ artifact_refs 列表）/ `plan_todo` 900 / `tool_previews` 400（lucid.ll
片段的 preview_text，原文走 artifact_ref）/ `recent_tail` 1400
/ `summary` 900。总 ≈ 5550。

**LLM 看不到**：child session 的完整 transcript、child session 自己
做的 5 次工具调用、child session 内部 todo 状态机的每一步——父
session 只拿 summary + artifact 指针。

### T30 · 第二次 compaction

**用户原话**：「runtime 前两段走完了，再压一下。child session 那边我
知道它自己也在跑，不用动它。」

**session_todos 树变化**：Compaction 第二次触发。新增/更新 1 条
`summary`（覆盖 T22–T28 的叙述，version=2，token_estimate≈1000）；
旧 summary v1 `status='archived'` 保留。`plan_state` 骨架不变（严格
A6）。已完成 subtask 从 17 升到 31，全部 archived；`delegation_state`
row 继续保留 active，不进入 compaction 归档（child 尚在跑）。
`decision` 条目从 4 升到 6（T25 delegation 决策 + T28 handoff 约定各
独立 row）。

**manifest zone**：`summary` 1000（覆盖旧 narrative）/ `plan_todo`
900 / `delegation_state` 400（child summary 仍活跃）/ `recent_tail`
1500 / `session_anchor` 400 / `retrieved_facts` 300。总 ≈ 4500，
`compaction_version='v1.1'`，`dropped_count=31`（archived subtask
不再进 prompt 预算）。

**LLM 看不到**：T22–T28 那段代码讨论的自然语言原文；已 archived 的
31 条 subtask 的 description；child session 侧任何它自己归档的条目。

### T35 · 按 plan 树结构出总结报告

**用户原话**：「最后阶段写文章附录了——给我一份**按 plan 树结构的**
总结报告：每个 phase 下的 section、每个 section 下的关键 decision、
每个 decision 附带 rationale、child session 出的 IR artifact 在
codegen 那节列出来。别当成线性 changelog 写，我要树。」

**session_todos 树变化**：无写入。这是一次**结构化投影的跨表
join**——context builder 对 `session_todos` 全树查询（不做状态过滤，
因为要报告 done 与 archived）+ `session_state_items(category IN
('decision','delegation_state'))` 按 `item_key` 的前缀（如
`parser.*`）归并到对应 section 下 + `session_artifacts` 按
`source_section` 字段归并。**关键检查**：agent 生成的报告必须来自
这三张表的结构化查询结果，而不是回放 transcript 现场总结。否则容易
漏掉已 archived 的老 decision。

**manifest zone**：`retrieved_facts` 破例扩到 4500（全树 node title
+ 6 条 decision 全文 + delegation_state 摘要 + artifact 清单）/
`plan_todo` 600（骨架折叠 + 当前"交付"位置）/ `recent_tail` 1000
/ `session_anchor` 500 / `summary` 1000。总 ≈ 7600，
`reason='tree_structured_report'`（**新 reason，设计文档未枚举——
见 §8**），`dropped_count=0`（全数据为本轮主材料，无需 drop）。

**LLM 看不到**：T1 以来的全部 transcript；child session 的工具调用
细节；parser 中各 subtask 的代码改动（artifact 指针可点开）。

### T38 · 第三次 compaction

**用户原话**：「差不多了，最后收尾前再压一次，保证最终那轮能放得下
交付物。」

**session_todos 树变化**：Compaction 第三次。`summary` version=3
覆盖 T30–T36 的叙述；`plan_state` 骨架**再次验证完整**（phase 数 5
未变、section 数 26 未变，只在 T15 / T20 时各 +1 +3 过）；所有 done
subtask（59 条）标 archived；`delegation_state` row 因 child session
已 completed，status 从 active → closed，payload_json 追加 final
artifact 清单 + child session 最终 summary（≈400 tokens）。`decision`
条目达 8 条，全部 active 保留。

**manifest zone**：`summary` 1100 / `plan_todo` 400（仅剩 3 条交付
确认 todo）/ `delegation_state` 500（closed 但保留 final handoff
摘要）/ `session_anchor` 400 / `recent_tail` 1300 / `retrieved_facts`
300。总 ≈ 4000，`compaction_version='v1.2'`，`dropped_count=59`。

**LLM 看不到**：59 条 archived subtask 的 description；T30–T36 的
自然语言原文；child session 的所有内部事件。

### T40 · 交付

**用户原话**：「都过了。把最终报告定稿一下，作为文章附录。代码
snapshot 打 tag `v0.1.0-lucid`。」

**session_todos 树变化**：新增 1 条 `anchor` 子项（"delivered
v0.1.0-lucid"）+ 1 条 `summary(scope='final')`。剩余 3 条交付 todo
全部 done。累计 todo 总数 68（含 archived），active 归零。

**manifest zone**：`retrieved_facts` 3800（最终交付报告引用全部 8
条 decision + plan 骨架 + delegation final 摘要）/ `plan_todo` 300
/ `recent_tail` 1200 / `session_anchor` 600。总 ≈ 7400，
`reason='final_delivery_summary'`。

**LLM 看不到**：40 轮完整 transcript；child session 全部 transcript。
交付沿 plan 树投影 + decision 两条 L1a 轴合成，不回放原始对话。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 60+ 条 todo × 4 层 plan 树，不能整棵进 prompt | A11 `session_todos.parent_id` 树 + context builder 的"当前路径祖先链 + 当前 section active subtask + 下一节 title"渲染策略 |
| T10 "给我看 typechecker 未动的子任务"——跨子树查询 | 递归 CTE / 应用层 DFS，按 `parent_id + status='pending'` 过滤，结果一次性塞 `plan_todo` zone（本轮预算临时放宽） |
| T15 / T20 plan 树动态增删 | `session_todo_deps` 补依赖；`plan_state.version` bump；`session_state_item_events` 作为 projection audit 每步 mutation 独立 row |
| T25 codegen 派 sub-agent，父 session 不膨胀 | A13 `delegation_state` 指针 + child 独立 `agent_sessions` + `agent_runs.parent_run_id` 关系边 |
| T28 child artifact 回推父 session | child 写自己 L1b summary → 父读取 `child_summary_text` 字段进 prompt；child transcript 永不进父 prompt |
| 3 次 compaction 不能让 plan 骨架碎片化 | A6 compaction 白名单（`plan_state` / `decision` / `delegation_state` 不压，`todo_state` 的 done 叶子降级 archived） |
| T35 按 plan 树结构的报告 | 结构化投影 join 而非 transcript 回放：`session_todos` 全树 × `decision` × `delegation_state` × `session_artifacts` |
| 每轮 manifest 可解释 dropped | A4 `context_manifests` 必写；plan 树过滤轮的 `drop_reason` 需区分 `out_of_scope_phase` / `archived_subtask` / `done_leaf` |

## 5. 预期 state projection 演化

下面是 4 个关键节点的快照。`session_todos` 按层级 / status 统计；
`session_state_items` 按 category 计行数（括号为 active）。

**T1（顶层 plan 首次生成后）**：

| 项 | 值 |
| --- | --- |
| session_todos 总数 | 41（phase 5 + section 24 + subtask 12） |
| active | 41（全 pending） |
| plan 树最大深度 | 3 |
| session_state_items / plan_state | 1 (1)，version=1 |
| session_state_items / decision | 0 |
| session_state_items / delegation_state | 0 |

**T15（parser 插入 Pratt 子树之后）**：

| 项 | 值 |
| --- | --- |
| session_todos 总数 | 46（新增 1 section + 4 subtask） |
| active | 28（其余 18 已完成或 blocked） |
| plan 树最大深度 | 3 |
| plan_state | 1 (1)，version=2 |
| decision | 2 (2)（Pratt 选型 + token 分类） |
| delegation_state | 0 |
| session_todo_deps | 0 |

**T25（delegation 发起之后）**：

| 项 | 值 |
| --- | --- |
| session_todos 总数（父 session） | 52 |
| active（父 session） | 14（codegen.ir-emission 折叠为 delegated 引用） |
| plan 树最大深度 | 3 |
| plan_state | 1 (1)，version=4 |
| decision | 5 (5)（含 delegation 决策） |
| delegation_state | 1 (1)，status=active，child_session_id=sess-lucid-codegen-llvm |
| child session_todos（独立统计） | 5（child 自治） |

**T35（plan 树报告之前）**：

| 项 | 值 |
| --- | --- |
| session_todos 总数 | 68 |
| active | 6 |
| archived | 55 |
| blocked | 0（T20 的 blocked 已在 T22 后 unblock） |
| plan_state | 1 (1)，version=4（骨架自 T20 后未再变） |
| decision | 8 (8) |
| delegation_state | 1 (1)，status=progress→closed（T38 切 closed） |
| summary | 3 (1)（v1/v2 archived，v3 active） |
| session_state_item_events 累计 | ~180 行（projection audit） |

**注**：全程物理 delete 计数始终 = 0；所有"不再活跃"都走 status
字段。`session_todos` 的 `parent_id` 没有任何环形引用（通过每次
insert 前校验 ancestor chain 保证）。

## 6. Token 节省估算

**朴素方案**：Claude Sonnet 4.5，200k 窗口。每轮把 60+ todo 全量渲
染 + 累积 transcript 全塞。

- 每 todo title + description 平均 90 tokens，60 todo ≈ 5400 tokens
  单这一栏，plus 每轮新增 ~2200 tokens transcript。
- T15 朴素 ≈ 5400 + 15×2200 ≈ 38k（主要 plan_todo 吃掉）。
- T25 朴素 ≈ 60 todo × 90 + 25×2200 = 60400，plus child session
  transcript 回放 ~8k = 68k。
- T35 朴素（按 plan 树报告）需要全 transcript + 全 todo：
  40×2200 + 60×90 + child 8k = 102k。
- T40 朴素 ≈ 40×2200 + child 全 ≈ 96k，若再塞全 todo description
  逼近 110k。
- **朴素方案在 T35 的报告回合会因需要"回看所有 transcript"直接撞
  满 cost 上限**，虽然未必溢出 200k，但 input cost 已接近场景 6×。

**本设计方案**（按 zone 加总）：

| zone | T1 | T15 | T25 | T35 | T40 |
| --- | --- | --- | --- | --- | --- |
| system_static | 1200 | 1200 | 1200 | 1200 | 1200 |
| tool_schemas | 1600 | 1600 | 1600 | 1600 | 1600 |
| skills | 900 | 900 | 900 | 900 | 900 |
| session_anchor | 400 | 400 | 400 | 500 | 600 |
| plan_todo | 800 | 1300 | 900 | 600 | 300 |
| recent_tail | 0 | 1600 | 1400 | 1000 | 1200 |
| summary | 0 | 0 | 900 | 1000 | 1100 |
| retrieved_facts | 0 | 400 | 0 | 4500 | 3800 |
| tool_previews | 0 | 300 | 400 | 0 | 0 |
| delegation_state | 0 | 0 | 200 | 500 | 500 |
| workspace | 200 | 200 | 200 | 200 | 200 |
| **合计** | **5100** | **7900** | **8100** | **12000** | **11400** |

**对比**：

| 节点 | 朴素 (tokens) | 本设计 (tokens) | 节省 |
| --- | --- | --- | --- |
| T15 | ~38k | 7.9k | 79% |
| T25 | ~68k（+child 回放） | 8.1k | 88% |
| T35 | ~102k | 12k | 88% |
| T40 | ~96k | 11.4k | 88% |

**关键结论**：本场景的收益来源跟 S01 不同——S01 是"transcript 爆"，
S06 是"todo 树自身就很大 + delegation 回放 + 按树报告"。在 T35
报告回合，结构化 join 取代 transcript 回放，是 88% 节省的主轴；
delegation_state 把 child session 钉成一个指针 + summary，消掉了
整段 child transcript。

## 7. 潜在失败模式

**F1 · plan 树在 UI 里好看但 prompt 里结构丢失**：如果
context builder 把 `session_todos` 渲染成"按 position 拍平的 bullet
list"丢进 `plan_todo` zone，agent 会看不出 `parser.pratt-precedence`
是 `parser.expr` 的子节点。**可观测信号**：T15 之后 agent 在回答里
把 Pratt 相关 subtask 误报为与 `parser.expr` 平级；`context_manifest_
items` 里 `plan_todo` zone 的 content 没有缩进 / 没有 parent→child
关系标记。

**F2 · sub-session summary 未回推父 session**：T28 child 产出
artifact 后，如果实现只是把 artifact_ref 写进 child 自己的 state，
而忘记 UPDATE 父 session 的 `delegation_state.payload_json`（追加
last_handoff_at / child_summary_text），父 agent 下一轮就不知道
child 推进到哪步。**可观测信号**：父 `session_state_item_events`
里 T28 前后没有针对 `delegation_state` 的 update；父 prompt 里
`delegation_state` zone 的 child_summary_text 为空。

**F3 · `parent_id` 环形引用**：T20 回溯追加 `parser.ast-design`
subtask 时，如果实现没做 ancestor 校验，容易在人工拖拽下把某 subtask
挂到自己子孙下。**可观测信号**：CTE 递归查询永不收敛；
`session_todos` 某 row 的 ancestor 链含自身；UI 侧 plan 面板白屏。
**防守**：插入/更新 `parent_id` 时在事务内 DFS 校验。

**F4 · compaction 后 plan 骨架碎片化**：如果 T22 / T30 / T38 的
compaction 实现把 `plan_state.payload_json` 也当作"叙述性内容"压成
一句话总结（"parser 阶段已基本完成"），T35 报告回合就重建不出完整
phase → section 骨架。**可观测信号**：T35 报告里缺失中期插入的
`pratt-precedence` section；`plan_state.version` 在 compaction 轮次
被错误地 bump；`session_state_item_events` 出现针对 `plan_state`
的 mutation=`replace`（应为不变）。

**F5 · plan 树过滤让 agent 看不到该看到的**（T10 的相反侧）：过滤
策略太激进——例如 T15 之后 context builder 一直不把
`typechecker.adt-inference` 的 blocked 状态塞进 prompt，agent 以为
"没有这个 todo"继续往下走，结果绕开了需要 ADT 的那一组 section，
最后才发现类型检查缺失。**可观测信号**：blocked todo 在 `plan_todo`
zone 中完全缺席；agent 输出里对"typechecker 当前卡点"的描述与 DB
真实状态矛盾。

**F6 · "按 plan 树结构的总结报告"退化为 transcript 回放**：T35
若实现图省事直接把最近 N 条 transcript 塞给 agent 让它"总结一下"，
输出看似合理，但会漏掉已 archived 的 decision，也会把 child session
的工具细节误拉进父 session 的叙述。**可观测信号**：T35 的
`context_manifests.reason` 不是 `tree_structured_report` 而是
`generic_summary`；报告里 codegen.ir-emission 那节引用的是父
session 看到的 summary 一句话而非 child final artifact 清单。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Proposed Schema §6 Plans and Todos — **建议补强**：文档里只写
   "context builder should render only active/in-progress todos and
   immediate dependencies"，这对线性 todo 够用，但对 4 层 plan 树
   严重欠定义。S06 需要的最小策略集：（a）当前路径祖先链必渲染；
   （b）当前 section 下所有非 archived subtask 渲染；（c）兄弟
   section 仅渲染 title；（d）跨子树查询（T10）作为 explicit API
   而非副作用。建议在 §6 后面补一节 "Plan Tree Rendering Policy"，
   明确这 4 条并给对应 SQL 模式。
2. §Proposed Schema §2 Session State Projection（`delegation_state`
   category）— **建议补强**：文档只说了 "child sessions, subagent
   work, handoff summary"，未规定 `payload_json` 的强 schema。S06
   实际需要 `{child_session_id, delegation_id, scope_section,
   handoff_input_ref, expected_output, last_handoff_at,
   child_summary_text, child_artifact_refs[], status}` 九字段。
   建议把这九字段写进设计里作为 `delegation_state` 的约定 shape，
   并和 `agent_runs.parent_run_id / delegation_id` 的建边规则合并
   一张图说明。
3. §Context Assembly / Compaction — **建议补强**："Preserve
   structured L1a facts: files, decisions, active errors, plan/todos"
   这一句里 "plan/todos" 过于笼统。S06 要求 **plan 骨架（phase
   →section 两层）**绝对不可压缩、**decision 行**绝对不可压缩、
   **delegation_state active 行**绝对不可压缩；done 叶子 subtask
   可 archived。建议在 §Compaction 下补一节 "Compaction Invariants
   for Plan Tree & Delegation"，显式列白名单。
4. §Proposed Schema §3 Context Manifests — **建议补强**：
   `context_manifests.reason` 在 S06 出现两个 S01 没涉及的新取值：
   `plan_subtree_query`（T10）、`tree_structured_report`（T35）。
   建议把 manifest reason 做成枚举并收录到设计附录，让失败观测
   能用 SQL 聚合（"为什么这一轮 prompt 这么大"有标准答案）。

**三条改进建议**：

- **Plan Tree Rendering Policy**：在 §Plans and Todos 后增一节，
  覆盖 4 条渲染规则 + 跨子树 query 的 API 约定 + token 预算临时放
  宽的触发条件。配 2 段 SQL（祖先链查询 / 子树 pending 查询）作
  为参考实现。
- **Delegation State Shape 一等化**：把 `delegation_state.payload
  _json` 升级为有 schema 的 JSON（甚至考虑抽成 `session_delegations`
  独立表，按 `parent_session_id + delegation_id` 主键），并规定
  child summary 回推父 session 的 trigger（child run 每次 L1b
  compaction 必须触发父 session 的 delegation_state.payload_json
  patch）。
- **Compaction Invariants 契约化**：与 S01 的反馈合并，落地成一节
  带 SQL assertion 的 "Compaction Invariants"：plan_state /
  decision / delegation_state(active) / error_state(active) 永不
  压缩；done subtask 可降级 archived；`plan_state.version` 在
  compaction 轮次**禁止** bump（compaction 不是 plan 变更）。
