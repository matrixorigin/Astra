# S08 — 某公司 DBA 做 MySQL→MatrixOne 迁移前审计

> Status: Draft v0.1
> DAUT: **A3, A9, A10**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：老陈，某垂类电商公司 DBA，15 年经验，MySQL 8.0 专
家，最近在评估把主库迁到 MatrixOne（HTAP + vector + fulltext 一把
梭）。说话简短、术语密，回一句比写一句省事，但凡 agent 输出一整
屏他就一句「就列出不兼容的」打断。不关心 UI 美观，只在意两件
事：（a）别让他自己去翻 3GB 的 dump；（b）第二天打开这个 session
还要能查回昨天哪个文件在哪个 FK 上炸了。

**业务背景**：公司要在下个季度把主库 MySQL 迁 MatrixOne，老陈负
责迁前 schema / SQL 兼容性 / 慢查询负载三样审计。用 astra web
agent 驱动一串数据量超大的工具：`pg_dump --schema-only` 导整库结
构（3GB 纯 SQL）、本地 1000+ 业务 SQL 脚本跑 `sql_compat_scan`
扫不兼容语法、把近 7 天 slowlog（800MB）喂给 `slow_query_
analyzer` 分析 TOP 50 慢查询，最后让 agent 合成一份迁移风险报告。
老陈全程在工位 Chrome 开着 web agent，会话要持续一整天。

**一句话目标**：把 ~5GB 的结构化 + 半结构化 tool 输出全部过一遍，
生成一份迁移风险报告；prompt 永远不能塞入任一条原始大输出；一天
后的二次查询（「第 3 条慢查询为啥慢？」「昨天那个 FK 报错在哪个
文件？」）要在亚秒级定位到对应 artifact 切片。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 会话总轮数 | 12 回合 |
| 会话持续时间 | 7 小时（首日）+ 次日 10 分钟追查 |
| 最大单次 tool 输出 | 3,221,225,472 B（pg_dump 3GB） |
| 次大单次 tool 输出 | 838,860,800 B（slowlog 800MB） |
| `session_tool_outputs` 行数 | 约 1006 行（1 pgdump + 1000 scan + 1 agg + 3 slowlog + 1 report） |
| `session_artifacts` 行数 | 6（pgdump / scan_detail / slowlog_raw / top50_digest / risk_report / decision_log） |
| 累计 artifact byte_size | ≈ 4.02 GB |
| Prompt 累计 tool_previews token | ≤ 18k（12 轮相加） |

## 2. 被测 DAUT

S08 的核心矛盾是「单次 tool 输出体积 × 长尾结构 × 历史回查」，下
列 3 条假设被同时压测。

| 编号 | 压测方式 |
| --- | --- |
| **A9** | T3 的 3GB pg_dump、T7 的 800MB slowlog **绝不进 prompt**；必须作为 `session_tool_outputs` 一行 + `session_artifacts` 一行双投影落库：`byte_size` 精确到字节、`content_hash` 走规范化（见 §8 建议 3）后的 sha256、`artifact_ref` 指向 OSS 对象、`preview_text ≤ 1000 字符`且末尾带 `truncated at X bytes, full in artifact Y`。T5 的 1000 个 file scan 结果要在单行聚合 preview（TopN 摘要 + 计数），而不是拼接 1000 行原文。`tool_previews` zone 每轮 ≤ 1500 tokens 硬上限。 |
| **A3** | T12 用户刷新页面、UI 重绘 tool 时间轴时，**只**查 `session_transcript_items` + `session_tool_outputs` 两个索引投影（`idx_tool_outputs_user_session_created`），**不**打开 artifact 原文；即使会话里有 3GB + 800MB 两块巨无霸，first-paint 也要在 500ms 内完成。热路径绝不扫 JSON、绝不拉 raw。展开某一条 tool output 时才按需 GET artifact（presigned URL 直连 OSS，不走 API server）。 |
| **A10** | T8 老陈问「第 3 条慢查询为啥慢」时，检索走三级：（1）structured filter：`WHERE session_id=? AND tool_name='slow_query_analyzer' ORDER BY created_at DESC LIMIT 1` 命中 `idx_tool_outputs_tool_created`；（2）拿到 `artifact_ref` 后走 artifact 内的 chunk 索引（query_rank=3 的 offset），按需加载那一段 EXPLAIN plan + 调用栈，**不** load 800MB 全部。T9 问 "FK 报错在哪个文件" 时走 tier 2：`MATCH(preview_text) AGAINST ('foreign key')`，返回文件列表；vector 检索在本场景**不触发**（有精确关键词时不浪费）。 |

## 3. 会话时间线

12 个回合覆盖：T1 任务交付、T2 计划、T3 pg_dump 3GB、T4 manifest
快照、T5 1000 文件扫描聚合、T6 structured filter 定位不兼容、T7
slowlog 800MB、T8 按需 chunk 加载、T9 FTS 回查、T10 风险报告合
成、T11 presigned 下载、T12 次日刷新页面。每回合字段：**用户原话**
（模拟真实 DBA 语气）、**state 变化**（具体 byte_size / content_hash
策略 / `session_tool_outputs` + `session_artifacts` 双投影写入）、
**manifest zone**（tokens 分配）、**LLM 看不到**（体积巨大但被挡在
prompt 之外的那部分）。

### T1 · 09:10 开局「做迁前审计」

**用户原话**：「下季度要把主库 MySQL 迁到 MatrixOne，迁前做个审
计：（1）schema 整个导出来过一遍；（2）repo 里 1000 多个业务 SQL
脚本扫不兼容语法；（3）最近 7 天 slowlog 出一份 TOP N 分析。完
了出份迁移风险报告。」

**state 变化**：新增 `anchor`（目标：MySQL→MatrixOne 迁前审计）；
`workspace_state` 固定在 cloud（DB 生产库凭据只在 cloud workspace
可用，edge 工具被请求级 runtime capability 约束排除）；`plan_state` version=0；
`session_tool_outputs` / `session_artifacts` 均为空。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1800（保留
pg_dump_schema、sql_compat_scan、slow_query_analyzer、fts_scan、
renderer 5 类，其余裁掉）/ `session_anchor` 240 / `plan_todo` 0 /
`tool_previews` 0。总 ≈ 3240，`reason='initial_turn'`。

**LLM 看不到**：老陈脑子里"上次 HR 系统迁过一次 Oracle→MySQL
被炸"的经验（没说出来）。

### T2 · 09:15 agent 拟计划 + 写 todos

**用户原话**：（隐式）agent 基于 T1 输入自发规划，老陈默认同意。

**state 变化**：`plan_state` version=1，phase='audit_v1'；`session_
todos` 批量 insert 5 条（pg_dump schema 导出 / 1000 文件扫描 /
slowlog 分析 / 风险报告合成 / decision log 汇总），均 `status=
'active'`，`source='agent'`；`session_todo_deps` 4 条（报告 ← 前三
个并行前置；decision_log ← 报告）；新增 1 条 `decision`
（key='audit.scope.v1'，"审计范围锁定 schema + sql_syntax +
slowlog，不涉及数据量"）。

**manifest zone**：`plan_todo` 320 / `session_anchor` 260 / `tool_
schemas` 1800 / `system_static` 1200 / `recent_tail` 420。总 ≈
4000，`dropped_count=0`。

**LLM 看不到**：无。

### T3 · 09:20 执行 pg_dump（3GB schema-only）

**用户原话**：「先跑 pg_dump，schema only，整库。」

**state 变化**：agent 发起 `pg_dump_schema` 工具调用，执行 ~240
秒，原始输出 `3,221,225,472 B`（3GB 纯文本 SQL）。工具执行器流
式写 OSS：`artifact_ref='artifact:pgdump-prod-20260601-schema'`，
`session_artifacts` 新增 1 行（artifact_type='tool_output_raw'，
`byte_size=3221225472`，`content_hash=sha256(normalized)` — 规范化
剔除 dump header 里的 `-- Dumped on 2026-06-01 09:24:13` 和
`-- Dumped by pg_dump version 16.1` 两行防时间戳污染 hash，见 §8
建议 3）。`session_tool_outputs` 新增 1 行：`tool_name='pg_dump_
schema'`, `status='ok'`, `byte_size=3221225472`, `token_estimate=
210`, `preview_text`（800 字符）= 前 600 字符的 SET 语句 + `CREATE
SCHEMA` 头部 + 末尾自动追加 "...truncated at 800 chars / 3221225472
bytes, stats: tables=482, views=37, sequences=12, routines=156, FK=
1204, indexes=3890; full in artifact:pgdump-prod-20260601-schema"。
**原始 3GB 数据从未进过 API server 内存**（走 tool executor 流式上
传 OSS，executor 返回给 runtime 的仅是 metadata + preview）。

**manifest zone**：本轮 agent 只需 ACK 工具结果，不需要 reasoning
over 全部 schema。`tool_previews` 240（1 条 pg_dump preview）/
`plan_todo` 320 / `recent_tail` 600 / 其余照旧。总 ≈ 4160。

**LLM 看不到**：3,221,224,672 字节的 `CREATE TABLE` / `ALTER
TABLE` / `CREATE INDEX` 原文、全部 FK 定义原文、全部 view 定义原
文。全部仅在 artifact OSS 对象里。

### T4 · 09:25 agent 对 pg_dump 做结构抽取（manifest 快照验证）

**用户原话**：（隐式）agent 要对 schema 做进一步结构化抽取，供后
续报告使用。

**state 变化**：agent 发起 `pg_schema_structurize` 工具，该工具**在
tool executor 侧**直接消费 artifact OSS（不经 API server），扫描 3GB
dump 后输出一份 8MB 的结构化 JSON（每个 table / 索引 / FK 一条
记录）。JSON 落第二个 artifact：`artifact:pgdump-prod-20260601-
struct-json`，`byte_size=8388608`，`content_hash=sha256(json)`，
**依赖关系**：`session_artifacts.derived_from='artifact:pgdump-prod-
20260601-schema'`（说明派生自 raw dump，GC 时要连带判断）。
`session_tool_outputs` 再写 1 行，preview_text 900 字符：表数量、
索引分布直方图、FK 跨 schema 比例、高风险模式列表（比如
`ENUM` / `AUTO_INCREMENT` 触发器）。

**manifest zone**：`tool_previews` 520（2 条 preview 累加：raw
dump 240 + structurized 280）/ `plan_todo` 320 / `recent_tail` 700 /
其余照旧。总 ≈ 4460。**验证不变量**：`tool_previews` zone 始终只
引用 `preview_text + artifact_ref`，不引用 payload_ref；context
builder 的渲染器 SELECT 明确带 `WHERE byte_size < 16384` 兜底保
险（超过 16KB 的 tool 输出强制走 preview 路径）。

**LLM 看不到**：8MB 结构化 JSON 原文；3GB raw dump 任何字节。

### T5 · 09:40 扫描 1000 个 SQL 脚本（聚合写入）

**用户原话**：「repo 下 `/apps/*/sql/*.sql` 全扫一遍，MySQL 方
言、用 MatrixOne 语法。」

**state 变化**：agent 发起一次**批量 tool 调用** `sql_compat_scan`
（input=glob，tool executor 内部 fan-out）。executor 返回 1 条聚合
result + 1000 条 per-file record。写入策略：（a）`session_tool_
outputs` 批量 insert **1001 行**（1 聚合 + 1000 per-file），走单事
务 + 多值 `INSERT INTO ... VALUES (...), (...), ...` 拆成 10 批 × 100
行，避免 N+1（见 §7 F2）；per-file 行的 `tool_name='sql_compat_
scan'`, `status` ∈ {'ok','fail','warn'}（**枚举必须严格对齐** 见 §7
F5），`preview_text` 取单行 80 字符格式：
`"path/to/payments.sql: 3 issues [ROW_FORMAT, UTF8MB3, FT_PARSER]"`；
（b）聚合行 `status='fail'`, `preview_text` 取 1200 字符：
`"scanned=1000, fail=137, warn=214, ok=649, top_issue_types:
ROW_FORMAT_COMPRESSED=82, INTO_OUTFILE=31, charset_utf8mb3=24,
generated_column_virtual=17, fulltext_parser_ngram=14; truncated /
full in artifact:sql-scan-detail-20260601"`；（c）per-file 详细报告
（每个文件含行号 + 片段）汇总为 1 个 artifact `artifact:sql-scan-
detail-20260601`，`byte_size≈56MB`，`content_hash=sha256(normalized)`
（规范化把扫描时间戳剔除）。

**manifest zone**：本轮 prompt 只引用**聚合行的 preview_text**（1200
tokens 级别 → 压到 480 tokens 渲染），1000 条 per-file **不**进
prompt（见 §4 压力点 P1）。`tool_previews` 480 / `plan_todo` 360 /
`recent_tail` 900 / 其余照旧。总 ≈ 4500。

**LLM 看不到**：1000 条 per-file preview（累计 ~80KB，~20k
tokens，如果塞进去直接打爆预算）；56MB artifact JSON 原文。

### T6 · 09:50 老陈「就列出不兼容的给我看」

**用户原话**：「agent 废话少点，就列出 fail 的文件名 + 不兼容点，
别给我贴全文。」

**state 变化**：agent 不再调新工具，而是发一次 **structured filter
检索**：`SELECT output_id, title, preview_text FROM session_tool_
outputs WHERE session_id=? AND tool_name='sql_compat_scan' AND
status='fail' ORDER BY created_at ASC`，命中 `idx_tool_outputs_
tool_created`（复合索引覆盖），返回 137 行、每行仅 preview_text
（平均 80 字符），合计 ~11KB → ~2800 tokens。agent 渲染为 markdown
列表作为 assistant 回复。`session_tool_outputs` **无新增写入**（纯
读操作）；`session_state_items` 新增 1 条 `tool_ref`
（key='scan.fail.cursor.v1'，payload 记住 137 这个数与检索 hash，
供下次定位）。

**manifest zone**：本轮 agent 的 reasoning 输入仅是 retrieved 137 行
的 preview_text（渲染阶段 LLM 不需要"看见"全部 137 行原文，LLM
只看到摘要 + 前 20 条 + "剩 117 条已附录到 UI"）。`tool_previews`
1100（137 × 80 = 10.96KB 之中取前 20 条 + 类型直方图 = 1100
tokens，**硬撞 1500 上限前主动降档**）/ `plan_todo` 360 / `recent_
tail` 700。总 ≈ 4800。

**LLM 看不到**：其余 117 条 per-file preview（UI 侧分页拉取）；56MB
artifact JSON 原文。

### T7 · 10:15 慢查询分析（slowlog 800MB）

**用户原话**：「近 7 天 slowlog 拉下来，TOP 50 慢查询。」

**state 变化**：agent 调 `slow_query_analyzer`（tool executor 消费上
游 OSS 的 slowlog，`byte_size=838,860,800`），在 executor 侧完成解
析 + fingerprint 聚合 + TOP50 digest。两个 artifact：（a）raw log
`artifact:slowlog-raw-20260525-20260601`，`byte_size=838860800`，
`content_hash=sha256(raw)` — 此处**有意不规范化**（日志本来就是
time-seried），`retention_days=30`；（b）结构化 TOP50 digest
`artifact:slowlog-top50-20260601`，`byte_size=412000`，`content_
hash=sha256(json)`，`derived_from` 指向 raw。`session_tool_
outputs` 新增 1 行聚合：`tool_name='slow_query_analyzer'`, `status=
'ok'`, `byte_size=838860800`, `token_estimate=280`, `preview_text`
1000 字符 — 前 800 字符是 TOP 10 query digest（每条 ~80 字符：
`rank=3 digest=SELECT ... FROM orders o JOIN items i ... qps=12.4
avg_ms=1842 p99_ms=8321 scan=full`），末尾 200 字符全局指标
（total_slow=4812, unique_digests=340, top10_share=71%）+
"truncated / full in artifact:slowlog-top50-20260601"。

**manifest zone**：`tool_previews` 已经到 **1100（T6）+ 280（T7）=
1380**，逼近 1500 上限，context builder 在 T7 渲染前做一次 LRU
降级，把 T5 那条聚合 preview 压到 240、T6 的 137 行取样压到
580，给 T7 空出位置。最终 `tool_previews` 1480 / `plan_todo` 380
（T7 前置 todo 标 done） / `recent_tail` 1000。总 ≈ 5100。

**LLM 看不到**：800MB raw slowlog 原文；412KB digest JSON 原文；
T5 的 1000 per-file preview；T6 的 117 条补充。

### T8 · 10:40 老陈「第 3 条慢查询为啥慢？」

**用户原话**：「第 3 条那个 join 慢的，explain 给我看看，为啥
慢。」

**state 变化**：检索走 **A10 三级**：（1）structured filter 命中
T7 聚合行，拿 `artifact_ref='artifact:slowlog-top50-20260601'`；
（2）按 digest rank=3 读 digest artifact 的该 chunk（digest JSON 本
身才 412KB，可以整块拉回）— 包含该 digest 的 10 条代表性采样、
EXPLAIN plan、涉及表/索引列表；（3）若仍需原始事件，按 chunk
索引对 raw slowlog artifact 做 **byte-range GET**（只读该 digest
指向的 offset 区间，约 1.8MB），绝**不**加载 800MB 全量。加载的
1.8MB 切片在 tool executor 侧解析为结构化 JSON（~22KB）后才
send 给 runtime，runtime 再渲染成 preview_text（800 字符）+ 落
`session_tool_outputs` 一行 `tool_name='slow_query_explain_for_
rank3'`, `parent_output_id`=T7 聚合行 output_id（延续链）。

**manifest zone**：`tool_previews` LRU 再降级，留位置给 T8 的
explain preview 400 tokens / T7 聚合降到 180 / 其余保留。总 ≈
5000。

**LLM 看不到**：800MB raw slowlog 的其余 99.78%；TOP50 digest
JSON 的其他 49 条。

### T9 · 11:10 老陈「昨天那个 FK 报错在哪个文件？」

**用户原话**：「等下——上午扫脚本的时候有个 foreign key 报错，
是哪个文件来着？」

**state 变化**：`NextActionExtractor` 识别出是**历史回查**，优先级：
（1）先试 structured filter — `WHERE tool_name='sql_compat_scan'
AND status IN ('fail','warn')` 返回 137 行太多、无法靠 status 精确
定位；（2）进入 tier 2 **full-text**：`MATCH(preview_text) AGAINST
('+foreign +key' IN BOOLEAN MODE)`（`preview_text` 字段上有 FTS
index，见设计 §Retrieval Tiers），返回 4 行，含文件路径。FTS 在
`session_tool_outputs` 上**append-only**（没有 UPDATE/DELETE 频繁
导致索引 bloat，符合 A10 + §SQL Performance 的约束）。agent 把 4
个文件名 + 具体不兼容点按 preview_text 拼成一条 assistant 答复。
**不触发 vector 检索**（关键词精确，无需语义近似，tier 3 省下）。

**manifest zone**：本轮纯检索读，`tool_previews` 保持 T8 的状态
+ 360 tokens（4 条 FTS 命中）/ 其余照旧。总 ≈ 5120。

**LLM 看不到**：FTS 检索用到的倒排索引本身；56MB scan_detail
artifact 任何字节（FTS 只搜 preview_text 列）。

### T10 · 11:40 合成迁移风险报告

**用户原话**：「上面这些齐了，出份迁移风险报告：schema 风险 +
SQL 兼容性 + slowlog TOP 风险，分级。」

**state 变化**：agent 发起 `render_migration_risk_report` 工具，
input=多个 `artifact_ref`（pgdump-struct-json、sql-scan-detail、
slowlog-top50-20260601）+ 本会话 `decision` 全部 active 行。
executor 在 cloud 侧读取 artifact 并合成 markdown（~180KB）。落
`session_artifacts`：`artifact:migration-risk-report-20260601-v1`，
`byte_size=184320`，`content_hash=sha256(normalized markdown)`，
`provenance_refs=['artifact:pgdump-prod-20260601-struct-json',
'artifact:sql-scan-detail-20260601', 'artifact:slowlog-top50-
20260601']`。`session_tool_outputs` 新 1 行，preview_text 800 字
符为报告目录 + 每章顶层要点（H1/H2 级）+ 高危风险数（
"high=14, medium=37, low=82"）+ "full in artifact:migration-risk-
report-20260601-v1"。`session_todos` 中"风险报告合成"这条 `status
='done'`。

**manifest zone**：`tool_previews` 压缩 T8 explain preview 到 180
+ T10 报告 preview 400 / `retrieved_facts` 440（含全部 active
decision） / `plan_todo` 280（只剩 "decision_log 汇总" 一条）。总
≈ 5000。

**LLM 看不到**：报告 markdown 全文 180KB；任何 artifact raw 内
容（报告生成完全在 tool executor 侧完成，runtime 只拿 metadata）。

### T11 · 11:55 老陈下载报告

**用户原话**：「把报告下下来，我发群里。」

**state 变化**：UI 对 `artifact:migration-risk-report-20260601-v1` 调
`GET /api/artifacts/:id/download`，服务端**不**流式代理文件，而
是返回 `302 Location` 到 OSS presigned URL（TTL 5 分钟）。浏览
器直接从 OSS 下载 180KB markdown。API server 内存占用 **0 字
节的文件数据**（见 §7 F6：若这里改成代理流式，3GB artifact 场景
会把 API server 内存打爆）。`agent_events` 写 1 条 `artifact_
downloaded` event（audit 留痕），`session_tool_outputs` **无新增**。

**manifest zone**：与 T10 基本一致，仅 `recent_tail` +120 tokens
记录"老陈下载 v1 报告"。总 ≈ 5100。

**LLM 看不到**：presigned URL 本身（带签名参数，安全不上 prompt）。

### T12 · 次日 09:00 老陈刷新页面，UI 重绘时间轴

**用户原话**：（无交互，纯页面刷新，浏览器重新打开 session。）

**state 变化**：Web UI 请求走 Web display history 模式（设计
§MatrixOne Load Model）：（1）`SELECT item_seq, item_type, role,
created_at, preview_text, payload_ref FROM session_transcript_items
WHERE session_id=? AND is_deleted=FALSE ORDER BY item_seq DESC
LIMIT 50`；（2）对其中 `item_type='tool_result'` 的条目再一次
`SELECT output_id, tool_name, status, byte_size, preview_text,
artifact_ref FROM session_tool_outputs WHERE user_id=? AND session_id=? AND
output_id IN (?)`，命中 `idx_tool_outputs_user_session_created`。两查询
合计 ~3ms，读回约 50 行 × 平均 800 字节 preview = 40KB 传输。
3GB pg_dump、800MB slowlog、56MB scan_detail、180KB 报告等
artifact **一个字节都不读**。UI 把 tool output 条目渲染为"卡片折
叠态"，显示 tool_name + byte_size + 时间 + preview 前 80 字符；
用户点击"展开"才触发 `GET` artifact（presigned），且大部分用户在
第二天刷新后只需要看报告那一条，其他 artifact 实际打开率 < 5%。

**manifest zone**：（本轮不进 LLM，纯 UI 回放，不新建 context
manifest。）**重要验证**：刷新后的 first-paint 应该 < 500ms；
`context_manifests` 无新增行；`agent_events` 只有一条 `session_
opened_for_display`。

**LLM 看不到**：任何 artifact 原文（本回合 LLM 根本没被调用）。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 单次 3GB tool 输出如果误进 prompt 即爆预算 | A9：`session_tool_outputs` 强制分 `preview_text` + `artifact_ref`，`byte_size > 16KB` 的输出 prompt 渲染器硬路由到 preview 分支；原始数据仅在 tool executor 与 OSS 之间流动，API server / runtime 内存从不承载 raw |
| 1000 文件 fan-out 扫描，per-file 详细结果若都进 prompt 约 20k tokens，爆 `tool_previews` 上限 | A9：聚合行 + per-file 行双写，prompt 侧只引用聚合行 preview；per-file 行只供 **A10 structured filter / FTS** 后按需出场，不做默认渲染 |
| 老陈"第 3 条为啥慢"需要精确定位到 artifact 内部切片 | A10 三级检索：structured → FTS → vector；本场景 T8 精确 digest rank 走 structured filter + artifact 内部 chunk index，vector tier 不触发（不浪费） |
| "FK 报错在哪个文件" 的关键词回查 | A10 tier 2：`preview_text` 上的 FTS index，append-only 写入不造成 bloat |
| T12 刷新页面若误走 runtime resume 会材质化 manifest + 重算 tool_previews + 读 artifact | A3：Web display history 模式走 `session_transcript_items` + `session_tool_outputs` 两个投影，bounded 索引扫描，**不读 artifact**，不重算 manifest |
| tool_previews zone 每轮预算 ≤1500 但 T5–T10 累计候选超出 | A5 + A9：`tool_previews` 在 T7/T8/T10 主动做 LRU 降级，最老的 preview 压缩 title-only 或被挤出 zone（DB 行永远保留） |
| 3GB artifact 的用户下载不能走 API server | A3 + §MatrixOne Load Model：`GET /api/artifacts/:id/download` 返回 302 到 OSS presigned，内存占用 0 |

## 5. 预期 state projection 演化

下面是三个关键节点快照（行数 + 累计 byte_size + `tool_previews`
zone 用量）。

**T3（09:25 pg_dump 完成后）**：

| 投影 | rows | 累计 byte_size | 说明 |
| --- | --- | --- | --- |
| `session_tool_outputs` | 1 | 3,221,225,472 | pg_dump 聚合行 |
| `session_artifacts` | 1 | 3,221,225,472 | artifact:pgdump-prod-20260601-schema |
| manifest `tool_previews` | — | — | 240 tokens（1 条 preview） |
| `session_todos` | 5 | — | 1 active, 4 pending |

**T7（10:20 slowlog 分析完成后）**：

| 投影 | rows | 累计 byte_size | 说明 |
| --- | --- | --- | --- |
| `session_tool_outputs` | 1006 | 4,067,938,880 | 1 pgdump + 1 struct + 1000 scan_per_file + 1 scan_agg + 1 slowlog agg + 2 slowlog derivative |
| `session_artifacts` | 5 | 4,068,022,272 | pgdump raw / struct json / sql-scan-detail / slowlog-raw / slowlog-top50 |
| manifest `tool_previews` | — | — | 1480 tokens（经 LRU 降级后压到上限内） |
| `session_todos` | 5 | — | 2 done (pgdump, scan), 1 active (slowlog→report), 2 待执行 |

**T10（11:45 风险报告合成后）**：

| 投影 | rows | 累计 byte_size | 说明 |
| --- | --- | --- | --- |
| `session_tool_outputs` | 1008 | 4,068,207,872 | +T8 explain切片 +T10 report |
| `session_artifacts` | 6 | 4,068,207,872 | +migration-risk-report-20260601-v1 |
| manifest `tool_previews` | — | — | 580 tokens（T10 渲染完成后压缩历史） |
| `session_todos` | 5 | — | 4 done, 1 active (decision_log) |
| `decision` | 2 | — | audit.scope.v1 + final.report.delivered |

**关键不变量**：API server 进程 RSS 峰值涨幅 < 200MB（全会话）；
`context_manifests.total_estimated_tokens` 每轮 ≤ 5200；所有 artifact
的 `content_hash` 在"同 schema / 同 slowlog 范围" 重跑时稳定（见
§8 建议 3）。

## 6. Token 节省估算

**朴素方案**（假设 agent 把所有 tool 输出按原样塞入 prompt）：

- T3 pg_dump raw：3,221,225,472 字节 ≈ **10 亿 tokens**（按
  ~3 bytes/token 的 SQL 文本密度估算），**单次 prompt 根本无法
  容纳**，context_window 再大也不够。
- T5 sql_compat_scan 1000 per-file 原始扫描结果合计 ~56MB，
  ~18M tokens，同样不可能。
- T7 slowlog raw 838MB，~280M tokens，不可能。
- 结论：**朴素方案在 T3 就直接崩溃**，后续轮次无从谈起。即便
  把 raw 先塞进"某种 summary 工具" 一次性压成自然语言，也要为
  压缩付 ~50M tokens 级别的一次性成本，且压缩质量不可控、信息
  无法定位到 chunk。

**本设计方案**（T10 报告生成那一轮的实际 prompt 构成）：

| zone | T10 | 说明 |
| --- | --- | --- |
| `system_static` | 1200 | 常驻 |
| `tool_schemas` | 1800 | 5 类工具 schema |
| `session_anchor` | 260 | 审计目标 + workspace=cloud |
| `plan_todo` | 280 | 仅剩 1 条 active |
| `tool_previews` | 580 | T5 聚合 + T7 top50 + T8 explain + T10 报告 4 条 preview（LRU 压缩后） |
| `retrieved_facts` | 440 | 2 条 decision |
| `recent_tail` | 600 | T8–T10 三轮原文 |
| summary | 0 | 本场景未触发 compaction |
| **合计** | **≈ 5160** | |

**对比**：

| 指标 | 朴素方案 | 本设计 | 说明 |
| --- | --- | --- | --- |
| T3 pg_dump 进 prompt | 10 亿 tokens | 210 tokens（preview） | 节省 ≈ 100% |
| T7 slowlog 进 prompt | 2.8 亿 tokens | 280 tokens（preview） | 节省 ≈ 100% |
| 12 轮累计 `tool_previews` 上限 | — | ≤ 18k tokens | 每轮硬上限 1500 |
| artifact 总体积（落库不落 prompt） | — | 4.07 GB | 永远在 OSS，按需加载 |

节省比例在 raw 层面**趋近于 100%**。但真正的工程风险是质量：
preview_text 的**每字符都是"agent 后续推理的唯一依据"**。

**preview_text 推荐长度（按 tool_name 分级）**：

| tool_name 类别 | 推荐长度 | 理由 |
| --- | --- | --- |
| schema_export（pg_dump 类） | 800 字符 | 头部 SET/CREATE 语句 + 尾部统计摘要，够了 |
| scan_aggregate（聚合行） | 1000–1200 字符 | 总计 + TopN 类型直方图 |
| scan_per_file（单文件行） | 80 字符 | 路径 + 命中条数 + 主要原因 tag；per-file 永远不进 prompt，只供 FTS / 筛选 |
| slow_query_analyzer（聚合） | 1000 字符 | TOP10 digest 每条 ~80 字符 + 全局指标 |
| slow_query_explain（单 digest chunk） | 500–800 字符 | EXPLAIN plan 结构化摘要 |
| report_render（markdown 报告） | 500–800 字符 | 目录 + 每章要点 + 高危计数 |
| default | 400 字符 | 未分类工具兜底 |

预算冲突时的降档顺序：先从 `scan_aggregate` → `slow_query_
analyzer` → `schema_export` 按 LRU 压到 title-only（即只保留
"tool_name + byte_size + artifact_ref"，preview 完全舍弃），保护
最近一轮 tool 的完整 preview。

## 7. 潜在失败模式

**F1 · preview_text 在 SQL/JSON 半结构化输出截断到非法边界**：如
果 `pg_dump_schema` 的 preview 正好切在 `CREATE TABLE orders (`
中间，agent 下一轮 reasoning 时把 preview 当 SQL parse，遇到不闭
合括号抛 parse error，把"schema 本身语法错误"误传给用户。**可
观测信号**：`session_tool_outputs.preview_text` 末尾出现不配对
括号/引号；agent assistant 回复里出现 "schema has parse error at
line 1" 但 artifact 校验正常。缓解：preview 截断算法按**语法感知
边界**（整行、整个 statement、整个 JSON object），不按字节；末
尾固定追加 `truncated at X bytes` 文案让 agent 知道是截断不是错误。

**F2 · 1000 条 per-file `session_tool_outputs` 写入走 N+1**：如果
tool executor 每处理一个文件就 `INSERT ... VALUES (...)` 单行一次，
1000 次 insert 在 MatrixOne 上 ~2–4 秒网络往返，直接把 T5 延迟
拖到 "UI 卡顿阈值" 之上；更糟会触发 connection pool 打满。**可
观测信号**：T5 回合 `latency_ms > 5000`；DB 监控看到 `INSERT
count` 突发 1000+；tool 执行时间里 "DB write" 占比 > 50%。缓
解：executor 必须 batch insert（10 批 × 100 行），并为 `session_
tool_outputs` 增加覆盖 `(session_id, output_id)` 的主键 hash 分片
以避免热点。

**F3 · artifact OSS 对象过期而 hot projection 未感知**：`session_
artifacts` 落了 `retention_days=30` 的 slowlog-raw，31 天后 OSS 清
理了对象，但 `session_tool_outputs.artifact_ref` 和 `session_
artifacts` 行仍然存在且 `status='ok'`。T8 用户回查时点击"展开 raw
slowlog 切片"，presigned URL 返回 404。**可观测信号**：用户报告
"展开 artifact 出 404"；`session_artifacts` 行存在但 OSS HEAD 对
象不存在。缓解：GC 必须同时更新 `session_artifacts.status=
'expired'` 或 soft-delete 一行；manifest items 若引用 expired
artifact，context builder 要能识别并降级 preview 为"已过期，请联
系审计导出"。且 GC 之前必须检查 `session_artifacts.derived_from`
反向引用（派生 artifact 未过期时 raw 不能删，否则破坏溯源）。

**F4 · content_hash 不稳定：同一天重跑 pg_dump 两次，hash 不同**：
pg_dump 输出头部自带 `-- Dumped on <timestamp>`、`-- Dumped by
pg_dump version 16.1`，末尾可能包含 `SELECT pg_catalog.set_
config('search_path', '', false);` 等运行时状态。若 `content_hash`
直接 sha256(raw)，同 schema 两次运行 hash 必然不同，artifact 复
用（dedup）完全失效，磁盘、hash 索引、context 去重都被污染。
**可观测信号**：`SELECT COUNT(DISTINCT content_hash) FROM
session_artifacts WHERE tool_name='pg_dump_schema' AND user_id=?`
返回值随每次运行单调 +1，即使 schema 明确未变。缓解：每类工
具定义一段 `normalize(raw) -> bytes` 规则（剔除时间戳行、版本
行、文件路径、随机 session id 等），先 normalize 再 hash，并把
规范化规则本身的版本号（`norm_version='v1'`）也存进 artifact 行
供未来算法升级。

**F5 · T6 structured filter 漏网：status 枚举不规范**：如果 tool
executor 偶尔写入 `status='FAIL'`（大写）、`status='failed'`（过去
式）或 `status='incompat'`，T6 的 `WHERE status='fail'` 就会漏掉
部分行，137 变 129，用户直接少看到 8 个不兼容文件。**可观测信
号**：`SELECT DISTINCT status FROM session_tool_outputs WHERE
tool_name='sql_compat_scan'` 返回 > 3 个不同值；用户追问"上次扫
出来不是 137 条吗？" 缓解：schema 层面给 `status` 加 CHECK 约
束 `status IN ('ok','warn','fail','error','timeout','skipped')`，
tool executor 统一经由 enum 转字符串，禁止自由文本。

**F6 · 3GB artifact 下载走 API server 流式代理**：如果 `GET /api/
artifacts/:id/download` 的实现是 `pipe(S3.getObject(), res)`，单用
户下载 3GB pg_dump 时 API server 进程会保持一条长连接 +
64KB/chunk 读写缓冲，正常；但并发 20 个用户同时下载就可能把
API server 的 file descriptor / goroutine 数打爆，尤其在 k8s pod
内存硬限制下。**可观测信号**：API server RSS 随下载请求数线性
上涨；下载速度受限于 API server 带宽而非 OSS 带宽；`/api/
artifacts/:id/download` p99 latency > 10 分钟。缓解：必须返回
302 到 OSS presigned URL（TTL 5–15 分钟），API server 仅处理
"签名 + 审计 event + 返回 Location"，对象流量不经过 API server。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Tool Result and Artifact References（§4） — **现有设计缺口**：
   `session_tool_outputs` schema 定义了 `preview_text TEXT NULL`，
   但**没有写长度上限和分级规则**。本场景 T5 的聚合 preview 与
   per-file preview 分级要求 1200 vs 80 字符，差 15 倍；如果实现
   层统一按 500 字符截断，聚合 preview 会把 TopN 统计切掉，per-
   file preview 反而浪费空间。建议在设计文档里显式加一张"按
   tool_name 类别推荐 preview_text 长度"表（§6 的那张可直接纳
   入）。
2. §Lazy Indexing / §Artifact and Tool Output Policy — **artifact
   retention 与 `manifest_items` 引用关系未显式**：本场景 T3 的
   3GB raw dump 若按 90 天策略删除，但 `context_manifests` 的历
   史行还在 `tool_previews` zone 引用这个 `artifact_ref`，UI 回放旧
   manifest 就会得到 404 preview。设计文档应补一条约束：GC
   artifact 前必须检查 (a) 派生 artifact 链（`derived_from` 反向）
   未过期；(b) 活跃 `context_manifests` 未引用；且 GC 后必须
   双写 `session_artifacts.status='expired'`，context builder 在
   assembly 时对 expired artifact 降级为 "historical, raw no longer
   available, summary preserved" 占位。
3. §Tool Result and Artifact References（§4） — **`content_hash`
   规范化规则未规定**：当前 schema 只说 `content_hash VARCHAR
   (128)`，没说"hash 之前是否规范化"。不同工具的规范化策略差
   别极大（见 §7 F4），不统一会导致 dedup/审计/复用全线失效。
   建议在设计文档里加一段「Content Hash Normalization Contract」，
   要求每个 tool executor 声明 `normalize_version` 并文档化规则，同
   schema 的 tool 变更规范化规则时 `normalize_version` 必须 bump。

**三条具体建议**：

- **`preview_text` 长度策略按 `tool_name` 分级**：在设计文档附录
  里放一张 default profile 表（本文 §6 给出了 7 行），允许按
  `tool_name` 注册自定义长度；context builder 在 `tool_previews`
  zone 渲染时按长度上限 + LRU 两条维度裁剪。
- **artifact retention 与 manifest 依赖双向感知**：`session_
  artifacts` 增 `retention_until`、`status`（active / expiring /
  expired）、`referenced_by_manifest_count` 三列；GC 守护进程在
  expiration 前的 T-7 天做一次"活跃 manifest 扫描"，把仍被引用
  但即将过期的 artifact 迁移到长期 tier（冷存）或延期，避免 UI
  展开历史 session 时 404。
- **`content_hash` 规范化规范**：每个 tool 声明 `normalize_version
  VARCHAR(16)` 存在 `session_artifacts`；规范化函数剔除时间戳、
  版本字符串、绝对路径、随机标识；pg_dump、slow_query_
  analyzer、sql_compat_scan 各给一份基线规则（设计文档附录）。
  这样"同一天重跑 pg_dump schema 无变化"才真正能复用现有
  artifact，不做重复 OSS 上传与 3GB 冗余存储。
