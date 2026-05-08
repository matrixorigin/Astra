# S13 — 用户在一个 session 内现场迭代 7 版 SKILL.md：写一个"go 代码审查"个人 skill

> Status: Draft v0.1
> DAUT: **A2, A12, A15**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：老陈，某中厂基础架构组，写了 11 年 Go。嘴碎、务实、讨
厌啰嗦。他心里一直有一份"Go PR review 必看清单"——ctx 传参、goroutine
泄漏、map 并发、defer 在循环里、error wrap 丢栈帧这些东西——但一直
是写在 Notion 里给组里同事看。今天他决定把这份清单做成 astra 的**个
人 skill**（`source_type='user'`、`visibility='private'`、owner 就是自
己），以后每次 PR review 让 agent 按这个 skill 跑一遍。

**业务背景**：老陈不是 skill 作者，没耐心写一遍就到位。他要的是**边
写边试**——写一版、拿自己上周已合入的某个 PR 当 sandbox 跑一遍、看
结果再改。过程中必然会经历"写严了 agent 乱报警"、"写松了漏关键点"、
"前言 yaml 的 key 顺序不稳定"这些事。设计文档第 9 节 Personal Skills
说了 `user_skill_versions` append-only + `skills_registry.active_
version` 指针切换，本场景就是用它。

**一句话目标**：在**同一个 session** 里把一个个人 skill 从 v1 迭到
v7 并激活，期间任意一版都可以 rollback，**prompt 里永远只看得见
当前 active 的那一版**，不会把 7 版 14k tokens 的 SKILL.md 全塞给
LLM；最后还能跨 session 找到自己上周写的 Rust 版本做结构对齐。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 会话总轮数 | 约 15 回合 |
| `user_skill_versions` 最终行数 | 7（v1..v7，全部 append-only） |
| `skills_registry.active_version` 指针切换次数 | 4（v1→v2→v4→v7；T8 切回 v2） |
| 试用 run 次数 | 5（T3/T6/T7/T9/T10） |
| `session_state_items(category='skill_eval')` 行数 | 5（每次试用一行） |
| 单版 SKILL.md 大小 | ~2k tokens（含前言 yaml） |
| 跨 session skill 查询 | 1 次（T11，查 `skill_name LIKE 'rust%'`） |

## 2. 被测 DAUT

本场景把 A12（skill 版本化 / append-only / 指针切换）顶在中间；A2
（audit/projection/manifest 三张表不互相替代）负责证明"7 版历史不污
染 prompt、但审计链完整"；A15（structured next action）负责证明"切
回 v4"这种跨版本指令应该走 `skill_version_id=v4` 结构化意图，不走
LLM 现场理解自然语言回溯。

| 编号 | 压测方式 |
| --- | --- |
| **A12** | 7 次 submit = 7 行 `user_skill_versions`（`version='v1'..'v7'`、`content_hash` 彼此不同），共享同一 `source_id`；`skills_registry` 这边 `owner_user_id=老陈`、`source_type='user'`、`visibility='private'`，`content_hash` 随 `active_version` 指针切换；任何历史版 `user_skill_versions` 都不物理删除、不覆盖，**T13 随时可以把 active_version 指回 v3 复现当时的 40% 误报率**。 |
| **A2** | 7 版 SKILL.md 内容只存在 `user_skill_versions.content_markdown`（**cold**，永不进 prompt）；每版试用结果（命中数 / 可疑数 / 误报数）走 `session_state_items(category='skill_eval', item_key='go-code-review.v{N}')`（**warm**，进 prompt 但只最近两条）；每次激活切换走 `session_state_item_events(category='active_skill_ref', mutation='activate', previous_hash=<v_prev 的 content_hash>, next_hash=<v_new 的 content_hash>)`（**audit**，永不进 prompt）。三张表从不互相替代。 |
| **A15** | T8 老陈说"误报太多，v3 不行，先切回 v2 用"——这句话有 2 种解释路径：（a）走 `intent.skill_activate{skill_version_id='v2'}` structured event，直接切 `skills_registry.active_version`；（b）把这句话扔给 LLM 让它"回忆 v2 是啥"。A15 要求前者：UI 面板上老陈点"切回 v2"或发 slash command `/skill activate v2` 会生成一条结构化 event，context builder 读这条 event 改 active 指针，**不依赖 LLM 理解**。同理 T11 "跨 session 查 rust skill" 应该走 `SELECT FROM user_skill_sources WHERE user_id=? AND skill_name LIKE 'rust%'` 这条 structured path，而不是 RAG。 |

## 3. 会话时间线

下面列 12 个关键回合。每回合：**用户原话 / SQL 写入 / state 变化
（版本号、content_hash、active 指针、skill_eval 行）/ manifest 的
skills zone 字节数 / LLM 明确看不到什么**。语气跟老陈本人一样直白。

### T1 · 开 session，声明意图

**用户原话**：「我准备写个自己的 go code review skill，直接在这个
session 里改到定稿，你别啰嗦流程，我边写你边试。」

**SQL 写入**：`agent_sessions` 新 session；暂不写 `user_skill_
sources`（要等 v1 submit 才建）；`session_state_items(category=
'session_anchor', item_key='intent', payload_json={goal:'author personal
skill go-code-review', mode:'iterative'})` 一条。**注意**：skill 的
source 行要到 T2 submit v1 那一刻才 insert，否则会留一条空壳 source。

**manifest skills zone**：0 tokens（还没装）；`session_anchor` 写入
本 session 的目标。`reason='session_open'`。

### T2 · 粘 v1 SKILL.md 原文，server 建 source + v1 版本

**用户原话**：「v1 先来：前言 yaml `name: go-code-review / scope:
pr-review`，body 8 条 checklist——ctx 就不写了我下版加，先跑起来。」
（贴了大约 1.6k tokens 的 markdown）

**SQL 写入**：`user_skill_sources (source_id='src-gocr-老陈', user_id=
老陈, skill_name='go-code-review', visibility='private', status=
'active')` 首次 insert；`user_skill_versions (version_id='ver-v1',
source_id='src-gocr-老陈', version='v1', content_markdown=<原文>,
content_hash=sha256(<canonical>)=0xabc...01, token_estimate=1620,
status='draft')` 一条；`skills_registry` upsert：`owner_user_id=老陈,
source_type='user', visibility='private', active_version='v1',
content_hash=0xabc...01`；`skill_installations` 插入 `user_id=老陈,
skill_id=skills_registry.id, scope=?`（**scope 字段设计文档未定义——见
§8**）。

**state 变化**：`user_skill_versions` 行数 0→1；`active_version`
NULL→v1；`session_state_item_events(category='active_skill_ref',
mutation='activate', previous_hash=NULL, next_hash=0xabc...01)` 一条。

**manifest skills zone**：1650 tokens（v1 全文 + 前言元数据）。
`reason='skill_install'`。

### T3 · 拿上周的 PR 当试用靶子

**用户原话**：「上周我合的 PR #4712 那个 service/auth/token.go，你用
v1 跑一遍，给我列出它会命中 skill 里哪几条。」

**SQL 写入**：启 `agent_runs(run_id='run-t3', kind='skill_eval',
skill_registry_id=..., active_version='v1')` 一条；agent 读 PR diff
（走 tool），按 v1 的 8 条 checklist 对照 diff。`agent_run_events`
每条 checklist 检查一个 event。

**state 变化**：**本轮不写 `user_skill_versions`**（skill 没变）；
只在 run 走完后（T4）写 skill_eval 行。

**manifest skills zone**：1650 tokens（v1 全文保留在 prompt，因为
本轮就是要跑它）。

### T4 · 试用 run 完成，结果 4 命中 / 2 可疑

**用户原话**：（不说话，看 agent 输出）「嗯……你说 `defer
resp.Body.Close()` 在 if err 之后那条可疑是合理的；但 `ctx 没传`
这个最该命中的点你没抓到——因为 v1 里根本没这条。」

**SQL 写入**：`session_state_items(category='skill_eval', item_key=
'go-code-review.v1', payload_json={run_id:'run-t3', hits:4, suspects:2,
false_positives:0, missed_by_design:['ctx-propagation'], target_pr:
'#4712'}, token_estimate=180)` 一条；`session_state_item_events
(mutation='insert')` 一条。

**manifest skills zone**：1650（v1 不变）+ `skill_eval` 180。v1
SKILL.md 仍是 active，故继续渲染。

**LLM 看不到**：还没进场的 v2（不存在）；其他 user 的 skill；skills_
registry 里其他 source_type='system' 的内建 skill 里凡未 install 的全
部被过滤。

### T5 · "遗漏了 ctx 检查" → 提交 v2

**用户原话**：「v2 来：加一条 `每个跨进程/跨 IO 函数首参必须是
context.Context`；顺便给这条打 tag `severity: blocker`。」（贴了修
改后的全文，约 1.8k tokens）

**SQL 写入**：`user_skill_versions` 新增 1 行 `(version_id='ver-v2',
version='v2', content_hash=0xabc...02, token_estimate=1810, status=
'draft')`——**append-only**，v1 行**不动**；`skills_registry.active_
version` v1→v2，`content_hash` 跟着切；`session_state_item_events
(category='active_skill_ref', mutation='activate', previous_hash=
0xabc...01, next_hash=0xabc...02)` 一条。

**manifest skills zone**：1810 tokens（v2 全文）；v1 **完全不进
prompt**（其 token_estimate 仍在 `user_skill_versions.token_estimate`
列活着，但 context builder 的 skills zone 渲染器只拉 `active_version`
那行的 content_markdown）。

### T6 · v2 再试用

**用户原话**：「同一个 PR，v2 再跑。」

**SQL 写入**：新 `agent_runs(run_id='run-t6', active_version='v2')`；
完成后 `session_state_items(category='skill_eval', item_key='go-code-
review.v2', payload_json={run_id:'run-t6', hits:6, suspects:2,
false_positives:0})`。`item_key` 不同于 v1 那条，两条并存。

**state 变化**：`skill_eval` 总行数 1→2；skills_registry 无变化
（still v2）；manifest `skills` zone 仍是 1810 (v2)；`skill_eval`
zone 渲染最近 2 条（v2 + v1，各 ~180 tokens）。

**LLM 看不到**：v1 的 content_markdown；只看得到 v1 的 eval 结果摘
要（"v1 漏 ctx"）。

### T7 · v3 提交（goroutine 泄漏检查），误报率飙到 40%

**用户原话**：「v3：加 `每个 go func() 必须有显式退出路径——要么
ctx.Done() 要么 channel close 要么 errgroup 收敛`，漏一个算 blocker。」

**SQL 写入**：`user_skill_versions` 新增 ver-v3（content_hash=
0xabc...03，token_estimate=2040）；`active_version` v2→v3；再跑一次
`run-t7`。结果：`session_state_items(skill_eval, v3) payload=
{hits:9, suspects:1, false_positives:6}`——6 个 false_positive
（包括 server.go 的 main loop、一个 worker pool 的长期 goroutine）。
**误报率 6/15 = 40%**。

**state 变化**：`user_skill_versions` 3 行；active v3；skill_eval 3 行。
**注意**：40% 误报率是**数据层事实**，skill 本身依然 active——设计
文档没规定"skill 质量门槛"机制（**见 §8 改进建议**）。

**manifest skills zone**：2040 tokens（v3 全文）。

### T8 · "切回 v2 用" — structured intent, 不走 LLM 回溯

**用户原话**：「误报太多，v3 这版先放着别删，切回 v2，我想想白名
单怎么写。」

**SQL 写入**：**关键点**——UI 面板发出 `intent.skill_activate
{skill_registry_id=..., skill_version_id='v2'}` 一条结构化 event，
`session_state_item_events(category='active_skill_ref', mutation=
'activate', previous_hash=0xabc...03, next_hash=0xabc...02)` 一条。
`skills_registry.active_version` v3→v2，`content_hash` 切回 0xabc...02。
**v3 的 `user_skill_versions` 行不动**（status 仍 draft / 也可以打
`status='quarantined'` 但设计文档没定义这个状态，见 §8）。

**manifest skills zone**：1810 tokens（v2 全文回来）；v3 内容从
prompt 消失，只留 eval 摘要"v3 误报 6 条"在 skill_eval zone 最近
两条之一。**A15 验证点**：老陈这句话里"切回 v2"是自然语言，但系统
走的是 UI 点"激活 v2"→ structured event 这条路径，**LLM 不参与回溯
判断**（否则老陈哪天说"切回误报少的那版"agent 要自己在 3 个 eval 里
挑一个，不稳）。

### T9 · v4 提交（加白名单正则）

**用户原话**：「v4：goroutine 检查加白名单——函数名匹配
`^run[A-Z]|Serve$|mainLoop$` 的豁免。」

**SQL 写入**：`user_skill_versions(ver-v4, content_hash=0xabc...04,
token_estimate=2210)`；active v2→v4；跑 run-t9。结果：`skill_eval(v4)
payload={hits:8, suspects:2, false_positives:1}`——误报从 6 降到 1，
可接受。

**state 变化**：v4 active；`skill_eval` 4 行；session_state_item_
events 激活链：v1→v2→v3→v2→v4（共 4 次 activate，每次都有
previous_hash/next_hash）。

**manifest skills zone**：2210 tokens（v4 全文，含 regex 白名单语法）。

### T10 · v5 → v6 → v7，快速收敛到定稿

**用户原话**：「v4 的正则你理解得飘——`^run[A-Z]` 是想说大写开头的
run* 函数豁免，结果 v4 跑下来你把 `runtime.GC()` 调用所在函数也豁免了。
v5 我改成 yaml 结构化：`whitelist: {function_name_prefixes:['run'],
function_name_suffixes:['Serve','MainLoop'], match_case_sensitive: true}`。
v6 我再加两个 positive / negative example code block。v7 定稿激活。」

**SQL 写入**：连续三次 `user_skill_versions` insert：v5（content_hash=
0xabc...05, token_est=2180）、v6（0xabc...06, token_est=2450，含
examples code block）、v7（0xabc...07, token_est=2520）；每次都跑一次
eval（但 v5/v6 复用短 smoke 测不全量）。v7 跑完：`skill_eval(v7)
payload={hits:8, suspects:1, false_positives:0}`。激活指针 v4→v5→
v6→v7；`user_skill_versions.status` 对 v7 改为 `'published'`，v1–v6
均留 `'draft'`（**也可考虑 `'superseded'` 状态但设计文档未枚举，见
§8**）。

**manifest skills zone**：2520 tokens（v7 全文）；v1–v6 全部**不在
prompt**，仅通过 content_hash 在 audit 可追。

**LLM 看不到**：v1–v6 的 content_markdown 原文；只看得到最近两条
skill_eval（v7 + v4 或 v6，取决于 recency policy）。

### T11 · 跨 session："上周我写过的 Rust 那版拿来对齐"

**用户原话**：「上周我还写过一版 rust 的呢，结构应该跟这个差不多，你
也看看，给我对齐一下两版的结构。」

**SQL 写入**：**关键点**——A15 要求这里走 structured query，不走
RAG。UI 侧发 `intent.skill_search{owner_user_id=老陈, skill_name_
prefix='rust'}` → server `SELECT source_id, skill_name, created_at
FROM user_skill_sources WHERE user_id=老陈 AND skill_name LIKE 'rust%'
AND status='active'`，命中 1 行 `src-rust-code-review`；再
`SELECT version, version_id, content_hash FROM user_skill_versions
WHERE source_id=? AND version=(SELECT active_version FROM skills_
registry WHERE owner_user_id=? AND source_id=?)` 找到 active 版本的
content_markdown。

**state 变化**：**不创建新 skill、不装 skill**（老陈只是要参考）；
`session_state_items(category='skill_reference', item_key='rust-code-
review@active', payload_json={source_id, version_id, content_hash})`
一条，供本轮 prompt 渲染时把 Rust 版本 content 拉进来做对齐用，**本
session 的 active skill 仍是 go-code-review.v7 不变**。

**manifest skills zone**：2520（go v7）+ 2100（rust active 内容，
临时入 prompt 做对比）= 4620；`reason='cross_skill_alignment'`
（**新 reason，设计文档未枚举——见 §8**）。

### T12 · agent 对齐两版 skill 结构，给 diff 建议

**用户原话**：「diff 给我列一下——前言字段、checklist 分组、白名单
语法三轴，哪些该对齐哪些本就该不同。」

**SQL 写入**：无新 skill version 写入（只是分析）；生成的 diff 文本
作为 artifact 落盘 `session_artifacts(kind='skill_diff', ref='go-
v7-vs-rust-active')`；`session_state_items(category='decision',
item_key='skill-structure-alignment', payload_json={aligned_fields:
[...], divergent_fields:[...], action:'update-rust-frontmatter-next-
session'})` 一条——注意 action 留给"下一个 session"，本 session 不
改 rust skill。

**manifest skills zone**：回到 2520（只有 go v7；rust 内容完成对齐
任务即离场，不长期驻留 prompt）。`recent_tail` 里保留 diff artifact
的 preview_text ~400 tokens。

**LLM 看不到**：go skill v1–v6 原文；rust skill 的历史版本；其他
user 的 skill。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 7 版 SKILL.md × 每版 ~2k tokens，整塞 ≈ 14k 吃掉半个窗口 | A12 `user_skill_versions` append-only + `skills_registry.active_version` 指针；context builder 的 skills zone 渲染器**只拉 active 版本 content_markdown**，非 active 版本只保留 `(version_id, content_hash, token_estimate, summary_text)` 元数据，不进 prompt |
| 5 次试用每次都生成结果，不能都进 prompt | `session_state_items(category='skill_eval')` warm 投影 + context builder 仅渲染**最近 2 条 skill_eval**（recency + relevance）；历史 eval 原始计数仍可通过 `SELECT` 查到 |
| T8 "切回 v2" 的语义必须结构化 | A15 `intent.skill_activate{skill_version_id}` event 直达 `skills_registry.active_version`；不依赖 LLM 理解"切回误报少的那版" |
| 激活链路要可审计 | A2 `session_state_item_events(category='active_skill_ref', mutation='activate', previous_hash, next_hash)` 每次激活写一行，v1→v2→v3→v2→v4→v5→v6→v7 共 7 条 mutation 形成链 |
| T11 跨 session 找 rust skill 必须 structured | `SELECT FROM user_skill_sources WHERE user_id=? AND skill_name LIKE 'rust%'`，不是 full-text search 也不是 vector RAG（因为 skill_name 本就是 structured 字段） |
| T12 临时双 skill 同场，skills zone 预算暴涨 | manifest `reason='cross_skill_alignment'` + 临时预算放宽 + 完成即收回，不长期双占 |

## 5. 预期 state projection 演化

**T2（v1 首装后）**：

| 项 | 值 |
| --- | --- |
| `user_skill_sources` | 1 行（go-code-review） |
| `user_skill_versions` | 1 行（v1，content_hash=0xabc...01） |
| `skills_registry.active_version` | v1 |
| `skill_installations` | 1 行（scope 字段未定义，见 §8） |
| `session_state_items(skill_eval)` | 0 |
| `session_state_item_events(active_skill_ref)` | 1 行（NULL→0xabc...01） |

**T7（v3 误报率 40% 之后）**：

| 项 | 值 |
| --- | --- |
| `user_skill_versions` | 3 行（v1/v2/v3 并存） |
| `active_version` | v3 |
| `session_state_items(skill_eval)` | 3 行（v1/v2/v3） |
| `active_skill_ref` 激活链 | 3 条（NULL→01→02→03） |
| **质量信号**：v3 payload_json 里 false_positives=6 | 数据层可见，但**无自动降权**（见 §7 F6） |

**T10（v7 定稿激活后）**：

| 项 | 值 |
| --- | --- |
| `user_skill_versions` | 7 行（v1..v7） |
| `active_version` | v7 |
| `user_skill_versions.status` 分布 | v7='published'，v1..v6='draft'（也可扩 'superseded'，见 §8） |
| `session_state_items(skill_eval)` | 5 行 |
| `active_skill_ref` 激活链累计 | 7 条（含 T8 的回切） |
| rollback 可用性 | 任选 v1..v6 之一重新 `skill_activate{skill_version_id=v_k}` 即可原样复现当时 prompt |

**T12（跨 skill 对齐后）**：

| 项 | 值 |
| --- | --- |
| 本 session `user_skill_sources` | 仍 1 行（未建新 source） |
| `session_state_items(skill_reference)` | 1 行（指向 rust-code-review active 版本） |
| `session_state_items(decision)` | 1 行（alignment 决策） |
| 跨 session 查找命中 | 1 条 `user_skill_sources` row（rust-code-review） |

## 6. Token 节省估算

**朴素方案**：Claude Sonnet 4.5，200k 窗口。每版 SKILL.md 都塞 +
每次 eval 结果全塞。

- v1..v7 全文 7 × 2200 ≈ **15.4k tokens** 光 skills zone 就吃掉。
- 5 条 eval 结果全塞（每条 payload + 中间推理 ~600 tokens）≈ **3k**。
- T10 本轮朴素 ≈ 15.4k (skills) + 3k (eval) + 10k (transcript tail)
  + 2k (anchor/tool) ≈ **30k**。
- T12 朴素（还要加 rust skill active 全文 + rust 历史） ≈ **36k+**。

**本设计方案**（按 zone 加总）：

| zone | T2 | T7 | T10 | T12 |
| --- | --- | --- | --- | --- |
| system_static | 1200 | 1200 | 1200 | 1200 |
| tool_schemas | 1400 | 1400 | 1400 | 1400 |
| skills (active only) | 1650 | 2040 | 2520 | 2520+2100 |
| skill_eval (last 2) | 0 | 360 | 360 | 360 |
| session_anchor | 300 | 300 | 350 | 400 |
| recent_tail | 400 | 1200 | 1800 | 2000 |
| summary | 0 | 0 | 0 | 0 |
| workspace | 200 | 200 | 200 | 200 |
| **合计** | **5150** | **6700** | **7830** | **10180** |

**对比**：

| 节点 | 朴素 (tokens) | 本设计 (tokens) | 节省 |
| --- | --- | --- | --- |
| T7 | ~22k | 6.7k | 70% |
| T10 | ~30k | 7.8k | 74% |
| T12 | ~36k | 10.2k | 72% |

**关键结论**：节省来自**三件事**，缺一不可：(a) append-only +
`active_version` 指针让"7 版并存 × 只 render 1 版"成为必然而非偶然；
(b) `skill_eval` 作为 warm 投影而不是被塞进 recent_tail 自然语言里，
让"最近 2 条 eval"的规则能被 SQL 精确表达（`ORDER BY created_at
DESC LIMIT 2`）；(c) T11 跨 session 查询走 structured 路径，只把命
中的 1 版 rust 内容拉进来，而不是把老陈所有历史 skill 全 RAG 一
把。三件事都写在设计文档 §9 Personal Skills，但（c）的 structured
skill lookup API 在文档里只提了"selector 过滤 user ownership"一
句，需要落成明确端点（见 §8）。

## 7. 潜在失败模式

**F1 · `content_hash` 对前言 yaml key 顺序敏感**：老陈 v1→v2 的
修改只加了一条 checklist，但 v2 的前言 yaml 自动化工具把 key 重排
过（原 `name / scope / owner` 变 `owner / name / scope`），结果
`content_hash` 会把"纯排序变化 + 新增一条"这两件事搅在一起算，之后
用户问"v2 相对 v1 改了什么" agent 回答困难；更糟的是如果老陈哪天把
v2 原样再粘一遍，`content_hash` 跟原 v2 不同，会 insert 一条"内容
相同的新版本"。**可观测信号**：同一份 content_markdown 经 2 次
submit 生成 2 个不同 `content_hash`。**防守**：hash 前先做
canonicalization——yaml key 字典序、空白归一、末尾 newline 归一。
设计文档 §9 未规定 hash 输入的规范化形式（见 §8）。

**F2 · 激活指针切换的原子性**：老陈在 web 端 T8 点"切回 v2"，同时
他的同事（share 模式下）或他自己另一个 tab 正在点"激活 v4"。若
`skills_registry.active_version` UPDATE 没带 `WHERE content_hash=
<expected_prev>` 的 CAS 条件，两次 UPDATE 会互相覆盖，`session_
state_item_events` 两条 activate 记录的 `previous_hash/next_hash`
链会断。**可观测信号**：audit 里相邻两条 activate event 的
`previous_hash` ≠ 上一条的 `next_hash`。**防守**：activate 走
`UPDATE ... WHERE content_hash = ?`（乐观锁）并在失败时把最新
hash 回吐给调用方。

**F3 · `skill_eval` payload_json 的 schema 未定义**：v1..v7 各版的
`payload_json` 目前里各写各的——v1 用 `{hits, suspects}`，v3 加了
`false_positives`，v4 加了 `target_pr`，v7 开始写 `{hits, suspects,
false_positives, missed_by_design, target_pr, run_id, version_ref}`。
因为 `session_state_items.payload_json` 是 LONGTEXT，schema 松；三个
月后产品想做"skill 质量看板"时会发现同一 category 下 payload 不
齐。**可观测信号**：SELECT 聚合 false_positives 列时一半行是 NULL。
**防守**：把 skill_eval 抽到一等表 `user_skill_evaluations`（见 §8
改进建议）。

**F4 · "切回 v4" 的语义：本 session 激活还是全局激活？**：
`skills_registry.active_version` 这列本身没有 session 维度。老陈在
session A 里把 go-code-review 切到 v4 做试验，他的另一个 session B
（同一 user）本来在用 v7，被意外拖下水。设计文档 §9 说
"skill_installations 标记 installed for the same user"，但未明确
active_version 是 per-user 还是 per-session 生效。**可观测信号**：
session B 下一轮开始时发现 active 版本莫名变了；UI 上 session B
的 skill 面板显示与上次不符。**防守**：引入 `skill_installations.
scope IN ('user','session','workspace')` 字段（见 §8）。

**F5 · 跨 session skill 查找的权限与可见性**：T11 老陈查"上周那版
rust skill"，SQL 里必须带 `WHERE user_id = 老陈 AND (visibility =
'private' OR source_user_id IN <老陈 subscribed authors>)`。漏掉
visibility 检查会把别的 user 的 private skill 命中出来；不带
`user_id` 约束会跨租户泄漏。**可观测信号**：同样的 skill_name 前
缀，两个不同 user 查到同一 source_id。**防守**：`user_skill_sources`
查询路径必须强制 `user_id` filter + `visibility` 白名单。

**F6 · v3 误报率 40% 但系统不自动降权**：T7 之后 v3 是 active，
prompt 里渲染的就是 v3 全文——即使 `skill_eval(v3).false_positives=
6` 这条数据在 warm 可见，context builder 也不会"因为误报率高就拒绝
把 v3 作为 active 渲染"。设计上"skill 质量信号 → active 选择"这条
反馈回路是**缺失的**。T8 老陈必须**手动**切回 v2。**可观测信号**：
有 skill 跑到 `false_positives / hits > 0.3` 后仍稳居 active；没
任何 warning 贴在 skill 面板。**防守**：把 `user_skill_evaluations`
+ "激活前质量门" 做成一等机制（见 §8 改进建议）。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. **§Proposed Schema §9 Personal Skills** — 建议补强：(a) 规定
   `content_hash` 的规范化输入形式（yaml key 字典序 / 空白 / 末尾
   newline 归一），否则 F1 会把 hash 变成不稳的 content 指纹；
   (b) 补 `user_skill_versions.status` 枚举 `draft / published /
   superseded / quarantined`，其中 quarantined 用于"误报率超阈" 的
   T7 场景；(c) `skill_installations` 表加 `scope VARCHAR(32)
   NOT NULL DEFAULT 'user'`，枚举 `user / session / workspace`，
   解决 F4 的跨 session active 污染。
2. **§Proposed Schema §2 Session State Projection** — 建议新增
   category 专用表 `user_skill_evaluations(version_id, run_id,
   target_ref, hits, suspects, false_positives, missed_by_design,
   hit_rate, false_positive_rate, created_at)`，替代用
   `session_state_items(category='skill_eval')` + 松 JSON。收益：
   F3 的 schema 一致性、F6 的质量门可以直接写成 `SELECT ... WHERE
   false_positive_rate > 0.3` 的告警条件。
3. **§Context Assembly / Rendering Policy** — 建议补强：明确
   `skills` zone 的渲染规则：(a) 只渲染 `active_version` 的
   content_markdown；(b) 最近 N=2 条 `user_skill_evaluations`
   作为 warm 补充；(c) `reason` 枚举需补两值：
   `cross_skill_alignment`（T11/T12 双 skill 临时同场）、
   `skill_quality_review`（专为 skill 作者回顾 eval 趋势用）。

**三条改进建议落地项**：

- **`user_skill_evaluations` 独立表**：从 `session_state_items` 里
  把 skill 试用反馈提出来，字段明确、可聚合、可做激活前质量门。
- **`skill_installations.scope` 字段**：解决"active_version 到底
  对谁生效"的歧义（user / session / workspace 三值），规避 F4。
- **content_hash 规范化契约**：在 §9 里固定 hash 输入为
  `canonicalize(manifest_json) + "\n" + normalize(content_markdown)`
  的字节串，并提供一段参考伪码；避免前言 yaml key 排序、尾换行、
  制表符/空格混用引起的 hash 抖动。
