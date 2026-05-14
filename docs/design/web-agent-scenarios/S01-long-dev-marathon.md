# S01 — 超长开发马拉松：Rust 电商后台 60 轮重构

> Status: Draft v0.1
> DAUT: **A2, A3, A4, A6, A10**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：7 年 Rust 后端工程师，现负责一套线上电商后台（order /
payment / inventory / settlement / notification 等 12 个 crate，约 8 万行代
码，含 1200 个集成测试）。团队上周决定从 actix-web 1.x 迁移到 axum 0.8，
原因是 actix 的 extractor 生态滞后、tower 中间件生态无法复用。用户选
astra web agent 做驾驶员式协作（不自动无审改仓库，每次写磁盘都要 diff 审）。

**业务背景**：迁移不是"改 import"级别，而是牵动整个请求生命周期——
actix 的 `Handler<T, R>`、`FromRequest`、`App::wrap` 与 axum 的
`Handler<T, S>`、`FromRequestParts`、`tower::Layer` 在语义、生命周期、
Send/Sync 约束上都不等价。迁移必须逐 crate 推进，保留原 binary 可回滚。

**一句话目标**：把电商后台从 actix-web 1.x 迁到 axum 0.8，编译通过 +
全部集成测试绿 + 生产镜像能启动，历史 decision 不丢。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 会话总轮数 | 60 回合 |
| 会话持续时间 | 3 天（分 5 段工作，每段 4–8 小时） |
| 工具调用总数 | ~400 次（edit、run_cargo、grep、read、web_search） |
| 产生 artifact 数 | ~80（patch、cargo 输出、设计笔记、review 报告） |
| 全量 transcript 裸大小 | ~15 MB |
| 触发 compaction 次数 | 3 次（约 T8 / T38 / T58） |
| 三级检索触发次数 | ~12 次（其中 2 次进到 vector 层） |

## 2. 被测 DAUT

本场景瞄准"长会话 × 多轮检索 × 多次 compaction"这条主轴，下列 5 条假设
将被同时压到边界。

| 编号 | 压测方式 |
| --- | --- |
| **A2** | 审计（`conversation_log` + `agent_run_events`）、投影（`session_state_items`）、manifest（`context_manifests`）三张表在 60 轮里各自写入 400+ / 300+ / 60 行，验证它们从未互相覆盖，也没有 manifest 污染投影的情况。 |
| **A3** | T1、T20、T40、T60 四个点打开会话时，hot 路径查询只许命中 `agent_sessions`、活跃 `agent_runs`、`session_state_items`、最新 `session_transcript_items` 页；任何一次对 `conversation_log` 的全表扫描或 JSON 内过滤都算失败。 |
| **A4** | 每一回合（含 compaction turn、retrieval turn、tool-heavy turn）必须写出一条 `context_manifests`，字段 `reason`、`dropped_count`、`total_estimated_tokens`、`compaction_version` 全部非空。T45 至少产生 1 个 dropped_items（巨型 cargo test 输出被降级）。 |
| **A6** | 3 次 compaction 后，`session_state_items` 里 `category='decision'`、`plan_state`、`todo_state`、`error_state` 的行数只增不减（只可标 `status='archived'`），不允许物理删除；L1a 结构化事实必须能在 T60 之前任意时刻被检索回来。 |
| **A10** | 三级检索在 T15、T31、T52 三次被触发，覆盖三种路径：只走 structured 过滤（T15）、structured + full-text（T31 失败回退）、三级全开命中 vector（T31 第二次尝试）。vector/FTS 表在本 session 内不出现 UPDATE 或 DELETE。 |

## 3. 会话时间线

下面只挑 10 个代表性回合，覆盖开局、首次 compaction、回忆、plan 大改、
深度 bug、二次 compaction、大工具输出、老 decision 检索、三次 compaction、
交付。每回合字段含义：**用户原话**（中文）、**state_items 变化**（category
+ 新增/改动条数）、**manifest zone**（token 分配，单位 tokens）、**LLM
看不到**（哪些数据留在 DB 但没进 prompt）。

### T1 · 开局对齐

**用户原话**：「帮我把这个仓库从 actix-web 1.x 迁到 axum 0.8，整体 12 个
crate，先别动代码，给我一份迁移 plan 和风险清单，我来对齐。」

**state_items 变化**：新增 `anchor`（目标 + 迁移约束）、`workspace_state`
（cloud workspace，Rust toolchain 1.83，git4data 分支 `migrate/axum`）、
`plan_state`（12 阶段 plan 指针）、`skill_hint`（`rust-migration` +
`axum-handler-analysis` 两个候选）、`todo_state`（生成 12 条 P0/P1 todo，
写入 `session_todos`，带 `session_todo_deps` 依赖图）。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1800（剪到 Rust
相关 + shell + 文件工具）/ `skills` 900 / `session_anchor` 400 /
`plan_todo` 600 / `recent_tail` 0 / `retrieved_facts` 0 / `tool_previews`
0 / `workspace` 200。总 `total_estimated_tokens` ≈ 5100，
`dropped_count=0`，`reason='initial_turn'`。

**LLM 看不到**：仓库全部 8 万行代码。agent 只看到 plan skeleton 和 skill
摘要，真实代码要等到具体 todo 被激活才按需读。

### T8 · 首次 compaction 触发

**用户原话**：「`order` crate 已经 green 了，继续 `payment`，先扫一遍
`actix_web::dev::Service` 的用法看看哪里不能直接映射。」

**state_items 变化**：`anchor` 不动；`plan_state` phase `order→done` /
`payment→active`；新增 3 条 `decision`（tokio::select! vs futures::select!、
Bearer auth 改走 `axum::extract::FromRequestParts`、错误类型统一到
`thiserror`）；5 条 `active_file` 滚入；`todo_state` 完成 4 / 新开 3。
`CompactionPolicy` 在 turn 结束检测到 `recent_tail` 已累积 9200 tokens
超阈值（8000），触发 L1b summary 生成：写入 1 条
`session_state_items(category='summary')`，同时把 T2–T6 的 7 条工具输出
在 `session_transcript_items` 标 `is_compacted=TRUE`（不物理删）。

**manifest zone**：`session_anchor` 400 / `plan_todo` 700（plan 膨胀）
/ `recent_tail` 1800（压缩后） / `summary` 900（新 L1b） / `retrieved_facts`
300 / `tool_previews` 600。总 ≈ 4700，`reason='post_compaction_turn'`，
`compaction_version='v1.0'`，`dropped_count=7`。

**LLM 看不到**：被 `is_compacted=TRUE` 标记的 7 条原始 tool_call /
tool_output 全文（总 ~11k tokens），它们继续活在 `conversation_log` 和
`session_tool_outputs`，通过 `raw_ref` 可重放。

### T15 · 回忆 T3 的设计决策（纯 structured 检索）

**用户原话**：「等下——我们 T3 的时候是不是讲过 error 类型要走
`thiserror`？我忘了当时是怎么约束 `From<sqlx::Error>` 的。」

**state_items 变化**：无新增。`RetrievalPolicy` 识别出用户在指涉历史
decision，启动检索：**第一级 structured 过滤**直接命中——
`SELECT * FROM session_state_items WHERE session_id=? AND
category='decision' AND payload_hash IS NOT NULL ORDER BY priority DESC,
updated_at DESC LIMIT 5`，拿到 T8 写入的那条 `decision:error-type-
unified`，完全不走 full-text 或 vector。

**manifest zone**：标准 zones 照旧，额外 `retrieved_facts` 从 300 → 700
（塞入 1 条 decision 全文，240 tokens）；其余 zone 不变。
`reason='history_recall_structured'`，`dropped_count=0`。

**LLM 看不到**：T3 当时 assistant 那条长达 1800 tokens 的"思考过程"叙
述（已是 L2 audit），检索只取 `decision.payload_json` 里的约 240 tokens
结论 + rationale，不拉整条 assistant_message。

### T24 · 引入新 crate `settlement-v2`（plan / todo 大改）

**用户原话**：「临时决定——`settlement` crate 里的 retry 逻辑要拆一个
`settlement-v2` 单独承接，老的那块先冻结。你帮我把 plan 切开。」

**state_items 变化**：`plan_state` version 从 7 跳到 8，payload_json 里阶
段从 12 扩成 14；旧 `settlement` todos 批量 UPDATE 到 `status='paused'`
（不物理删）；新 6 条 `settlement-v2` todo 写入 `session_todos` 并在
`session_todo_deps` 补依赖（依赖 `payment` 完成）；`decision` 新增一条
（"为什么不直接在原 crate 里加 v2 模块：编译期隔离 + 热 rollback"）。
`session_state_item_events` 批量写入 9 条 mutation（`update` + `insert`）。

**manifest zone**：`plan_todo` 从 700 跳到 1100（plan skeleton + 新 todos
+ 被 paused 的旧 todos 各保留 title 引用） / `session_anchor` 400 /
`recent_tail` 1800 / `summary` 900。总 ≈ 5100，`dropped_count=2`（两条已
完成且非热点的老 todo 不进 prompt）。

**LLM 看不到**：被 paused 的 8 条老 `settlement` todo 的完整 description
（平均每条 300 tokens），它们在 UI 侧仍可见，但 prompt 只放 title。

### T31 · 深度 bug：检索老错误信息（三级检索全开）

**用户原话**：「CI 挂了，看这个 traceback：`SqlxError(PoolTimedOut)`
跟上次 `inventory` 调通之前那个 pool 配置报错好像一样，你能翻回去看
当时是怎么调的么？」

**state_items 变化**：无新增 L1 投影；`RetrievalPolicy` 启动三级检索。
**第一级 structured** 尝试 `category='error_state' + tool_name LIKE
'cargo%'` 返回 0 行（老错误已被 compaction 归档）。**第二级 full-text**
对 `session_history_chunks.preview_text` 查 `PoolTimedOut AND pool`
命中 4 条 chunk（包括 T13 的 tool_output_preview），仍不确定是否就是"那
次"。**第三级 vector** 对 `chunk_id` 的 embedding 查询语义近邻，
distance=cosine（与索引一致），取 top-3 + rerank（recency + importance），
最终锁定 T13 一条 `chunk_type='error'` chunk，通过 `raw_ref` 加载原始
2300 tokens 的 cargo 输出。

**manifest zone**：`retrieved_facts` 暴涨到 3800（塞入 T13 错误原文
+ 当时的修复 decision 摘要） / `recent_tail` 1800 / `tool_previews` 800
/ `plan_todo` 1100。总 ≈ 8700。`reason='deep_retrieval_error_recall'`，
`dropped_count=6`（其他 vector top-K 结果被剪掉）。

**LLM 看不到**：vector 搜索返回的其余 6 条 chunk（tokens 合计 ~9k），以
及 T13 之前"试错"的 14 条工具调用——它们继续躺在 `agent_run_events`
和 `session_tool_outputs` 里。

### T38 · 第二次 compaction + 清理陈旧 artifact 引用

**用户原话**：「OK，bug 修完了，继续 `notification` crate。你先把之前
那些 cargo 输出清理一下，别占着。」

**state_items 变化**：Compaction 第二次触发。新增/更新 1 条 `summary`
（覆盖 T9–T30 的压缩叙述，token_estimate ≈ 1100）。T1–T30 期间积累的
旧 `tool_ref`（cargo check 输出 preview）批量降级：`status` 从 `active`
改为 `archived`，`payload_json` 保留，`token_estimate` 重新估为 0（不再
参与 prompt 预算）。`session_artifacts` 里对应 80 个 artifact 中的 32 个
被标 `ref_status='compacted'`（artifact 本体不删，只降热度）。

**manifest zone**：`summary` 1100（新版） / `session_anchor` 400 /
`plan_todo` 1000（settlement-v2 部分已 done，收窄） / `recent_tail` 1600
/ `retrieved_facts` 500 / `tool_previews` 400。总 ≈ 5000，
`compaction_version='v1.1'`，`dropped_count=32`（归档的 tool_ref）。

**LLM 看不到**：32 条 archived `tool_ref` 的完整 preview（共 ~18k tokens）
；它们在 Web UI "已归档"折叠区仍可展开，也可被后续检索（但
importance 降级）。

### T45 · 大型工具输出入库（cargo test 全量失败）

**用户原话**：「跑全量 integration test 看看是不是都绿了。」

**state_items 变化**：agent 执行 `cargo test --workspace`，输出 2.1 MB
（约 520k tokens 等价），里面含 47 个失败。`session_tool_outputs` 新增
1 行：`tool_name='cargo'`、`status='error'`、`byte_size=2_214_912`、
`preview_text` 裁到 2000 字符（前 20 个失败的 name + 头尾上下文）、
`artifact_ref='artifact:cargo-test-T45'`、`content_hash='sha256:…'`、
`token_estimate=480`。新增 1 条 `error_state`（"47 failing tests,
clustered in settlement-v2 + notification, pattern: missing `tower::Service`
impl"）。`session_history_chunks` 也写入 1 条 `chunk_type='error'`（供
后续 T49、T52 检索）。

**manifest zone**：`tool_previews` 480（preview_text 完整进入，artifact_
ref 以链接形式呈现，**原 2.1 MB 一字未进 prompt**） / `error_state` 进入
`session_anchor` 补丁（+120） / `recent_tail` 1800。总 ≈ 5100，
`reason='large_tool_output_gated'`，`dropped_count=1`（原始 full output 被
显式 drop，理由 `exceeded_tool_preview_cap`）。

**LLM 看不到**：2.1 MB 的 cargo 全量输出。它通过 `artifact_ref` 可被用
户在 UI 侧下载，也可被下一轮通过"expand artifact"显式加载，但不主动进
prompt。

### T52 · 检索 T8 的 `tokio::select!` decision（纯 structured 检索）

**用户原话**：「等下，我们之前为什么选了 `tokio::select!` 不用
`futures::select!`？我记得有个原因跟 cancellation 有关。」

**state_items 变化**：无新增。检索走最短路径：**第一级 structured**
直接 `category='decision' AND (title LIKE '%select%' OR summary_text
LIKE '%select%')`，命中 T8 写入的那条 decision 记录，payload_json 含
rationale（"futures::select! 不保证 future 的 cancel-safety 在
stream::StreamExt poll 之间的语义、且不支持 biased mode"），token_estimate=
210。**不进 full-text，也不进 vector**——这正是 A10 分层的价值。

**manifest zone**：`retrieved_facts` 从 500 → 710（+210 tokens 的
decision 原文），其他 zone 不变。`reason='history_recall_structured'`，
`dropped_count=0`。

**LLM 看不到**：T8 整条对话上下文（~3.5k tokens），包括 agent 当时列出
的 3 种备选方案的逐条比较——只拿结论 + rationale，不拿思考过程。

### T58 · 第三次 compaction

**用户原话**：「`notification` 和 `settlement-v2` 都过了，最后一步是整
workspace 一起跑 release build，你先准备。」

**state_items 变化**：Compaction 第三次触发，`summary` 版本号 v3，覆
盖 T31–T57 的叙述（token_estimate ≈ 1300）。`plan_state` 再 bump 到
version 11（最后阶段切到 `workspace_release` phase）。`todo_state`
`active` 只剩 3 条；已完成的 52 条保留在 DB（供 T60 总结用），但不进
prompt。`session_state_item_events` 再写入 18 条 mutation。

**manifest zone**：`summary` 1300（新版覆盖旧版） / `plan_todo` 600
（仅剩 3 个 active todo + 2 个依赖） / `session_anchor` 400 / `recent_tail`
1600 / `retrieved_facts` 400 / `tool_previews` 300 / `workspace` 200。
总 ≈ 4800，`compaction_version='v1.2'`，`dropped_count=47`（已完成的
todo 不再进 prompt）。

**LLM 看不到**：52 条 done todo 的 description、T32–T57 期间的详细 tool
调用序列、第二次 compaction 之前的 summary 旧版本（被新版覆盖但
`session_state_item_events` 里仍可追）。

### T60 · 交付总结

**用户原话**：「都过了，给我出一份迁移交付报告，要列出所有 decision、
遗留风险、回滚路径，直接贴到 PR description。」

**state_items 变化**：新增 1 条 `anchor` 子项（"delivered"） + 1 条
`summary(scope='final')`；agent 跨全程批量检索：`category IN ('decision',
'error_state', 'plan_state')`，一次性拉 62 条结构化事实（18 条 decision
+ 12 条 error_state 里仍活跃的 3 条 + 所有 plan 阶段转移），拼成 Markdown
交付报告，写入 `session_artifacts`（`artifact_ref='artifact:delivery-
report-v1'`）。

**manifest zone**：`retrieved_facts` 破例扩到 4000（final turn 一次性加载
全部 decision 原文） / `plan_todo` 300（只剩交付确认 todo） / `recent_tail`
1400 / `session_anchor` 600。总 ≈ 7200，`reason='final_delivery_summary'`。

**LLM 看不到**：60 轮完整 transcript（15 MB）。交付报告按 decision /
error / plan 这三条 L1a 结构化轴合成，不回放原始对话。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 60 轮 transcript ~15 MB，不能整体进 prompt | A3 分层 + `session_transcript_items` 分页 + `session_history_chunks` 索引 |
| 3 天跨 5 段工作，每次"打开 session"必须 <1s 出首屏 | hot 路径 4 条索引查询（session / run / state_items / transcript tail） |
| 3 次 compaction 不能让 agent 忘记 plan 状态和关键 decision | A6：compaction 只写 summary，不覆盖 `category='plan_state' / 'decision' / 'todo_state'` 行 |
| T15 / T31 / T52 三次回忆历史，检索成本差异巨大 | A10 三级检索：structured → FTS → vector，按需升级；distance function 与索引一致（cosine） |
| T45 的 2.1 MB cargo 输出不能线性吃掉剩余预算 | A9：落 `session_tool_outputs` + preview_text 进 prompt；`artifact_ref` 留指针 |
| 每轮 manifest 可回放、可解释失败 | A4：`context_manifests` + `context_manifest_items` 带 dropped_count + reason + token_estimate |
| plan 在 T24 大改，不能让 agent 继续按老 plan 跑 | A11（投影而非 JSON 埋藏） + A6（状态迁移显式记录） |
| 审计（log）/ 投影（state）/ manifest 三者不互相覆盖 | A2：三张独立表，写入路径各自独立，compaction 只动投影 |

## 5. 预期 state projection 演化

下面是 `session_state_items` 在 4 个关键节点的快照（按 category 分组计
row 数；括号内为 `status='active'` 的条数）。

**T1（开局对齐后）**：

| category | rows | 说明 |
| --- | --- | --- |
| anchor | 1 (1) | 迁移目标 + 约束 |
| workspace_state | 1 (1) | cloud workspace + Rust 1.83 |
| plan_state | 1 (1) | 12 阶段 skeleton |
| todo_state | 12 (12) | P0/P1 初始 todo |
| skill_hint | 2 (2) | rust-migration / axum-handler-analysis |
| decision | 0 | 尚无 |
| error_state | 0 | — |

**T20（早期推进中 · 两次小 compaction 之间）**：

| category | rows | 说明 |
| --- | --- | --- |
| anchor | 1 (1) | 无变化 |
| plan_state | 1 (1) | version=5，phase=`payment` |
| todo_state | 19 (7) | 完成 10 / paused 2 / active 7 |
| decision | 6 (6) | T8 那 3 条 + 后续 3 条（FromRequest、错误类型、tracing layer） |
| active_file | 14 (8) | 近期热文件，8 条 active |
| tool_ref | 22 (18) | cargo 输出 preview 等 |
| summary | 1 (1) | T8 compaction 产物 |
| error_state | 2 (1) | T13 那条 pool_timeout 仍 active |

**T40（settlement-v2 重切之后 · 第二次 compaction 完成）**：

| category | rows | 说明 |
| --- | --- | --- |
| anchor | 1 (1) | — |
| plan_state | 1 (1) | version=9，phase=`notification` |
| todo_state | 31 (9) | 完成 18 / paused 4 / active 9 |
| decision | 11 (11) | 新增 settlement-v2 拆分决策等 |
| active_file | 27 (7) | 热度窗口收窄到 7 |
| tool_ref | 40 (6) | 34 条被归档（status=archived） |
| summary | 2 (1) | 旧版保留，新版 active |
| error_state | 3 (1) | — |
| skill_hint | 3 (2) | 追加了 `cargo-test-triage` |

**T60（交付）**：

| category | rows | 说明 |
| --- | --- | --- |
| anchor | 2 (1) | 交付确认条为 status=final |
| plan_state | 1 (1) | version=11，phase=`delivered` |
| todo_state | 55 (0) | 全部 done/archived |
| decision | 18 (18) | 全程决策留痕 |
| summary | 3 (1) | v1/v2 archived，v3 active |
| tool_ref | 80 (4) | 76 条归档 |
| error_state | 6 (0) | 全部 resolved |

**注**：无论哪个节点，physical delete 计数始终 = 0。所有降级都走
`status` 字段；`session_state_item_events` 的 mutation 总数在 T60 约
为 380 行，作为投影的 audit trail。

## 6. Token 节省估算

**朴素方案**：Claude Sonnet 4.5，200k context。每轮把累计 transcript 裸
塞进 prompt，消耗 = `min(cum_tokens, 200k)`。

- 粗估每轮平均产生 ~2500 tokens（user + assistant + tool I/O，已剔重）。
- T1 朴素 prompt ≈ 2500；T30 朴素 ≈ min(30 × 2500, 200k) = 75k；T60 朴
  素 ≈ min(60 × 2500, 200k) = 150k（还没触顶，但接近）。
- 若算上 tool 输出（T45 那条 2.1 MB ≈ 520k tokens 单独），实际朴素方案
  在 T45 就已爆 200k 窗口，必须截断或丢弃——**朴素方案在 T45 直接失
  败**，无法完成场景。
- 假设忽略 T45 这种极端，按 tool 输出等价 token 平均摊薄（+ ~1000 / 轮），
  T30 ≈ 105k，T60 ≈ 210k（溢出）。

**本设计方案**（按 zone 加总，每轮实际进 prompt 的 tokens）：

| zone | T1 | T8 | T30 | T45 | T60 |
| --- | --- | --- | --- | --- | --- |
| system_static | 1200 | 1200 | 1200 | 1200 | 1200 |
| tool_schemas | 1800 | 1800 | 1800 | 1800 | 1800 |
| skills | 900 | 900 | 900 | 900 | 900 |
| session_anchor | 400 | 400 | 400 | 520 | 600 |
| plan_todo | 600 | 700 | 1000 | 900 | 300 |
| recent_tail | 0 | 1800 | 1600 | 1800 | 1400 |
| summary | 0 | 900 | 1100 | 1100 | 1300 |
| retrieved_facts | 0 | 300 | 500 | 500 | 4000 |
| tool_previews | 0 | 600 | 400 | 480 | 300 |
| workspace | 200 | 200 | 200 | 200 | 200 |
| **合计** | **5100** | **8800** | **9100** | **9400** | **12000** |

**对比**：

| 节点 | 朴素 (tokens) | 本设计 (tokens) | 节省 |
| --- | --- | --- | --- |
| T8 | ~20k | 8.8k | 56% |
| T30 | ~105k | 9.1k | ~91% |
| T45 | **溢出 200k**（失败） | 9.4k | 实际 >95%，朴素根本不可行 |
| T60 | ~210k（溢出） | 12k | 实际 >94%，朴素根本不可行 |

**关键结论**：本设计让长会话在 60 轮后仍保持单轮 prompt < 12k tokens，
而朴素方案在 T45 就已物理溢出 200k 上下文窗口。节省的不仅是成本
（Sonnet $3/M input tokens 级别的等比下降），更是**能不能跑完**这个硬
门槛。

## 7. 潜在失败模式

如果设计实现有缺漏，S01 会在下面 5 处暴露：

**F1 · compaction 后 agent 忘 plan**：如果 `CompactionPolicy` 实现时把
`session_state_items(category='plan_state')` 也当作 "旧状态" 一起压成叙
述性 summary，T38 之后 agent 会在 `settlement-v2` 阶段把 actix 遗留代
码当成新代码改回去。**可观测信号**：T39–T45 的工具调用里出现 edit 操
作指向已被 T24 冻结的 `settlement` 目录 + 新 plan 里没有对应 todo。

**F2 · 审计被投影覆盖**：如果 compaction 路径误把 `conversation_log`
里的旧行 UPDATE 成 summary，A2 就被破坏。**可观测信号**：
`conversation_log` 的 `created_at` 和 `item_seq` 不再单调；debug/export
模式下回放某一天的 session 对不上当时的 manifest。

**F3 · hot 路径退化成全扫**：T20 / T40 用户打开 session 如果实现把
`session_state_items` 的 `payload_json` 拿去做 LIKE 过滤（而不是走
`category + status` 索引），3 天的累积会让首屏延迟飙到 >3s。
**可观测信号**：慢查询日志出现 `WHERE payload_json LIKE …` 或
`SELECT ... FROM conversation_log` 的全表扫描；SSE 首条事件的 TTFB >
1s。

**F4 · 三级检索乱序**：T15 / T52 本该只走 structured 命中，但实现把所
有检索请求统一送进 vector store，既浪费 embedding 成本又可能返回语义
相近但事实错误的老 decision。**可观测信号**：`context_manifests.reason`
里 T15/T52 的标签是 `vector_retrieval` 而非 `history_recall_structured`；
monthly LLM embedding 调用量异常。另一种错法：索引用 L2 距离、查询用
cosine，返回结果与预期不符。

**F5 · manifest 丢失 compaction turn**：如果 compaction 作为"内部"步骤
不写 `context_manifests`，T8 / T38 / T58 就缺 3 条 manifest，`dropped_
count`、`compaction_version`、`reason` 字段都无处可查。**可观测信号**：
`SELECT COUNT(*) FROM context_manifests WHERE session_id=? AND reason
LIKE 'post_compaction%'` 不等于 3；出现 bug 时无法解释"第 9 轮为什么
少了 T2 的工具输出"。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Core Model / §Compaction — **建议补强**：明确写出 compaction 时哪
   些 category 绝对不可改写（`plan_state` / `decision` / `todo_state` /
   `error_state` 的 active 行），哪些可以降级（`tool_ref` / `active_file`
   → status=archived），哪些可以合并（`summary` 旧版保留、新版 active）。
   目前文字上能读出意图，但没有一个硬约束清单，工程实现很可能遗漏。
2. §Cheap Context Construction / §Proposed Schema §3 — **建议补强**：
   `context_manifests.reason` 目前是 VARCHAR(64)，但设计里没给枚举。
   S01 至少需要 `initial_turn` / `post_compaction_turn` /
   `history_recall_structured` / `history_recall_fts` /
   `history_recall_vector` / `large_tool_output_gated` /
   `final_delivery_summary` 等标签；建议在设计里把合法取值定义成枚举，
   否则 F4 / F5 这类失败无法用 SQL 聚合观测。
3. §History Chunks and Retrieval Index — **建议补强**：对"vector/FTS
   表不频繁 UPDATE/DELETE"这一铁律，目前只提了"append versions and
   soft-delete stale rows if needed"，但没写清楚 compaction 如何与向
   量表交互。S01 的 T38 归档了 32 条 `tool_ref`，它们对应的
   `session_history_chunks` 是否需要新 embedding？建议明确"降级不重
   嵌入，仅改 importance"作为默认策略。

**三条改进建议**：

- **Compaction 契约化**：新增一节 "Compaction Invariants"，用 5–8 条
  bullet 固定不变量（category 白名单、physical delete 禁止、manifest
  必写等），并给每条配一条 SQL assertion，作为 contract test 的输入。
- **Manifest reason 枚举化**：在 schema 旁边挂一个 reason lookup 表
  或 Rust enum，覆盖 S01–S14 将会触发的所有场景标签，避免字符串自由
  书写导致观测失效。
- **三级检索 cost budget**：建议在 §Cheap Context Construction 加一个
  每轮 retrieval 预算表（如每轮最多 1 次 vector 查询，structured / FTS
  各不限但有超时阈值），否则 S01 的 T31 这类多次失败回退会放大成本。
