# S12 — 研究型长会话：AI 研究员做「小型 LLM 在医疗领域微调」综述

> Status: Draft v0.1
> DAUT: **A3, A9, A10**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：Miranda，某医学院计算所 PI 博后，研究方向是"小型
LLM（≤ 7B）在临床文本上的 domain adaptation"。典型研究员语气：
好奇、细致、重证据，说话常以「等等、刚才那篇…在哪儿？」开
头，对数字（benchmark、parameter 量、样本量）锱铢必较，一旦引
用原文就要 agent 逐字回到 PDF 里核对，不允许"我记得大概是
78%"这种模糊转述。她对 UI 美观无感，但极其在乎两件事：（a）
引用能追溯到原始 artifact 的哪一段；（b）两周之后再打开会话，
上周读过的那 50 篇 PDF 的 `key_findings` 一条都不能丢。

**业务背景**：Miranda 要在 3 周内向期刊投一份 survey，题目「Small
LLM Fine-tuning Approaches for Clinical Text Understanding」。用
astra web agent 驱动三类工具：`fetch_url`（arxiv / huggingface /
medium 技术博客 / 机构 blog，约 200 个 URL，每个返回 10–200KB
HTML/PDF 元数据）、`parse_pdf`（下载 + 文本抽取 50 篇论文，每
篇 raw 3–10MB、text 100–500KB）、`llm_extract_findings`（对每篇
text 做结构化抽取：title / authors / task / dataset / method /
key_findings / benchmark）。会话跨 2 周，从周一下午开第一次起跑
到周末第二周交付，浏览器在工位上一直挂着，期间数次切 laptop /
iPad。最终聚合一份 ~30 页 markdown 综述。

**一句话目标**：在 ~45 轮用户交互、~300 次 tool 调用、~250 个
artifact、raw 内容累计数 GB 的压力下，prompt 永远只见**结构化
preview**，所有"那篇 BioBERT…"「LoRA 那 5 篇…」的回查都通过
structured filter / FTS 走索引路径；两周后综述撰写阶段引用原文
要按 artifact_ref 精准读到指定 section byte-range 而不是把 PDF 全
塞 prompt。

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 会话总轮数 | ~45 回合（本文抽样 12 个关键回合） |
| 会话持续时间 | 14 天 |
| tool 调用总数 | ~300 次（200 fetch_url + 50 parse_pdf + 50+ llm_extract + 其余 render/search） |
| `session_tool_outputs` 累计行数 | ~300 |
| `session_artifacts` 累计行数 | ~250 |
| 累计 raw byte_size | ~6.8 GB（200 HTML 合计 ~2 GB + 50 PDF raw ~4 GB + 解析 text ~800 MB） |
| Prompt 单轮 `tool_previews` 上限 | ≤ 1500 tokens |
| Prompt 单轮总上限 | ≤ 8k tokens（本场景从不触碰上限） |

## 2. 被测 DAUT

本场景的核心矛盾是「同质工具海量小输出 × 结构化 preview 依赖 ×
跨周回查精度」，下列 3 条假设被同时压测。与 S08 的对照：S08 是
**单次超大输出**（3GB pg_dump、800MB slowlog），靠 preview 截断
+ artifact_ref；S12 是**同质工具的海量小输出聚合**（200 fetch_url、
50 parse_pdf），靠 preview 的**结构化字段**让 FTS / filter 能精确
命中，preview 不结构化就等于把 agent 丢进一堆"标题相似但内容
各异"的海里瞎猜。

| 编号 | 压测方式 |
| --- | --- |
| **A9** | 200 URL + 50 PDF 的 raw 内容（累计 ~6.8GB）**绝不进 prompt**。每个 tool 调用必须对应 1 行 `session_tool_outputs`（外加 1 行 `session_artifacts`），`preview_text` ≤ 1k token 且**必须结构化**：`fetch_url` 的 preview schema = `{url, http_status, title, first_paragraph, keywords[], relevance_score, content_type}`；`parse_pdf` 的 preview schema = `{title, authors[], year, venue, abstract, key_findings[], benchmarks[], section_count}`。同工具共用 `preview_template` 版本号（见 §8 建议 1）。context assembler 对 `byte_size > 16KB` 的 tool 输出强制走 preview 路径，artifact 原文只在 tool executor 与 OSS 之间流动。 |
| **A3** | `Tools` / `Files` 两个 UI tab 在打开时**绝不扫全表**。Tools tab 按时间倒序分页 `WHERE user_id=? AND session_id=? ORDER BY created_at DESC LIMIT 50`，命中 `idx_tool_outputs_user_session_created` ；Files tab（基于 `session_artifacts`）同理。Miranda 在第 14 天打开 session 时页面 first-paint < 500ms，尽管背后有 300 行 tool_outputs + 250 行 artifacts + 6.8GB raw；UI 只读两个索引投影。展开任何单条 PDF artifact 才按需拉 OSS presigned URL。 |
| **A10** | Miranda 反复问的三类问题走三种检索：（i）"列前 20 个相关度高的" → structured filter `WHERE tool_name='fetch_url' ORDER BY JSON_EXTRACT(preview_text, '$.relevance_score') DESC`（但注意：A10 + §SQL Performance 禁 JSON 列过滤，解决方案见 §8 建议 1 把 relevance_score 提升为一等列）；（ii）"关于 LoRA 微调的 5 篇" → tier 2 FTS `MATCH(preview_text) AGAINST ('+LoRA +fine-tun*' IN BOOLEAN MODE)`；（iii）"上周那个 BioBERT 方法论在哪" → time filter + FTS 组合 `WHERE created_at BETWEEN ... AND MATCH(preview_text) AGAINST ('BioBERT')`。FTS 在 `session_tool_outputs` 上 append-only，无 UPDATE/DELETE bloat。 |

## 3. 会话时间线

12 个抽样回合覆盖 14 天：T1 开题、T2 拆 plan、T3 批量爬 200 URL、
T4 相关度排序、T5 下载解析 50 PDF、T6 FTS 找 LoRA 那几篇、T7
每篇抽 key_findings 落结构化投影、T8 跨周回查 BioBERT、T9 精
度要求触发 raw 切片加载、T10 第二次 compaction、T11 撰写时逐
条核对、T12 交付。每回合 3–5 行：用户原话 / state 变化 / manifest
zone / LLM 看不到什么。

### T1 · Day 1 14:00 开题「做一份小型 LLM 医疗微调综述」

**用户原话**：「想做一篇 survey，主题是小型 LLM（≤7B，LoRA /
QLoRA / adapter 一类 PEFT 方法）在临床文本（EHR、出院小结、放
射报告）上的 fine-tuning。先帮我拉一批文献过来，arxiv 近两年的
为主，加几个 huggingface model card 和技术 blog。」

**state 变化**：`anchor` 新增 `{goal: "survey on small LLM PEFT for
clinical NLP", scope_years: [2023, 2025], param_cap: 7e9}`；`workspace_
state='cloud'`（本场景无 edge 工具）；`plan_state` version=0；
`session_tool_outputs` / `session_artifacts` 均空。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1600（保留
fetch_url、parse_pdf、llm_extract_findings、fts_search、render_survey
5 类）/ `session_anchor` 280 / 总 ≈ 3080，`reason='initial_turn'`。

**LLM 看不到**：Miranda 脑子里 5 个候选关键词（"医学大模型"、
"BioGPT"、"clinical BERT"、"discharge summary"、"MIMIC-III"——她
只说了前两个）。

### T2 · Day 1 14:10 agent 拟 plan + 建 todos

**用户原话**：（隐式）agent 基于 T1 自发规划，Miranda 默认同意。

**state 变化**：`plan_state` version=1，phase='literature_collection'；
`session_todos` 批量 insert 6 条（①批量 fetch 200 URL、②下载解析
50 PDF、③每篇抽 key_findings、④按方法论分组、⑤撰写综述各章、
⑥交叉核对引用），`source='agent'`；`session_todo_deps` 5 条链式依
赖；新增 2 条 `decision`：`key='scope.param_cap.v1'`（≤7B） /
`key='scope.domain.v1'`（clinical text only）。

**manifest zone**：`plan_todo` 380 / `session_anchor` 300 / `tool_
schemas` 1600 / `system_static` 1200 / `recent_tail` 480。总 ≈ 3960。

**LLM 看不到**：无。

### T3 · Day 1 14:30–16:00 批量爬 200 URL

**用户原话**：「先把候选文献抓过来，arxiv 用关键词 'clinical LLM
fine-tuning'、'LoRA medical'、'BioGPT'、'PEFT healthcare' 各拉 50
条，再加 20 个 huggingface model card 和 20 个 blog。」

**state 变化**：agent 发起 `fetch_url` **批量调用**（tool executor 侧
fan-out + 并发 10），共 200 次独立 HTTP fetch。每个 URL 对应 1
行 `session_tool_outputs`（`tool_name='fetch_url'`、`status ∈ {'ok',
'fetch_fail', 'parse_fail'}`、`byte_size` 实际 HTML 字节 10KB–200KB、
`token_estimate`=80–120）、1 行 `session_artifacts`（`artifact_type=
'html_raw'`，存 OSS）。**每行 `preview_text` 严格按 schema v1**：
200 字符 JSON 序列化 `{url, http_status:200, title:"<90 char>",
first_paragraph:"<80 char>", keywords:[...], relevance_score:0.82,
content_type:"arxiv_abstract"}`。批量 insert 拆 10 批 × 20 行避免
N+1。relevance_score 由确定性的词法、域名、文档类型信号在 executor
侧就地计算，**不**经 LLM。

**manifest zone**：本轮 agent 只需 ACK 200 个 fetch 完成，不对任
何单 URL 做 reasoning。`tool_previews` 320（1 条聚合 preview：
"fetched 200 URLs, ok=182, fetch_fail=11, parse_fail=7, top domains:
arxiv=148, hf=22, medium=12"）/ `plan_todo` 380 / `recent_tail` 600。
总 ≈ 4100。**关键**：200 行 per-URL preview **不进** prompt，仅待
T4 structured filter 时被检索。

**LLM 看不到**：200 个 HTML 原文累计 ~2 GB；200 行 per-URL
preview 的任何一条；具体 keywords 数组内容。

### T4 · Day 1 16:10「列一下相关度最高的前 20 个」

**用户原话**：「刚才抓的这 200 个，按相关度从高到低列前 20 给
我，我先筛一轮 PDF 下哪些。」

**state 变化**：agent 不调新工具，发一次 **structured filter**：
`SELECT output_id, preview_relevance_score, preview_title,
preview_url FROM session_tool_outputs WHERE session_id=? AND
tool_name='fetch_url' AND status='ok' ORDER BY preview_
relevance_score DESC LIMIT 20`。此处要求 `preview_relevance_
score` 是**一等列**而非 `JSON_EXTRACT(preview_text)`（见 §7 F1 +
§8 建议 1：A10 + §SQL Performance 禁止 JSON 列过滤 / 排序）。
命中 `idx_tool_outputs_tool_created` + `idx_tool_outputs_session_
relevance`（新增复合索引）。返回 20 行共 ~5KB → ~1200 tokens。
Miranda 对 20 行勾选了 50 篇候选 PDF（通过 UI multiselect；
runtime 落 `session_state_items(category='tool_ref', item_key=
'pdf.candidate.batch.v1', payload_json=[artifact_ref list])`）。

**manifest zone**：`tool_previews` 1100（20 条 preview 渲染）/
`plan_todo` 380 / `recent_tail` 700。总 ≈ 4400。

**LLM 看不到**：其余 180 条 fetch_url preview；200 个 HTML 原文。

### T5 · Day 1 16:30–Day 2 11:00 下载 + 解析 50 PDF

**用户原话**：「把刚才选的 50 篇下载下来，PDF 解析成文，每篇
给我做个 structured metadata。」

**state 变化**：agent 发起 `parse_pdf` 批量调用，50 次并发。每篇：
（a）下载 PDF raw（3–10MB）→ `session_artifacts` 新 1 行
`artifact_type='pdf_raw'`；（b）executor 侧用 pdfminer 解析为 text
（100–500KB）→ `session_artifacts` 新 1 行 `artifact_type='pdf_
text'`，`derived_from=<pdf_raw>`；（c）`session_tool_outputs` 新 1 行
`tool_name='parse_pdf'`，`status='ok'`，`byte_size`=text 字节数，
`preview_text` 严格按 `preview_template='parse_pdf.v1'`（600 字符
JSON）：`{title:"...", authors:["...",...], year:2024, venue:"NeurIPS",
abstract:"<200 char>", section_count:7, byte_text:287043}`。**注意**：
T5 阶段还没跑 `llm_extract_findings`，preview 中 `key_findings` /
`benchmarks` 字段**暂缺**，schema v1 允许 null，T7 再补。50 行
batch insert 拆 5 批 × 10 行。

**manifest zone**：`tool_previews` 500（1 条聚合 preview："parsed
50 PDFs, total text 18.4MB, avg section_count=6.8, top venues:
arxiv/cs.CL=22, NeurIPS=9, ACL=7"）/ `plan_todo` 380 / `recent_tail`
900。总 ≈ 4500。

**LLM 看不到**：50 个 PDF raw 累计 ~4GB；50 篇 text 合计
~18MB；50 行 per-PDF preview 的原始 JSON。

### T6 · Day 2 14:00「关于 LoRA 微调的 5 篇再列一下」

**用户原话**：「等等——刚才解析的 50 篇里，专讲 LoRA 或 QLoRA
微调的那几篇，列一下？我想先看方法论那章。」

**state 变化**：`NextActionExtractor` 识别为历史回查，走 tier 2 FTS：
`SELECT output_id, preview_text FROM session_tool_outputs WHERE
session_id=? AND tool_name='parse_pdf' AND MATCH(preview_text)
AGAINST ('+LoRA OR +QLoRA' IN BOOLEAN MODE) ORDER BY
MATCH(...) DESC LIMIT 10`。FTS 命中 preview 里 title / abstract /
authors 字段中含 LoRA 的 6 行，相关度排序返回。agent 渲染为
markdown 列表。**无新 tool_output 写入**。`session_state_items` 新
增 1 条 `tool_ref` `key='findings.lora.subset.v1'` 记录这 6 篇的
artifact_ref 快照，供后续"那批 LoRA 文献"代词解析。

**manifest zone**：`tool_previews` 900（6 条 preview 渲染 + 聚合头）
/ `plan_todo` 380 / `recent_tail` 800。总 ≈ 4500。**关键验证**：本轮
FTS 相关度排序能区分出"LoRA 是主方法"和"LoRA 仅在 related work
里提一句"的两类论文——这只有在 preview 结构化且 title/abstract
独立字段时才能实现（见 §7 F2）。

**LLM 看不到**：其余 44 篇 parse_pdf preview；50 篇 pdf_text 原文。

### T7 · Day 2 15:00–Day 3 18:00 逐篇抽 key_findings

**用户原话**：「对刚才选定的这 50 篇，每篇都做一次详细抽取：
method / dataset / base model / param / benchmark / key claim，存
结构化。」

**state 变化**：agent 发 50 次 `llm_extract_findings` 工具（输入=
pdf_text artifact_ref；executor 侧直接从 OSS 流式读 text 给 LLM，
**不**经 runtime）。每次 LLM 输出 1–3KB JSON，executor 写回两处：
（a）`session_tool_outputs` 新 1 行 `tool_name='llm_extract_
findings'`，preview 300 字符只含 `{paper_title, main_method, main_
dataset, top_benchmark}`；（b）`session_state_items` 新 50 条
`category='finding'`（本场景**新增 category**，见 §8 建议 2），
`item_key='finding.<paper_id>'`，`payload_json` 存完整结构化
findings，`payload_hash=sha256(json)`。**为什么 finding 要 state_items
而不仅 tool_outputs**：后续 T11 撰写时要按方法/dataset group by
查询，state_items 的 `idx_state_session_category` 直接支持；而
tool_outputs 索引是按 tool_name + time 的。50 条 state_items 批量
insert 走 5 批 × 10 行。

**manifest zone**：`tool_previews` 600（1 条聚合 preview："extracted
findings from 50 papers, methods: LoRA=18, QLoRA=7, adapter=9,
full_ft=6, prompt_tuning=10; datasets: MIMIC-III=22, i2b2=14,
MedNLI=8, custom=6"）/ `retrieved_facts` 480（选 Top-K decision +
finding）/ `plan_todo` 360 / `recent_tail` 900。总 ≈ 4800。

**LLM 看不到**：50 次 llm_extract 的完整 output JSON（只见聚合
摘要）；pdf_text 原文。

### T8 · Day 7 10:00 跨周回查「上周那个 BioBERT 方法论在哪儿」

**用户原话**：「上周读的…那篇 BioBERT continual pretraining 的，
方法那章他是怎么处理 domain shift 的？我记得作者是清华那边
的？忘了是哪一篇了。」

**state 变化**：`NextActionExtractor` 识别到**时间+关键词**双线索
（"上周"、"BioBERT"、"清华"），构造组合查询：`SELECT * FROM
session_tool_outputs WHERE session_id=? AND tool_name='parse_pdf'
AND created_at BETWEEN DATE_SUB(NOW(), INTERVAL 10 DAY)
AND DATE_SUB(NOW(), INTERVAL 3 DAY) AND MATCH(preview_
text) AGAINST ('+BioBERT +Tsinghua') LIMIT 5`。命中 `idx_tool_
outputs_tool_created` 的时间范围 + FTS 二次过滤，返回 2 行，最
终 top-1 确认。agent 根据该论文的 `artifact_ref` 定位到 pdf_text
artifact，**按 section_id 读取**「Method」章节的 byte-range（约
8KB）交 LLM 回答——**不**加载整篇 text（~300KB）。**session_
state_items(finding)** 中该论文的 `key_claim` 字段命中，supplement
LLM 回答的首句。

**manifest zone**：`tool_previews` 720（1 条定位到的 preview + 1
条按 section 加载的 raw 切片 preview，"method_section_byte_range=
48213-56789"）/ `retrieved_facts` 520（含该论文 finding state_item）/
`plan_todo` 360 / `recent_tail` 1100。总 ≈ 5100。

**LLM 看不到**：该 PDF 其余 ~290KB 文本；其他 49 篇的任何内
容。

### T9 · Day 8 11:00「综合 5.2 节那批文献的 benchmark 再对比一下」

**用户原话**：「之前第 5.2 节（method=LoRA 那批）里的 benchmark
数字，我现在要画个 table，EM / F1 / ROUGE 都要精确到小数点后
两位。刚才 preview 里只给了 top_benchmark，不够。」

**state 变化**：精度要求**超出 preview**（preview 只存 top_
benchmark 一个数字）。agent 识别需要走 **按需 raw 切片加载**：
对 T6 已经定位的 6 篇 LoRA 论文，逐篇做 `pdf_text_section_read`
工具调用（input=artifact_ref + section_hint='benchmark|results|
evaluation'），executor 侧用正则切出 benchmark 表格部分（每篇
~3–8KB），写入 `session_tool_outputs` 6 行新 tool 调用，preview
300 字符结构化 `{paper_id, benchmark_name, scores:{em:0.78,
f1:0.82, rouge:0.65}}`（schema `preview_template='benchmark_slice.
v1'`）。**同时**写 6 条 `session_state_items(category='benchmark')`
（本场景新增二等 category，见 §8 建议 2）。agent 最后用这 6 条
state_items 聚合渲染 markdown table。

**manifest zone**：`tool_previews` 1400（6 条 benchmark_slice 的结
构化 preview，逼近 1500 上限）/ `retrieved_facts` 440 / `plan_todo`
340 / `recent_tail` 900。总 ≈ 5600。**关键**：若 preview 不结构化，
LLM 要从每篇 text 8KB 原文里自己抽数字，出错概率大且 token 成
本 × 6。

**LLM 看不到**：6 篇论文的非 benchmark 章节；其余 44 篇 PDF。

### T10 · Day 10 09:00 第二次 compaction（综述跨两周）

**用户原话**：（无，compaction 后台触发。）

**state 变化**：`recent_tail` 累积超 3000 tokens 触发 compaction。
按设计 §Compaction：（1）`conversation_log` 全量保留不动；（2）
写 1 条 `session_state_items(category='summary')`
`key='summary.week1.fetch_and_parse.v1'`，压缩 T3–T5 的 fetch +
parse 交互为 ~500 tokens 叙事；（3）**关键 L1a 保留**：50 条
finding state_items、6 条 benchmark state_items、2 条 decision、
`session_todos` 当前状态均**不动**；（4）老 tool_output rows 的
`manifest_visibility='compacted'`（新增列，或用 `session_history_
chunks` 投影，`chunk_type='tool_output_preview'` / `'summary'`），
新 `context_manifest` 不再默认加载 T3–T5 的 tool_previews；（5）
artifact 全部保留（两周项目不能 GC）。

**manifest zone**：`summary` 500（week-1 叙事）/ `retrieved_facts`
600（提升权重：decision + finding 进入主区）/ `plan_todo` 320 /
`tool_previews` 480（只含最近 3 轮）/ `recent_tail` 1000。总 ≈
5100。**验证不变量**：`session_state_items` 50 条 finding 条目**一
条不少**；`session_artifacts` 250 行无任何 `status='expired'`。

**LLM 看不到**：T3–T5 的 tool_previews 详情（已被 summary 替
代）；老 recent_tail 原文（仍可通过 history_chunks 按需检索）。

### T11 · Day 12 14:00–Day 13 22:00 撰写综述，逐条核对引用

**用户原话**：「开始写第 3 章「方法论分类」。按 PEFT 方法 group：
LoRA / QLoRA / adapter / prompt_tuning 各一小节。每段引用至少
3 篇，引用前你必须回到原 PDF 验证我给的数字和你 preview 里写
的一致。」

**state 变化**：agent 进入"撰写 + 核对"循环，每写一段引用
~3 篇文献，每篇：（a）按 artifact_ref 拉 pdf_text 对应 section
（~5KB byte-range），校对 preview/finding 中的数字；（b）若发
现不一致（本场景发生 2 次），更新 `session_state_items(finding)`
的对应字段，写 1 条 `session_state_item_events(mutation=
'update', previous_hash, next_hash)` 审计；（c）写 1 条
`session_state_items(category='citation')`（新增，一等），`item_
key='citation.<paper_id>.<section>'`，`payload_json={quote, page,
byte_offset}`。共约 30 次引用，30 次 section_read。

**manifest zone**：`tool_previews` 1200（最近 5–6 次 section_read
+ 撰写当前段的 preview） / `retrieved_facts` 800（本段引用的 3
篇 finding + 2 条 decision） / `plan_todo` 280 / `recent_tail` 1200 /
`summary` 500。总 ≈ 6000。

**LLM 看不到**：未被本段引用的其余 ~47 篇论文；所有 PDF raw。

### T12 · Day 14 16:00 交付 + 下载

**用户原话**：「写完了吧？生成最终 markdown，把 bibliography 附
上，我下载。」

**state 变化**：agent 调 `render_survey` 工具合成最终 markdown
（~180KB，30 页）。落 1 条 `session_artifacts` `artifact:survey-
clinical-small-llm-peft-v1`，`byte_size=184320`，`content_hash=
sha256(normalized)`，`provenance_refs` = 全部 50 pdf_text +
50 finding state_items + 30 citation state_items hash 的数组。
`session_todos` 最后一条 `status='done'`。UI 下载走 302 →
OSS presigned URL，API server 不流式代理。

**manifest zone**：本轮 LLM 仅做一次 "confirm final rendering"，
`tool_previews` 500 / `retrieved_facts` 600 / `plan_todo` 80（全 done）
/ `summary` 500 / `recent_tail` 800。总 ≈ 4600。

**LLM 看不到**：final markdown 全文（executor 侧生成）；任何 PDF。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 200 fetch_url + 50 parse_pdf + 50 llm_extract = ~300 同名工具行，FTS 相关度排序容易失真（同 title 相似论文、同 keyword 多命中） | A9 + A10：`preview_text` 必须结构化（title/abstract/authors 各字段独立可索引），FTS 查询带字段权重（`MATCH(preview_text) AGAINST (... IN BOOLEAN MODE)` + 业务层按 schema field 二次 re-rank），而不是把整个 preview 当 bag-of-words |
| 单轮 `tool_previews` ≤1500 tokens，但 T9 要同时引用 6 篇的 benchmark | A5 + A9：preview 结构化后 6 条 × 200 tokens 刚好塞下；若 preview 是自由文本截断（400 字符），6 条 ~2400 tokens 超预算 |
| 跨 14 天，`recent_tail` 累计自然超出上限 | A6：compaction 在 T10 触发，summary 压缩会话叙事，但 L1a（50 finding / 30 citation / 2 decision）**按 category 保留**不压缩 |
| Miranda 的"上周那个 BioBERT"代词 + 时间线索 | A10 tier 1 + tier 2 组合：时间 filter 走 `idx_tool_outputs_tool_created`，关键词走 FTS，二者合取即可精确定位 |
| 撰写综述需要精确数字 —— 仅靠 preview 不够 | A9：preview 只保存 `top_benchmark`，精确对比触发 `pdf_text_section_read` 按 byte-range 读取，仍不把整 PDF 拉进 prompt |
| artifact_ref 大量外部（arxiv PDF 的原始 URL 在 HTML fetch 中引用），外链可能失效 | A9 + §Artifact Retention：所有 raw 必须落本项目 OSS，不依赖原站 URL；`session_artifacts` 写入时即完成 mirror |
| 同工具的 preview schema 演进（v1→v2 字段变化） | A9 + §8 建议 1：preview_template 版本化，context builder 按版本渲染；FTS 索引要处理版本差异（见 §7 F5） |

## 5. 预期 state projection 演化

三个关键节点快照，覆盖 fetch 完成 / finding 抽取完成 / 交付前。

**T3 末（Day 1 16:00, 200 fetch_url 完成后）**：

| 投影 | rows | 累计 byte_size | 说明 |
| --- | --- | --- | --- |
| `session_tool_outputs` | 200 | ~2.0 GB（指向 HTML） | 200 行 fetch_url |
| `session_artifacts` | 200 | ~2.0 GB | 200 html_raw |
| `session_state_items` | 5 | — | anchor + 2 decision + plan + workspace |
| `session_todos` | 6 | — | 1 done (fetch), 5 active/pending |
| manifest `tool_previews` | — | — | 320 tokens（1 条聚合） |

**T7 末（Day 3 18:00, 50 findings 抽取完成后）**：

| 投影 | rows | 累计 byte_size | 说明 |
| --- | --- | --- | --- |
| `session_tool_outputs` | 300 | ~2.8 GB（raw 指向 OSS） | +50 parse_pdf +50 llm_extract |
| `session_artifacts` | 250 | ~6.8 GB | +50 pdf_raw +50 pdf_text |
| `session_state_items (finding)` | 50 | — | 每篇 1 条，含完整结构化 findings |
| `session_state_items (其他 category)` | 5 | — | 同 T3 |
| `session_todos` | 6 | — | 3 done, 3 active |
| manifest `tool_previews` | — | — | 600 tokens |

**T12 末（Day 14 16:00, 综述交付）**：

| 投影 | rows | 累计 byte_size | 说明 |
| --- | --- | --- | --- |
| `session_tool_outputs` | ~310 | ~2.8 GB | +6 benchmark_slice +~30 section_read +1 render_survey |
| `session_artifacts` | 251 | ~6.98 GB | +1 survey markdown |
| `session_state_items (finding)` | 50 | — | 其中 2 条在 T11 被 update（update 事件有审计） |
| `session_state_items (benchmark)` | 6 | — | T9 产生 |
| `session_state_items (citation)` | ~30 | — | T11 产生 |
| `session_state_items (summary)` | 1 | — | T10 compaction 产生 |
| `session_todos` | 6 | — | 全 done |
| `context_manifests` | ~45 行 | — | 每轮一条 |

**关键不变量**：全过程 API server 进程 RSS 峰值涨幅 < 150MB（所
有 raw 流量都在 executor↔OSS 之间）；`content_hash` 在同 URL 重
抓、同 PDF 重解析时稳定（规范化剔除下载时间戳）；`session_state_
items(finding)` 50 条在 T10 compaction 后**条数不变**。

## 6. Token 节省估算

**朴素方案**（把所有 tool 输出直接塞 prompt）：

- 200 HTML raw 累计 ~2 GB，按 3 bytes/token 估算 ~6.7 亿 tokens，
  单 prompt 根本无法容纳。
- 50 PDF text 累计 ~18MB，~6M tokens，单 prompt 超任何窗口。
- 50 次 `llm_extract_findings` 输出若不结构化而是自由文本回帖，
  累计 ~150KB、~40K tokens，仅"抽取一轮"就把 8k 窗口挤爆。
- **结论**：朴素方案在 T3 就崩溃；即便把 200 HTML 先压成自然
  语言摘要塞入，摘要成本就要 ~500K tokens 级，且语义丢失严
  重，T6 "关于 LoRA 的 5 篇" 从自由文摘里根本筛不准确。

**本设计方案**（T11 撰写第 3 章那一轮的实际 prompt 构成）：

| zone | T11 当轮 | 说明 |
| --- | --- | --- |
| `system_static` | 1200 | 常驻 |
| `tool_schemas` | 1600 | 5 类工具 |
| `session_anchor` | 300 | survey 目标 + scope |
| `plan_todo` | 280 | 当前章节 todo |
| `tool_previews` | 1200 | 最近 5 次 section_read + 撰写 preview |
| `retrieved_facts` | 800 | 本段引用的 3 条 finding + 2 decision |
| `recent_tail` | 1200 | 最近 3 轮原文 |
| `summary` | 500 | week-1 叙事（T10 compaction 产物） |
| **合计** | **≈ 6480** | 在 8k 预算内留余量 |

**对比表**：

| 指标 | 朴素 | 本设计 | 节省 |
| --- | --- | --- | --- |
| T3 200 HTML 进 prompt | 6.7 亿 tokens | 320 tokens | ≈ 100% |
| T5 50 PDF text 进 prompt | 6M tokens | 500 tokens（聚合 preview） | ≈ 99.99% |
| T7 50 findings 进 prompt | 40K tokens | 600 tokens（聚合 + 按需） | ~98.5% |
| 45 轮累计 prompt tokens | 无法估算（早已崩溃） | ~280K tokens（45×~6k） | — |
| artifact 总体积（落库不落 prompt） | — | ~6.98 GB | — |

raw 层面节省比 > **99.98%**。

**但真正的价值点不是节省本身，而是"preview 结构化 vs 朴素截断"
的差异**：

| 维度 | 自由文本截断 400 字符 | 结构化 preview 600 字符（本设计） |
| --- | --- | --- |
| "关于 LoRA 的 5 篇"（T6） | LLM 对 50 段文本瞎猜 | FTS 命中 title/abstract 精确返回 |
| "相关度 Top 20"（T4） | 不可能，没有 score 字段 | `ORDER BY preview_relevance_score` 一步到位 |
| "benchmark 精度"（T9） | 数字被截断概率高 | 结构化字段 scores 不截断 |
| "上周 BioBERT"（T8） | 时间 filter OK，但 Tsinghua 线索从截断文里可能漏 | author 字段独立命中 |

换句话说，结构化 preview 让 agent 在 **不看原文** 的前提下**知
道自己有什么**；非结构化的等价方案不是"省 token 少一点"，而
是"agent 的检索质量系统性崩坏"。

## 7. 潜在失败模式

**F1 · preview_text JSON 列被 A10/§SQL Performance 禁止过滤，T4
"按 relevance 排序" 无法实现**：若实现者把 `relevance_score` 塞进
`preview_text` JSON 内而不独立成列，T4 的 `ORDER BY JSON_
EXTRACT(preview_text, '$.relevance_score')` 触发全表扫描，200 行
还能扛，一旦这类 session 积累到 2000 行就不可用。**可观测信号**：
T4 延迟 > 2s；EXPLAIN 显示 full scan。**缓解**：关键结构化字段
（`relevance_score`、`paper_year`、`paper_venue`、`preview_template_
version`）必须提升为一等列，JSON 只存其余辅助字段；`session_
tool_outputs` 增加 `idx_tool_outputs_session_tool_relevance (session_
id, tool_name, preview_relevance_score DESC)`。

**F2 · 50 篇 PDF 都是 `parse_pdf` 同一工具名，FTS 相关度排序在
"LoRA" 这类高频词下失真**：若 FTS 只查 preview 整段文本，大量
论文只要 related_work 提及 LoRA 就会命中；用户期望的是"主方法
是 LoRA"。**可观测信号**：T6 返回 15+ 行而非 5 行；用户反馈"这
篇明明讲 QLoRA 的，LoRA 只是对比基线"。**缓解**：FTS index 按
preview 结构化字段分字段权重（title / abstract / keywords 权重
> related_work 提及）；实现上用 MatrixOne 的 boolean mode +
phrase match；或在 `session_tool_outputs` 派生列 `main_method`
（从 preview 抽取）并建普通索引。

**F3 · T10 compaction 把 50 条 key_findings 当老 recent_tail 丢
掉，只留 URL 列表**：若 compaction 实现只看 `recent_tail` 长度决
定压什么、不区分 `session_state_items.category`，`finding`
category 的 50 条可能在"L1b 叙事摘要"环节被糊成"Miranda 抓了
50 篇论文"一句话，T11 撰写时 agent 要不停地去读 PDF text 补
findings。**可观测信号**：T10 后 `retrieved_facts` zone 中 finding
条目从 50 骤降；T11 `pdf_text_section_read` 调用数飙升（预期 ~30，
失败时 ~200）。**缓解**：compaction 规则**明文白名单**：`category
IN ('finding','benchmark','citation','decision','active_file')` 的
state_items **永远不进压缩**；同时 `session_state_item_events` 不因
compaction 而 rewrite（append-only 历史）。

**F4 · artifact_ref 大量指向外部 OSS，外链 expire 后 raw 读不到**：
若实现者偷懒把 arxiv HTML 的 `<a href>` 直接存 artifact_ref 而不
mirror 到本项目 OSS，6 个月后某 URL 失效，T11 撰写需要回核时
返回 404，且 preview 已被 compaction 压缩。**可观测信号**：
`session_artifacts` 的 `raw_ref` 域名分布出现 `arxiv.org` /
`huggingface.co` 等外部域；`HEAD` 检测出现 404。**缓解**：
artifact 写入必须强制 mirror 到项目 OSS（`raw_ref` 只能是本项目
OSS bucket），fetch_url executor 负责下载 + 上传；外部 URL 仅作
`source_url` 元数据留档。

**F5 · preview_template 版本演进，v1 → v2 字段改名（`authors` →
`authors_list`）导致 FTS 跨版本失真**：研发迭代 agent 时更新了
parse_pdf 的 preview schema（增加 `arxiv_id` 字段 + 改 `author`
单数为 `authors` 数组），旧数据不重算，T6 的 FTS 可能在新数据
里找不到某几篇。**可观测信号**：T6 返回数字波动；`session_tool_
outputs` 出现 `preview_template_version` 多个值；同一 session 内
preview schema 混用。**缓解**：`preview_template_version` 提为一
等列 + FTS 查询分版本过滤 / 合并；schema bump 时后台 backfill
旧数据（改写 preview 列，保持 content_hash 与 raw 分离的前提下
安全重算）；或新旧字段并存一段时间，context builder 做字段兼
容映射。

**F6 · T9 "综合 5.2 节文献 benchmark" token 预算不够**：若某次需
要对比 8 篇论文而非 6 篇，6 × 200 tokens = 1200 没问题，8 ×
200 = 1600 超 `tool_previews` 1500 上限。**可观测信号**：context
builder 报"zone overflow, dropped_count=2"；LLM 回答遗漏 2 篇。
**缓解**：设计层面引入**按需 zone 弹性**——`tool_previews` 上限可
由当轮显式 intent（如 `intent='benchmark_comparison'`）暂时上调
到 2500，挤压 `recent_tail`；compact 历史 recent_tail 腾位置；或在
UI 侧提示 Miranda "当前对比 8 篇超预算，建议分两组"。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Tool Result and Artifact References（§4）与 §History Chunks —
   **现有设计缺口**：`session_tool_outputs.preview_text` 是自由
   `TEXT NULL`，没有按 `tool_name` 的 schema 注册机制。本场景
   `fetch_url` / `parse_pdf` / `llm_extract_findings` / `benchmark_
   slice` 四种工具的 preview schema 差别巨大，每种 schema 的
   FTS 权重、结构化字段提升为一等列的策略各不同。建议在设计
   里新增「Preview Template Registry」：每工具注册
   `preview_template_version`、字段 → 一等列映射、FTS 字段权重、
   按版本迁移策略。
2. §History Chunks（§6）`chunk_type` 枚举 — **缺少研究类一等
   chunk**：当前枚举含 `decision` / `tool_output_preview` /
   `artifact_text` 等；但 `finding`（论文核心主张）、`benchmark`
   （跨论文数字对比）、`citation`（引用+原文定位）这三种在综述
   类长期项目里是一等结构化单元，不是普通 tool_output。建议
   `chunk_type` 增补 `finding` / `benchmark` / `citation` 三个，
   `session_state_items.category` 同步增补。compaction 白名单应显
   式把这三类置于"L1a 绝不压缩"层级。
3. §Artifact Retention — **长期项目需"永久保留"开关**：默认
   retention 策略（如 90 天）对 survey 这类 14 天–数月工期的研
   究会话不友好，项目进行到一半老 artifact 被 GC 会直接破坏引
   用链。建议 `session_artifacts` 增 `retention_policy` 列
   （`default` / `project_long_term` / `permanent`）、`retention_
   until`；`agent_sessions` 增 `project_tag` 便于按项目整体设置
   retention；GC 前对 `referenced_by_state_items_count` +
   `referenced_by_citation_count` 反向查询，非零即延期。

**三条具体建议**：

- **Preview Template Registry**：每个 tool 声明一份
  `preview_template.yaml`（`version`、`first_class_columns`、
  `fts_field_weights`、`max_preview_bytes`），agent runtime 在渲
  染 preview 时按模板校验；无模板声明的工具默认走 400 字符
  自由文本兜底。T4 "相关度排序" / T6 "LoRA 筛选" / T8 "BioBERT
  时间定位" 都依赖这个 registry 的字段提升。
- **一等结构化 chunk 类别**：`session_state_items.category` 增
  `finding` / `benchmark` / `citation` 三类；`session_history_
  chunks.chunk_type` 同步。compaction 规则明文：`category IN
  ('finding','benchmark','citation','decision','active_file')` 的项绝
  不进 summary 压缩，永远以结构化形式驻留 `retrieved_facts`
  zone。
- **项目级 retention 钩子**：`agent_sessions` 增 `project_id` +
  `project_retention_policy`（`session` / `project_long_term` /
  `permanent`）；同一 `project_id` 下所有 artifacts / state_items
  统一按项目策略，而非按 session 生命周期。survey、book、thesis
  这类跨月项目必须显式标记 `permanent`，GC 守护进程对该标记
  始终跳过。
