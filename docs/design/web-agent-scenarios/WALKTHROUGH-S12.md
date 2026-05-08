# WALKTHROUGH S12 — 14 天研究综述 · 200 URL + 50 PDF

> v0.2 端到端推演。一位有经验的工程师以运行视角把 12 个采样回合走一遍。

## §1 场景标识与 v0.2 映射

**场景**：研究员 Miranda 14 天完成「小型 LLM 临床文本 PEFT 综述」，
~45 轮交互、~300 tool 调用、~250 artifact、raw 累计 ~6.8 GB。
**被测 DAUT**：A3（UI tab 不扫全表）/ A9（raw 绝不进 prompt）/
A10（三类检索走三级路径）。核心矛盾是**同质工具海量小输出 × 结构化
preview × 跨周回查精度**。

v0.2 最相关字段/章节（引具体行号）：
- `session_tool_outputs` schema 含 `relevance_score DECIMAL(8,4)` 一等列 +
  复合索引 `idx_tool_outputs_session_tool_score` @L1577、L1591。
- **§Preview Template Registry** @L1617–1669：`first_class_columns[]`、
  `fts_field_weights{}`、baseline 模板（fetch_url 1000B / parse_pdf 1400B）。
- **§Artifact Retention and Access Scope** @L1671–1749：`retention_policy=
  'project_long_term'`、`referenced_by_*_count`、T-7 GC preflight。
- **§Compaction Invariants** @L2496–2596：8 条白名单 + SQL assertion。
- **§Content Hash Normalization** @L2163–2194：`fetch_url_v1` / `parse_pdf_v1`。
- **§Budget Policy v1** @L2381–2407：`tool_previews` zone cap 参考（8k 预算）。
- **§Manifest reason 枚举** @L1504–1515：`large_tool_output_gated` /
  `history_recall_fts` / `post_compaction` 是本场景主用 reason。

## §2 运行前提

- **初始 schema 状态**：`agent_sessions` 一条新会话 `project_id='proj_
  clinical_survey_2025', project_retention_policy='project_long_term'`；
  `session_state_items` 空；`session_tool_outputs` / `session_artifacts`
  空；`preview_template_registry` 预置 5 条（`fetch_url.v1` /
  `parse_pdf.v1` / `llm_extract_findings.v1` / `benchmark_slice.v1` /
  `render_survey.v1`，均 `status='active'`）。
- **Workspace**：cloud-only（tool runner 在 API server 上游，OSS 挂本项目
  bucket `mo-astra-proj-clinical-survey-2025`）。工具集合 5 类：fetch_url /
  parse_pdf / llm_extract_findings / pdf_text_section_read / render_survey。
- **Context window 预算**：`budget_template_id='budget_v1_8k'` 不适用（本场
  景用大模型 200k 窗口，但主动设 8k 软上限作压测）。`tool_previews` 硬
  上限 1500 tokens。

## §3 端到端运行轨迹（T1–T12）

### T1 · Day 1 14:00 开题「做一份小型 LLM 医疗微调综述」

**User**：「想做一篇 survey 关于小型 LLM（≤7B）在临床文本（EHR、
放射报告）的 PEFT 微调，arxiv 近两年为主，加几个 HF model card 和 blog。」

**Server**：插入 `session_state_items` 1 条 `anchor`：`{goal, scope_years:
[2023,2025], param_cap:7e9}`；1 条 `workspace_state='cloud'`。`agent_runs`
起 1 条 `status='running'`。写 1 条 `context_manifest` `reason='initial_
turn'`，`budget_template_id='budget_v1_200k'`。未调任何 tool。

**User sees**：SSE `anchor.created` / `run.started`；UI 左栏出现目标
chip。延迟 < 400ms。

**Manifest zones**（估）：system_static 1200 / tool_schemas 1600 /
session_anchor 280 / plan_todo 0 / recent_tail 200 / 合计 ≈ 3280。

**LLM 看不到**：Miranda 脑里的 5 个候选关键词（只说了 2 个）。

### T2 · Day 1 14:10 agent 拟 plan + 建 todos

**User**：（隐式）默认同意 agent 规划。

**Server**：写 `session_state_items` `plan_state` version=1、phase=
`literature_collection`；批量 insert 6 条 `session_todos`（①批量 fetch
200、②下载解析 50 PDF、③逐篇抽 findings、④按方法分组、⑤撰写综述、
⑥交叉核对）；5 条 `session_todo_deps` 链式依赖；2 条 `decision`（
`scope.param_cap.v1`、`scope.domain.v1`）。`context_manifest` reason=
`plan_subtree_query`。

**User sees**：Plan 面板渲染 6 个 todo 卡片，T1 的 anchor 已 pin。

**Manifest zones**：plan_todo 380 / session_anchor 300 / tool_schemas
1600 / system_static 1200 / recent_tail 480。合计 ≈ 3960。

**LLM 看不到**：无。

### T3 · Day 1 14:30–16:00 批量爬 200 URL

**User**：「先抓 arxiv 'clinical LLM fine-tuning'/'LoRA medical'/
'BioGPT'/'PEFT healthcare' 各 50 条，再 20 HF model card + 20 blog。」

**Server**：runner fan-out 并发 10，共 200 次 fetch_url。每次写 1 行
`session_tool_outputs`（`tool_name='fetch_url'`，`preview_template_
version='fetch_url.v1'`，`normalize_version='fetch_url_v1'`，`status
∈{ok,fetch_fail,parse_fail}`，一等列 `http_status` / `content_type` /
`relevance_score`（runner 侧 TF-IDF 算），`byte_size`=HTML 字节，
`artifact_ref` 指向 OSS），+1 行 `session_artifacts`（`artifact_type=
'html_raw'`，`retention_policy='project_long_term'`，`access_scope=
'delegation'`，`normalize_version='fetch_url_v1'`，`content_hash=
sha256(normalize_fetch_url_v1(raw))`）。`preview_text` 严格按模板 schema
JSON 序列化，≤1000 bytes。批量插入拆 10 批 × 20 行。本轮 prompt 只塞
**1 条聚合 preview**，per-URL preview 不进 manifest。manifest reason=
`large_tool_output_gated`。

**User sees**：Tools tab 新增 200 行；进度条 ok=182 / fail=18；SSE
`tool.batch.completed`。Files tab 增 200 html_raw。

**Manifest zones**：tool_previews 320（聚合串）/ plan_todo 380 /
recent_tail 600 / 合计 ≈ 4100。

**LLM 看不到**：200 个 HTML 原文累计 ~2 GB；200 行 per-URL preview；
keywords[] 数组。

### T4 · Day 1 16:10「列一下相关度最高的前 20 个」

**User**：「刚才抓的 200 个，按相关度从高到低列前 20，我先筛 PDF。」

**Server**：不调新 tool。发一次 **structured filter**，走一等列
`SELECT output_id, relevance_score, title, preview_text FROM session_
tool_outputs WHERE session_id=? AND tool_name='fetch_url' AND status=
'ok' ORDER BY relevance_score DESC LIMIT 20`，命中索引
`idx_tool_outputs_session_tool_score` @L1591。返回 20 行 ~5 KB。
Miranda UI 多选勾 50 篇候选 → 1 条 `session_state_items(category=
'tool_ref', item_key='pdf.candidate.batch.v1', payload_json=[50 artifact
_ref])`。manifest reason=`history_recall_structured`。

**User sees**：有序列表，每行标题+域名+score。延迟 ~350 ms。

**Manifest zones**：tool_previews 1100（20 条 preview 渲染）/ plan_todo
380 / recent_tail 700 / 合计 ≈ 4400。

**LLM 看不到**：其余 180 条 fetch_url preview；200 HTML 原文。

### T5 · Day 1 16:30 – Day 2 11:00 下载 + 解析 50 PDF

**User**：「选好的 50 篇下载解析，每篇给我 structured metadata。」

**Server**：`parse_pdf` 并发 50。每篇：（a）下载 PDF raw（3–10 MB）→
`session_artifacts` 1 行 `artifact_type='pdf_raw'`；（b）pdfminer 解析
text（100–500 KB）→ `session_artifacts` 1 行 `pdf_text`、`derived_from_
artifact_id=<pdf_raw_id>`；（c）`session_tool_outputs` 1 行 `tool_name=
'parse_pdf'`，`preview_template_version='parse_pdf.v1'`，`max_preview_
bytes=1400`，preview JSON `{title, authors[], year, venue, abstract<200
字>, section_count, byte_text}`，first-class `content_type='application/
pdf'` / `row_count=section_count` / `relevance_score`（继承 T3）。本轮
**T5 preview 的 key_findings/benchmarks 字段暂为 null**（schema 允许），
T7 补。batch 5×10。两周项目 retention 自动延期。manifest reason=
`large_tool_output_gated`。

**User sees**：Files tab 扫出 100 条新 artifact（50 raw + 50 text）；进度
条实时跳；SSE `tool.parse_pdf.stream`。

**Manifest zones**：tool_previews 500（1 聚合："parsed 50, text 18 MB, avg
6.8 sections, arxiv=22/NeurIPS=9/ACL=7"）/ plan_todo 380 / recent_tail
900 / 合计 ≈ 4500。

**LLM 看不到**：50 PDF raw ~4 GB；50 pdf_text ~18 MB；per-PDF preview
原始 JSON。

### T6 · Day 2 14:00「关于 LoRA 微调的 5 篇再列一下」

**User**：「等等——刚才 50 篇里专讲 LoRA/QLoRA 微调的那几篇，列一下？
先看方法论那章。」

**Server**：`NextActionExtractor` 识别为历史回查 → tier 2 FTS。
`SELECT output_id, title, preview_text FROM session_tool_outputs
WHERE session_id=? AND tool_name='parse_pdf' AND preview_template_
version='parse_pdf.v1' AND MATCH(preview_text) AGAINST ('+LoRA +QLoRA'
IN BOOLEAN MODE) ORDER BY MATCH(...) DESC LIMIT 10`。FTS 按
`fts_field_weights` @L1639（title:4, first_paragraph:2, keywords:3）
加权 → title 命中权重远高于 abstract 中提一句 → 返回 6 行，**区分主
方法 vs related work 提及**。写 1 条 `session_state_items(tool_ref)`
key=`findings.lora.subset.v1` 记录 6 个 artifact_ref 快照。manifest
reason=`history_recall_fts`。

**User sees**：6 行卡片，method=LoRA/QLoRA 徽标；标题可点。

**Manifest zones**：tool_previews 900（6 条 parse_pdf preview + 聚合头）
/ plan_todo 380 / recent_tail 800 / 合计 ≈ 4500。

**LLM 看不到**：其余 44 篇 parse_pdf preview；所有 pdf_text 原文。

### T7 · Day 2 15:00 – Day 3 18:00 逐篇抽 key_findings

**User**：「对 50 篇每篇抽 method/dataset/base_model/param/benchmark/
key_claim，结构化存。」

**Server**：50 次 `llm_extract_findings`（input=pdf_text artifact_ref，
runner 从 OSS 流式读给 LLM，不经 runtime）。每次输出 1–3 KB JSON。
runner 写两处：（a）`session_tool_outputs` 1 行 `tool_name=
'llm_extract_findings'`，`preview_template_version='llm_extract_
findings.v1'`，`max_preview_bytes=1200`，`default_chunk_type='finding'`
@L1655，preview 300 字 `{paper_title, main_method, main_dataset, top_
benchmark}`；（b）`session_state_items` 50 条 `category='finding'`（
G8 一等类别 @L1658–1662），`item_key='finding.<paper_id>'`，`payload_
json={claim, evidence_refs[pdf_text artifact_ref + byte_range], confidence,
source_artifact_ref}`，`provenance_event_id=<extract_event>`。batch 5×10。
manifest reason=`large_tool_output_gated`。

**User sees**："方法分布" dashboard：LoRA=18/QLoRA=7/adapter=9/
full_ft=6/prompt_tuning=10；MIMIC-III=22 等。

**Manifest zones**：tool_previews 600（1 聚合 + Top-3）/ retrieved_facts
480（Top-K decision + finding） / plan_todo 360 / recent_tail 900 / 合计
≈ 4800。

**LLM 看不到**：50 次 llm_extract 完整 output JSON；pdf_text 原文。

### T8 · Day 7 10:00 跨周回查「上周那个 BioBERT」

**User**：「上周读的那篇 BioBERT continual pretraining，方法章怎么处
理 domain shift？作者好像清华那边的，忘了是哪篇了。」

**Server**：`NextActionExtractor` 识别**时间+关键词**双线索 → 组合
查询 `SELECT ... WHERE tool_name='parse_pdf' AND created_at BETWEEN
DATE_SUB(NOW(),INTERVAL 10 DAY) AND DATE_SUB(NOW(),INTERVAL 3 DAY)
AND MATCH(preview_text) AGAINST ('+BioBERT +Tsinghua') LIMIT 5`。命中
`idx_tool_outputs_tool_created`（L1590）二级过滤 FTS；top-1 确认。按
该论文 `artifact_ref` 加 `section_hint='method'` 调
`pdf_text_section_read`：runner 按 section_id 读 byte-range（~8 KB），
写 1 行 `session_tool_outputs`（preview：`{paper_id, section, byte_range,
excerpt_hash}`）。`session_state_items(finding)` 的 `key_claim` 供 agent
首句锚定。manifest reason=`history_recall_fts`。

**User sees**：agent 首句「你说的是 2024-05 那篇 Wang et al. BioBERT-Ft
（清华 KEG），他们通过 TAPT（task-adaptive pretraining）+ LoRA 组合
处理 domain shift……」并在旁栏展开 8 KB 方法章节原文。

**Manifest zones**：tool_previews 720（1 定位 preview + 1 byte-range
preview）/ retrieved_facts 520（含该论文 finding）/ plan_todo 360 /
recent_tail 1100 / 合计 ≈ 5100。

**LLM 看不到**：该 PDF 其余 ~290 KB；其他 49 篇任何内容。

### T9 · Day 8 11:00「对比 5.2 节那批 benchmark，EM/F1/ROUGE 到小数点两位」

**User**：「第 5.2 节 LoRA 那批的 benchmark 数字，画个 table，EM/F1/
ROUGE 精确到小数点后两位。preview 里只有 top_benchmark 不够。」

**Server**：精度超 preview → **按需 raw 切片加载**。对 T6 定位的 6 篇，
逐篇 `pdf_text_section_read`（section_hint='benchmark|results|
evaluation'），runner 正则切出表格（每篇 3–8 KB），写 `session_tool_
outputs` 6 行 `tool_name='benchmark_slice'`，`preview_template_
version='benchmark_slice.v1'`，`max_preview_bytes=1200`，`default_chunk_
type='benchmark'` @L1656，preview `{paper_id, benchmark_name, scores:
{em, f1, rouge}}`；**同时**写 6 条 `session_state_items(category=
'benchmark')`（G8 @L1661：`{metric, value, unit, baseline, method,
source_artifact_ref}`）。agent 最终用 6 条 state_items 渲染 markdown
table。manifest reason=`large_tool_output_gated`。

**User sees**：markdown 表格 6 行 × 4 列 + 数据源脚注；每格数字可
hover 跳到 PDF 对应 byte-range。

**Manifest zones**：tool_previews 1400（6×~230 tokens 结构化）/
retrieved_facts 440 / plan_todo 340 / recent_tail 900 / 合计 ≈ 5600。
**逼近 tool_previews 1500 上限**（详见 §7）。

**LLM 看不到**：6 篇非 benchmark 章节；44 篇其他 PDF。

### T10 · Day 10 09:00 第二次 compaction（后台触发）

**User**：（无，compaction 后台。）

**Server**：`recent_tail` 累积 > 3000 tokens 触发。按 §Compaction
Invariants @L2504–2525：（1）raw `conversation_log` 不改；（2）
`finding`×50 / `benchmark`×6 / `decision`×2 / active `todo_state` /
active `delegation_state` **禁动**（L2508–2510）；（3）`plan_state.
version` **不 bump**（L2511）；（4）先校验 `agent_runs.status NOT IN
('running','waiting')`（L2513）；（5）append 1 条 `session_state_items
(category='summary', item_key='summary.week1.fetch_and_parse.v1')`，
~500 tokens 叙事；（6）archive 已完成的 todo_state，但保留
`provenance_event_id`；（7）写 `context_manifests reason=
'post_compaction'` @L2518 + L1506；（8）全 8 条 SQL assertion 过。
所有 250 artifact `retention_policy='project_long_term'`，两周项目
未触 GC。manifest reason=`post_compaction`。

**User sees**：侧栏出现"Week 1 摘要"卡片；老 tool_output 行变灰
`manifest_visibility='compacted'`（投影标记）。无 UI 阻塞。

**Manifest zones**：summary 500 / retrieved_facts 600（finding/benchmark
权重提升入主区）/ plan_todo 320 / tool_previews 480（仅最近 3 轮）/
recent_tail 1000 / 合计 ≈ 5100。

**LLM 看不到**：T3–T5 per-URL / per-PDF preview 详情（已由 summary 替
代，可按 `session_history_chunks` 按需回放）。

### T11 · Day 12 14:00 – Day 13 22:00 撰写第 3 章「方法论分类」

**User**：「写第 3 章，PEFT 方法 group：LoRA/QLoRA/adapter/prompt_tuning
各一小节，每段 ≥3 篇引用，引用前回原 PDF 核对数字。」

**Server**：进入"撰写+核对"循环。每段引 ~3 篇，每篇：（a）按
`artifact_ref` 拉 pdf_text 对应 section（5 KB byte-range）；（b）发现
不一致 → 更新 `session_state_items(finding)`：写 1 条
`session_state_item_events(mutation='update', previous_version, next_
version, previous_hash, next_hash)`（G14 mutation 枚举）；（c）写
`session_state_items(category='citation')` @L1662：`{source_artifact_
ref, locator(page+byte_offset), quote_hash, summary}`。共 ~30 次
section_read + ~30 条 citation。**每次 prompt 组 manifest 时，
`tool_previews` zone 需承载 "当前正写段落已用到的若干 artifact_ref 展
开"**（详见 §7 token 账）。manifest reason=`history_recall_structured`
或 `large_tool_output_gated`（按回合）。

**User sees**：实时 markdown 预览 + 旁栏高亮当前引用的 PDF 段；2 次
"数字不一致，已更新 finding 版本"黄条提示。

**Manifest zones**（典型一轮）：tool_previews 1200（最近 5–6 次 section
_read + 本段撰写 preview）/ retrieved_facts 800（本段 3 条 finding +
2 条 decision）/ plan_todo 280 / recent_tail 1200 / summary 500 / 合计
≈ 6000。

**LLM 看不到**：未引用的 ~47 篇；所有 PDF raw；其他章节。

### T12 · Day 14 16:00 交付 + 下载

**User**：「写完了，生成最终 markdown + bibliography，我下载。」

**Server**：调 `render_survey`：合成 180 KB markdown → 1 行
`session_artifacts` `artifact_type='survey_markdown'`，`content_hash=
sha256(normalize_markdown_v1(text))`，`provenance_refs` 数组 = 50
pdf_text + 50 finding + ~30 citation 的 id/hash。`session_todos` 全
`done`。UI 下载走 302 → OSS presigned URL（G9 "Large artifact
downloads should return a presigned object-store URL" @L1741–1742），
API server 不代理字节。`context_manifest` reason=`normal_turn`。

**User sees**：下载按钮 → 浏览器直接拉 OSS，30 页 markdown。SSE
`run.completed`。

**Manifest zones**：tool_previews 500 / retrieved_facts 600 / plan_todo
80（全 done）/ summary 500 / recent_tail 800 / 合计 ≈ 4600。

**LLM 看不到**：final markdown 全文（runner 侧生成）；任何 PDF。

## §4 Frictions（按严重度）

**F1 · `preview_template_registry.schema_json` 缺 per-tool migration 协议（`notable`）**
Ref：T5/T6 + §L1595–1609。模板表有 `version` 和 `status='active'`，但未
规定"旧 session 的 parse_pdf.v0 与新 parse_pdf.v1 混存时 FTS 如何查"。
S12 §7 F5 已预警（作者字段从 `author` 单数改 `authors` 数组）。v0.2
设计允许 backfill 但未强制，跨周会话（14 天）极易踩到：Day 1 用 v1
写 50 行，Day 8 registry 推 v2，T11 FTS `+LoRA` 可能漏掉新版本写入
的条目。**为什么是 friction 而非 gap**：G8 已给模板机制，但 migration
是实现细节，不必上升为设计 gap；文档可加一句"bump 时必须同步
backfill 或双读"即可。

**F2 · `session_history_chunks.chunk_type` 的 `finding`/`benchmark`/
`citation` 并非显式枚举（`notable`）**
Ref：T10 compaction + §G2 r2 closed note @GAP-TRACKER L113。GAP-TRACKER
声称 "chunk_type 列表现含三类"，但 grep 设计文主体未直观看到这三类
chunk_type 的 CREATE TABLE 定义，若实现者直接读 §History Chunks 原表
而未翻 changelog 会错过。运行上不会立刻坏（state_items 类别独立覆
盖），但 `context_manifest_items.ref_kind` 回投到 history_chunks 时
可能找不到类型对应的 chunk 渲染器。**缓解**：v0.2 文档应把 chunk_type
完整枚举表直接贴在 §History Chunks 章节旁，而非埋在 changelog。

**F3 · `relevance_score` 在多租户 / 多类工具下**语义歧义**（`minor`）**
Ref：T3/T4 + §L1577、§Preview Template Registry fetch_url first_class
`relevance_score`。fetch_url 的 score 是 runner TF-IDF；llm_extract_
findings 的 score 定义"相对用户目标的相关度"；benchmark_slice 的 score
是"该切片对本 survey 章节贡献度"——三种语义都叫 `relevance_score`
塞同一 DECIMAL 列，T4 `ORDER BY relevance_score DESC WHERE tool_name
='fetch_url'` 能用，跨 tool_name 查询会混淆排序。设计上 A9 已经
tool-scoped 避开这一点，但跨工具聚合查询（如 T9"列所有 T7 findings
+ T9 benchmarks 按 score"）会踩。**缓解**：文档补一句"跨 tool_name
聚合查询必须按 `tool_name` 分组，不允许 global ORDER BY relevance_
score"，或在 preview_template 声明 `relevance_score_scope='per_tool'`。

**F4 · T9 `tool_previews` 1400 tokens 逼近 1500 上限，8 篇即溢出（`notable`）**
Ref：T9 manifest / §L2416 budget_v1_8k 或 §Budget Policy v1 @L2395。
设计文本对 `tool_previews` 的上限是**软策略**，无按 intent 动态上调
机制。场景 §7 F6 已预警。Miranda 的确可能要对比 8–10 篇 benchmark
（综述常见），届时触发 "zone overflow, dropped_count≥2"。**缓解**：
intent-aware budget flex——context assembler 识别 `intent=
'benchmark_comparison'` 时 tool_previews cap 临时上调至 2500，从
recent_tail 借。这是新的 design 需求（见 §5 候选 G20）。

**F5 · 14 天 `project_long_term` retention 下，`referenced_by_*_count`
的刷新时机不明（`minor`）**
Ref：T10 compaction + §G9 @L1689–1691、L1722–1728。counter 要求
"Recompute manifest/state/citation counters from context_manifest_items,
session_state_items, and citation items" 在 T-7 GC preflight 时做。
但本场景 T11 频繁增 citation（30 条），counter 若仅在 GC preflight
时回算，T11 期间读到的是 stale 值；若改为 per-write trigger 又有并发
问题。v0.2 已说 "G9 does not require that mutation to define retention"
但没给实时 counter 的推荐做法。**缓解**：补一条 "counter 可 lazily
stale；GC decision 以 preflight 时的重算为准"就够。

**F6 · `pdf_text_section_read` 工具在 §Preview Template Registry
baseline 表缺失（`minor`）**
Ref：T8/T9/T11 + §L1647–1656 baseline 表。baseline 只列 pg_dump /
slow_query_analyzer / fetch_url / parse_pdf / llm_extract_findings /
benchmark_slice 六个，不含 `pdf_text_section_read`（本场景关键工具，
14 天项目调 ~36 次）。按 v0.2 兜底规则 @L1621–1622 "Tools without a
template use a 400 character fallback preview and `preview_status=
'fallback'`"——意味着 byte-range 切片被迫用 400B 自由文本 preview，
T9 的结构化 `scores{em,f1,rouge}` 无法写 preview，退化为 LLM 读 8 KB
原文抽数字，与 A9 精神相悖。**缓解**：v0.2 附录 baseline 加
`pdf_text_section_read.v1` 模板：`first_class_columns=[section_name,
byte_range_start, byte_range_end, excerpt_hash]`、`default_chunk_type=
'artifact_text'`。

## §5 新 gap 候选

**发现 1 条新 gap 候选**（其余 F1–F6 属 friction 或可通过现有机制解决）。

```
- title: Intent-aware tool_previews zone cap flex
- triggered_by_turn: T9 / T11
- related_v0.2_section: §Budget Policy v1 @L2381–2407；§Manifest Reason
  Enumeration @L1504–1515
- severity: medium
- rationale: 当前 tool_previews 的 cap（1500 tokens 或 budget_v1_8k 的
  500）是静态值，不随 turn intent 调整。研究综述场景的 "benchmark
  comparison" / "citation verification" 等 intent 天然需要同时展开
  6–10 条结构化 preview（每条 200 tokens），触顶概率极高。其他 zone
  如 retrieved_facts / recent_tail 在此类 turn 上反而有空闲空间。静
  态 cap 强迫 assembler drop 条目或触发 fallback 400B 截断，削弱 A9
  "结构化 preview" 核心价值。manifest reason 已有 `large_tool_output_
  gated`，但这是 "有大输出被压" 语义，不是 "当前 turn 意图需要更多
  preview" 语义。
- proposed_fix: 新增 context_manifest 字段 `turn_intent VARCHAR(64)`
  枚举 `benchmark_comparison` / `citation_verification` / `normal` 等；
  对应在 Budget Policy 定义 per-intent 的 zone cap profile，允许
  tool_previews 临时上调至 2500 tokens，从 recent_tail (floor 1600)
  借用。assembler 读 turn_intent 后走 profile override；manifest 记
  录原 cap / override cap / 实际占用三个值便于审计。
```

其余观察点（F1–F6）不构成新 gap：F1 是 G8 内部 migration 细节；F2 是
文档索引问题；F3 通过字段 scoping 可解；F5 通过一句话澄清可解；F6
只需在 baseline 附录补一条模板。

## §6 Gap 组合效应

**组合 1：G8（preview template）+ G17（content hash normalize）**
运行时可能发生的坑：T3 fetch_url 同一 URL 被重新抓取时（比如 T8 回
查 BioBERT 时附带刷新一次最新 HTML），`fetch_url.v1` + `fetch_url_v1`
分别管 preview schema 和 hash normalize。若 runner 实现时**只 bump
preview_template_version**（引入 keywords 新字段）而未 bump `normalize
_version`，同一 URL 的 `content_hash` 稳定 → artifact 被判定 dedup →
旧 preview 不被替换 → T6 的 FTS 找不到新 keywords。反向：若 bump
了 normalize 但未 bump preview，hash 变化触发新 artifact，但 preview
还是旧 schema → FTS 在两版 preview 混用。
v0.2 是否覆盖：**partial**。G8 @L1567–1568 `preview_template_version`
和 `normalize_version` 是两列独立；G17 @L2190 "`normalize_version`
must bump" 但未强制 bump 两者的联动规则。
建议：文档补一条"preview_template bump 时建议（非强制）同步 bump
normalize_version；若不同步，必须在 template changelog 声明‘仅 view
层变更’"。

**组合 2：G2（compaction invariant）+ G8（preview template）**
运行时可能发生的坑：T10 compaction 按白名单保 `finding/benchmark/
citation` state_items，但对 `session_tool_outputs` 的 preview_text 是
否压缩/丢弃未被 G2 直接约束（G2 只管 state_items）。T3 的 200 行
fetch_url preview 聚合后，原始 per-URL preview 若被 history_chunks 层
"replace" 为聚合 preview，T8 回查 BioBERT 时还能不能按 `MATCH(preview
_text)` 命中？G2 invariant #1 说 "Raw conversation_log, agent_events,
agent_run_events, and history chunk rows are never physically rewritten"
——注意未列 `session_tool_outputs`！若 compactor 物理改写 tool_outputs
的 preview_text，FTS 索引失效。
v0.2 是否覆盖：**yes**。§G8 @L1664–1667 "protected by compaction
invariants" + G2 白名单隐含 `session_tool_outputs` 也不物理改写（
通过 `manifest_visibility='compacted'` 投影标记，不改行）。**但文档
应显式把 `session_tool_outputs` 加到 G2 invariant #1 的枚举里**，
避免实现者漏掉。

**组合 3：G9（retention `project_long_term`）+ 长期项目 citation chain**
运行时可能发生的坑：14 天项目用 `project_long_term`，默认项目活着
就不 GC。但 Miranda 在 T11 写了 ~30 条 citation，每条 `payload_json`
含 `source_artifact_ref` 指向 pdf_text。假设项目 Day 21 结束（超出
project 期），若直接 GC pdf_text artifact，citation 的 `source_
artifact_ref` 变悬空；T12 交付的 survey markdown 的 `provenance_refs`
也失效，用户回读 6 个月后的综述时，"看不到出处原文"。G9 @L1688–1691
`referenced_by_citation_count` 正是为此设，GC preflight @L1724–1728
会检查非零计数 → 延期或迁冷存。
v0.2 是否覆盖：**yes**，且 G9 考虑了 derived artifact（L1728 "GC
derived artifacts only after their source chain is safe to expire"）。
pdf_text 是 pdf_raw 的 derived，`derived_from_artifact_id` 形成链，
pdf_raw 必须在 pdf_text safe 之后才能 GC。**但建议**：项目 long_term
结束时让 user 选"归档到 cold storage"而非直接 expire，保留 citation
可读性，对应 `status='archived_cold'` + `cold_storage_ref`（L1712）。

## §7 T11 综述生成 30 artifact_ref manifest token 账

**场景**：T11 某一撰写轮（写 "LoRA" 小节 3 篇引用），本段已累计引用
30 个 artifact_ref（覆盖 finding + citation + pdf_text section）。
context assembler 要把"最相关"的若干个展开进 `tool_previews`。

**每 artifact_ref 展开成本**（按模板）：
- `llm_extract_findings.v1` preview：max 1200 bytes ≈ 300 tokens（按
  4 bytes/token），结构化 JSON 实际 ~220 tokens（含字段名 overhead）。
- `benchmark_slice.v1` preview：max 1200 bytes ≈ 230 tokens（scores 对
  象紧凑）。
- `parse_pdf.v1` preview：max 1400 bytes ≈ 320 tokens。
- `pdf_text_section_read` fallback 400 bytes ≈ 100 tokens（见 F6）。

**零散展开 30 条算术**：
- 若全部按 parse_pdf.v1 最厚：30 × 320 = **9600 tokens**，远超
  tool_previews 1500 cap。**不可行**。
- 若全按 finding preview：30 × 220 = **6600 tokens**。**不可行**。
- 若全走 fallback 100 tokens：30 × 100 = 3000 tokens。**仍超**。

**实际 assembler 策略**（v0.2 可推导）：
1. tool_previews 分层：本段"当前正在写"的 3 个主引用展开结构化
   preview（3 × 230 = 690 tokens），另 ~5 个"上下文引用"折成 title+
   artifact_ref 列表（8 × 20 tokens = 160 tokens），共 850 tokens。
2. 剩 30 - 3 - 5 = 22 个历史引用**不入 tool_previews**，转由
   `retrieved_facts` zone 以 `session_state_items(finding/citation)`
   Top-K 投影承载（每条 40 tokens 结构化摘要 = 880 tokens，落在
   retrieved_facts 800 zone 内 + 80 溢出 → drop 2 条最低 score）。
3. 合计 tool_previews 850 + retrieved_facts 800 + 其余（plan_todo 280
   + recent_tail 1200 + summary 500 + session_anchor 300 + system_
   static 1200 + tool_schemas 1600）= **6730 tokens**。
4. 对照 §Budget Policy v1 @L2395 `tool_previews` "include short previews
   only; link artifacts" 与 @L2403 "Prefer structured L1a facts over
   verbose older prose"——策略 2 正是设计意图。

**结论**：30 artifact_ref 展开**不能天真全部进 tool_previews**。v0.2
设计隐含要求 assembler 做 "当前焦点 3 条厚 preview + 辅助 5 条薄链接
+ 余下走 retrieved_facts state_items 摘要" 的三层分发。该策略文档未
明写；建议在 §Budget Policy v1 补一段 "High-fanout artifact_ref
展开策略"作为规范。

## §8 最终判断

- **本场景下 v0.2 是否可实现**：✅（核心机制齐备）
- **顺畅度**：⚠️ 有明显摩擦（F4/F6 + §7 三层分发策略需文档补丁）
- **总结一句话**：v0.2 用 G8 + G9 + G2 + G17 四项联动正好覆盖 S12 的
  "结构化 preview + 长期 artifact + compaction 保护 + hash 稳定" 主干，
  但 `tool_previews` 静态 cap 在 high-fanout 撰写轮不够用，需要
  intent-aware flex 机制（新 gap 候选 G20）与 `pdf_text_section_read`
  模板补全（friction F6），跨周项目的 citation → artifact 悬空链在
  `project_long_term` 到期时建议默认走 `archived_cold` 而非 expire。
