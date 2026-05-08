# WALKTHROUGH S13 — 老陈现场写 7 版 go-code-review SKILL.md

> Walker: exploration agent · 视角: 运行时执行 v0.2 能否跑通
> 被测场景: [S13-personal-skill-iteration.md](S13-personal-skill-iteration.md) §1–§3
> 设计文档: [../web-agent-session-state.md](../web-agent-session-state.md) v0.2
> Gap 参考: [GAP-TRACKER.md](GAP-TRACKER.md)
> Scope: 一次 session 内 v1→v7 append-only + rollback + 跨 session rust skill 对齐

---

## §1 场景标识与 v0.2 映射

**一句话场景**：老陈一次 session 内现场迭代 7 版 `go-code-review` personal skill（source_type='user'、visibility='private'），中途 T8 "切回 v2" 用 structured UI event，T11 还要跨 session 把上周 rust 版拉来对齐；全程 prompt skills zone 只渲染 active 那一版 content_markdown，历史 6 版永远 cold。

**被测 DAUT**：A12（skill 版本化 / append-only / 指针切换）、A2（audit/projection/manifest 三张表互不替代）、A15（structured intent 不走 LLM 回溯）。

**v0.2 关键章节/字段定位**：
- `user_skill_sources` / `user_skill_versions` / `user_skill_evaluations` schema — §9 Personal Skills L2038–2118（evaluations 表 L2078–2096、`normalize_version` 列 L2065）。
- G16 Personal Skill Activation and Evaluation — L2120–2161：`session_state_items(scope='session', category='active_skill').payload_json.version_id` 冻结；quarantined 不能 auto-activate；`user_skill_evaluations` 独立表不塞 session_state_items。
- G17 Content Hash Normalization Contract — L2163–2194：SKILL.md 用 `normalize_version='skill_md_v1'`，canonical input = `canonicalize(manifest_json) + "\n" + normalize_markdown(content_markdown)`，规则含 YAML key 字典序、LF 换行、trim 尾空白、code fence 外合并空行。
- G12 Next-Action Confidence — L657–698：structured_event confidence=1.0 直接 auto-accept，T8 "切回 v2" 走这条不走 LLM。
- Skill user APIs — L2751–2758：`POST /skills/user/{skill_name}/versions` / `/activate` / `/evaluations`。
- `session_state_items.category` 允许值含 `active_skill` — L1144。

---

## §2 运行前提

- **Schema 初值**：`agent_sessions` 为老陈新建一条；`user_skill_sources` / `user_skill_versions` / `user_skill_evaluations` / `skills_registry` / `skill_installations` 已执行 v0.2 DDL（含 `skill_installations.scope` ALTER L2099 和 `normalize_version` 列 L2065）。老陈此前已有一条 `src-rust-code-review`（visibility=private，active v1，1 条 evaluation）用于 T11。
- **Workspace**：cloud-only，工具集含 `read_pr_diff(pr_id)`、`fetch_url`、`SkillMd.validate`；无 edge bridge。
- **Budget template**：老陈用 Claude Sonnet 4.5（200k 窗口）→ 走默认 `budget_v1` 非 8k 小窗口，`context_manifests.budget_template_id='budget_v1'`、`tokenizer_id='claude_sonnet_4_5'`。
- **Zones 初始 cap**：skills zone 软上限未在 Budget Policy v1 L2381–2407 明确枚举（v0.2 漏项，见 §5），假定 ~3500；delegation_state 本场景禁用（无子 agent）。
- **UI 侧**：skill 面板可发 `POST /skills/user/{skill_name}/activate {version_id}`（structured intent），`GET /skills/user?prefix=rust`（若支持，见 §5）。

---

## §3 端到端运行轨迹（T1–T12）

### T1 · 开 session，声明意图
- **User**：「我准备写个自己的 go code review skill，直接在这个 session 里改到定稿，你边写我边试。」
- **Server**：`INSERT agent_sessions(session_id='sess-1', user_id='laochen', workspace_id=..., created_at=NOW())`；`session_state_items(scope='session', category='session_anchor', item_key='intent', payload_json={goal:'author personal skill go-code-review', mode:'iterative'})` 1 条；**尚不建 `user_skill_sources`**（等 T2 提交 v1）。
- **Manifest**：第 1 轮 `context_manifests(reason='initial_turn', budget_template_id='budget_v1', tokenizer_id='claude_sonnet_4_5')`；skills zone=0 tokens；active_skill 状态为空。
- **UI sees**：SSE `session.opened`；skill 面板提示"你还没激活任何 skill"；anchor 300 tokens。
- **LLM 看不到**：其他 user 的 skill；rust skill（owner 检查在 selector 里挂住）。
- **判断**：✅ 机制通顺；轻微 friction—空壳 source 避免写是对的，但"intent 已写但 skill 未建"的展示层需前端做空态（非 v0.2 责任）。

### T2 · 粘 v1 SKILL.md 原文 — 建 source + v1 版本 + active
- **User**：「v1 先来：前言 yaml `name: go-code-review / scope: pr-review`，body 8 条 checklist；ctx 那条下版再加。」（~1.6k tokens）
- **Server**（4 表联动）：`INSERT user_skill_sources(source_id='src-gocr-1', user_id='laochen', skill_name='go-code-review', visibility='private', status='active')` — UNIQUE KEY `(user_id, skill_name)` 保护；`INSERT user_skill_versions(version_id='ver-v1', source_id='src-gocr-1', version='v1', manifest_json={...}, content_markdown=<原文>, normalize_version='skill_md_v1', content_hash=sha256(canonicalize(manifest)+"\n"+normalize_md(body))='0xabc...01', token_estimate=1620, status='draft')` — G17 L2163–2194 规则生效；`UPSERT skills_registry(owner_user_id='laochen', source_type='user', visibility='private', active_version='v1', content_hash='0xabc...01')`；`INSERT skill_installations(user_id='laochen', skill_id=..., scope='session', session_id='sess-1', auto_activate_on_topic_match=FALSE)` — 注意走 scope='session' 而不是 'user'（v0.2 L2099 支持，避免 F4 污染其他 session）。
- **Activation 写入**：G16 要求——`INSERT session_state_items(scope='session', category='active_skill', item_key='go-code-review', payload_json={source_id:'src-gocr-1', version_id:'ver-v1', content_hash:'0xabc...01', activation_source:'user_explicit'})`（L2130–2140）；`INSERT session_state_item_events(mutation='activate', previous_hash=NULL, next_hash='0xabc...01')` — L1156–1165 枚举含 `activate`。
- **Manifest**：第 2 轮 `reason='skill_install'`（v0.2 G1 reason 枚举 L2038 附近含 lifecycle/skill_install，假定已注册——见 §5）；skills zone 1650 tokens（v1 全文）。
- **UI sees**：skill 面板显示"go-code-review v1 • draft • active"；版本抽屉 1 项；SSE `state_item_inserted{category='active_skill'}`。
- **LLM 看不到**：rust skill；system skill 库中未 install 的条目（skill selector 按 user ownership + installation 过滤 L2114）。

### T3 · 拿上周的 PR 当试用靶子
- **User**：「上周我合的 PR #4712 `service/auth/token.go`，你用 v1 跑一遍，列出它会命中 skill 里哪几条。」
- **Server**：`INSERT agent_runs(run_id='run-t3', session_id='sess-1', kind='skill_eval', root_goal='eval go-code-review v1 on PR#4712')`；agent 调 `read_pr_diff(pr_id='4712')` tool（diff 原文走 `session_artifacts` cold，不入 prompt，preview 400 tokens 入 recent_tail）；逐条 checklist 比对 diff，`agent_run_events(event_type='tool.invoked'/'tool.completed')` 10+ 条。
- **State Δ**：本轮**不写 `user_skill_versions`**（skill 未变）；`active_skill` item 不变；PR diff artifact `session_artifacts(kind='pr_diff', normalize_version='fetch_url_v1', content_hash=..., token_estimate=3200)`。
- **Manifest**：reason='large_tool_output_gated'；skills zone 仍 1650（v1 全文）；recent_tail 含 diff preview。
- **UI sees**：skill 面板顶部小 badge "evaluating v1 on PR#4712"；SSE run 进度。
- **判断**：✅ 机制顺；friction—T3 仅读 PR、没写 eval 行，本轮"skill_eval"这个 category 被 v0.2 弃用（见 T4），实现需要提前知道 evaluation 应写 `user_skill_evaluations` 表而不是 state item。

### T4 · v1 试用完成 — 4 hits / 2 suspects / 0 FP
- **User**（看输出）：「`defer resp.Body.Close()` 可疑合理；但 `ctx 没传` 这个最该命中你没抓到——v1 里压根没这条。」
- **Server**：run-t3 完成，`UPDATE agent_runs SET status='completed'`；**v0.2 要求**走独立表：`INSERT user_skill_evaluations(evaluation_id='eval-v1-t3', source_id='src-gocr-1', version_id='ver-v1', user_id='laochen', run_id='run-t3', target_ref='pr:4712', hits=4, suspects=2, false_positives=0, missed_by_design=1, hit_rate=0.667, false_positive_rate=0.000, payload_json={missed:['ctx-propagation'], suspects_detail:[...]}, created_at=NOW())` — L2078–2096 定义的一等表，G16 明确 "stays out of session_state_items"（L2157–2159）。
- **关键验证（v0.2 和 S13 原文的冲突）**：S13 draft §2 A2 栏还在写 `session_state_items(category='skill_eval')` L51、L109–124；**v0.2 不允许这条路径**——`session_state_items.category` 枚举 L1130–1149 不含 `skill_eval`，只含 `active_skill`。实现端必须忽略 S13 原文的 category='skill_eval'，改走 `user_skill_evaluations`。context builder 在 skill_eval 显示时按 `SELECT ... WHERE version_id=? ORDER BY created_at DESC LIMIT 2` 读（L2151–2153 N=2）。
- **Manifest**：skills zone 1650 + warm skill_eval zone 180 tokens（最近 2 条 evaluation 的摘要 render）；total ~6000。
- **UI sees**：面板下方 evaluation 时间线第 1 条 "v1 • 4/2/0 • missed ctx"。
- **LLM 看不到**：`user_skill_evaluations.payload_json` 全文（只取 hits/suspects/false_positives + 一句 missed_by_design 摘要入 warm）。

### T5 · v2 提交（补 ctx propagation）
- **User**：「v2：加一条 `每个跨进程/跨 IO 函数首参必须是 context.Context`；打 tag `severity: blocker`。」（~1.8k tokens）
- **Server**：`INSERT user_skill_versions(version_id='ver-v2', source_id='src-gocr-1', version='v2', content_markdown=<v2>, normalize_version='skill_md_v1', content_hash='0xabc...02', token_estimate=1810, status='draft')` — **append-only**，v1 行一字不动；`UPDATE skills_registry SET active_version='v2', content_hash='0xabc...02' WHERE owner_user_id='laochen' AND source_type='user' AND skill_name='go-code-review' AND content_hash='0xabc...01'` — **CAS 条件**（F2 防守，用 prev_hash 做乐观锁）。
- **Activation 切换**：`UPDATE session_state_items SET payload_json.version_id='ver-v2', payload_json.content_hash='0xabc...02' WHERE session_id='sess-1' AND category='active_skill' AND item_key='go-code-review'`；`INSERT session_state_item_events(mutation='activate', previous_hash='0xabc...01', next_hash='0xabc...02')` — G16 L2144–2146 强调 `version_id` 在 active 时刻冻结，registry 之后变化不污染本 session 该记录。
- **Manifest**：reason='skill_install'（或需新 reason 'skill_version_bump'，见 §5）；skills zone 1810（v2 全文）；**v1 content_markdown 彻底退出 prompt**（仅 `token_estimate` 列保留在 DB，audit 可查）。
- **UI sees**：版本抽屉 2 项（v1 / v2），active 徽章移至 v2；SSE `state_item_updated{category='active_skill'}`。

### T6 · v2 再跑同一个 PR
- **User**：「同一个 PR，v2 再跑。」
- **Server**：新 `agent_runs(run_id='run-t6', kind='skill_eval')`；tool reuse 上轮 diff artifact（`content_hash` 命中，满足 G17+G9 dedup）；run 完成后 `INSERT user_skill_evaluations(evaluation_id='eval-v2-t6', version_id='ver-v2', run_id='run-t6', target_ref='pr:4712', hits=6, suspects=2, false_positives=0, false_positive_rate=0.000)`。
- **State Δ**：`user_skill_evaluations` 2 行（v1/v2 并存）；active 不变（仍 v2）；warm skill_eval zone 按 `(version_id=ver-v2) ORDER BY created_at DESC LIMIT 2` 拉——只命中 v2 的 1 条。v1 的 eval 在 active_version=v2 时**不渲染**（L2151 "for an active version"），只可 `GET /skills/user/go-code-review/evaluations?version=v1` 查历史。
- **Manifest**：skills zone 1810；skill_eval warm 180；total ~6200。
- **UI sees**：evaluation 时间线 "v2 • 6/2/0" 置顶；"v1 • 4/2/0" 折叠到历史 tab。
- **判断**：✅ artifact dedup 省 3200 tokens；minor—N=2 只取当前 active 版本的 eval 是对的（否则 rollback 时旧版 eval 会干扰判断），但 S13 §3 T6 描述"最近 2 条（v2 + v1 各 ~180）"与 v0.2 L2151 "for an active version" 矛盾，以 v0.2 为准。

### T7 · v3 提交（goroutine 泄漏）— 误报率 40%
- **User**：「v3：`每个 go func() 必须有显式退出——ctx.Done() / channel close / errgroup 收敛`，漏一个 blocker。」
- **Server**：`INSERT user_skill_versions(ver-v3, content_hash='0xabc...03', token_estimate=2040, status='draft')`；`UPDATE skills_registry SET active_version='v3', content_hash='0xabc...03' WHERE content_hash='0xabc...02'`；`UPDATE session_state_items ... version_id='ver-v3'`；`INSERT session_state_item_events(mutation='activate', previous_hash='0xabc...02', next_hash='0xabc...03')`；新建 `run-t7` 跑同 PR。结果：`INSERT user_skill_evaluations(evaluation_id='eval-v3-t7', version_id='ver-v3', hits=9, suspects=1, false_positives=6, false_positive_rate=0.400, payload_json={fp_cases:['server.go:mainLoop', 'worker.go:pool#3', ...]})`。
- **质量信号**：`false_positive_rate=0.400`（S13 §3 T7 文字称 40%，实际值 6/(9+6)=0.400 或 6/8=0.75 取决于分母定义，v0.2 没规定分母口径——**见 §5 新 gap**）。无自动动作：v0.2 L2154 "quarantined versions cannot be auto-activated" 但没说何时**自动转** quarantined。
- **Manifest**：skills zone 2040；skill_eval warm 180（只 v3 的 1 条，v1/v2 折叠）；total ~6700。
- **UI sees**：面板顶部红 badge "v3 • false_positive_rate=0.40 • consider rollback"——**这是 UI 职责**，v0.2 context builder 无此告警机制。
- **LLM 看不到**：v1/v2 的 content；v1/v2 的 eval 详情。

### T8 · "切回 v2 用" — UI structured event 不走 LLM
- **User**：「误报太多，v3 先放着，切回 v2。」
- **Server 关键路径**：UI 侧 skill 面板按钮 → `POST /skills/user/go-code-review/activate` body `{version_id:'ver-v2'}` + `Idempotency-Key: hash(sess-1,ver-v2,ts)`（L2755）。Server：`UPDATE skills_registry SET active_version='v2', content_hash='0xabc...02' WHERE content_hash='0xabc...03'` — CAS 防 F2 race；`UPDATE session_state_items ... payload_json.version_id='ver-v2', payload_json.content_hash='0xabc...02'`；`INSERT session_state_item_events(mutation='activate', previous_hash='0xabc...03', next_hash='0xabc...02', payload_json={source:'ui_structured_event', activation_source:'user_explicit'})`。**G12 L676–677** 语义：`structured_event hit → suggestion(status=accepted, source=structured_event, confidence=1.0)`，LLM 不参与回溯 v2 内容。
- **v3 状态**：`user_skill_versions.status` 维持 `draft`（S13 §3 T8 说"可打 quarantined 但文档未定何时"——v0.2 L2154 枚举已含 quarantined，**但 auto-quarantine 触发条件仍缺**，见 §5）。
- **Manifest**：skills zone 1810（v2 content_markdown 回到 prompt，**非从 cache 重算**——因 content_hash 稳定满足 G17 prompt-cache 前缀复用条件）；v3 content 从 prompt 消失。
- **UI sees**：active 徽章滑回 v2；SSE `state_item_updated{category='active_skill'}`，watermark `state_revision` bump。
- **LLM 看不到**：v3 content（虽然 1 轮前还在，但这轮 manifest 重算后被替换）；但 v3 的 eval `false_positive_rate=0.400` 这条**也不在 warm**（因 active 现在是 v2，按 L2151 "for an active version" 只取 v2 的 eval）。**注意**：这制造了 friction——老陈明明是因为"v3 误报高"才切回，但 v2 active 后 prompt 里看不到 v3 FP 证据，LLM 下一轮若问"为什么切回 v2"得靠 transcript recent_tail 里的用户原话（见 §4 F3）。

### T9 · v4 提交（加白名单正则）
- **User**：「v4：goroutine 检查加白名单——`^run[A-Z]|Serve$|mainLoop$` 豁免。」
- **Server**：`INSERT user_skill_versions(ver-v4, content_hash='0xabc...04', token_estimate=2210)`；activate v2→v4（events 链 `0xabc...02 → 0xabc...04`）；run-t9 跑同 PR；`INSERT user_skill_evaluations(eval-v4-t9, version_id='ver-v4', hits=8, suspects=2, false_positives=1, false_positive_rate=0.100)`。误报从 6 降到 1 可接受。
- **激活链完整性验证**：`SELECT * FROM session_state_item_events WHERE session_id='sess-1' AND category='active_skill' ORDER BY event_idx` 返回 NULL→01, 01→02, 02→03, 03→02, 02→04 共 5 条——next_hash[i] 必须等于 previous_hash[i+1]（A2 审计链不断）。CAS 保护下该不变式成立。
- **Manifest**：skills zone 2210（v4 全文）；skill_eval warm 180（v4 的 1 条 eval）。
- **UI sees**：版本抽屉 4 项；"v4 • 8/2/1 ✓ acceptable" 绿 badge。

### T10 · v5 → v6 → v7 连提，v7 定稿
- **User**：「v4 的 regex 你理解飘；v5 改 yaml 结构化 `whitelist: {function_name_prefixes:['run'], function_name_suffixes:['Serve','MainLoop'], match_case_sensitive:true}`；v6 加两个正反 code block；v7 定稿。」
- **Server**：连 3 次 `INSERT user_skill_versions`（v5=0xabc...05/2180, v6=0xabc...06/2450, v7=0xabc...07/2520）；activate v4→v5→v6→v7；v5/v6 跑**短 smoke**（只扫新加规则涉及 5 个文件而非全 PR），各写 1 条 `user_skill_evaluations`；v7 跑**完整**（eval-v7: hits=8/suspects=1/FP=0/FP_rate=0.000）。
- **Publish**：`UPDATE user_skill_versions SET status='published' WHERE version_id='ver-v7'`；v1–v6 保留 `draft`（v0.2 L2154 枚举允许 `superseded`，老陈选择不批量转 superseded——留 draft 便于 rollback 复现，**但 v0.2 未规定 superseded 的触发时机**，见 §5）。
- **Installation**：若老陈想"以后新 session 自动加载"，需另发 `POST /skills/user/go-code-review/install` 升级 `skill_installations.scope='user'` 并 `auto_activate_on_topic_match=TRUE` L2102——本 session 的 scope='session' 行不变。
- **Manifest**：skills zone 2520（v7 全文，`normalize_version='skill_md_v1'` + content_hash='0xabc...07' 双字段命中 prompt-cache）；v1–v6 content_markdown 全部 cold。
- **UI sees**：版本抽屉 7 项，v7 `published` 徽章；evaluation 时间线 5 行（v1/v2/v3/v4/v7 — v5/v6 smoke 另开子 tab）。
- **LLM 看不到**：v1–v6 content；仅看 v7 active 的 1 条最新 eval（eval-v7）。

### T11 · 跨 session："上周那版 rust 拿来对齐"
- **User**：「上周我写过一版 rust 的，结构应该差不多，拉来对齐一下。」
- **Server 关键路径（A15+G12）**：UI 发 `intent.skill_search{owner_user_id='laochen', skill_name_prefix='rust'}` structured event → server 对应 **`GET /skills/user?prefix=rust&visibility=private`**（L2751 基础 endpoint 存在，**但 query 参数 `prefix` 未在 API Additions 显式声明**，见 §5 新 gap）。SQL：`SELECT source_id, skill_name, created_at FROM user_skill_sources WHERE user_id='laochen' AND skill_name LIKE 'rust%' AND status='active'` — 命中 1 行 `src-rust-code-review`；第二步 `SELECT version_id, content_hash, content_markdown, token_estimate FROM user_skill_versions uvs JOIN skills_registry sr ON uvs.source_id=sr.source_id WHERE uvs.source_id='src-rust-code-review' AND uvs.version=sr.active_version`。
- **引入 prompt**：**不激活**（本 session active_skill 仍是 go-v7）；写 `INSERT session_state_items(scope='session', category='skill_reference', item_key='rust-code-review@active', payload_json={source_id:'src-rust-code-review', version_id:'ver-rust-v1', content_hash:'0xdef...01', token_estimate:2100})`。**但 `skill_reference` 不在 L1130–1149 category 枚举里**——需要新增（见 §5 新 gap）或复用 `active_skill` + 新增 `role='reference'` 字段。
- **Manifest**：reason 需新枚举 `cross_skill_alignment` L2038–2407 Budget Policy 附近没列；skills zone 2520（go v7） + 2100（rust active 临时）= 4620；`reason` 若无注册走 `context_manifests.reason` 校验会拒（G1 L52 "unknown reason rejected"）。
- **权限**：SQL 必带 `user_id='laochen'` 过滤（F5），否则可能泄漏其他 user 的 rust skill。
- **UI sees**：对话面板出现"已引入 rust-code-review（仅本轮参考，不激活）"提示；skill 面板 active 仍 go-v7 不变。

### T12 · agent 对齐两版 skill，给 diff 建议
- **User**：「diff 列一下：前言字段、checklist 分组、白名单语法三轴。」
- **Server**：无新 skill version 写入；agent 读 `active_skill` item + `skill_reference` item 中的两份 content_markdown，生成对齐分析；`INSERT session_artifacts(artifact_id='art-diff-1', kind='skill_diff', normalize_version='skill_diff_v1', content_hash=..., ref='go-v7-vs-rust-v1', preview_text=<400 tokens>, byte_size=8200)` — **`skill_diff_v1` normalize_version 未在 G17 L2182–2188 baseline 表列出**（见 §5）；`INSERT session_state_items(category='decision', item_key='skill-structure-alignment', payload_json={aligned:[...], divergent:[...], followup_action:'update-rust-frontmatter-next-session'})`。
- **Manifest**：本轮末 rust 内容离场 → skills zone 回到 2520；recent_tail 里保留 diff artifact preview_text ~400 tokens；若老陈下一轮再追问 rust 细节，需重新查 `skill_reference` 或打开 artifact。
- **UI sees**：右侧 artifact 抽屉弹出 `skill_diff` 卡片（可下载）；decision 时间线多一条"alignment 待下 session 改 rust 前言"。
- **LLM 看不到**：v1–v6 go content；rust 历史版本（只取 active）；其他 user 的 skill。

---

## §4 发现的别扭点（Frictions）

### F1 · `skill_reference` 与 `cross_skill_alignment` 两个枚举值在 v0.2 未登记（T11）
- **Ref**: T11 / L1130–1149 (session_state_items.category 枚举) + L2038–2407 (context_manifests.reason 未展示完整枚举，G1 声称已枚举但文档没贴 canonical 列表)
- **Observation**: 老陈要把 rust skill 临时拉进 prompt，但 `session_state_items.category` 没有适合的条目（`active_skill` 是"选中激活"语义，不是"参考对照"），`context_manifests.reason` 也没 `cross_skill_alignment`。G1 校验会直接拒写。
- **Why friction**: 实现时要么违反 G1 扩新 reason、要么复用 `active_skill` 塞 `role` 字段（payload_json 里扩）、要么暂存 recent_tail 里的对话 prose（丢掉结构化）。三种都是退让。
- **Severity**: notable

### F2 · `user_skill_evaluations` vs 原 S13 §2 所写 `session_state_items(category='skill_eval')` 的 migration gap（T4）
- **Ref**: T4 / L2078–2159 (v0.2) vs S13 §2 A2 L51 + §3 T4 L120
- **Observation**: S13 场景原文是 Draft v0.1，早于 v0.2；它定义的"skill_eval 作为 state item"路径在 v0.2 G16 已被替换为独立表 `user_skill_evaluations`。实现团队如果照抄 S13 §2 去写 `session_state_items(category='skill_eval')`，会命中 L1130–1149 的 category 校验失败。
- **Why friction**: 文档内部不一致；读者可能在 S13 §2 和 v0.2 §9 之间反复横跳。应在 S13 §2 加 v0.2 修订注释。
- **Severity**: notable

### F3 · T8 切回 v2 后 v3 的误报证据从 prompt 消失
- **Ref**: T8 / L2151 "most recent N=2 for an active version"
- **Observation**: `user_skill_evaluations` warm 渲染只按当前 active version 拉，切回 v2 后 LLM 只能看到 v2 的 eval，看不到"v3 误报 6 个"这条决策依据。若 LLM 下一轮被问"为啥切回 v2"，它得靠 recent_tail transcript 里老陈那句"误报太多"，碰上 compaction 会丢。
- **Why friction**: "active-only eval warm" 是合理节流，但对"rollback 上下文"这个用例伤害大。建议 warm 增加"上一个 active version 的最近 1 条 eval"的豁免路径。
- **Severity**: notable

### F4 · v3 `false_positive_rate` 分母口径未定义
- **Ref**: T7 / L2085–2090 (user_skill_evaluations 字段定义)
- **Observation**: v0.2 列 `hit_rate DECIMAL(8,4)` 和 `false_positive_rate DECIMAL(8,4)` 但未写 SQL 计算公式——是 `FP / (hits + FP)` 还是 `FP / (hits + suspects + FP)`？S13 原文 T7 写 "6/15=40%" 暗示分母是 `hits + FP`，但若 suspects 也算 denominator 则 6/(9+1+6)=37.5%。
- **Why friction**: 下游 quality gate（L2914）`WHERE false_positive_rate > 0.3` 阈值要可比，口径摇摆会造成 skill 间不公。
- **Severity**: minor

### F5 · Auto-quarantine 触发条件 v0.2 未明（T7/T8）
- **Ref**: T7+T8 / L2154 (quarantined 允许但不能 auto-activate) + L2155 空白
- **Observation**: v3 误报率 0.400 已达到任何合理阈值（默认 0.3），v0.2 定义了 `quarantined` 状态但没规定"谁/何时把 status 翻到 quarantined"。S13 §8 反馈 3 点明提，但 G16 response L668–690 只说了 "Quarantined versions cannot be auto-activated"——反方向没说。
- **Why friction**: 老陈只能手动切回；未来 10 个 skill × 每周 1 次迭代的规模下这是 ops 负担。应定义后台 job：`UPDATE user_skill_versions SET status='quarantined' WHERE version_id IN (SELECT version_id FROM user_skill_evaluations WHERE false_positive_rate > 0.3 AND created_at > NOW()-INTERVAL 7 DAY GROUP BY version_id HAVING COUNT(*) >= 3)`。
- **Severity**: notable

### F6 · `/skills/user?prefix=...` 查询参数未在 API Additions 声明（T11）
- **Ref**: T11 / L2751 (`GET /skills/user` 仅 endpoint 名，无 query 参数)
- **Observation**: T11 跨 session 查 rust skill 需要 prefix 过滤，但 L2751 裸签名下 `prefix` / `name` / `limit` 都没定义；UI 无法可靠调用。A15 要求"走 structured path 不走 RAG"，但 structured path 的入参没 spec。
- **Why friction**: 前端实现时要么猜参数（可能 400）、要么退化成 RAG（违反 A15）、要么走 `GET /skills/user` 全拉再前端过滤（用户 skill 多时浪费）。
- **Severity**: notable

### F7 · Budget Policy v1 未列 `skills` zone 的 hard cap（T10/T11）
- **Ref**: L2385–2407 (Zone 表没列 skills zone 行) + L2410–2440 (8k template 也没列)
- **Observation**: Budget Policy v1 的 zone 表包含 "Plan/todos"/"Recent tail"/"Summary" 等但缺 `skills` zone 条目；T11 skills zone 4620 tokens 是否越界没参照。G18 补齐了 `delegation_state` 但 skills 漏补。
- **Why friction**: 定稿 v7 2520 tokens 已经不小；若 skill 作者写到 4k+（加 code examples）context builder 该如何降级未知。
- **Severity**: minor

---

## §5 发现的新 gap 候选（v0.2 未覆盖）

### 候选 G20 · Auto-Quarantine Trigger 未契约化
- **title**: Personal skill auto-quarantine 触发条件缺
- **triggered_by_turn**: T7（v3 FP rate 0.400 后系统无反应）
- **related_v0.2_section**: §9 Personal Skills L2154 + G16 L2120–2161
- **severity**: medium
- **rationale**: v0.2 允许 `status='quarantined'` 且禁止其 auto-activate，但从未规定"何时/何进程把 status 翻到 quarantined"。F5 展开：这是 schema ≠ 行为契约的典型漏项，运行时会发现"定义了但没人写"。
- **proposed_fix**: 新增 §"Skill Quality Gate"，定义后台 job 和阈值：`false_positive_rate > 0.3` 且 `COUNT(evaluations) >= 3` 在 7 日内 → 写 `status='quarantined'` + 事件 `skill.auto_quarantined`，skill selector 同步过滤。

### 候选 G21 · Cross-Session Skill Search API 入参未声明
- **title**: `GET /skills/user` 缺 `prefix` / `owner_user_id` query 参数 spec
- **triggered_by_turn**: T11（intent.skill_search → server 端点入参缺）
- **related_v0.2_section**: §API Additions L2751 + A15 约束（structured 不走 RAG）
- **severity**: low
- **rationale**: F6 展开。A15 的 structured path 依赖 API 入参契约，当前只给 endpoint 名；等前端实现时再设计入参会造成口径漂移。
- **proposed_fix**: L2751 扩成 `GET /skills/user?prefix=&name=&visibility=&status=&limit=&cursor=`，在 API Additions 表里列清每参的类型和默认值。

### 候选 G22 · `session_state_items.category` 缺 `skill_reference` 与 `cross_skill_alignment`
- **title**: 跨 skill 临时引入 prompt 的 category / reason 未枚举
- **triggered_by_turn**: T11–T12
- **related_v0.2_section**: L1130–1149 (category 枚举) + G1 reason 枚举 L2038
- **severity**: medium
- **rationale**: F1 展开。T11 "把别的 skill 拉来参考"是真实需求（alignment / template reuse / comparison），v0.2 只有 `active_skill` 一个相关条目，语义是"激活"而非"参考"。硬塞 active_skill 会破坏 G16 "skill zone 只渲染 active version" 的单一性假设。
- **proposed_fix**: 扩 `category` 枚举加 `skill_reference`（可带 role='reference'），reason 枚举加 `cross_skill_alignment`；Skills zone 渲染规则明确"active 1 版 + reference ≤2 版，total ≤ 6k"。

### 候选 G23 · `false_positive_rate` 分母口径未定义
- **title**: `user_skill_evaluations` 比率列缺计算公式
- **triggered_by_turn**: T7
- **related_v0.2_section**: L2085–2090 (列定义仅给类型)
- **severity**: low
- **rationale**: F4 展开。列存在但公式不在 schema 也不在 doc 章节，三个团队会写出三种口径，跨 skill 聚合（quality gate / 看板）失真。
- **proposed_fix**: 在 §9 附小节给公式：`hit_rate = hits / GREATEST(hits + suspects + false_positives, 1)`；`false_positive_rate = false_positives / GREATEST(hits + false_positives, 1)`；并用 generated column 实现，insert 时只写原始计数。

### 候选 G24 · Skills zone 未纳入 Budget Policy v1 zone 表
- **title**: Budget Policy v1 zone 表缺 `skills` 和 `skill_eval` 行
- **triggered_by_turn**: T10/T11
- **related_v0.2_section**: L2385–2397
- **severity**: low
- **rationale**: F7 展开。G18 补了 delegation_state 但 skills 系列漏补；T11 临时双 skill 同场 4.6k tokens，无 cap 也无降级策略。
- **proposed_fix**: zone 表新增：`skills` 硬 cap 3500（单 active）/软 cap 6000（含 reference）；`skill_eval` cap 600；降级顺序"drop reference skill → collapse eval to count-only → drop oldest eval"。

---

## §6 Gap 组合效应检验

### 组合 G16 + G17 + G12（T5 / T8 激活流）
- **运行时坑**: T5 v2 activate 触发 `UPDATE skills_registry ... WHERE content_hash='0xabc...01'` CAS。若同一秒老陈在另一 tab 也点"切到 v3"，两条 structured_event（G12 confidence=1.0 auto-accept）都会发到 server。第一条成功，第二条 CAS 失败。G16 要求 `version_id` 冻结进 session state item，两条 intent 尝试写同一 item 需 state_revision 并发保护。G17 的 `content_hash` 既做内容指纹也做 CAS 键，依赖规范化稳定（v0.2 L2180–2188 已给规则）。
- **v0.2 是否覆盖**: partial — G16 说"version_id frozen at activation time"（L2145–2146），G17 规定 SKILL.md canonical 输入，但**"同 session 内并发激活请求"**这个冲突路径没有明说只能靠 G19 watermark。
- **建议**: 在 G16 rules 列表末加一行 "activate 必须带 prev_content_hash 做 CAS，失败返回 409 + 当前 active hash"。

### 组合 G6 + G16 + scope（T2 install scope 选择）
- **运行时坑**: T2 老陈首次安装时，UI 默认 `scope='user'`（G6 跨 session）还是 `scope='session'`（G16 per-session freeze）？S13 场景意图是本 session 迭代、其他 session 不受影响，应选 'session'；但若不小心选了 'user'+`auto_activate_on_topic_match=TRUE`，老陈明天开新 session 写无关代码也会被 v7 污染 skills zone。v0.2 L2099 给了 scope 字段但没给 UI 默认值建议。
- **v0.2 是否覆盖**: partial — scope 字段存在（L2099），auto_activate 字段存在（L2102），但 T2 这种"iterative authoring session"的推荐默认没写进 publish flow（L2106–2115）。
- **建议**: publish flow 里显式："authoring mode → scope='session'；publish to user → 提示升级 scope='user' 并提示 auto_activate 副作用"。

### 组合 G16 + G18 + Budget（T11 跨 skill 同场）
- **运行时坑**: T11 引入 rust skill 的 2100 tokens + go-v7 2520 tokens = 4620；若同时 session 也在做 delegation（假设有 3 个子 agent），G18 的 `delegation_state` zone 占 1500，加上 skills 4620 + tool_schemas 1400 + system_static 1200 + recent_tail 1800 + anchor 400 = 10920；还好 200k 窗口扛得住。但若换 `budget_v1_8k`（L2410），8k 总 budget 里 skills 4620 直接挤爆。G24 缺 cap，8k 时得降级——但降级优先级"drop reference skill"与 G18 "drop tool schemas first" 的竞争未定序。
- **v0.2 是否覆盖**: no — Budget Policy v1 的降级顺序没写 skills/reference 条目；8k template 只规定 tool schemas 和 retrieved facts 先裁。
- **建议**: 给 8k template 加补丁："若 skills zone > 2000 则拒绝引入 reference skill（返回 UI 提示'切到大窗口模式再对齐'）"。

---

## §7 Token 实际 walkthrough（T10 v7 定稿激活那一轮）

**现场状态**：第 10 轮，老陈刚点击 v7 `publish + activate`；skills zone 装 v7 content_markdown（含 yaml manifest + 8 条 checklist + 2 个 code example block + whitelist yaml）=2520 tokens。

| Zone | 计算过程 | 占用 |
| --- | --- | ---: |
| `system_static` | 默认 system prompt + cold agent manifesto，基线不变 | 1200 |
| `tool_schemas` | 本场景工具：`read_pr_diff`(320) + `fetch_url`(260) + `SkillMd.validate`(180) + `search_user_skills`(240) + 其余杂项 ~400 | 1400 |
| `skills` (active only) | v7 content_markdown 2520（yaml 前言 180 + 8 条 checklist 约 1400 + whitelist yaml 240 + 2 个 code block 700） | 2520 |
| `session_state_items(active_skill)` | 本 session active_skill item payload_json 序列化 ~90 tokens | 90 |
| `skill_eval` warm（N=2 for v7，实际 v7 只 1 条 eval-v7） | `SELECT ... WHERE version_id='ver-v7' ORDER BY created_at DESC LIMIT 2` 返 1 行，渲染 "v7@pr4712: 8/1/0, fp_rate=0.000" ~90 tokens | 90 |
| `session_anchor` | intent + mode='iterative' + target_pr='#4712' + decision="v7 is canonical" | 350 |
| `recent_tail` | 最近 2–4 轮 transcript：老陈提 v5/v6/v7 的 prose + agent smoke eval 响应（剪裁后） | 1800 |
| `summary`（L1b） | 本 session 短，无需 L1b summary | 0 |
| `workspace_state` | cloud workspace identity + PR context | 200 |
| `delegation_state` | 无子 agent | 0 |
| `tool_previews` | PR diff preview 400 + v7 smoke eval preview 150 | 550 |

**合计 prompt 前**：`1200 + 1400 + 2520 + 90 + 90 + 350 + 1800 + 0 + 200 + 0 + 550 = 8200 tokens`。

**对比预留**：Claude Sonnet 4.5 200k 窗口 - reserved output 8000 - safety buffer 2000 = 190k 可用；8200 占 ~4.3%，远低于任何 cap。

**G24 Budget Policy 对比**：v0.2 没列 skills/skill_eval cap，参照建议 3500/600 软 cap，2520/90 都在范围。

**T11 同题对比**：T11 引入 rust skill 临时入场后 skills zone 跳到 `2520 + 2100 = 4620`；加上 skill_reference state item 约 80，同一轮总量 `8200 - 2520 + 4620 + 80 = 10380` 仍宽裕；8k template 下该轮会 overflow（见 §6 组合 3）。

**关键结论**：
- **省在哪**：v1–v6 六版 content_markdown（~12k tokens 合计）从未入 prompt，只 audit 可查；v1–v6 的 4 条历史 evaluation 也不 warm（L2151 限定 active version）。朴素方案会塞 ~15.4k skills + ~2.4k eval = ~18k；v0.2 实际 2520+90=2610。**节省 ~15.4k tokens，占 200k 窗口 7.7%**，对 8k 窗口则是从"爆炸"到"可运行"。
- **代价**：LLM 看不到 v3 FP 证据（F3）；看不到 v5/v6 演进 rationale（只在 transcript tail，compaction 后丢）。这是正确取舍但需 UI 在必要时"拉历史 eval"把证据回灌。

---

## §8 最终判断

- **本场景下 v0.2 是否可实现**: ⚠️ **绝大部分可实现**
  - ✅ A12 append-only + active_version 指针切换：完全可实现（§9 L2057–2076 + G16 L2120–2161）。
  - ✅ A2 三表不互相替代：可实现（`user_skill_versions` cold / `user_skill_evaluations` warm aggregation / `session_state_item_events` audit）。
  - ✅ A15 structured intent 不走 LLM：可实现（G12 L676–677 + `POST /skills/user/{skill_name}/activate` L2755）。
  - ✅ G17 content_hash 规范化：**2 次提交相同 yaml 但 key 顺序不同 → 规则 L2188 "Sort YAML/manifest keys" 保证同 hash**；F1 原文担忧已闭环。
  - ⚠️ 跨 session rust 查找：逻辑可实现，但 `GET /skills/user?prefix=` 参数未显式声明（G21）；实现团队需先补接口 spec。
  - ⚠️ T7 v3 FP rate 0.400 → 自动 quarantine：**v0.2 定义了状态但没定义触发**（G20）。本场景靠老陈手动 T8 切回，短期可运行；长期 ops 负担。
  - ⚠️ T11 skill_reference 跨 skill 引入：category + reason 枚举未列（G22）；硬跑会被 G1 校验拒，需要先扩枚举或降级复用 active_skill。

- **顺畅度**: ⚠️ **有明显摩擦**（7 处 friction：2 处 notable-implementation、3 处 notable-UX、2 处 minor-spec）
  - 主摩擦集中在"warm context 的取舍（F3）" 和 "枚举未全集（F1/F5/F6）"。
  - v0.2 相比 v0.1（S13 原文依据的版本）已经闭环了最硬的 3 个：hash 规范化、eval 独立表、activation session scope。
  - 剩余摩擦都是"明确已定的 schema 边缘还有未定的流程契约"——小 PR 即可补齐。

- **一句话总结**：**v0.2 把 S13 的核心骨架（append-only + per-session freeze + structured rollback）打得扎实；场景端到端 mechanically 跑得通，但"auto-quarantine 触发、skill_reference 枚举、cross-session search API 入参、FP 分母口径"这 4 个周边契约缺口在首批压测时会立刻暴露，建议随 Phase 5 一起打 4 个小补丁（G20–G23）再上线。**


---

## §9 附录：端到端 SQL 轨迹（verification oracle）

本附录给出**逐 T 的 SQL 断言序列**，用作回归测试的 oracle。每行是一次 state 变更后可运行的 assertion，对照 §3 的 T1–T12。

### T1 后
```sql
-- session 已建
SELECT 1 FROM agent_sessions WHERE session_id='sess-1' AND user_id='laochen';
-- intent anchor 已写，skill source 尚未建
SELECT COUNT(*) FROM session_state_items
  WHERE session_id='sess-1' AND category='session_anchor' AND item_key='intent'; -- = 1
SELECT COUNT(*) FROM user_skill_sources WHERE user_id='laochen' AND skill_name='go-code-review'; -- = 0
SELECT COUNT(*) FROM session_state_items WHERE session_id='sess-1' AND category='active_skill'; -- = 0
```

### T2 后（v1 首装 + activate）
```sql
-- append-only source + version 各 1 行
SELECT COUNT(*) FROM user_skill_sources WHERE source_id='src-gocr-1'; -- = 1
SELECT COUNT(*) FROM user_skill_versions WHERE source_id='src-gocr-1' AND version='v1'; -- = 1
-- normalize_version 已写
SELECT normalize_version FROM user_skill_versions WHERE version_id='ver-v1'; -- = 'skill_md_v1'
-- registry active 指针 = v1
SELECT active_version, content_hash FROM skills_registry
  WHERE owner_user_id='laochen' AND source_type='user'
    AND visibility='private' AND skill_name='go-code-review'; -- ('v1','0xabc...01')
-- session-scope installation
SELECT scope, session_id FROM skill_installations
  WHERE user_id='laochen' AND skill_id=(SELECT id FROM skills_registry WHERE ...);
  -- ('session','sess-1')
-- session-scope active_skill state item payload 含 frozen version_id
SELECT JSON_EXTRACT(payload_json,'$.version_id'),
       JSON_EXTRACT(payload_json,'$.content_hash'),
       JSON_EXTRACT(payload_json,'$.activation_source')
FROM session_state_items
  WHERE session_id='sess-1' AND category='active_skill' AND item_key='go-code-review';
  -- ('ver-v1','0xabc...01','user_explicit')
-- activate 事件 1 条
SELECT previous_hash, next_hash FROM session_state_item_events
  WHERE category='active_skill_ref' AND mutation='activate'
  ORDER BY event_idx ASC;  -- [(NULL, '0xabc...01')]
```

### T4 后（v1 eval 已写）
```sql
-- 独立表一等记录
SELECT hits, suspects, false_positives, missed_by_design,
       hit_rate, false_positive_rate, run_id, target_ref
FROM user_skill_evaluations
  WHERE version_id='ver-v1';  -- (4, 2, 0, 1, 0.667, 0.000, 'run-t3', 'pr:4712')
-- 没有任何 skill_eval 被错写进 session_state_items
SELECT COUNT(*) FROM session_state_items
  WHERE category='skill_eval';  -- = 0（v0.2 category 白名单不含此值）
```

### T7 后（v3 误报率 0.400）
```sql
-- 3 个 version 并存
SELECT version, status FROM user_skill_versions
  WHERE source_id='src-gocr-1' ORDER BY created_at;
  -- [(v1, draft), (v2, draft), (v3, draft)]
-- 每版各 1 条 eval
SELECT version_id, false_positive_rate FROM user_skill_evaluations
  WHERE source_id='src-gocr-1' ORDER BY created_at;
  -- [(ver-v1, 0.000), (ver-v2, 0.000), (ver-v3, 0.400)]
-- 激活链 NULL→01→02→03
SELECT previous_hash, next_hash FROM session_state_item_events
  WHERE session_id='sess-1' AND category='active_skill_ref'
    AND mutation='activate' ORDER BY event_idx;
  -- [(NULL,01),(01,02),(02,03)]
-- v3 active、尚未 quarantined（G20 缺失证据）
SELECT active_version FROM skills_registry WHERE skill_name='go-code-review' AND owner_user_id='laochen';
  -- 'v3'
SELECT status FROM user_skill_versions WHERE version_id='ver-v3';  -- 'draft'，非 'quarantined'
```

### T8 后（切回 v2，structured event）
```sql
-- 激活链 extend 一条 03→02
SELECT previous_hash, next_hash FROM session_state_item_events
  WHERE session_id='sess-1' AND category='active_skill_ref'
    AND mutation='activate' ORDER BY event_idx DESC LIMIT 1;
  -- ('0xabc...03', '0xabc...02')
-- payload 里 activation_source 指明结构化来源
SELECT JSON_EXTRACT(payload_json,'$.activation_source')
  FROM session_state_item_events
  WHERE event_idx = (SELECT MAX(event_idx) FROM session_state_item_events
                     WHERE session_id='sess-1' AND category='active_skill_ref');
  -- 'ui_structured_event'
-- session active_skill item 已 mutate 不是 replace（保持历史可 diff）
SELECT mutation FROM session_state_item_events
  WHERE session_id='sess-1' AND category='active_skill' ORDER BY event_idx DESC LIMIT 1;
  -- 'update'（payload_json.version_id 字段变了）
```

### T10 后（v7 定稿）
```sql
-- 7 版齐全，v7 published，v1..v6 draft
SELECT version, status FROM user_skill_versions
  WHERE source_id='src-gocr-1' ORDER BY version;
  -- v1..v6=draft, v7=published
-- 激活链累计 7 条
SELECT COUNT(*) FROM session_state_item_events
  WHERE session_id='sess-1' AND category='active_skill_ref' AND mutation='activate'; -- = 7
-- 5 条 evaluation
SELECT COUNT(*) FROM user_skill_evaluations WHERE source_id='src-gocr-1'; -- = 5
-- warm prompt 只会取 v7 的 eval（active version filter）
SELECT COUNT(*) FROM user_skill_evaluations
  WHERE version_id='ver-v7'
  ORDER BY created_at DESC LIMIT 2;  -- = 1
-- content_hash normalization：v7 相同字节重 submit 应 hash 相同
-- （golden test 断言 G17 规则生效）
SELECT COUNT(DISTINCT content_hash) FROM user_skill_versions
  WHERE source_id='src-gocr-1'; -- = 7（7 版每版内容确有差异）
```

### T11 后（rust skill 跨 session 查找）
```sql
-- structured path：user_id filter 必有（F5/G21）
EXPLAIN SELECT source_id FROM user_skill_sources
  WHERE user_id='laochen' AND skill_name LIKE 'rust%' AND status='active';
  -- uses idx_user_skill_status, rows ≤ small
-- 本 session active 不变
SELECT JSON_EXTRACT(payload_json,'$.version_id') FROM session_state_items
  WHERE session_id='sess-1' AND category='active_skill'; -- 'ver-v7'
-- 尝试写 skill_reference（会受 category 白名单约束 — G22）
-- 若 v0.2 未扩白名单，此 INSERT 应被 pre-commit 校验拒绝
```

---

## §10 Prompt cache 影响（T2/T5/T7/T8/T10）

Anthropic prompt cache 按前缀命中，以"前缀稳定 + 稀少插入"最省钱。skills zone 在 system/tool_schemas 之后、anchor/tail 之前。

| 轮次 | skills zone content | 前缀是否稳定 | 预期 cache 行为 |
| --- | --- | --- | --- |
| T2 | v1 全文（首次） | —（首轮） | cache miss，写入新前缀（1650 tokens） |
| T3 | v1（不变） | 稳定 | cache hit（1650 tokens 前缀复用），省 99% input cost |
| T5 | v2 全文（replace） | 不稳定 | cache miss，前缀改写（1810 tokens） |
| T6 | v2（不变） | 稳定 | cache hit |
| T7 | v3（replace） | 不稳定 | cache miss |
| T8 | v2（回滚） | **稳定？** | **若 hasher 保证 v2 两次生成字节完全相同，content_hash 相同 → 前缀与 T5/T6 的 v2 prompt 字节一致，理论 cache hit** |
| T9 | v4 | 不稳定 | cache miss |
| T10a | v5 | 不稳定 | cache miss |
| T10b | v6 | 不稳定 | cache miss |
| T10c | v7 | 不稳定 | cache miss |

**关键洞察**：G17 content_hash normalization 规则（yaml key 排序 + LF + trim）不仅是 hash 语义，**顺手保证了 prompt cache 前缀字节稳定**。没有 G17，老陈 T8 回滚到 v2 时，prompt 里的 v2 字节可能因空白/换行漂移而与 T5/T6 的 v2 字节不完全相同，导致 cache miss 白白多花一轮写入费用。G17 + "skills zone 只 render `content_markdown` 而不重新序列化"两件事合起来才真省钱。

这条省钱路径在 v0.2 没被写成 rule，但可从 G17 推出——建议在 §Context Assembly / Rendering Policy 下加一行："Skills zone rendering must emit exactly the bytes stored in `user_skill_versions.content_markdown`; do not re-canonicalize at render time."

---

## §11 回归测试清单（本场景必须过的断言）

按 DAUT 分组。凡失败需回 v0.2 对应章节修订，不在此打补丁。

### A12 · append-only + 指针切换
1. `user_skill_versions` 连续 7 次 INSERT 后，SELECT COUNT = 7；无 UPDATE 发生（`created_at` 分布递增）。
2. 任一 `version_id` 的 `content_markdown` 列被手工 UPDATE 应被 schema trigger 或应用层拒绝（§9 L2117–2118 "do not update historical skill content in place"）。
3. `skills_registry.active_version` 在 T2/T5/T7/T8/T9/T10a/T10b/T10c 共 8 次 mutation（T8 是回滚，也算一次），CAS 条件 `WHERE content_hash = <expected_prev>` 成立。
4. 同一 registry 行被两个 session 同时尝试切换 active_version 时，只有 1 个成功；失败方收到 409 带最新 content_hash（F2 防守）。

### A2 · 三表互不替代
5. `user_skill_versions` 只存 content（cold）；grep 整个 prompt 历史找不到 v1–v6 的 `content_markdown` 字面（T10 后验证）。
6. `user_skill_evaluations` 只存聚合（warm）；`session_state_items` 全表 SELECT `category='skill_eval'` 返 0 行。
7. `session_state_item_events` 激活链 7 条，任意相邻 `next_hash[i] == previous_hash[i+1]`（T9 末的链完整性）。
8. `context_manifests` 每轮 INSERT 1 行，`reason` 取值全部在 G1 枚举内。

### A15 · structured intent
9. T8 `POST /skills/user/go-code-review/activate {version_id:'ver-v2'}` 响应 200，且 `agent_run_events` 无任何 LLM 调用记录涉及该切换决策（审计：`SELECT * FROM agent_run_events WHERE session_id='sess-1' AND event_type LIKE 'llm.%' AND created_at BETWEEN ...`）。
10. T11 `GET /skills/user?prefix=rust&visibility=private` 200 + 1 行命中；SQL EXPLAIN 不触发 vector / FTS，只走 `idx_user_skill_status`。
11. T11 response 中 `user_id` filter 生效：伪造 `user_id='bob'` 的 session 发同请求应返 0 行或 403。

### G16 · per-session activation
12. T2 另起一个 session B（同 user laochen），其 `session_state_items(category='active_skill')` 为空；即使 skills_registry.active_version=v1，session B 的 prompt skills zone 也为 0 tokens。
13. T5 session A 切 v2 后，session B（若之前 activate 过 v1）的 active_skill.payload_json.version_id 仍是 'ver-v1'——**冻结不跟随** registry。
14. T10 old evaluations（v1/v2/v3/v4）在 active=v7 时**不进入** skill_eval warm zone；手动查询 API `GET /skills/user/go-code-review/evaluations?version=v3` 仍可返回。

### G17 · content_hash normalization
15. 粘贴同一份 SKILL.md 但 yaml key 顺序从 `name/scope/owner` → `owner/name/scope` 重排：`content_hash` 相同。
16. 粘贴时末尾多一个空行或改用 CRLF：`content_hash` 相同。
17. 在 markdown body 的 code fence 内插入等价空行（fence 内不归一）：`content_hash` **应不同**（G17 "collapse repeated blank lines outside code fences"）。
18. 改 `normalize_version='skill_md_v2'`（未来 bump）：即使字节相同，`content_hash` 列应不被等同判定（G17 L2190–2192）。

---

## §12 运营风险与回退策略

本节给运维团队：场景被压测到生产后可能被投诉的 3 类 ticket + 应对。

### R1 · "我切回 v2 了但 agent 还在按 v3 判误报"
- **诊断**：查 `session_state_items(category='active_skill').payload_json.version_id` 与 `skills_registry.active_version` 是否一致；若 A 已切 B 未切 → context_manifests 的上一轮 reason 应是 'skill_install' 或等价，若是旧轮的 'skill_eval' 说明 manifest 没重算。
- **回退**：强制 `POST /sessions/{id}/context/rebuild?dry_run=false`（L2727）。
- **根因**：G19 watermark 在高并发下偶发漂移 + G22 缺 reason 导致新版本写入但 manifest 命中旧缓存。

### R2 · "老陈昨天 v7 定稿，今天新 session 里 v7 没自动加载"
- **诊断**：`skill_installations.scope` 是不是 'session'（T2 默认）而非 'user'？`auto_activate_on_topic_match` 是不是 FALSE？
- **回退**：引导老陈在 skill 面板点"升级为常驻（user scope）"，server `UPDATE skill_installations SET scope='user', session_id=NULL, auto_activate_on_topic_match=TRUE WHERE user_id='laochen' AND skill_id=...`。
- **根因**：scope 默认值语义 under-documented（见 §6 组合 2）；publish flow 应提示升级。

### R3 · "我写过的 skill 查不到了"
- **诊断**：`user_skill_sources.status` 是否 'archived' 或 'deleted'？visibility 是否被改成 'deleted'？跨 session search 有没有带 status 过滤？
- **回退**：放宽查询 `WHERE status IN ('active','draft')`；若确实被 archived 需二次确认是老陈主动还是 bug。
- **根因**：F6 + G21——search API 入参未 spec，前端可能默认只查 'active'。

---

## §13 小结：v0.2 对 S13 的净增量

相对 S13 draft v0.1 原文（2024-*-月）的几处核心改进（已在 v0.2 落地）：

1. **`user_skill_evaluations` 一等表**（L2078–2096）：原 S13 §2 A2 写"skill_eval 作为 session_state_items category"被 G16 推翻；改为带 `false_positive_rate` generated/计算列的一等表，quality gate 可直接 `SELECT WHERE false_positive_rate > 0.3`，不再 JSON filter。
2. **`skill_installations.scope` 三态**（L2099）：原 S13 §8 F4 指出"active_version 到底对谁生效"的歧义；v0.2 落 ALTER 加 scope/session_id/workspace_id/auto_activate 字段。
3. **content_hash normalize_version 契约**（L2163–2194）：原 S13 §7 F1 + §8 建议 3；v0.2 G17 明确 SKILL.md 规则（YAML key 排序、LF、trim、code fence 保留）。
4. **per-session active_skill 冻结**（L2130–2146）：原 S13 §7 F4；v0.2 G16 payload_json.version_id 在 activate 时冻结，registry 后续 mutation 不追溯。
5. **skills zone 渲染规则**（L2150）："only active versions' content_markdown" 一锤定音。
6. **user_skill_evaluations warm 限 active + N=2**（L2151–2153）：明确 warm context 边界。
7. **status 枚举补 `superseded` / `quarantined`**（L2154）：给未来 G20 的 auto-quarantine 留位。
8. **API Additions 扩展 `/skills/user/*` 7 个端点**（L2751–2758）：替代原 S13 §8 建议里口述的 activate/install/evaluate 路径。

相对 v0.1 **未完全闭环**的 4 处（本 walkthrough 的 G20–G23 候选）：

9. **Auto-quarantine 触发（G20）**：v0.2 有状态无触发，运营会被 R2 式 ticket 打。
10. **skill search API 入参 spec（G21）**：T11 跨 session 查找的 structured path 入参还需写。
11. **skill_reference category + cross_skill_alignment reason（G22）**：T11/T12 跨 skill 临时引入 prompt 的 schema 位置还没准备好。
12. **false_positive_rate 公式（G23）**：跨 skill quality 比对的口径需写死。

这 4 处可以作为 Sprint D 的小 PR 包（schema 无破坏性迁移，枚举+check+API spec 补齐即可）。
